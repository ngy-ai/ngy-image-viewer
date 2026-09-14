//! FLIF（Free Lossless Image Format，`.flif`）解码：封装纯 Rust 的 `flif` crate。
//!
//! # 走哪条路
//!
//! `flif` crate 是纯 Rust 的 FLIF 解码器，**没有 C 依赖**，且 `decode` 返回 `Result`，
//! 出错时不会 panic —— 正合本项目的「绝不静默失败」底线。
//!
//! # 像素口径
//!
//! crate 解出的 `raw()` 是按「每像素 `channels` 字节、标准 RGB(A) 顺序」紧密排列的：
//! 灰度 1 字节、RGB 3 字节、RGBA 4 字节。这里把它展开成项目统一的**非预乘 RGBA8、逐行紧密排列**。
//!
//! # 已知限制（来自上游）
//!
//! `flif` crate 的 `decode_image` 仅支持 **8 位/通道、非隔行、非动画** 的 FLIF，
//! 其余形态会在解码阶段直接报 `Unimplemented`。这些都被分流为 `Unsupported`，
//! 给用户一句「请先转成 8 位 PNG/RGB」的可操作提示，而不是崩溃或静默。

use std::io::Cursor;
use std::path::Path;

use flif::{Error as FlifError, Flif};

use super::Decoder;
use super::types::{DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat};

pub struct FlifDecoder;

impl Decoder for FlifDecoder {
    fn id(&self) -> &'static str {
        "flif"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Flif]
    }

    fn probe(&self, head: &[u8]) -> bool {
        head.starts_with(b"FLIF")
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_path(src, limits)
    }
}

fn decode_path(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let bytes = std::fs::read(src).map_err(|error| DecodeError::io(src, error))?;
    let image = Flif::decode(Cursor::new(&bytes)).map_err(decode_error_from)?;

    let header = &image.info().header;
    let width = header.width;
    let height = header.height;
    limits.check_dimensions(width, height)?;

    // `header` 的字段类型是 crate 内部的 `pub(crate)` 类型，无法在外部按名引用；
    // 但 `raw()` 的字节数 = `宽 × 高 × 通道数`（灰度 1 / RGB 3 / RGBA 4），
    // 反推通道数即可，既绕开私有类型，也自带「高位深/多通道 → 数据长度不对」的校验。
    let raw = image.raw();
    let pixels = width as usize * height as usize;
    if pixels == 0 {
        return Err(DecodeError::corrupt("FLIF 尺寸为 0"));
    }
    if raw.len() % pixels != 0 {
        return Err(DecodeError::corrupt("FLIF 像素布局异常"));
    }
    let channels = raw.len() / pixels;
    if !matches!(channels, 1 | 3 | 4) {
        return Err(DecodeError::unsupported(
            Some(ImageFormat::Flif),
            "仅支持 8 位/通道、非隔行、非动画的 FLIF，请先转为 8 位 RGB/PNG 再打开。",
        ));
    }

    let rgba = expand_to_rgba8(raw, channels, width, height)?;
    if rgba.len() != pixels * 4 {
        return Err(DecodeError::corrupt(
            "FLIF 解出的像素数量与声明尺寸不符，文件可能已损坏",
        ));
    }

    Ok(ImageData::single(
        ImageFormat::Flif,
        Frame::new(width, height, rgba, 0),
    ))
}

/// 把 FLIF 的逐像素交织数据展开成项目统一的 RGBA8。
///
/// - 灰度：`(g, g, g, 255)`
/// - RGB：`(r, g, b, 255)`
/// - RGBA：原样透传（已经是 4 字节/像素）
fn expand_to_rgba8(raw: &[u8], channels: usize, width: u32, height: u32) -> DecodeResult<Vec<u8>> {
    let pixels = width as usize * height as usize;
    if raw.len() < pixels * channels {
        return Err(DecodeError::corrupt("FLIF 像素数据不完整"));
    }
    let mut out = Vec::with_capacity(pixels * 4);
    match channels {
        1 => {
            for &g in &raw[..pixels] {
                out.extend_from_slice(&[g, g, g, 255]);
            }
        }
        3 => {
            for p in 0..pixels {
                let o = p * 3;
                out.extend_from_slice(&[raw[o], raw[o + 1], raw[o + 2], 255]);
            }
        }
        4 => out.extend_from_slice(&raw[..pixels * 4]),
        _ => return Err(DecodeError::corrupt("FLIF 通道数异常")),
    }
    Ok(out)
}

/// 把 `flif` crate 的错误分流：`Unimplemented`（高位深/动画/隔行/自定义 bitchance）
/// → `Unsupported`（引导转格式）；其余（损坏、截断、未知元数据）→ `Corrupt`。
fn decode_error_from(error: FlifError) -> DecodeError {
    match error {
        FlifError::Unimplemented(msg) => DecodeError::unsupported(
            Some(ImageFormat::Flif),
            format!("该 FLIF 暂不支持（{msg}），请先转为 8 位 PNG 或 RGB 图像再打开。"),
        ),
        other => DecodeError::corrupt(format!("FLIF 解码失败：{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_grey_expands_to_opaque_rgb() {
        let raw = vec![10u8, 200];
        let rgba = expand_to_rgba8(&raw, 1, 2, 1).expect("灰度应展开");
        assert_eq!(rgba, vec![10, 10, 10, 255, 200, 200, 200, 255]);
    }

    #[test]
    fn expand_rgb_keeps_channels_and_sets_alpha() {
        let raw = vec![1u8, 2, 3, 4, 5, 6];
        let rgba = expand_to_rgba8(&raw, 3, 2, 1).expect("RGB 应展开");
        assert_eq!(rgba, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn expand_rgba_passes_through_verbatim() {
        let raw = vec![9u8, 8, 7, 6];
        let rgba = expand_to_rgba8(&raw, 4, 1, 1).expect("RGBA 应原样透传");
        assert_eq!(rgba, raw);
    }

    #[test]
    fn incomplete_raw_is_rejected() {
        let err = expand_to_rgba8(&[1, 2], 3, 2, 1).expect_err("数据不足应失败");
        assert!(matches!(err, DecodeError::Corrupt { .. }));
    }
}
