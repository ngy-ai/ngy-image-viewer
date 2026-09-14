//! MNG / JNG 处理：识别 + 友好拒绝。
//!
//! # 技术现状
//!
//! MNG（多图像/动画容器）与 JNG（JPEG 变体容器）在 Rust 生态里**没有任何可用的解码库**，
//! 唯一实现是已停更的 C 库 libmng（需系统库、且需 C 构建链）。因此本项目只做两件事：
//!
//! 1. **识别**：由 sniff 的 8 字节签名判定（见 [`crate::decode::sniff`]）；
//! 2. **友好拒绝**：解码时给出一条「这是什么格式、为什么打不开、能怎么转」的提示，
//!    绝不静默、绝不 panic。
//!
//! 将来若需要真解码，可在此接入 libmng（或自行实现 MNG 的 PNG/JNG 子图拼装），
//! 届时只改这一处即可，调用方无需变动。

use std::path::Path;

use super::Decoder;
use super::types::{DecodeError, DecodeLimits, DecodeResult, ImageData, ImageFormat};

pub struct MngDecoder;

impl Decoder for MngDecoder {
    fn id(&self) -> &'static str {
        "mng"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Mng, ImageFormat::Jng]
    }

    fn probe(&self, head: &[u8]) -> bool {
        crate::decode::sniff::sniff_magic(head) == Some(ImageFormat::Mng)
            || crate::decode::sniff::sniff_magic(head) == Some(ImageFormat::Jng)
    }

    fn decode(&self, src: &Path, _limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_path(src)
    }
}

fn decode_path(src: &Path) -> DecodeResult<ImageData> {
    // 不需要读整个文件，只要头部就能判定是 MNG 还是 JNG。
    let head = super::read_head(src, 8)?;
    let format = if head.starts_with(&[0x8A, b'M', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        ImageFormat::Mng
    } else {
        ImageFormat::Jng
    };

    Err(DecodeError::unsupported(
        Some(format),
        match format {
            ImageFormat::Mng => {
                "MNG（多图像 / 动画容器）当前版本暂不支持解码。可先用 ffmpeg 或专用工具将其拆为 PNG/GIF/APNG 再打开。"
            }
            ImageFormat::Jng => {
                "JNG（JPEG 变体容器）当前版本暂不支持解码。可先将其转为 JPEG/PNG 再打开。"
            }
            _ => "该格式当前版本暂不支持解码。",
        },
    ))
}
