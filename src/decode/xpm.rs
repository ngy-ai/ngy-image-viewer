//! X PixMap（`.xpm`）解码：自写纯 Rust 解析器，零依赖。
//!
//! # 格式要点
//!
//! XPM 是纯文本的 C 字符串数组（标准开头是 `/* XPM */`）：
//!
//! ```text
//! static char *name[] = {
//!   "<width> <height> <num_colors> <chars_per_pixel>",
//!   "<key> c <color>", ...,
//!   "<pixel_row>", ...
//! };
//! ```
//!
//! - `chars_per_pixel`（cpp）通常 1，少数 2；像素行里每 `cpp` 个字符组成一个「颜色键」。
//! - 颜色行形如 `"a c #FF0000"` 或 `"a c red"`；键为 `none` / `transparent` 表示透明。
//! - 颜色值支持 `#RGB` / `#RRGGBB` / `#RRRGGGBBB` / `#RRRRGGGGBBBB` 与少量常见 X11 名。
//!
//! 没有现成纯 Rust 库能干净接进本项目的 [`Decoder`](super::Decoder) 架构，故自写。
//! 对不支持的变体（cpp 超出 1/2、无法识别的颜色键）一律**显式拒绝**，绝不交付错图。

use std::collections::HashMap;
use std::path::Path;

use super::Decoder;
use super::types::{DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat};

pub struct XpmDecoder;

impl Decoder for XpmDecoder {
    fn id(&self) -> &'static str {
        "xpm"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Xpm]
    }

    fn probe(&self, head: &[u8]) -> bool {
        let head_prefix = &head[..head.len().min(512)];
        !head_prefix.iter().any(|&b| b == 0)
            && {
                let text = String::from_utf8_lossy(head_prefix);
                text.contains("XPM")
                    && (text.to_ascii_lowercase().contains("/* xpm */")
                        || (text.to_ascii_lowercase().contains("static")
                            && text.contains("char")
                            && text.contains("*[")))
            }
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_path(src, limits)
    }
}

fn decode_path(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let bytes = std::fs::read(src).map_err(|error| DecodeError::io(src, error))?;
    let text = String::from_utf8_lossy(&bytes);

    let body = extract_array_body(&text)
        .ok_or_else(|| DecodeError::corrupt("XPM 找不到字符串数组 `{ ... }`"))?;
    let strings = parse_quoted_strings(body);
    if strings.is_empty() {
        return Err(DecodeError::corrupt("XPM 缺少头信息字符串"));
    }

    // 第一行：`<width> <height> <num_colors> <chars_per_pixel>`。
    let header: Vec<&str> = strings[0].split_whitespace().collect();
    if header.len() < 4 {
        return Err(DecodeError::corrupt("XPM 头信息不足（需要 宽 高 颜色数 每像素字符数）"));
    }
    let width = header[0]
        .parse::<u32>()
        .map_err(|_| DecodeError::corrupt("XPM 宽度不是整数"))?;
    let height = header[1]
        .parse::<u32>()
        .map_err(|_| DecodeError::corrupt("XPM 高度不是整数"))?;
    let num_colors = header[2]
        .parse::<usize>()
        .map_err(|_| DecodeError::corrupt("XPM 颜色数不是整数"))?;
    let cpp = header[3]
        .parse::<usize>()
        .map_err(|_| DecodeError::corrupt("XPM 每像素字符数不是整数"))?;
    if cpp == 0 || cpp > 2 {
        return Err(DecodeError::unsupported(
            Some(ImageFormat::Xpm),
            format!("当前版本仅支持每像素 1 或 2 个字符的 XPM（文件为 {cpp}）"),
        ));
    }
    limits.check_dimensions(width, height)?;

    if strings.len() < 1 + num_colors + height as usize {
        return Err(DecodeError::corrupt("XPM 数据行数不足（颜色表 + 像素行缺失）"));
    }

    // 颜色表：键 → (R, G, B, A)。遇到无法识别的颜色直接拒绝，避免交付错图。
    let mut palette: HashMap<String, (u8, u8, u8, u8)> = HashMap::with_capacity(num_colors);
    for line in &strings[1..1 + num_colors] {
        // 键是**行首的 cpp 个字符**，不是第一个空白分隔的词 —— 空格本身就是合法键。
        // `"  c None"`（以空格为键、意义为透明）是很多编辑器生成 XPM 时的标准写法，
        // 用 `split_whitespace()` 取词会把这种键整个吃掉，像素行里就永远查不到它。
        let Some((key, rest)) = line.split_at_checked(cpp) else {
            return Err(DecodeError::corrupt(format!(
                "XPM 颜色行的键长度不足（需要 {cpp} 个字符）：{line}"
            )));
        };
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        // 类型标记 c/m/g/s 之后才是真正的颜色说明；过滤掉类型标记，取最后一个候选。
        let is_type_marker = |t: &&str| {
            t.eq_ignore_ascii_case("c")
                || t.eq_ignore_ascii_case("m")
                || t.eq_ignore_ascii_case("g")
                || t.eq_ignore_ascii_case("s")
        };
        let color = tokens
            .iter()
            .filter(|t| !is_type_marker(t))
            .last()
            .copied()
            .ok_or_else(|| DecodeError::corrupt(format!("XPM 颜色行缺少有效的颜色说明：{line}")))?;

        if color.eq_ignore_ascii_case("none") || color.eq_ignore_ascii_case("transparent") {
            palette.insert(key.to_string(), (0, 0, 0, 0));
        } else {
            let rgb = parse_color(color).ok_or_else(|| {
                DecodeError::unsupported(
                    Some(ImageFormat::Xpm),
                    format!("XPM 包含无法识别的颜色「{color}」，请改用 #RRGGBB 等写法或常见颜色名"),
                )
            })?;
            palette.insert(key.to_string(), (rgb.0, rgb.1, rgb.2, 255));
        }
    }

    // 像素行。
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        let row = &strings[1 + num_colors + y as usize];
        if row.len() < width as usize * cpp {
            return Err(DecodeError::corrupt("XPM 像素行长度不足"));
        }
        for x in 0..width {
            let key = &row[(x as usize * cpp)..(x as usize * cpp + cpp)];
            match palette.get(key) {
                Some(&(r, g, b, a)) => rgba.extend_from_slice(&[r, g, b, a]),
                None => {
                    return Err(DecodeError::corrupt(format!(
                        "XPM 像素引用了未定义的颜色键「{key}」"
                    )))
                }
            }
        }
    }

    Ok(ImageData::single(
        ImageFormat::Xpm,
        Frame::new(width, height, rgba, 0),
    ))
}

