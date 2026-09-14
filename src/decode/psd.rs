//! Photoshop 文档（`.psd` / `.psb`）解码。
//!
//! # 走哪条路
//!
//! 用 `psd` crate 读出文件级的「合并图像数据」（`rgba()`），它就是 Photoshop 保存时写进文件的
//! 最终合成图——用户双击看到的成品。比起从图层自底向上重建，合并图更快也更贴合用户预期。
//!
//! # 这里头号风险是「静默错图」
//!
//! `psd` crate 的 `rgba()` **不会**对不支持的颜色模式 / 位深报错，而是把 CMYK 的 C/M/Y/K 通道
//! 当成 R/G/B/A 直接吐出来，得到一张颜色全错、却「看起来解码成功」的图。这比直接报错更糟，
//! 因为用户会以为自己看到了真图。所以解码前必须主动校验颜色模式与位深，不支持的形态一律
//! 显式拒绝：
//!
//! - 仅支持 8 位 RGB 与 8 位灰度；
//! - CMYK / Lab / 索引色 / 双色调 / 多通道 / 位图 → `Unsupported`；
//! - 1 / 16 / 32 位深 → `Unsupported`；
//! - 版本 2（`.psb` 大型文档）→ `Unsupported`（`psd` crate 完全不支持 PSB）。
//!
//! # 取舍
//!
//! 不做图层级重建：其一，合并图已是成品；其二，`psd` crate 的图层重建忽略混合模式，
//! 结果反而可能与用户在 Photoshop 里看到的图不一致。检出图层反而更不可信。

use std::path::Path;

use psd::{ColorMode, Psd, PsdDepth};

use super::Decoder;
use super::types::{
    DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat, Orientation,
};

pub struct PsdDecoder;

impl Decoder for PsdDecoder {
    fn id(&self) -> &'static str {
        "psd"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Psd]
    }

    fn probe(&self, head: &[u8]) -> bool {
        // "8BPS" 同时是 PSD 与 PSB 的签名；版本号差异留给 `decode` 区分。
        head.starts_with(b"8BPS")
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_path(src, limits)
    }
}

fn decode_path(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let bytes = std::fs::read(src).map_err(|error| DecodeError::io(src, error))?;
    if bytes.len() < 26 || &bytes[0..4] != b"8BPS" {
        return Err(DecodeError::corrupt("文件头不是 PSD（应以 `8BPS` 开头）"));
    }

    // 版本号：1 = PSD，2 = PSB（大型文档）。crate 不解析 PSB，及早给出可操作提示，
    // 免得用户对着一个「打开失败」却不知道原因。
    let version = u16::from_be_bytes([bytes[4], bytes[5]]);
    if version == 2 {
        return Err(DecodeError::unsupported(
            Some(ImageFormat::Psd),
            "这是 PSB（大型文档格式，.psb 扩展名），当前版本无法解码。请用 Photoshop 导出为常规 PSD 或 PNG。",
        ));
    }
    if version != 1 {
        return Err(DecodeError::corrupt("PSD 版本号无法识别（应为 1 或 2）"));
    }

    let psd = Psd::from_bytes(&bytes)
        .map_err(|error| DecodeError::corrupt(format!("PSD 解析失败：{error}")))?;

    // 颜色模式与位深：这是「静默错图」的重灾区，必须在转 RGBA 之前拦截。
    match psd.color_mode() {
        ColorMode::Rgb | ColorMode::Grayscale => {}
        ColorMode::Cmyk => {
            return Err(DecodeError::unsupported(
                Some(ImageFormat::Psd),
                "该 PSD 使用 CMYK 颜色模式，当前版本解码出的颜色会失真。请先转换为 RGB 模式再打开。",
            ));
        }
        ColorMode::Lab => {
            return Err(DecodeError::unsupported(
                Some(ImageFormat::Psd),
                "该 PSD 使用 Lab 颜色模式，当前版本不支持。请先转换为 RGB 模式再打开。",
            ));
        }
        ColorMode::Indexed => {
            return Err(DecodeError::unsupported(
                Some(ImageFormat::Psd),
                "该 PSD 使用索引色，当前版本不支持。请先转换为 RGB 模式再打开。",
            ));
        }
        ColorMode::Duotone => {
            return Err(DecodeError::unsupported(
                Some(ImageFormat::Psd),
                "该 PSD 使用双色调（Duotone），当前版本不支持。请先转换为 RGB 模式再打开。",
            ));
        }
        ColorMode::Multichannel => {
            return Err(DecodeError::unsupported(
                Some(ImageFormat::Psd),
                "该 PSD 使用多通道模式，当前版本不支持。请先转换为 RGB 模式再打开。",
            ));
        }
        ColorMode::Bitmap => {
            return Err(DecodeError::unsupported(
                Some(ImageFormat::Psd),
                "该 PSD 是位图（1 位）模式，当前版本不支持。请导出为 8 位 / 通道的 PSD 或 PNG。",
            ));
        }
    }

    if psd.depth() != PsdDepth::Eight {
        return Err(DecodeError::unsupported(
            Some(ImageFormat::Psd),
            format!(
                "该 PSD 每通道 {} 位，当前版本仅支持 8 位。请导出为 8 位 / 通道的 PSD 或 PNG。",
                psd.depth() as u8
            ),
        ));
    }

    // `rgba()` 取的就是文件级的合并图像数据，已是最终合成图，顺序为 [R, G, B, A, ...] 紧密逐行，
    // 与项目的统一像素口径完全一致（非预乘 RGBA8、逐行无 padding）。
    let rgba = psd.rgba();
    let width = psd.width();
    let height = psd.height();
    limits.check_dimensions(width, height)?;

    // 缓冲长度与尺寸对不上说明 crate 内部没按 8 位 RGBA 产数，宁报损坏也不交付错图。
    if rgba.len() != (width as usize) * (height as usize) * 4 {
        return Err(DecodeError::corrupt(
            "PSD 解码出的像素数量与文件声明的尺寸不符，文件可能已损坏",
        ));
    }

    Ok(ImageData {
        format: ImageFormat::Psd,
        frames: vec![Frame::new(width, height, rgba, 0)],
        orientation: Orientation::Normal,
        exif: None,
        supersample: 1.0,
    })
}
