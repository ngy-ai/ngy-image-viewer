//! JPEG 2000（JP2 / J2K / JPX 等）解码。
//!
//! # 走哪条路
//!
//! 用 `jpeg2k` crate 的 **C 后端**（openjpeg-sys，工业级 OpenJPEG 库的绑定）做跨平台解码。
//! 之前试过的纯 Rust 后端（openjp2）在当前 Rust 工具链下会在 codec 析构时触发 UB 崩溃，
//! 因此这里只走 C 后端（默认 feature 即是）。代价是构建环境必须具备 C 编译器与 Windows SDK，
//! 但换来一个稳定、可靠的跨平台 JP2 解码器，也符合项目「不引入不可验证的崩溃」的底线。
//!
//! # 像素口径
//!
//! `jpeg2k` 把各分量按精度（8/16 位）缩放到 0..=255 / 0..=65535，再由这里统一收敛成
//! 项目要求的**非预乘 RGBA8、逐行紧密排列**（每行 `width*4`，无 padding）。灰度与 RGB
//! 都补足 alpha 通道：原图没有透明分量时填 255（不透明），有透明分量时以真实 alpha 为准。

use std::path::Path;

use jpeg2k::error::Error as J2kError;
use jpeg2k::{Image as J2kImage, ImageData as J2kImageData, ImagePixelData};

use super::Decoder;
use super::types::{DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat};
use super::sniff;

pub struct Jp2Decoder;

impl Decoder for Jp2Decoder {
    fn id(&self) -> &'static str {
        "jp2"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Jp2]
    }

    fn probe(&self, head: &[u8]) -> bool {
        sniff::sniff_magic(head) == Some(ImageFormat::Jp2)
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_path(src, limits)
    }
}

fn decode_path(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let bytes = std::fs::read(src).map_err(|error| DecodeError::io(src, error))?;

    // 整文件交给 OpenJPEG 解码：它会自行区分 JP2 容器与裸 J2K codestream，无需我们预先分流。
    let image = J2kImage::from_bytes(&bytes).map_err(decode_error_from)?;

    // 分量数据按精度缩放后，收敛成统一的 RGBA 布局。
    // alpha 默认 255：无透明分量时视为不透明，有透明分量时以真实 alpha 为准。
    let pixel = image.get_pixels(Some(255)).map_err(decode_error_from)?;

    let width = pixel.width;
    let height = pixel.height;
    limits.check_dimensions(width, height)?;

    let rgba = to_rgba8(&pixel)?;
    if rgba.len() != width as usize * height as usize * 4 {
        return Err(DecodeError::corrupt(
            "JPEG 2000 解码出的像素数量与声明的尺寸不符，文件可能已损坏",
        ));
    }

    Ok(ImageData::single(
        ImageFormat::Jp2,
        Frame::new(width, height, rgba, 0),
    ))
}

/// 把 `jpeg2k` 的解码错误分流成我们自己的 [`DecodeError`]。
///
/// 色彩空间 / 分量布局不支持（如 CMYK）→ `Unsupported`（引导用户转 RGB PNG/JPEG）；
/// 其余（文件损坏、不支持的 JP2 特性、空指针等）→ `Corrupt`。
fn decode_error_from(error: J2kError) -> DecodeError {
    match error {
        J2kError::UnsupportedColorSpaceError(_) | J2kError::UnsupportedComponentsError(_) => {
            DecodeError::unsupported(
                Some(ImageFormat::Jp2),
                "该 JPEG 2000 使用了当前版本无法转换的色彩空间或分量布局（如 CMYK）。请先转换为 RGB 的 PNG 或 JPEG 再打开。",
            )
        }
        other => DecodeError::corrupt(format!("JPEG 2000 解码失败：{other}")),
    }
}