/// 提取第一个 `{ ... }` 之间的内容（字符串数组里不会嵌套花括号）。
fn extract_array_body(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let rest = &text[start + 1..];
    let end = rest.find('}')?;
    Some(&rest[..end])
}

/// 解析花括号内的全部双引号字符串（忽略内部转义，XPM 罕见转义引号）。
fn parse_quoted_strings(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_str = false;
    let mut current = String::new();
    for ch in body.chars() {
        if ch == '"' {
            if in_str {
                out.push(std::mem::take(&mut current));
                in_str = false;
            } else {
                in_str = true;
            }
        } else if in_str {
            current.push(ch);
        }
    }
    out
}

/// 解析颜色值：支持 `#RGB` / `#RRGGBB` / `#RRRGGGBBB` / `#RRRRGGGGBBBB`
/// 以及少量常见 X11 颜色名。
fn parse_color(spec: &str) -> Option<(u8, u8, u8)> {
    if let Some(hex) = spec.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    named_color(spec)
}

fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8)> {
    match hex.len() {
        3 => {
            let r = hex_digit(hex.as_bytes()[0])?;
            let g = hex_digit(hex.as_bytes()[1])?;
            let b = hex_digit(hex.as_bytes()[2])?;
            // 1 位/通道：复制到 8 位（`d → dd`）。
            Some(((r << 4) | r, (g << 4) | g, (b << 4) | b))
        }
        6 => Some((
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        )),
        9 | 12 => {
            let per = hex.len() / 3;
            let r = group_value(hex, 0, per)?;
            let g = group_value(hex, per, per)?;
            let b = group_value(hex, 2 * per, per)?;
            // 把 12/16 位的每通道分量线性缩放到 8 位。
            let max = (1u32 << (per * 4)) - 1;
            let scale = |v: u32| ((v * 255 + max / 2) / max) as u8;
            Some((scale(r), scale(g), scale(b)))
        }
        _ => None,
    }
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 取出 `hex` 中从 `start` 起、长 `len` 位的十六进制分量值。
fn group_value(hex: &str, start: usize, len: usize) -> Option<u32> {
    u32::from_str_radix(&hex[start..start + len], 16).ok()
}

/// 常见 X11 颜色名 → RGB（未知返回 None，由调用方判定为不可支持而拒绝）。
fn named_color(name: &str) -> Option<(u8, u8, u8)> {
    Some(match name.to_ascii_lowercase().as_str() {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "lime" => (0, 255, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "cyan" => (0, 255, 255),
        "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "gray" | "grey" => (128, 128, 128),
        "lightgray" | "lightgrey" => (211, 211, 211),
        "darkgray" | "darkgrey" => (169, 169, 169),
        "silver" => (192, 192, 192),
        "maroon" => (128, 0, 0),
        "olive" => (128, 128, 0),
        "navy" => (0, 0, 128),
        "teal" => (0, 128, 128),
        "purple" => (128, 0, 128),
        "orange" => (255, 165, 0),
        "pink" => (255, 192, 203),
        "brown" => (165, 42, 42),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_xpm(width: u32, height: u32, colors: &[(&str, &str)], rows: &[&str]) -> String {
        let ncolors = colors.len();
        let cpp = colors[0].0.len();
        let mut s = String::new();
        s.push_str("/* XPM */\n");
        s.push_str("static char *pic[] = {\n");
        s.push_str(&format!("\"{width} {height} {ncolors} {cpp}\",\n"));
        for (key, color) in colors {
            s.push_str(&format!("\"{key} c {color}\",\n"));
        }
        for row in rows {
            s.push_str(&format!("\"{row}\",\n"));
        }
        s.push_str("};\n");
        s
    }

    #[test]
    fn opaque_and_transparent_pixels() {
        // 2×2，cpp=1：a=红 b=绿 c=透明。
        let xpm = build_xpm(
            2,
            2,
            &[("a", "#FF0000"), ("b", "#00FF00"), ("c", "none")],
            &["ab", "ca"],
        );
        let data = decode_bytes_for_test(&xpm).expect("XPM 应解出");
        assert_eq!((data.width(), data.height()), (2, 2));
        let px = &data.primary().rgba8;
        assert_eq!(&px[0..4], &[255, 0, 0, 255], "像素(0,0) 应红");
        assert_eq!(&px[4..8], &[0, 255, 0, 255], "像素(1,0) 应绿");
        assert_eq!(&px[8..12], &[0, 0, 0, 0], "像素(0,1) 应透明");
        assert_eq!(&px[12..16], &[255, 0, 0, 255], "像素(1,1) 应红");
    }

    #[test]
    fn short_pixel_row_is_rejected() {
        let xpm = build_xpm(4, 1, &[("a", "#FF0000")], &["aaa"]); // 行只有 3 字符，需 4
        let err = decode_bytes_for_test(&xpm).expect_err("行长度不足应失败");
        assert!(matches!(err, DecodeError::Corrupt { .. }));
    }

    #[test]
    fn unknown_color_is_rejected_not_silent() {
        let xpm = build_xpm(1, 1, &[("a", "chartreuseXX")], &["a"]);
        let err = decode_bytes_for_test(&xpm).expect_err("未知颜色应被拒绝");
        assert!(matches!(err, DecodeError::Unsupported { .. }));
    }

    #[test]
    fn a_space_is_a_valid_palette_key() {
        // `"  c none"`（空格做键、意义为透明）是大量 XPM 生成器的默认写法。
        // 键必须按 cpp 从**行首**切，不能靠 `split_whitespace` 取词 —— 那会把空格键吃掉。
        let xpm = build_xpm(2, 2, &[(" ", "none"), ("a", "#FF0000")], &["a ", " a"]);
        let data = decode_bytes_for_test(&xpm).expect("空格键的 XPM 应解出");
        let px = &data.primary().rgba8;
        assert_eq!(&px[0..4], &[255, 0, 0, 255], "像素(0,0) 应红");
        assert_eq!(&px[4..8], &[0, 0, 0, 0], "像素(1,0) 应透明");
        assert_eq!(&px[8..12], &[0, 0, 0, 0], "像素(0,1) 应透明");
        assert_eq!(&px[12..16], &[255, 0, 0, 255], "像素(1,1) 应红");
    }

    #[test]
    fn a_multi_char_key_still_parses() {
        // cpp = 2：键是行首两个字符，后面才是类型标记与颜色。
        let xpm = build_xpm(2, 1, &[("..", "#00FF00"), ("++", "#0000FF")], &["..++"]);
        let data = decode_bytes_for_test(&xpm).expect("cpp=2 的 XPM 应解出");
        let px = &data.primary().rgba8;
        assert_eq!(&px[0..4], &[0, 255, 0, 255], "像素(0,0) 应绿");
        assert_eq!(&px[4..8], &[0, 0, 255, 255], "像素(1,0) 应蓝");
    }

    fn decode_bytes_for_test(xpm: &str) -> DecodeResult<ImageData> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join("ngy-xpm-test");
        std::fs::create_dir_all(&dir).ok();
        let name = format!(
            "t{}-{}.xpm",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(name);
        std::fs::write(&path, xpm).unwrap();
        decode_path(&path, &DecodeLimits::default())
    }
}
