//! HEIC / HEIF 解码：走各平台的原生解码器。
//!
//! # 为什么不用纯 Rust 方案
//!
//! HEIC 的图像数据是 HEVC（H.265）的帧内编码。目前纯 Rust 的 HEVC 解码器
//! 要么不够成熟，要么带 AGPL 许可（与 Apache-2.0 的本项目不兼容）。
//! 而三大桌面系统**都已内置** HEVC 解码能力，复用它既可靠又零构建依赖：
//!
//! | 平台 | 后端 | 依赖的系统组件 |
//! | --- | --- | --- |
//! | Windows | WIC | 「HEIF 图像扩展」（Microsoft Store 免费） |
//! | macOS | ImageIO | 系统自带 |
//! | Linux | libheif | 由发行版提供 |
//!
//! # 统一的失败语义
//!
//! 三个后端最常见的失败模式完全一样：**系统缺解码组件**。
//! 所以它们在各自实现里都把这种情况映射成同一个
//! [`DecodeError::MissingPlatformSupport`]，并附上能照做的安装指引 ——
//! 用户看到的不是「0x88982F50」这种 HRESULT，而是「去装 HEIF 图像扩展」。

use std::path::Path;

use super::Decoder;
use super::types::{DecodeLimits, DecodeResult, ImageData, ImageFormat};

#[cfg(windows)]
mod heic_windows;

#[cfg(target_os = "macos")]
mod heic_macos;

#[cfg(all(unix, not(target_os = "macos")))]
mod heic_linux;

pub struct HeicDecoder;

impl Decoder for HeicDecoder {
    fn id(&self) -> &'static str {
        "platform-heic"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Heic]
    }

    fn probe(&self, head: &[u8]) -> bool {
        super::sniff::sniff_magic(head) == Some(ImageFormat::Heic)
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_heic(src, limits)
    }
}

#[cfg(windows)]
fn decode_heic(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    heic_windows::decode(src, limits)
}

#[cfg(target_os = "macos")]
fn decode_heic(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    heic_macos::decode(src, limits)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn decode_heic(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    heic_linux::decode(src, limits)
}

/// 既不是 Windows、也不是 macOS、也不是 unix 的目标（当前没有）。
///
/// 保留这个分支是为了让「不支持」本身就是一句明确的说明，
/// 而不是一个编译错误或一个静默的空实现。
#[cfg(not(any(windows, unix)))]
fn decode_heic(_src: &Path, _limits: &DecodeLimits) -> DecodeResult<ImageData> {
    Err(super::types::DecodeError::unsupported(
        Some(ImageFormat::Heic),
        "当前平台没有可用的 HEIC/HEIF 解码后端。",
    ))
}
