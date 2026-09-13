//! Windows 上的 HEIC 解码。
//!
//! 解码流程完全复用 [`crate::decode::wic`]（WIC 本身就能同时处理 HEIC 与 AVIF），
//! 这里只负责提供「缺组件时该说什么」这一条格式专属的信息。

use std::path::Path;

use crate::decode::types::{DecodeLimits, DecodeResult, ImageData, ImageFormat};
use crate::decode::wic::{self, WicRequest};

/// 缺组件时用户看到的「缺少什么」。
const COMPONENT: &str = "HEIC/HEIF";

/// 缺组件时可以照做的安装步骤。
///
/// WIC 自带 HEIC 的**容器**解析，但 HEVC 的实际解码要另装扩展。
/// 这是 Windows 用户打不开手机照片最常见的原因，所以提示要写得足够具体。
const INSTALL_HINT: &str = "请在 Microsoft Store 安装免费的「HEIF 图像扩展」（HEIF Image Extensions）后重新打开。\
                             若已安装，可在「设置 → 应用 → 已安装的应用」中确认它没有被移除或禁用。";

pub fn decode(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    wic::decode(
        src,
        limits,
        WicRequest {
            format: ImageFormat::Heic,
            component: COMPONENT,
            install_hint: INSTALL_HINT,
        },
    )
}
