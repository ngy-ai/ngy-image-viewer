//! PICT（Apple QuickDraw picture，`.pict` `.pct` `.pic`）解码：封装纯 Rust 的 `oxideav-pict` crate。
//!
//! # 走哪条路
//!
//! `oxideav-pict` 是纯 Rust 的 PICT 软栅格化解码器，关掉默认的 `registry` 特性后
//! 不引入 `oxideav-core` 框架依赖，`parse_pict(&[u8]) -> Result<PictImage>` 直接返回结果。
//!
//! # 像素口径
//!
//! 该 crate 的 `PictImage::data` 已**归一化为 RGBA8、逐行紧密排列**（1 位位图展开成黑/白、
//! 16 位 A1R5G5B5 展开成 8 位 RGBA、32 位 XRGB 重排成 RGBA），所以这里几乎可以直接搬进
//! 项目统一的 [`Frame`] 结构，无需再做色彩空间换算。
//!
//! # 错误分流
//!
//! `PictError` 的三种变体分别对应不同的用户动作：
//! - `Unsupported`（含未实现的 QuickTime/区域裁剪等）→ 引导用户换格式；
//! - `NoRaster`（只有矢量绘制指令、没有可被栅格化的位图）→ 明确说「打不开」；
//! - `InvalidData`（截断/损坏）→ `Corrupt`。

use std::path::Path;

use oxideav_pict::{parse_pict, PictError};

use super::Decoder;
use super::types::{DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat};

pub struct PictDecoder;

impl Decoder for PictDecoder {
    fn id(&self) -> &'static str {
        "pict"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Pict]
    }

    fn probe(&self, head: &[u8]) -> bool {
        // PICT 的判定交给 sniff 的 `looks_like_pict`（偏移 0 与 512 两处），这里与之对齐。
        crate::decode::sniff::sniff_magic(head) == Some(ImageFormat::Pict)
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_path(src, limits)
    }
}

fn decode_path(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let bytes = std::fs::read(src).map_err(|error| DecodeError::io(src, error))?;
    let image = parse_pict(&bytes).map_err(decode_error_from)?;

    let width = image.width;
    let height = image.height;
    limits.check_dimensions(width, height)?;

    if image.data.len() != width as usize * height as usize * 4 {
        return Err(DecodeError::corrupt(
            "PICT 解出的像素数量与声明尺寸不符，文件可能已损坏",
        ));
    }

    Ok(ImageData::single(
        ImageFormat::Pict,
        Frame::new(width, height, image.data, 0),
    ))
}

/// 把 `oxideav-pict` 的错误分流成项目统一的 [`DecodeError`]。
fn decode_error_from(error: PictError) -> DecodeError {
    match error {
        PictError::Unsupported(msg) => DecodeError::unsupported(
            Some(ImageFormat::Pict),
            format!("该 PICT 使用了暂不支持的特性（{msg}），可转换后重试。"),
        ),
        PictError::NoRaster => DecodeError::unsupported(
            Some(ImageFormat::Pict),
            "该 PICT 不含可被栅格化的图像数据（仅有矢量绘制指令），当前版本无法打开。".to_string(),
        ),
        PictError::InvalidData(msg) => DecodeError::corrupt(format!("PICT 数据损坏：{msg}")),
    }
}
