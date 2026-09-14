//! Windows 上的 JPEG XR 解码。
//!
//! 解码流程完全复用 [`crate::decode::wic`]（WIC 自带 HD Photo / WMP 编解码器，
//! 对 `.jxr` / `.wdp` / `.hdp` 都能识别），这里只负责提供「缺组件时该说什么」。

use std::path::Path;

use crate::decode::types::{DecodeLimits, DecodeResult, ImageData, ImageFormat};
use crate::decode::wic::{self, WicRequest};

/// 缺组件时用户看到的「缺少什么」。
const COMPONENT: &str = "JPEG XR";

/// 缺组件时可以照做的修复步骤。
///
/// Windows 的 WIC 从 Vista 起就内置了 HD Photo 编解码器，正常不会缺失。
/// 真出现 `COMPONENTNOTFOUND` 多半是系统组件损坏或被精简版系统移除，
/// 所以提示指向系统修复，而不是「去商店装扩展」（那适用于 HEIC/AVIF）。
const INSTALL_HINT: &str = "JPEG XR 解码依赖 Windows 自带的 WIC 组件。若提示缺失，多半是系统组件损坏：\
                             请以管理员身份运行 `sfc /scannow` 修复系统文件，或确认当前 Windows 版本未被精简掉图像组件。";

pub fn decode(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    wic::decode(
        src,
        limits,
        WicRequest {
            format: ImageFormat::Jxr,
            component: COMPONENT,
            install_hint: INSTALL_HINT,
        },
    )
}
