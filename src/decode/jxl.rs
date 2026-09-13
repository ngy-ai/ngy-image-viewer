//! JPEG XL 解码（jxl-oxide，纯 Rust 实现，无 C 依赖）。
//!
//! # 两种封装都能读
//!
//! `.jxl` 文件有两种完全不同的封装：裸 codestream（以 `0xFF 0x0A` 开头）与
//! ISOBMFF 容器（以 12 字节签名盒开头）。jxl-oxide 在解析阶段自己识别两者，
//! 因此这里不需要按 `sniff` 的结果分流。
//!
//! # 方向为什么不是 `ImageData::orientation`
//!
//! jxl-oxide 的 `Render::stream()` 文档明确写着「Orientation is applied」——
//! 它把方向**烘进像素**了。所以这里必须报 `Orientation::Normal`，
//! 否则渲染层会拿着已经摆正的像素再转一次，得到一张歪图。
//!
//! # 色彩空间
//!
//! `write_to_buffer::<u8>` 输出的已经是 sRGB 非线性 8 位采样，
//! 色彩管理（ICC / CICP）由 jxl-oxide 内部完成，我们不需要再做转换。
//! 唯一需要自己处理的是**通道布局**：灰度 / 灰度+alpha / RGB / RGBA / CMYK / CMYK+alpha。

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use jxl_oxide::{AllocTracker, JxlImage, PixelFormat};

use super::Decoder;
use super::types::{DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat, Orientation};

/// 单个 JPEG XL 解码器的内存上限。
///
/// `AllocTracker` 是 jxl-oxide 提供的「解码过程中累计分配」闸门：
/// 它比我们事后检查尺寸更早生效，能挡住精心构造的解压炸弹。
/// 这里直接复用 [`DecodeLimits::max_alloc_bytes`]，让两种限制口径一致。
fn alloc_budget(limits: &DecodeLimits) -> usize {
    limits.max_alloc_bytes.min(usize::MAX as u64) as usize
}

pub struct JxlDecoder;

impl Decoder for JxlDecoder {
    fn id(&self) -> &'static str {
        "jxl-oxide"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Jxl]
    }

    fn probe(&self, head: &[u8]) -> bool {
        super::sniff::sniff_magic(head) == Some(ImageFormat::Jxl)
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_jxl(src, limits)
    }
}

fn decode_jxl(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let file = File::open(src).map_err(|error| DecodeError::io(src, error))?;
    let reader = BufReader::with_capacity(64 * 1024, file);

    let image = JxlImage::builder()
        .alloc_tracker(AllocTracker::with_limit(alloc_budget(limits)))
        .read(reader)
        .map_err(|error| jxl_failure(src, "解析文件失败", error))?;

    let frame = render_first_frame(&image, src, limits)?;

    Ok(ImageData {
        format: ImageFormat::Jxl,
        frames: vec![frame],
        // 见模块文档：方向已经被 jxl-oxide 应用到像素上了。
        orientation: Orientation::Normal,
        exif: None,
        supersample: 1.0,
    })
}

/// 渲染第 0 个关键帧并转换成 RGBA8。
fn render_first_frame(image: &JxlImage, src: &Path, limits: &DecodeLimits) -> DecodeResult<Frame> {
    let pixel_format = image.pixel_format();

    let render = image
        .render_frame(0)
        .map_err(|error| jxl_failure(src, "渲染失败", error))?;

    let mut stream = render.stream();
    let width = stream.width();
    let height = stream.height();
    // 先按流自己报的尺寸校验：这比用 `image.width()` 更可靠，
    // 因为它描述的就是接下来真正要写入的那块缓冲区。
    limits.check_dimensions(width, height)?;

    let channels = stream.channels() as usize;
    let expected_channels = pixel_format.channels();
    if channels == 0 || channels != expected_channels {
        return Err(DecodeError::corrupt(format!(
            "JPEG XL 通道数与像素格式不一致（流 {channels} / 格式 {expected_channels}）"
        )));
    }

    let pixel_count = width as usize * height as usize;
    let sample_count = pixel_count * channels;
    let mut samples = vec![0u8; sample_count];

    // `write_to_buffer` 每次只保证写一部分，必须循环到写满为止。
    // 它在写不出任何数据时返回 0（而不是报错），所以这里自己判定「没写满就是失败」，
    // 免得把半张图当成完整图交给渲染层。
    let mut written = 0usize;
    while written < sample_count {
        let step = stream.write_to_buffer::<u8>(&mut samples[written..]);
        if step == 0 {
            break;
        }
        written += step;
    }
    if written < sample_count {
        return Err(DecodeError::corrupt(format!(
            "JPEG XL 像素数据不完整（{} / {} 字节）",
            written, sample_count
        )));
    }

    let rgba8 = samples_to_rgba8(pixel_format, &samples, pixel_count);

    Ok(Frame::new(width, height, rgba8, 0))
}

