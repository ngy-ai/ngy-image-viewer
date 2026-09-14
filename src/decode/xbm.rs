//! X BitMap（`.xbm`）解码：自写纯 Rust 解析器，零依赖。
//!
//! # 格式要点
//!
//! XBM 是历史遗留的纯文本 C 源码：开头用 `#define <name>_width` / `#define <name>_height`
//! 声明尺寸，随后一个 `static ... char/unsigned char <name>_bits[] = { ... }` 数组装着位图。
//! 每个字节是「一行里的 8 个像素」，位序是 **LSB-first**（字节最低位对应最左边的像素）。
//! 位为 1 表示前景色（一般渲染为黑色），位为 0 表示背景色（白色）。
//!
//! 没有现成的纯 Rust 库能干净地接进本项目的 [`Decoder`](super::Decoder) 架构（候选库走
//! `image` crate 的 `register()` 机制，与本项目直接注册解码器的方式不一致），所以这里自己解析。
//! 解析全程失败返回 [`DecodeError`](super::types::DecodeError)，绝不 panic、绝不静默。

use std::path::Path;

use super::Decoder;
use super::types::{DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat};

pub struct XbmDecoder;

impl Decoder for XbmDecoder {
    fn id(&self) -> &'static str {
        "xbm"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Xbm]
    }

    fn probe(&self, head: &[u8]) -> bool {
        // 文本格式：交给 sniff 的字符特征判断更准确，这里只在嗅探已指向 XBM 时自认。
        // 用与 sniff 一致的特征，避免「能嗅探到却 probe 不上」导致打不开。
        let head_prefix = &head[..head.len().min(512)];
        !head_prefix.iter().any(|&b| b == 0)
            && String::from_utf8_lossy(head_prefix)
                .contains("#define")
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_path(src, limits)
    }
}

fn decode_path(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let bytes = std::fs::read(src).map_err(|error| DecodeError::io(src, error))?;
    let text = String::from_utf8_lossy(&bytes);

    let width = parse_define(&text, "_width")
        .or_else(|| parse_define(&text, "width"))
        .ok_or_else(|| DecodeError::corrupt("XBM 缺少 `_width` 尺寸定义"))?;
    let height = parse_define(&text, "_height")
        .or_else(|| parse_define(&text, "height"))
        .ok_or_else(|| DecodeError::corrupt("XBM 缺少 `_height` 尺寸定义"))?;
    limits.check_dimensions(width, height)?;

    // 取第一个 `{...}` 数组块作为位图数据（XBM 文件里只有一个位图数组）。
    let body = extract_array_body(&text)
        .ok_or_else(|| DecodeError::corrupt("XBM 找不到位图数组 `{ ... }`"))?;
    let bits = parse_hex_bytes(&body);

    let bytes_per_line = ((width + 7) / 8) as usize;
    let expected = bytes_per_line * height as usize;
    if bits.len() < expected {
        return Err(DecodeError::corrupt(format!(
            "XBM 位图数据长度不足：需要至少 {expected} 字节，实际只有 {}",
            bits.len()
        )));
    }

    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        let row_start = y as usize * bytes_per_line;
        for x in 0..width {
            let byte = bits[row_start + x as usize / 8];
            // LSB-first：像素 x 对应第 (x % 8) 位；位为 1 → 黑，0 → 白。
            let set = (byte >> (x % 8)) & 1 == 1;
            let (r, g, b) = if set { (0, 0, 0) } else { (255, 255, 255) };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }

    Ok(ImageData::single(
        ImageFormat::Xbm,
        Frame::new(width, height, rgba, 0),
    ))
}

/// 在 `#define <id> <num>` 序列里找 id 以 `suffix` 结尾的那个，返回其数值。
fn parse_define(text: &str, suffix: &str) -> Option<u32> {
    let mut tokens = text.split_whitespace();
    while let Some(word) = tokens.next() {
        if word == "#define" {
            let id = tokens.next()?;
            let value = tokens.next()?;
            if id.ends_with(suffix) {
                return value.parse::<u32>().ok();
            }
        }
    }
    None
}

/// 找到第一个 `{` 与其匹配的 `}` 之间的内容（XBM 数组里没有嵌套花括号）。
fn extract_array_body(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let rest = &text[start + 1..];
    let end = rest.find('}')?;
    Some(&rest[..end])
}

/// 解析数组块里的字节：支持 `0xNN` 十六进制与裸十进制两种写法（用逗号 / 空白分隔）。
fn parse_hex_bytes(body: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for token in body.split(|c: char| c == ',' || c.is_whitespace()) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Some(hex) = token.strip_prefix("0x").or_else(|| token.strip_prefix("0X")) {
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                out.push(value);
            }
        } else if let Ok(value) = token.parse::<u8>() {
            out.push(value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_xbm(width: u32, height: u32, bits: &[u8]) -> String {
        let mut s = String::new();
        s.push_str(&format!("#define pic_width {width}\n"));
        s.push_str(&format!("#define pic_height {height}\n"));
        s.push_str("static unsigned char pic_bits[] = {\n");
        for (i, byte) in bits.iter().enumerate() {
            s.push_str(&format!("0x{byte:02X},"));
            if i % 12 == 11 {
                s.push('\n');
            }
        }
        s.push_str("\n};\n");
        s
    }

    #[test]
    fn lsb_first_black_white_expansion() {
        // 宽 4：位 0/2 设 1（黑），位 1/3 为 0（白） → 字节 0b0000_0101 = 0x05。
        let xbm = build_xbm(4, 1, &[0b0000_0101]);
        let data = decode_bytes_for_test(&xbm).expect("XBM 应解出");
        assert_eq!((data.width(), data.height()), (4, 1));
        let px = &data.primary().rgba8;
        assert_eq!(&px[0..4], &[0, 0, 0, 255], "像素 0 应为黑");
        assert_eq!(&px[4..8], &[255, 255, 255, 255], "像素 1 应为白");
        assert_eq!(&px[8..12], &[0, 0, 0, 255], "像素 2 应为黑");
        assert_eq!(&px[12..16], &[255, 255, 255, 255], "像素 3 应为白");
    }

    #[test]
    fn multi_row_layout_and_rejects_short_data() {
        // 宽 8、高 2：两行各一个字节。
        let xbm = build_xbm(8, 2, &[0xFF, 0x00]);
        let data = decode_bytes_for_test(&xbm).expect("XBM 应解出");
        assert_eq!((data.width(), data.height()), (8, 2));
        // 第一行全黑、第二行全白。
        let px = &data.primary().rgba8;
        assert!(px[0..32].iter().all(|&b| b == 0 || b == 255) && px[0] == 0);
        assert!(px[32..64].iter().all(|&b| b == 255));

        // 数据不足应明确报错而非 panic。
        let broken = build_xbm(8, 2, &[0xFF]);
        let err = decode_bytes_for_test(&broken).expect_err("数据不足应失败");
        assert!(matches!(err, DecodeError::Corrupt { .. }));
    }

    /// 直接在内存里跑解码路径（绕开文件系统），便于单测。
    /// 用进程 id + 原子计数生成唯一文件名，避免并行测试互相覆盖。
    fn decode_bytes_for_test(xbm: &str) -> DecodeResult<ImageData> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join("ngy-xbm-test");
        std::fs::create_dir_all(&dir).ok();
        let name = format!(
            "t{}-{}.xbm",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(name);
        std::fs::write(&path, xbm).unwrap();
        decode_path(&path, &DecodeLimits::default())
    }
}
