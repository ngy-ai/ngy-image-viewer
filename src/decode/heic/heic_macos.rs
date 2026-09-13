//! macOS 上的 HEIC 解码：ImageIO。
//!
//! # 当前状态：未接入
//!
//! macOS 的 ImageIO 天然支持 HEIC，正确做法是经由 objc2 系列的 ImageIO 绑定调用
//! `CGImageSourceCreateWithURL` + `CGImageSourceCreateImageAtIndex`，
//! 再把 `CGImage` 画进一块 RGBA 缓冲区。
//!
//! 但这段代码只能在 macOS 上编译与运行，而本项目当前没有 macOS 的验证环境。
//! 与其提交一段**从未被编译器检查过**的 unsafe 平台代码（很可能一上来就编不过，
//! 或者悄悄产出错误的像素），这里选择如实说明并提供替代路径。
//! 这符合本层「绝不静默失败、绝不假装支持」的一贯约定。
//!
//! 接入时只需要把 `decode` 换成 ImageIO 实现，其余各层一行都不用改。

use std::path::Path;

use crate::decode::types::{DecodeError, DecodeLimits, DecodeResult, ImageData, ImageFormat};

pub fn decode(src: &Path, _limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let name = src
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| src.display().to_string());

    Err(DecodeError::unsupported(
        Some(ImageFormat::Heic),
        format!(
            "当前构建尚未接入 macOS 的 ImageIO 解码路径，暂时无法打开「{name}」。\
             可先用「预览」或「照片」打开，并导出为 JPEG / PNG 再查看。"
        ),
    ))
}
