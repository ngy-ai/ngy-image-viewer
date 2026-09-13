//! Linux 上的 HEIC 解码：libheif。
//!
//! # 当前状态：未接入
//!
//! 正确做法是经由 `libheif-rs` 调用系统/发行版提供的 libheif
//! （`heif_context_read_from_file` → `heif_context_get_primary_image_handle`
//! → `heif_decode_image`，输出 RGBA）。
//!
//! 但 libheif 是 C 库，接入它会给构建引入一个必须由发行版提供的系统依赖；
//! 而本项目当前没有 Linux 验证环境，无法确认链接方式、版本兼容性与像素布局。
//! 与其提交一段没验证过的绑定，这里选择如实说明并提供替代路径。
//!
//! 接入时只需要把 `decode` 换成 libheif-rs 实现，其余各层一行都不用改。

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
            "当前构建尚未接入 Linux 的 libheif 解码路径，暂时无法打开「{name}」。\
             可先用 `heif-convert`（libheif-examples 包）转成 JPEG / PNG 再查看。"
        ),
    ))
}
