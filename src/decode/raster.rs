//! 主流栅格格式解码（PNG / JPEG / GIF / BMP / WebP / TIFF / ICO / PNM / TGA /
//! DDS / HDR / EXR / QOI / Farbfeld）。
//!
//! 像素活儿全部交给 `image` crate，本模块只负责它不管的三件事：
//!
//! 1. **限制先行**：解码前先看尺寸，超限直接拒绝，避免「打开一张图结果内存爆掉」；
//! 2. **多帧**：GIF / APNG / 动画 WebP 必须走各自的动画解码器，通用路径只能出第一帧；
//! 3. **统一产出**：无论源格式是什么，一律收敛成 RGBA8 + EXIF 摘要。
//!
//! 为什么不把 `image` 的 `DynamicImage` 直接往上传：那会把「有哪些格式」这个知识
//! 泄漏到渲染层，而且 `DynamicImage` 是十几种布局的枚举，渲染层还得再匹配一遍。

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::metadata::Orientation as CrateOrientation;
use image::{
    AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat as CrateFormat, ImageReader, Limits,
};

use super::exif;
use super::types::{DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat, Orientation};
use super::Decoder;

/// 本解码器负责的格式，与 `image` crate 实际启用的解码器一一对应。
///
/// 刻意不包含 AVIF：`image` 的 AVIF **解码**依赖 `dav1d`（C 库），
/// 与本项目「纯 Rust、不引入额外构建链」的约束冲突，改由独立解码器负责。
///
/// TIFF 则**有意留在这里**：多页 TIFF 归 `decode::tiff` 负责（`image` 只读第一个 IFD），
/// 只有它失败时才会轮到这条路径，用来兜住「至少还能出第一页」。注册表里
/// `tiff::TiffDecoder` 排在前面，所以正常情况走不到这里。
pub const FORMATS: &[ImageFormat] = &[
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::Gif,
    ImageFormat::WebP,
    ImageFormat::Tiff,
    ImageFormat::Bmp,
    ImageFormat::Ico,
    ImageFormat::Pnm,
    ImageFormat::Tga,
    ImageFormat::Hdr,
    ImageFormat::OpenExr,
    ImageFormat::Qoi,
    ImageFormat::Farbfeld,
];

pub struct RasterDecoder;

impl Decoder for RasterDecoder {
    fn id(&self) -> &'static str {
        "raster"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        FORMATS
    }

    fn probe(&self, head: &[u8]) -> bool {
        super::sniff::sniff_magic(head).is_some_and(|format| FORMATS.contains(&format))
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_path(src, limits)
    }
}

fn decode_path(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    // 用 `ImageReader::open` 而不是手工 `File::open`：它会把「扩展名 → 格式」
    // 作为一个初始猜测填进去，`with_guessed_format` 只在 magic bytes 可识别时覆盖它。
    // TGA 这类没有特征码的格式正是靠这条兜底路径才能被认出来。
    let mut reader = ImageReader::open(src).map_err(|error| DecodeError::io(src, error))?;
    reader.limits(crate_limits(limits));
    let reader = reader
        .with_guessed_format()
        .map_err(|error| DecodeError::io(src, error))?;

    let Some(crate_format) = reader.format() else {
        return Err(DecodeError::unsupported(
            None,
            "内容无法识别，扩展名也不是已知的图片格式",
        ));
    };
    let Some(format) = format_from_crate(crate_format) else {
        return Err(DecodeError::unsupported(
            None,
            format!("{crate_format:?} 不在当前构建支持的解码器范围内"),
        ));
    };

    match format {
        ImageFormat::Gif => decode_gif(reader, format, limits, src),
        ImageFormat::Png => decode_png(reader, format, limits, src),
        ImageFormat::WebP => decode_webp(reader, format, limits, src),
        _ => {
            let decoder = reader
                .into_decoder()
                .map_err(|error| from_image_error(error, src))?;
            decode_single(decoder, format, limits, src)
        }
    }
}

