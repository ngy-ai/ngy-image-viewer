//! Windows 上的 AVIF 解码。
//!
//! 解码流程完全复用 [`crate::decode::wic`]（WIC 本身就能同时处理 HEIC 与 AVIF），
//! 这里只负责提供「缺组件时该说什么」这一条格式专属的信息。

use std::path::Path;

use crate::decode::types::{DecodeLimits, DecodeResult, ImageData, ImageFormat};
use crate::decode::wic::{self, WicRequest};

/// 缺组件时用户看到的「缺少什么」。
const COMPONENT: &str = "AVIF";

/// 缺组件时可以照做的安装步骤。
///
/// 注意这里必须写 **AV1** 而不是 HEIF：两者是不同的扩展，
/// 提示错了会让用户装完还是打不开。
const INSTALL_HINT: &str = "请在 Microsoft Store 安装免费的「AV1 图像扩展」（AV1 Image Extension）后重新打开。\
                            若已安装，可在「设置 → 应用 → 已安装的应用」中确认它没有被移除或禁用。";

pub fn decode(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    wic::decode(
        src,
        limits,
        WicRequest {
            format: ImageFormat::Avif,
            component: COMPONENT,
            install_hint: INSTALL_HINT,
        },
    )
}