/// 把交错采样转成紧密排列的 RGBA8。
///
/// 采样是**非预乘**的 sRGB 8 位值（与 `image` crate 的输出口径一致），
/// 整条解码链统一约定非预乘，预乘只在渲染层上传纹理前做一次。
///
/// 这里对 [`PixelFormat`] 做穷尽匹配、不留兜底分支：
/// 将来 jxl-oxide 新增通道布局时会直接编译失败，逼我们显式处理，
/// 而不是悄悄产出一张颜色错误的图。
fn samples_to_rgba8(format: PixelFormat, samples: &[u8], pixels: usize) -> Vec<u8> {
    let mut out = vec![0u8; pixels * 4];

    // 逐像素三通道填充的公共部分：灰度、灰度+alpha、RGB 都走同一条路径，
    // 只是取值来源不同。
    match format {
        PixelFormat::Gray => {
            for (pixel, chunk) in out.chunks_exact_mut(4).enumerate() {
                let luma = samples[pixel];
                chunk[0] = luma;
                chunk[1] = luma;
                chunk[2] = luma;
                chunk[3] = 255;
            }
        }
        PixelFormat::Graya => {
            for (pixel, chunk) in out.chunks_exact_mut(4).enumerate() {
                let luma = samples[pixel * 2];
                chunk[0] = luma;
                chunk[1] = luma;
                chunk[2] = luma;
                chunk[3] = samples[pixel * 2 + 1];
            }
        }
        PixelFormat::Rgb => {
            for (pixel, chunk) in out.chunks_exact_mut(4).enumerate() {
                chunk[..3].copy_from_slice(&samples[pixel * 3..pixel * 3 + 3]);
                chunk[3] = 255;
            }
        }
        PixelFormat::Rgba => {
            out.copy_from_slice(samples);
        }
        PixelFormat::Cmyk => {
            for (pixel, chunk) in out.chunks_exact_mut(4).enumerate() {
                let base = pixel * 4;
                let (r, g, b) = cmyk_to_rgb(
                    samples[base],
                    samples[base + 1],
                    samples[base + 2],
                    samples[base + 3],
                );
                chunk[0] = r;
                chunk[1] = g;
                chunk[2] = b;
                chunk[3] = 255;
            }
        }
        PixelFormat::Cmyka => {
            for (pixel, chunk) in out.chunks_exact_mut(4).enumerate() {
                let base = pixel * 5;
                let (r, g, b) = cmyk_to_rgb(
                    samples[base],
                    samples[base + 1],
                    samples[base + 2],
                    samples[base + 3],
                );
                chunk[0] = r;
                chunk[1] = g;
                chunk[2] = b;
                chunk[3] = samples[base + 4];
            }
        }
    }

    out
}

/// 最朴素的 CMYK → sRGB 换算。
///
/// 真正的色彩管理需要 ICC 配置文件，但 JPEG XL 里的 CMYK 极少见，
/// 而乘性公式是「看起来是对的」的通行做法：C=M=Y=0 时得到纯白，
/// K=255 时得到纯黑。用整数运算以避免浮点舍入导致的色偏。
fn cmyk_to_rgb(c: u8, m: u8, y: u8, k: u8) -> (u8, u8, u8) {
    let k = 255 - k as u32;
    let scale = |channel: u8| -> u8 { ((channel as u32 * k + 127) / 255) as u8 };
    (scale(255 - c), scale(255 - m), scale(255 - y))
}

/// 把 jxl-oxide 的错误包装成我们的错误类型。
///
/// jxl-oxide 用的是 `Box<dyn Error + Send + Sync>`，没有任何可匹配的变体，
/// 所以这里只能按调用阶段给出一句能定位问题的说明。
fn jxl_failure(src: &Path, stage: &str, error: impl std::fmt::Display) -> DecodeError {
    let name = src
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| src.display().to_string());
    DecodeError::corrupt(format!("「{name}」JPEG XL {stage}：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmyk_conversion_hits_the_expected_extremes() {
        // 没有任何油墨 → 纯白。
        assert_eq!(cmyk_to_rgb(0, 0, 0, 0), (255, 255, 255));
        // 只有黑版拉满 → 纯黑。
        assert_eq!(cmyk_to_rgb(0, 0, 0, 255), (0, 0, 0));
        // 青色满版、其余为零 → 去掉红色通道。
        assert_eq!(cmyk_to_rgb(255, 0, 0, 0), (0, 255, 255));
    }

    #[test]
    fn gray_samples_expand_to_rgba() {
        // 2 个像素的灰度图。
        let out = samples_to_rgba8(PixelFormat::Gray, &[10, 200], 2);
        assert_eq!(out, vec![10, 10, 10, 255, 200, 200, 200, 255]);
    }

    #[test]
    fn rgba_samples_keep_their_own_alpha() {
        let out = samples_to_rgba8(PixelFormat::Graya, &[10, 40, 200, 255], 2);
        assert_eq!(out, vec![10, 10, 10, 40, 200, 200, 200, 255]);
    }

    #[test]
    fn rgb_samples_gain_an_opaque_alpha_channel() {
        let out = samples_to_rgba8(PixelFormat::Rgb, &[1, 2, 3, 4, 5, 6], 2);
        assert_eq!(out, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn rgba_samples_are_copied_verbatim() {
        let samples = [1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(samples_to_rgba8(PixelFormat::Rgba, &samples, 2), samples);
    }

    #[test]
    fn probe_only_claims_jpeg_xl() {
        // 回归保护：解码器只声明自己认识 JXL，
        // 不能因为 probe 写得宽松就把别的格式抢过来。
        let decoder = JxlDecoder;
        assert_eq!(decoder.formats(), &[ImageFormat::Jxl]);
        assert!(!decoder.probe(b"\x89PNG\r\n\x1a\n"));
        assert!(decoder.probe(&[0xFF, 0x0A, 0x03]));
    }
}