/// GIF：`GifDecoder` 对静态 GIF 也会产出恰好一帧，所以两条路径没有分支。
fn decode_gif(
    reader: ImageReader<BufReader<File>>,
    format: ImageFormat,
    limits: &DecodeLimits,
    src: &Path,
) -> DecodeResult<ImageData> {
    let mut decoder =
        GifDecoder::new(reader.into_inner()).map_err(|error| from_image_error(error, src))?;
    let (width, height) = decoder.dimensions();
    limits.check_dimensions(width, height)?;

    let orientation =
        orientation_from_crate(decoder.orientation().unwrap_or(CrateOrientation::NoTransforms));
    let frames = decoder
        .into_frames()
        .collect_frames()
        .map_err(|error| from_image_error(error, src))?;
    limits.check_frame_count(frames.len())?;

    let frames = convert_frames(frames, limits, src)?;
    finish_animated(format, frames, orientation, src)
}

/// APNG 与普通 PNG 共用容器，必须先用 `is_apng()` 判断，
/// 否则把静态 PNG 交给 `ApngDecoder` 会得到**空迭代器**（不是报错，是静默无帧）。
fn decode_png(
    reader: ImageReader<BufReader<File>>,
    format: ImageFormat,
    limits: &DecodeLimits,
    src: &Path,
) -> DecodeResult<ImageData> {
    let decoder = PngDecoder::with_limits(reader.into_inner(), crate_limits(limits))
        .map_err(|error| from_image_error(error, src))?;
    let (width, height) = decoder.dimensions();
    limits.check_dimensions(width, height)?;

    let animated = decoder.is_apng().map_err(|error| from_image_error(error, src))?;
    if animated {
        let frames = decoder
            .apng()
            .map_err(|error| from_image_error(error, src))?
            .into_frames()
            .collect_frames()
            .map_err(|error| from_image_error(error, src))?;
        limits.check_frame_count(frames.len())?;

        let frames = convert_frames(frames, limits, src)?;
        if frames.is_empty() {
            return Err(DecodeError::corrupt("APNG 未包含任何可解码的帧"));
        }
        // APNG 的 eXIf 段极少见，这里不为了元数据再付一次解析成本。
        return Ok(ImageData {
            format,
            frames,
            orientation: Orientation::Normal,
            exif: None,
            supersample: 1.0,
        });
    }

    decode_single(decoder, format, limits, src)
}

fn decode_webp(
    reader: ImageReader<BufReader<File>>,
    format: ImageFormat,
    limits: &DecodeLimits,
    src: &Path,
) -> DecodeResult<ImageData> {
    let mut decoder =
        WebPDecoder::new(reader.into_inner()).map_err(|error| from_image_error(error, src))?;
    let (width, height) = decoder.dimensions();
    limits.check_dimensions(width, height)?;

    let exif_raw = decoder.exif_metadata().ok().flatten();
    let orientation =
        orientation_from_crate(decoder.orientation().unwrap_or(CrateOrientation::NoTransforms));

    if decoder.has_animation() {
        let frames = decoder
            .into_frames()
            .collect_frames()
            .map_err(|error| from_image_error(error, src))?;
        limits.check_frame_count(frames.len())?;

        let frames = convert_frames(frames, limits, src)?;
        if frames.is_empty() {
            return Err(DecodeError::corrupt("动画 WebP 未包含任何可解码的帧"));
        }
        return Ok(ImageData {
            format,
            frames,
            orientation,
            exif: summary_from_raw(exif_raw),
            supersample: 1.0,
        });
    }

    let mut data = decode_single(decoder, format, limits, src)?;
    data.orientation = orientation;
    if data.exif.is_none() {
        data.exif = summary_from_raw(exif_raw);
    }
    Ok(data)
}

