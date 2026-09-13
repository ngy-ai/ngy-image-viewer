//! 以文件头字节（magic bytes）判定图片格式。
//!
//! 为什么不信任扩展名：实际使用中「打不开」的绝大多数原因是扩展名与内容不一致
//! —— 下载后被改名、相机/修图软件导出成 `.jpg` 而内容其实是别的容器、
//! 资源包里几十个文件都叫 `.bin`。因此这里**以内容为准**；
//! 扩展名只承担两件事：
//!
//! 1. **兜底**：TGA 等格式头部没有可靠特征码，只能靠扩展名；
//! 2. **消歧**：NEF/ARW/DNG 等相机原片与普通 TIFF 共用同一个文件头，靠扩展名区分；
//! 3. **告警**：两者不一致时给用户一句可理解的说明，而不是默默换一套解码器。
//!
//! 本模块是纯函数，不碰文件系统，便于单元测试。

use std::path::Path;

use super::types::ImageFormat;

/// 参与探测的文件头长度。
///
/// 取 16 KiB 而不是常见的 4 KiB，是因为 SVG 是文本格式，根元素 `<svg` 可能
/// 出现在相当靠后的位置（前面有大段 XML 声明、注释或 DTD）。
/// 这个读取发生在后台解码线程，不影响启动时间。
pub const HEAD_LEN: usize = 16 * 1024;

/// 格式结论的来源。用来区分「内容判定」与「扩展名兜底」。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SniffSource {
    /// 由文件内容（magic bytes）判定，可信度最高。
    Magic,
    /// 内容无法判定，退而使用扩展名。
    Extension,
    /// 两者都无法判定。
    None,
}

/// 探测结果。
#[derive(Clone, Debug)]
pub struct Sniffed {
    /// 最终采用的格式。
    pub format: Option<ImageFormat>,
    /// 结论来源。
    pub source: SniffSource,
    /// 仅由内容得到的格式。
    pub magic_format: Option<ImageFormat>,
    /// 仅由扩展名得到的格式。
    pub extension_format: Option<ImageFormat>,
}

impl Sniffed {
    /// 扩展名与内容是否互相矛盾（用于给用户一句说明）。
    ///
    /// TIFF 与 RAW 视为同一族，不算矛盾 —— 相机原片本来就是 TIFF 容器。
    pub fn is_mismatch(&self) -> bool {
        match (self.magic_format, self.extension_format) {
            (Some(magic), Some(extension)) => !compatible(magic, extension),
            _ => false,
        }
    }

    pub fn mismatch_note(&self) -> Option<String> {
        if !self.is_mismatch() {
            return None;
        }
        let magic = self.magic_format?;
        let extension = self.extension_format?;
        Some(format!(
            "文件扩展名指向 {}，但内容实际是 {}。已按内容解码。",
            extension.display_name(),
            magic.display_name()
        ))
    }
}

fn compatible(a: ImageFormat, b: ImageFormat) -> bool {
    if a == b {
        return true;
    }
    // 相机原片与 TIFF 共用容器，互相「不一致」是正常现象。
    matches!(
        (a, b),
        (ImageFormat::Tiff, ImageFormat::Raw) | (ImageFormat::Raw, ImageFormat::Tiff)
    )
}

/// 综合内容与扩展名给出结论。
pub fn sniff(head: &[u8], path: Option<&Path>) -> Sniffed {
    let magic_format = sniff_magic(head);
    let extension_format = path
        .and_then(Path::extension)
        .and_then(ImageFormat::from_extension_os);

    let (format, source) = match (magic_format, extension_format) {
        // 内容说是 TIFF、扩展名说是相机原片：这种歧义只有扩展名能消解，
        // 因为两者的文件头完全一样，必须把文件交给 RAW 解码器才可能出图。
        (Some(ImageFormat::Tiff), Some(ImageFormat::Raw)) => {
            (Some(ImageFormat::Raw), SniffSource::Extension)
        }
        (Some(magic), _) => (Some(magic), SniffSource::Magic),
        (None, Some(extension)) => (Some(extension), SniffSource::Extension),
        (None, None) => (None, SniffSource::None),
    };

    Sniffed {
        format,
        source,
        magic_format,
        extension_format,
    }
}

