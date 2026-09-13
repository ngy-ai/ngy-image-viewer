//! Linux 上的 AVIF 解码：libavif（或 libheif 的 AVIF 能力）。
//!
//! # 当前状态：未接入
//!
//! 正确做法是经 `libavif` 的 Rust 绑定调用 `avifDecoderReadFile`，
//! 再用 `avifImageYUVToRGB` 完成色彩转换。
//!
//! 但 libavif 是 C 库，接入它会给构建引入一个必须由发行版提供的系统依赖；
//! 而本项目当前没有 Linux 验证环境，无法确认链接方式与版本兼容性。
//! 与其提交一段没验证过的绑定，这里如实说明并提供替代路径。
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
            "当前构建尚未接入 Linux 的 libavif 解码路径，暂时无法打开「{name}」。\
             可先用 `avifdec`（libavif-bin 包）转成 PNG 再查看。"
        ),
    ))
}