/// 单帧路径。适用于除 GIF/APNG/动画 WebP 之外的全部格式。
fn decode_single(
    mut decoder: impl ImageDecoder,
    format: ImageFormat,
    limits: &DecodeLimits,
    src: &Path,
) -> DecodeResult<ImageData> {
    let (width, height) = decoder.dimensions();
    limits.check_dimensions(width, height)?;

    let orientation =
        orientation_from_crate(decoder.orientation().unwrap_or(CrateOrientation::NoTransforms));
    // 先取元数据再消费 decoder：`exif_metadata` 只需要头信息，开销很小，
    // 但错过这个时机就得为一张几十 MB 的 TIFF 再读一次文件。
    let exif_raw = decoder.exif_metadata().ok().flatten();

    let image = DynamicImage::from_decoder(decoder).map_err(|error| from_image_error(error, src))?;
    let buffer = image.into_rgba8();
    let (width, height) = buffer.dimensions();
    limits.check_dimensions(width, height)?;

    Ok(ImageData {
        format,
        frames: vec![Frame::new(width, height, buffer.into_raw(), 0)],
        orientation,
        exif: summary_from_raw(exif_raw),
        supersample: 1.0,
    })
}

fn finish_animated(
    format: ImageFormat,
    frames: Vec<Frame>,
    orientation: Orientation,
    src: &Path,
) -> DecodeResult<ImageData> {
    if frames.is_empty() {
        return Err(DecodeError::corrupt(format!(
            "动图未包含任何可解码的帧（{}）",
            src.display()
        )));
    }
    Ok(ImageData {
        format,
        frames,
        orientation,
        exif: None,
        supersample: 1.0,
    })
}

/// `image` 的帧序列 → 我们的帧序列，顺便把延迟与内存上限一起核掉。
fn convert_frames(
    frames: Vec<image::Frame>,
    limits: &DecodeLimits,
    src: &Path,
) -> DecodeResult<Vec<Frame>> {
    let mut out = Vec::with_capacity(frames.len());
    let mut total_bytes: u64 = 0;

    for frame in frames {
        let (numer, denom) = frame.delay().numer_denom_ms();
        // 分母为 0 是损坏数据；此时按 0 处理，播放层会退回 100ms 的默认值。
        let delay_ms = if denom == 0 {
            0
        } else {
            (u64::from(numer) / u64::from(denom)).min(u64::from(u32::MAX)) as u32
        };

        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        limits.check_dimensions(width, height)?;

        let frame = Frame::new(width, height, buffer.into_raw(), delay_ms);
        total_bytes += frame.byte_len() as u64;
        // 单帧都不大、加起来撑爆内存的多帧图，是最容易漏掉的一类。
        // 所以这里按「累计占用」而不是「单帧尺寸」来卡。
        if total_bytes > limits.max_alloc_bytes {
            return Err(DecodeError::TooMuchMemory {
                used: total_bytes,
                limit: limits.max_alloc_bytes,
            });
        }
        out.push(frame);
    }

    if out.is_empty() {
        return Err(DecodeError::corrupt(format!(
            "没有任何可解码的帧（{}）",
            src.display()
        )));
    }
    Ok(out)
}

/// 我们的限制 → `image` crate 的限制。
///
/// 对 `decode::tiff` 同样可见：那个解码器是自己构造 `image` 的解码器的，
/// 少了这一步 `max_alloc` 就不会生效（`ImageReader::into_decoder` 里做的正是这件事）。
pub(crate) fn crate_limits(limits: &DecodeLimits) -> Limits {
    // `Limits` 是 `#[non_exhaustive]`，不能用结构体字面量构造；
    // 从「无限制」出发再逐项收紧，语义也更贴合我们的意图。
    let mut crate_limits = Limits::no_limits();
    crate_limits.max_image_width = Some(limits.max_width);
    crate_limits.max_image_height = Some(limits.max_height);
    crate_limits.max_alloc = Some(limits.max_alloc_bytes);
    crate_limits
}

