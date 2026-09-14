//! JPEG XR（HD Photo）解码：目前仅 Windows 通过 WIC 原生支持。
//!
//! # 为什么只做 Windows
//!
//! JPEG XR 的解码能力分布极不均衡：Windows 的 WIC 从 Vista 起就内置了
//! HD Photo / WMP 编解码器，零安装即可用；而 macOS 与 Linux 没有普遍可用的
//! 系统级后端（ImageIO 与大多数发行版都不自带）。为避免引入 C 语言解码库拖慢构建，
//! 其它平台暂时显式告知「不支持」，而不是静默失败。
//!
//! # 统一的失败语义
//!
//! 复用 [`crate::decode::wic`]：缺组件时映射成 [`DecodeError::MissingPlatformSupport`]，
//! 并附上能让用户照做的修复提示。

use std::path::Path;

use super::Decoder;
use super::types::{DecodeLimits, DecodeResult, ImageData, ImageFormat};

#[cfg(windows)]
mod jxr_windows;

pub struct JxrDecoder;

impl Decoder for JxrDecoder {
    fn id(&self) -> &'static str {
        "platform-jxr"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Jxr]
    }

    fn probe(&self, head: &[u8]) -> bool {
        super::sniff::sniff_magic(head) == Some(ImageFormat::Jxr)
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_jxr(src, limits)
    }
}

#[cfg(windows)]
fn decode_jxr(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    jxr_windows::decode(src, limits)
}

#[cfg(not(windows))]
fn decode_jxr(_src: &Path, _limits: &DecodeLimits) -> DecodeResult<ImageData> {
    Err(super::types::DecodeError::unsupported(
        Some(ImageFormat::Jxr),
        "当前平台没有可用的 JPEG XR 解码后端（仅 Windows 通过系统组件支持）。",
    ))
}
