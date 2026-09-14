//! Windows 光标（`.cur`）解码。
//!
//! # 为什么能复用 ICO 解码器
//!
//! CUR 与 ICO 是**同一个容器**（`ICONDIR` + 若干 `ICONDIRENTRY` + 内嵌位图），
//! 唯一的区别是目录头的类型字段：1 = 图标，2 = 光标；光标额外用两个字段记录热点的
//! x/y 坐标。图像本身可能内嵌 BMP（DIB）或 PNG，与 ICO 完全一致。
//!
//! `image` crate 的 [`IcoDecoder`] 只接受类型 1，因此这里把类型字节就地改成 1，
//! 直接复用它的内嵌位图/PNG 解码能力，不必为热点的两个字段专门写一套解析。
//!
//! # 边界
//!
//! 光标的多尺寸只是同一画面在不同分辨率下的副本，查看器只需要解出其中一帧即可
//! （窗口会按需缩放），所以这里取目录里的第一帧，不再去挑选「最大那张」。

use std::io::Cursor;
use std::path::Path;

use image::codecs::ico::IcoDecoder;
use image::{DynamicImage, ImageDecoder};

use super::Decoder;
use super::types::{
    DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat, Orientation,
};

pub struct CurDecoder;

impl Decoder for CurDecoder {
    fn id(&self) -> &'static str {
        "cur"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Cur]
    }

    fn probe(&self, head: &[u8]) -> bool {
        super::sniff::sniff_magic(head) == Some(ImageFormat::Cur)
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_path(src, limits)
    }
}

fn decode_path(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let mut bytes = std::fs::read(src).map_err(|error| DecodeError::io(src, error))?;
    if bytes.len() < 6 {
        return Err(DecodeError::corrupt("文件过短，不是有效的 CUR 光标文件"));
    }

    // `ICONDIR`：reserved(u16)=0，type(u16)=1 图标 / 2 光标。
    let reserved = u16::from_le_bytes([bytes[0], bytes[1]]);
    let type_field = u16::from_le_bytes([bytes[2], bytes[3]]);
    if reserved != 0 || type_field != 2 {
        return Err(DecodeError::corrupt(
            "文件头不是 CUR 光标格式（类型字段应为 2）",
        ));
    }

    // 改成类型 1，交给 ICO 解码器。热点坐标字段对出图无影响，忽略即可。
    bytes[2] = 1;

    // SAFETY 无关：这里没有 unsafe。`Cursor` 同时满足 `Read + Seek`。
    let decoder =
        IcoDecoder::new(Cursor::new(bytes)).map_err(|error| from_image_error(error, src))?;
    let (width, height) = decoder.dimensions();
    limits.check_dimensions(width, height)?;

    let image =
        DynamicImage::from_decoder(decoder).map_err(|error| from_image_error(error, src))?;
    let buffer = image.into_rgba8();
    let (width, height) = buffer.dimensions();
    limits.check_dimensions(width, height)?;

    Ok(ImageData {
        format: ImageFormat::Cur,
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
