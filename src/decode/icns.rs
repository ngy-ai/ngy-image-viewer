//! macOS 图标（`.icns`）解码。
//!
//! # 容器长什么样
//!
//! 一条 `icns` 容器是「魔数 `icns` + 总长度 + 一串资源」：每个资源 = 4 字节 OSType
//! + 4 字节长度（大端，含本资源 8 字节头）+ 数据。现代 `.icns` 把各尺寸图像以
//! **PNG**（OSType 如 `ic07`/`ic08`/`ic09`/`ic10`/`ic11`/`ic12`/`ic13`/`ic14`、
//! 以及 `icp4`/`icp5`/`icp6`）内嵌；早期版本则用裸 RGB 或 JPEG 2000 存储。
//!
//! # 取舍
//!
//! 只处理 PNG 内嵌资源：覆盖几乎所有现代 `.icns`，且 PNG 解码直接复用 `image` crate。
//! 裸 RGB 与 JPEG 2000 内嵌两种老旧形态极少见，遇到时明确告诉用户「当前版本解不了」，
//! 而不是静默退化成空图——宁可报错也别假装成功。

use std::path::Path;

use image::ImageFormat as CrateFormat;

use super::Decoder;
use super::types::{
    DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat, Orientation,
};

/// 以 PNG 存储的 OSType 及其代表分辨率，用于挑「最清晰的那张」。
/// 分数越大越优先；同分时取先出现的（也就是 outer 循环里第一个达到该分数的）。
const PNG_TYPES: &[(&[u8; 4], u32)] = &[
    (b"ic14", 1024),
    (b"ic10", 1024),
    (b"ic13", 512),
    (b"ic09", 512),
    (b"ic08", 256),
    (b"ic12", 128),
    (b"ic07", 128),
    (b"ic11", 64),
    (b"icp6", 64),
    (b"icp5", 32),
    (b"icp4", 16),
];

pub struct IcnsDecoder;

impl Decoder for IcnsDecoder {
    fn id(&self) -> &'static str {
        "icns"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Icns]
    }

    fn probe(&self, head: &[u8]) -> bool {
        head.starts_with(b"icns")
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_path(src, limits)
    }
}

fn decode_path(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let bytes = std::fs::read(src).map_err(|error| DecodeError::io(src, error))?;
    if bytes.len() < 8 || &bytes[0..4] != b"icns" {
        return Err(DecodeError::corrupt("文件头不是 icns 容器（应以 `icns` 开头）"));
    }

    // 挑出分数最高的 PNG 资源；循环里用「严格大于」保证同分时保留先出现的。
    let mut best: Option<(&[u8], u32)> = None;
    let mut offset = 8;
    while offset + 8 <= bytes.len() {
        let type_bytes = &bytes[offset..offset + 4];
        let len = u32::from_be_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        // 长度字段不合法（小于头、或越出文件）就当作容器损坏，停止解析。
        if len < 8 || offset + len > bytes.len() {
            return Err(DecodeError::corrupt("icns 资源的长度字段不一致，文件可能已损坏"));
        }
        let data = &bytes[offset + 8..offset + len];

        if let Some(&(_, score)) = PNG_TYPES.iter().find(|(tag, _)| &tag[..] == type_bytes) {
            let better = match best {
                Some((_, best_score)) => score > best_score,
                None => true,
            };
            if better {
                best = Some((data, score));
            }
        }

        offset += len;
    }

    let (data, _score) = best.ok_or_else(|| {
        // 容器能解析但没有 PNG 资源：多半是老旧裸 RGB / JPEG 2000 形态。
        DecodeError::unsupported(
            Some(ImageFormat::Icns),
            "该 .icns 文件未使用 PNG 存储图像（可能为老旧的裸 RGB 或 JPEG 2000 形态），当前版本不支持。",
        )
    })?;

    // 内嵌的是标准 PNG，交给 `image` crate 解码。
    let image = image::load_from_memory_with_format(data, CrateFormat::Png)
        .map_err(|error| from_image_error(error, src))?;
    let buffer = image.into_rgba8();
    let (width, height) = buffer.dimensions();
    limits.check_dimensions(width, height)?;

    Ok(ImageData {
        format: ImageFormat::Icns,
        frames: vec![Frame::new(width, height, buffer.into_raw(), 0)],
        orientation: Orientation::Normal,
        exif: None,
        supersample: 1.0,
    })
}

/// `image::ImageError` → 我们的错误类型（与 [`crate::decode::raster`] 同口径）。
fn from_image_error(error: image::ImageError, src: &Path) -> DecodeError {
    match error {
        image::ImageError::IoError(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            DecodeError::corrupt("文件在读取过程中提前结束，可能下载或拷贝不完整")
        }
        image::ImageError::IoError(error) => DecodeError::io(src, error),
        image::ImageError::Limits(error) => {
            DecodeError::corrupt(format!("超出解码资源限制：{error}"))
        }
        image::ImageError::Unsupported(error) => {
            DecodeError::unsupported(None, error.to_string())
        }
        other => DecodeError::corrupt(other.to_string()),
    }
}