/// 把 `jpeg2k` 解出的像素收敛成项目统一的非预乘 RGBA8、逐行紧密排列。
fn to_rgba8(data: &J2kImageData) -> DecodeResult<Vec<u8>> {
    use ImagePixelData::*;
    let width = data.width as usize;
    let height = data.height as usize;

    let mut out = Vec::with_capacity(width * height * 4);
    match &data.data {
        // 8 位：分量已经按 0..=255 缩放好，直接拼装。
        Rgba8(d) => out.extend_from_slice(d),
        La8(d) => {
            for chunk in d.chunks_exact(2) {
                let [l, a] = [chunk[0], chunk[1]];
                out.extend_from_slice(&[l, l, l, a]);
            }
        }
        // 16 位：分量按 0..=65535 缩放，再线性映射到 0..=255。
        Rgba16(d) => {
            for chunk in d.chunks_exact(4) {
                out.push(scale16(chunk[0]));
                out.push(scale16(chunk[1]));
                out.push(scale16(chunk[2]));
                out.push(scale16(chunk[3]));
            }
        }
        La16(d) => {
            for chunk in d.chunks_exact(2) {
                let l = scale16(chunk[0]);
                out.extend_from_slice(&[l, l, l, scale16(chunk[1])]);
            }
        }
        // `get_pixels(Some(255))` 只会产出带 alpha 的 8/16 位布局（见 jpeg2k 内部逻辑）；
        // 万一出现其它布局（例如裸灰度/裸 RGB），当作不支持，引导用户转格式。
        other => {
            return Err(DecodeError::unsupported(
                Some(ImageFormat::Jp2),
                format!(
                    "该 JPEG 2000 的像素布局（{:?}）当前版本无法转换。请先转换为 PNG 或 JPEG 再打开。",
                    other
                ),
            ));
        }
    }
    Ok(out)
}

/// 16 位分量（0..=65535）映射到 8 位（0..=255）的线性缩放。
///
/// 用整数运算 `v*255/65535`（四舍五入），避免浮点误差在大量像素上累积。
fn scale16(v: u16) -> u8 {
    ((v as u32 * 255 + 32767) / 65535) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 直接验证像素收敛逻辑：无需真实 JP2 样本，构造 jpeg2k 的 `ImageData` 即可。
    fn j2k(width: u32, height: u32, data: ImagePixelData) -> J2kImageData {
        J2kImageData {
            width,
            height,
            format: jpeg2k::ImageFormat::Rgb8,
            data,
        }
    }

    #[test]
    fn rgba8_is_passed_through_verbatim() {
        // 2×1 的 RGBA8：应原样透传，不做任何改动。
        let src = vec![10, 20, 30, 40, 50, 60, 70, 80];
        let rgba = to_rgba8(&j2k(2, 1, ImagePixelData::Rgba8(src.clone())))
            .expect("RGBA8 应能收敛");
        assert_eq!(rgba, src);
    }

    #[test]
    fn gray_alpha_expands_to_opaque_rgb() {
        // 灰度 + alpha（La8）：(l, a) -> (l, l, l, a)。
        let src = vec![100, 200];
        let rgba = to_rgba8(&j2k(1, 1, ImagePixelData::La8(src))).expect("La8 应能收敛");
        assert_eq!(rgba, vec![100, 100, 100, 200]);
    }

    #[test]
    fn rgba16_scales_down_to_8bit_linearly() {
        // 16 位满量程 0xFFFF 映射到 255；0x0000 -> 0；中间值四舍五入。
        let src = vec![0x0000, 0xFFFF, 0x8000, 0x1234];
        let rgba = to_rgba8(&j2k(1, 1, ImagePixelData::Rgba16(src))).expect("Rgba16 应能收敛");
        assert_eq!(rgba, vec![0, 255, scale16(0x8000), scale16(0x1234)]);
    }

    #[test]
    fn unknown_layout_is_rejected_not_silent() {
        // 不应出现的裸灰度布局（无 alpha）：必须显式拒绝，绝不能交付错图。
        let result = to_rgba8(&j2k(1, 1, ImagePixelData::L8(vec![42])));
        assert!(matches!(result, Err(DecodeError::Unsupported { .. })));
    }
}
