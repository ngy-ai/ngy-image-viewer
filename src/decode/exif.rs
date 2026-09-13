//! EXIF 读取：方向纠正与信息面板所需的拍摄参数。
//!
//! 提供两条入口，按「能否拿到现成的 EXIF 段」来选：
//!
//! - [`from_raw`]：直接吃容器里挖出来的 EXIF 段字节。栅格路径优先用它 ——
//!   JPEG/PNG/WebP/TIFF 的解码器本身已经把 EXIF 段取出来了，
//!   再开一次文件纯属浪费（TIFF 尤其明显，可能要多读几十 MB）。
//! - [`from_path`]：对没有现成 EXIF 段的解码器（JXL/SVG/HEIC/RAW）用的兜底，
//!   由 kamadak-exif 自己去识别容器。
//!
//! 容错策略：EXIF 只是「锦上添花」。任何解析失败都返回 `None`，
//! 绝不让一张能解码出像素的图片因为元数据损坏而打不开。

use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;

use exif::{In, Tag};

use super::types::{ExifSummary, Orientation};

/// 从图片中提取出来的元数据。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExifData {
    /// EXIF 方向（1..=8 规范化后）。缺失或非法时为 `Normal`。
    pub orientation: Orientation,
    pub summary: ExifSummary,
}

impl ExifData {
    /// 是否有任何值得展示的内容。
    pub fn is_empty(&self) -> bool {
        self.orientation.is_identity() && self.summary.is_empty()
    }
}

/// 解析一段「TIFF 格式的 EXIF 数据块」。
///
/// `raw` 的形态是 JPEG APP1 / PNG eXIf / WebP EXIF chunk 里的裸 EXIF 数据
/// （以 `II*\0` 或 `MM\0*` 开头），也可能是完整的 TIFF 文件内容。
/// 两种形态 `read_from_container` 都能处理。
pub fn from_raw(raw: &[u8]) -> Option<ExifData> {
    if raw.len() < 8 {
        return None;
    }
    let mut cursor = Cursor::new(raw);
    let exif = exif::Reader::new().read_from_container(&mut cursor).ok()?;
    Some(build(&exif))
}

/// 让 kamadak-exif 直接读文件。适用于没有现成 EXIF 段的解码路径。
pub fn from_path(path: &Path) -> Option<ExifData> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    Some(build(&exif))
}

/// 只取方向。渲染层在「先出图、后补元数据」的时序下会用到这个轻量入口。
pub fn orientation_from_raw(raw: &[u8]) -> Option<Orientation> {
    from_raw(raw).map(|data| data.orientation)
}

fn build(exif: &exif::Exif) -> ExifData {
    let orientation_raw = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(|field| field.value.get_uint(0))
        .and_then(|value| u16::try_from(value).ok());

    let summary = ExifSummary {
        camera_make: ascii(exif, Tag::Make),
        camera_model: ascii(exif, Tag::Model),
        lens: ascii(exif, Tag::LensModel),
        software: ascii(exif, Tag::Software),
        exposure_time: display_with_unit(exif, Tag::ExposureTime),
        f_number: display_with_unit(exif, Tag::FNumber),
        iso: display_with_unit(exif, Tag::PhotographicSensitivity),
        focal_length: display_with_unit(exif, Tag::FocalLength),
        // 拍摄时间优先用 DateTimeOriginal（按下快门的那一刻），
        // 退而求其次才是 DateTime（可能是最后修改时间）。
        date_taken: ascii(exif, Tag::DateTimeOriginal).or_else(|| ascii(exif, Tag::DateTime)),
        orientation_raw,
    };

    ExifData {
        orientation: Orientation::from_exif_opt(orientation_raw),
        summary,
    }
}