/// 只依据文件内容判断格式。无法判断时返回 `None`。
pub fn sniff_magic(head: &[u8]) -> Option<ImageFormat> {
    if head.len() < 2 {
        return None;
    }

    // ---- 固定特征码，无歧义，可以按开销从低到高直接判 ----

    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(ImageFormat::Png);
    }
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(ImageFormat::Jpeg);
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some(ImageFormat::Gif);
    }
    if head.starts_with(b"BM") {
        return Some(ImageFormat::Bmp);
    }
    if head.starts_with(b"DDS ") {
        return Some(ImageFormat::Dds);
    }
    if head.starts_with(b"qoif") {
        return Some(ImageFormat::Qoi);
    }
    if head.starts_with(b"farbfeld") {
        return Some(ImageFormat::Farbfeld);
    }
    if head.starts_with(&[0x76, 0x2F, 0x31, 0x01]) {
        return Some(ImageFormat::OpenExr);
    }
    if head.starts_with(b"#?RADIANCE") || head.starts_with(b"#?RGBE") {
        return Some(ImageFormat::Hdr);
    }
    // ICO 目录头。CUR（00 00 02 00）不在支持范围内，因此不认。
    if head.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        return Some(ImageFormat::Ico);
    }
    // JPEG XL 裸 codestream（另一种封装是容器，见下）。
    if head.starts_with(&[0xFF, 0x0A]) {
        return Some(ImageFormat::Jxl);
    }
    // JPEG XL 容器：第一个盒是固定的 12 字节签名，之后才是带 brand 的 ftyp。
    if head.starts_with(&[
        0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A,
    ]) {
        return Some(ImageFormat::Jxl);
    }

    // ---- 容器类，需要看容器内部的标识 ----

    // RIFF 容器：只有 WebP 是我们关心的。
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        return Some(ImageFormat::WebP);
    }

    // ISOBMFF（ftyp）容器族：AVIF / HEIC / JPEG XL / Canon CR3 都住在里面，
    // 只能靠 major brand 区分。
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        if let Some(format) = isobmff_brand(&head[8..12]) {
            return Some(format);
        }
    }

    // TIFF 家族：普通 TIFF 与几乎全部相机原片都以 TIFF 头开始。
    if head.starts_with(b"II\x2a\x00") || head.starts_with(b"MM\x00\x2a") {
        // CR2 在 TIFF 头的偏移 8 处写了 "CR"，是唯一能在内容层面确定的原片格式。
        if head.len() >= 10 && &head[8..10] == b"CR" {
            return Some(ImageFormat::Raw);
        }
        return Some(ImageFormat::Tiff);
    }

    // PNM：`P1`..`P6` 后面必须跟空白字符，否则 `P1abc` 会被误判。
    if head.len() >= 3
        && head[0] == b'P'
        && (b'1'..=b'6').contains(&head[1])
        && matches!(head[2], b' ' | b'\t' | b'\r' | b'\n')
    {
        return Some(ImageFormat::Pnm);
    }

    // SVG：纯文本，没有魔数，只能找根元素。放在最后，避免误吞其它格式。
    if looks_like_svg(head) {
        return Some(ImageFormat::Svg);
    }

    None
}

fn isobmff_brand(brand: &[u8]) -> Option<ImageFormat> {
    Some(match brand {
        b"avif" | b"avis" => ImageFormat::Avif,
        b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"mif1" | b"mif2"
        | b"msf1" => ImageFormat::Heic,
        b"jxl " | b"jxs " => ImageFormat::Jxl,
        // Canon CR3 也是 ISOBMFF。能认出来才能给出「CR3 暂不支持」这种准确提示，
        // 而不是笼统地说「无法识别的格式」。
        b"crx " => ImageFormat::Raw,
        _ => return None,
    })
}

