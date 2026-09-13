//! AVIF 解码：走各平台的原生解码器。
//!
//! # 为什么不用纯 Rust 方案
//!
//! AVIF 的图像数据是 AV1 帧内编码。要自己接，需要一个 AV1 解码器
//! （唯一许可兼容的纯 Rust 选择是 `rav1d`）**加上**一整套 YUV→RGB 转换：
//! 4:2:0 / 4:2:2 / 4:4:4 / 单色四种采样、受限/全范围两种值域、
//! 8/10/12 三种位深、多套色彩矩阵，还要处理单独的 alpha 辅助图像项。
//! 那是几百行 `unsafe` FFI 加几百行色彩换算，且必须靠真实样本逐一验证。
//!
//! 而三大桌面系统都已内置 AV1 解码能力，复用它既可靠又零构建依赖：
//!
//! | 平台 | 后端 | 依赖的系统组件 |
//! | --- | --- | --- |
//! | Windows | WIC | 「AV1 图像扩展」（Microsoft Store 免费） |
//! | macOS | ImageIO | 系统自带（macOS 13+） |
//! | Linux | libavif | 由发行版提供 |
//!
//! 这个取舍与 HEIC 完全一致，因此 Windows 上两者共用同一份 [`super::wic`] 实现。
//!
//! # 统一的失败语义
//!
//! 与 HEIC 一样，最常见的失败是「系统缺解码组件」。这种情况统一映射成
//! [`DecodeError::MissingPlatformSupport`]，并给出该装什么的具体指引 ——
//! 用户看到的不是 HRESULT，而是「去装 AV1 图像扩展」。

use std::path::Path;

use super::Decoder;
use super::types::{DecodeLimits, DecodeResult, ImageData, ImageFormat};

#[cfg(windows)]
mod avif_windows;

#[cfg(target_os = "macos")]
mod avif_macos;

#[cfg(all(unix, not(target_os = "macos")))]
mod avif_linux;

pub struct AvifDecoder;

impl Decoder for AvifDecoder {
    fn id(&self) -> &'static str {
        "platform-avif"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Avif]
    }

    fn probe(&self, head: &[u8]) -> bool {
        super::sniff::sniff_magic(head) == Some(ImageFormat::Avif)
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_avif(src, limits)
    }
}

#[cfg(windows)]
fn decode_avif(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    avif_windows::decode(src, limits)
}

#[cfg(target_os = "macos")]
fn decode_avif(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    avif_macos::decode(src, limits)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn decode_avif(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    avif_linux::decode(src, limits)
}

/// 既不是 Windows、也不是 macOS、也不是 unix 的目标（当前没有）。
#[cfg(not(any(windows, unix)))]
fn decode_avif(_src: &Path, _limits: &DecodeLimits) -> DecodeResult<ImageData> {
    Err(super::types::DecodeError::unsupported(
        Some(ImageFormat::Avif),
        "当前平台没有可用的 AVIF 解码后端。",
    ))
}