/// 读取 ASCII 值，并清掉 EXIF 里常见的补零与空白。
fn ascii(exif: &exif::Exif, tag: Tag) -> Option<String> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    let exif::Value::Ascii(parts) = &field.value else {
        return None;
    };
    // 同一 tag 可能写入多段字符串（多值），看图场景只关心第一段。
    let raw = parts.first()?;
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end_matches('\0').trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// 按 tag 的规范格式渲染数值，并带上单位（如 "1/125 s"、"f/2.8"、"50 mm"）。
///
/// 直接复用 kamadak-exif 的格式化逻辑，而不是自己拼字符串：
/// 曝光时间要显示成 `1/125` 而不是 `0.008`，这类规则由库维护更可靠。
fn display_with_unit(exif: &exif::Exif, tag: Tag) -> Option<String> {
    let field = exif.get_field(tag, In::PRIMARY)?;
    let text = field.display_value().with_unit(exif).to_string();
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工构造一个最小可用的 TIFF/EXIF 块，只包含 Make 与 Orientation。
    ///
    /// 用真实字节而不是 mock：这一步验证的是「我们的字段读取与方向映射正确」，
    /// 构造真实数据才真正覆盖了 kamadak-exif 的解析路径。
    fn sample_exif_chunk(orientation: u16, make: &[u8]) -> Vec<u8> {
        // TIFF 头（小端）+ 1 个 IFD，含 2 个条目：0x0112(Orientation, SHORT)、0x010F(Make, ASCII)
        let mut data = Vec::new();
        data.extend_from_slice(b"II\x2a\x00");
        data.extend_from_slice(&8u32.to_le_bytes()); // 第一个 IFD 的偏移
        let entry_count: u16 = 2;
        data.extend_from_slice(&entry_count.to_le_bytes());

        let ifd_start = data.len();
        // 条目按 tag 升序排列。
        // Make (0x010F) 指向的字符串放在 IFD 之后。
        let make_offset = (ifd_start + 2 * 12 + 4) as u32;
        data.extend_from_slice(&0x010Fu16.to_le_bytes()); // tag
        data.extend_from_slice(&2u16.to_le_bytes()); // ASCII
        data.extend_from_slice(&(make.len() as u32).to_le_bytes()); // count
        data.extend_from_slice(&make_offset.to_le_bytes()); // value offset

        data.extend_from_slice(&0x0112u16.to_le_bytes()); // tag
        data.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        data.extend_from_slice(&1u32.to_le_bytes()); // count
        let mut short_value = [0u8; 4];
        short_value[..2].copy_from_slice(&orientation.to_le_bytes());
        data.extend_from_slice(&short_value);

        data.extend_from_slice(&0u32.to_le_bytes()); // 没有下一个 IFD
        data.extend_from_slice(make);

        data
    }

    #[test]
    fn reads_orientation_and_make() {
        let chunk = sample_exif_chunk(6, b"Canon\0");
        let data = from_raw(&chunk).expect("应能解析出 EXIF");
        assert_eq!(data.orientation, Orientation::Rotate90);
        assert_eq!(data.summary.camera_make.as_deref(), Some("Canon"));
        assert_eq!(data.summary.orientation_raw, Some(6));
        assert!(!data.is_empty());
    }

    #[test]
    fn all_orientations_map_correctly() {
        for exif_value in 1..=8u16 {
            let chunk = sample_exif_chunk(exif_value, b"X\0");
            let data = from_raw(&chunk).unwrap();
            assert_eq!(data.orientation.to_exif(), exif_value as u8);
        }
    }

    #[test]
    fn garbage_input_returns_none_instead_of_panicking() {
        assert!(from_raw(&[]).is_none());
        assert!(from_raw(b"not exif at all").is_none());
        // 只有 TIFF 头、后面被截断：解析失败也不应 panic。
        assert!(from_raw(b"II\x2a\x00\x08\x00\x00\x00").is_none());
    }

    #[test]
    fn missing_file_returns_none() {
        assert!(from_path(Path::new("definitely/not/here.jpg")).is_none());
    }

    #[test]
    fn orientation_only_helper_matches_full_parse() {
        let chunk = sample_exif_chunk(8, b"X\0");
        assert_eq!(orientation_from_raw(&chunk), Some(Orientation::Rotate270));
        assert_eq!(orientation_from_raw(b"junk"), None);
    }
}