/// `image` crate 的格式枚举 → 我们的格式枚举。不认识的返回 `None`。
fn format_from_crate(format: CrateFormat) -> Option<ImageFormat> {
    Some(match format {
        CrateFormat::Png => ImageFormat::Png,
        CrateFormat::Jpeg => ImageFormat::Jpeg,
        CrateFormat::Gif => ImageFormat::Gif,
        CrateFormat::WebP => ImageFormat::WebP,
        CrateFormat::Tiff => ImageFormat::Tiff,
        CrateFormat::Bmp => ImageFormat::Bmp,
        CrateFormat::Ico => ImageFormat::Ico,
        CrateFormat::Pnm => ImageFormat::Pnm,
        CrateFormat::Tga => ImageFormat::Tga,
        CrateFormat::Dds => ImageFormat::Dds,
        CrateFormat::Hdr => ImageFormat::Hdr,
        CrateFormat::OpenExr => ImageFormat::OpenExr,
        CrateFormat::Qoi => ImageFormat::Qoi,
        CrateFormat::Farbfeld => ImageFormat::Farbfeld,
        _ => return None,
    })
}

/// `image` crate 的方向枚举 → 我们的方向枚举。
///
/// 关键点：EXIF 5 对应 `Rotate90FlipH`，EXIF 7 对应 `Rotate270FlipH`。
/// 这两个名字看起来和我们的 `Transpose` / `Transverse` 对不上号，
/// 但按 EXIF 定义换算后是完全一致的（`decode/orientation.rs` 的测试覆盖了这点）。
///
/// 与下面两个函数一样对 `decode::tiff` 可见：那个解码器复用 `image` 的 TIFF 实现，
/// 方向、EXIF、错误归类这三处**必须与这里完全一致**，抄一份迟早会分叉。
pub(crate) fn orientation_from_crate(orientation: CrateOrientation) -> Orientation {
    match orientation {
        CrateOrientation::NoTransforms => Orientation::Normal,
        CrateOrientation::FlipHorizontal => Orientation::FlipHorizontal,
        CrateOrientation::Rotate180 => Orientation::Rotate180,
        CrateOrientation::FlipVertical => Orientation::FlipVertical,
        CrateOrientation::Rotate90FlipH => Orientation::Transpose,
        CrateOrientation::Rotate90 => Orientation::Rotate90,
        CrateOrientation::Rotate270FlipH => Orientation::Transverse,
        CrateOrientation::Rotate270 => Orientation::Rotate270,
    }
}

/// 用解码器挖出来的 EXIF 段构造信息摘要。解析失败或全是空字段时返回 `None`。
pub(crate) fn summary_from_raw(raw: Option<Vec<u8>>) -> Option<super::types::ExifSummary> {
    let summary = exif::from_raw(raw.as_deref()?)?.summary;
    if summary.is_empty() { None } else { Some(summary) }
}

/// `image::ImageError` → 我们的错误类型。
///
/// `image` 的错误分类太粗（Unsupported / Limits / Io / Decoding），
/// 这里把它映射到用户能据此采取行动的四类上。
pub(crate) fn from_image_error(error: image::ImageError, src: &Path) -> DecodeError {
    match error {
        // 「提前读到结尾」不是权限/占用问题，而是文件被截断。
        // 归到 Io 会让提示误导用户去检查文件是否被别的程序占用。
        image::ImageError::IoError(error)
            if error.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            DecodeError::corrupt("文件在读取过程中提前结束，可能下载或拷贝不完整")
        }
        image::ImageError::IoError(error) => DecodeError::io(src, error),
        image::ImageError::Limits(error) => DecodeError::corrupt(format!("超出解码资源限制：{error}")),
        image::ImageError::Unsupported(error) => DecodeError::unsupported(None, error.to_string()),
        other => DecodeError::corrupt(other.to_string()),
    }
}
