//! macOS 上的 AVIF 解码：ImageIO（macOS 13 起原生支持 AVIF）。
//!
//! # 当前状态：未接入
//!
//! 正确做法与 HEIC 一致：经 objc2 的 ImageIO 绑定调用
//! `CGImageSourceCreateWithURL` + `CGImageSourceCreateImageAtIndex`。
//!
//! 这段代码只能在 macOS 上编译与运行，而本项目当前没有 macOS 验证环境。
//! 与其提交一段从未被编译器检查过的 unsafe 代码，这里如实说明并提供替代路径。
//! 接入时只需要替换 `decode` 的实现，其余各层一行都不用改。

use std::path::Path;

use crate::decode::types::{DecodeError, DecodeLimits, DecodeResult, ImageData, ImageFormat};

pub fn decode(src: &Path, _limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let name = src
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| src.display().to_string());

    Err(DecodeError::unsupported(
        Some(ImageFormat::Avif),
        format!(
            "当前构建尚未接入 macOS 的 ImageIO 解码路径，暂时无法打开「{name}」。\
             可先用「预览」打开，并导出为 JPEG / PNG 再查看。"
        ),
    ))
}