/// 粗略判定是否为 SVG。
///
/// 判据是「头部文本里出现了 `<svg`」且整体看着像 XML/文本。
/// 不做真正的 XML 解析：这一步只需要在「扩展名也不认」时抢救一下文本矢量图，
/// 精确性由后面的 `usvg` 解析来保证。
fn looks_like_svg(head: &[u8]) -> bool {
    // 先排掉明显的二进制内容：SVG 的前若干字节里不应出现 NUL。
    let probe_len = head.len().min(4096);
    let probe = &head[..probe_len];
    if probe.contains(&0) {
        return false;
    }

    let text = String::from_utf8_lossy(probe).to_ascii_lowercase();
    if !text.contains("<svg") {
        return false;
    }
    // 必须是 SVG 元素本身，而不是注释或字符串里偶然出现的 "<svg"。
    let Some(index) = text.find("<svg") else {
        return false;
    };
    // 合法的下一个字符只有：属性前置空白、直接闭合的 `>`、以及自闭合的 `/`。
    let after = text[index + 4..].chars().next();
    matches!(
        after,
        Some(' ') | Some('\t') | Some('\r') | Some('\n') | Some('>') | Some('/') | None
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_common_magic_bytes() {
        assert_eq!(
            sniff_magic(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"),
            Some(ImageFormat::Png)
        );
        assert_eq!(sniff_magic(b"\xff\xd8\xff\xe0\x00\x10JFIF"), Some(ImageFormat::Jpeg));
        assert_eq!(sniff_magic(b"GIF89a\x01\x00"), Some(ImageFormat::Gif));
        assert_eq!(sniff_magic(b"BM\x36\x00\x00\x00"), Some(ImageFormat::Bmp));
        assert_eq!(sniff_magic(b"qoif\x00\x00\x00\x02"), Some(ImageFormat::Qoi));
        assert_eq!(sniff_magic(&[0x76, 0x2F, 0x31, 0x01, 0x02]), Some(ImageFormat::OpenExr));
        assert_eq!(sniff_magic(b"#?RADIANCE\n"), Some(ImageFormat::Hdr));
        assert_eq!(sniff_magic(&[0x00, 0x00, 0x01, 0x00, 0x01]), Some(ImageFormat::Ico));
    }

    #[test]
    fn detects_riff_webp_but_not_plain_riff() {
        assert_eq!(
            sniff_magic(b"RIFF\x24\x00\x00\x00WEBPVP8 "),
            Some(ImageFormat::WebP)
        );
        // WAV 也是 RIFF，但不应被认成图片。
        assert_eq!(sniff_magic(b"RIFF\x24\x00\x00\x00WAVEfmt "), None);
    }

    #[test]
    fn distinguishes_isobmff_brands() {
        assert_eq!(
            sniff_magic(b"\x00\x00\x00\x20ftypavif\x00\x00\x00\x00"),
            Some(ImageFormat::Avif)
        );
        assert_eq!(
            sniff_magic(b"\x00\x00\x00\x20ftypheic\x00\x00\x00\x00"),
            Some(ImageFormat::Heic)
        );
        assert_eq!(
            sniff_magic(b"\x00\x00\x00\x20ftypmif1\x00\x00\x00\x00"),
            Some(ImageFormat::Heic)
        );
        assert_eq!(
            sniff_magic(b"\x00\x00\x00\x20ftypcrx \x00\x00\x00\x00"),
            Some(ImageFormat::Raw)
        );
        // 普通 MP4 视频不应被当作图片。
        assert_eq!(sniff_magic(b"\x00\x00\x00\x20ftypisom\x00\x00\x00\x00"), None);
    }

    #[test]
    fn detects_jxl_both_encodings() {
        // 裸 codestream。
        assert_eq!(sniff_magic(&[0xFF, 0x0A, 0x03, 0x04]), Some(ImageFormat::Jxl));
        // 容器：12 字节签名盒 + ftyp。
        let container = b"\x00\x00\x00\x0CJXL \x0D\x0A\x87\x0A\x00\x00\x00\x14ftypjxl ";
        assert_eq!(sniff_magic(container), Some(ImageFormat::Jxl));
    }

    #[test]
    fn tiff_family_and_cr2() {
        assert_eq!(sniff_magic(b"II\x2a\x00\x08\x00\x00\x00"), Some(ImageFormat::Tiff));
        assert_eq!(sniff_magic(b"MM\x00\x2a\x00\x00\x00\x08"), Some(ImageFormat::Tiff));
        // CR2 在偏移 8 处有 "CR"。
        assert_eq!(
            sniff_magic(b"II\x2a\x00\x10\x00\x00\x00CR\x02\x00"),
            Some(ImageFormat::Raw)
        );
    }

    #[test]
    fn pnm_requires_whitespace_after_magic() {
        assert_eq!(sniff_magic(b"P6\n255\n"), Some(ImageFormat::Pnm));
        assert_eq!(sniff_magic(b"P3 1 1"), Some(ImageFormat::Pnm));
        // "P1abc" 不是 PNM。
        assert_eq!(sniff_magic(b"P1abc"), None);
    }

    #[test]
    fn detects_svg_text_but_not_binary() {
        assert_eq!(
            sniff_magic(b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            Some(ImageFormat::Svg)
        );
        assert_eq!(sniff_magic(b"<svg/>"), Some(ImageFormat::Svg));
        // 注释里出现 <svg 不算。
        assert_eq!(sniff_magic(b"<!-- not an image -->"), None);
        // 二进制内容不应被当作 SVG。
        assert_eq!(sniff_magic(b"\x89PNG\r\n\x1a\n\x00sv"), Some(ImageFormat::Png));
    }

    #[test]
    fn extension_fallback_for_headerless_formats() {
        // TGA 没有可靠特征码：只能靠扩展名。
        let sniffed = sniff(b"\x00\x00\x02\x00\x00\x00\x00\x00", Some(&PathBuf::from("a.tga")));
        assert_eq!(sniffed.format, Some(ImageFormat::Tga));
        assert_eq!(sniffed.source, SniffSource::Extension);
        assert!(!sniffed.is_mismatch());
    }

    #[test]
    fn raw_extension_wins_over_ambiguous_tiff_header() {
        let head = b"II\x2a\x00\x08\x00\x00\x00\x00\x00";
        let sniffed = sniff(head, Some(&PathBuf::from("DSC_0001.NEF")));
        assert_eq!(sniffed.format, Some(ImageFormat::Raw));
        assert_eq!(sniffed.source, SniffSource::Extension);
        // TIFF 头 + RAW 扩展名是正常组合，不应报警。
        assert!(!sniffed.is_mismatch());
    }

    #[test]
    fn real_mismatch_is_reported_not_hidden() {
        // 内容其实是 PNG，扩展名写成了 jpg：按内容解，但要让用户知道。
        let sniffed = sniff(
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
            Some(&PathBuf::from("photo.jpg")),
        );
        assert_eq!(sniffed.format, Some(ImageFormat::Png));
        assert_eq!(sniffed.source, SniffSource::Magic);
        assert!(sniffed.is_mismatch());
        let note = sniffed.mismatch_note().unwrap();
        assert!(note.contains("PNG"));
        assert!(note.contains("JPEG"));
    }

    #[test]
    fn unknown_content_without_extension_yields_nothing() {
        let sniffed = sniff(b"\x00\x01\x02\x03\x04", Some(&PathBuf::from("mystery")));
        assert_eq!(sniffed.format, None);
        assert_eq!(sniffed.source, SniffSource::None);
        assert!(!sniffed.is_mismatch());
    }
}
