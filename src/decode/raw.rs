//! 相机 RAW 原片解码。
//!
//! # 分工
//!
//! `rawloader` 负责它最擅长、也最难的那部分：从各家千奇百怪的容器里
//! 定位出传感器数据，并把标定参数（黑电平、白电平、白平衡系数、CFA 阵列、裁剪区）读出来。
//! 剩下三件事由我们自己做：
//!
//! 1. **裁剪**：`rawloader` 只告诉你「可用的矩形是哪一块」，不会替你裁；
//! 2. **去马赛克**：见 [`super::demosaic`]，双线性 + 按行并行；
//! 3. **色调映射**：线性光 → sRGB 编码值，否则画面会明显发暗。
//!
//! # 关于 CR3
//!
//! Canon 的 CR3 用的是 ISOBMFF 容器，`rawloader` 不支持。这里不假装能读，
//! 而是直接给一条可照做的说明（转 DNG）。**如实说明做不到的事，比含糊失败更有用。**

use std::path::Path;

use rawloader::{Orientation as RawOrientation, RawImage, RawImageData};

use super::Decoder;
use super::demosaic::{
    ChannelCalibration, SensorSamples, SensorView, bayer_to_rgba8, interleaved_rgb_to_rgba8,
    mono_to_rgba8,
};
use super::types::{
    DecodeError, DecodeLimits, DecodeResult, ExifSummary, Frame, ImageData, ImageFormat, Orientation,
};

/// 浮点 DNG 的满量程。
///
/// [`SensorSamples::Float`] 会把 0..1 的浮点值换算到 16 位整数的量纲，
/// 因此浮点数据的「白电平」就是这个常数。
const FLOAT_FULL_SCALE: f32 = 65535.0;

/// 白平衡增益的合理区间。
///
/// 少数机型的白平衡元数据是坏的（全零、负值、离群值），
/// 不夹一下会得到一张全绿或全品红的图。这个区间足以覆盖真实的白平衡差异。
const GAIN_RANGE: (f32, f32) = (0.25, 4.0);

pub struct RawDecoder;

impl Decoder for RawDecoder {
    fn id(&self) -> &'static str {
        "rawloader"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Raw]
    }

    fn probe(&self, head: &[u8]) -> bool {
        // 绝大多数相机原片与普通 TIFF 共用同一个文件头，此时结论完全来自扩展名。
        // 所以这里不仅要认 CR2 这种能在内容层确定的原片，还要对 TIFF 头自荐：
        // 一个被改名成 `.bin` 的 DNG，只能靠这条兜底路径才救得回来。
        matches!(
            super::sniff::sniff_magic(head),
            Some(ImageFormat::Raw | ImageFormat::Tiff)
        )
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_raw(src, limits)
    }
}

fn decode_raw(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    // CR3 是 ISOBMFF 容器，rawloader 明确不支持。与其把它丢给底层库、
    // 换来一句「找不到解码器」，不如直接把原因和下一步说出来。
    if has_extension(src, "cr3") {
        return Err(DecodeError::unsupported(
            Some(ImageFormat::Raw),
            "Canon CR3 使用了 ISOBMFF 容器，目前尚未支持解析。可先用相机自带软件或 Adobe DNG Converter 转成 DNG / JPEG。",
        ));
    }

    let raw = rawloader::decode_file(src).map_err(|error| raw_failure(&error))?;

    let cpp = raw.cpp.max(1);
    let sample_count = raw
        .width
        .checked_mul(raw.height)
        .and_then(|pixels| pixels.checked_mul(cpp))
        .ok_or_else(|| DecodeError::corrupt("相机原片声明的尺寸溢出（文件头可能已损坏）"))?;

    let samples = match &raw.data {
        RawImageData::Integer(data) => SensorSamples::Integer(data),
        RawImageData::Float(data) => SensorSamples::Float(data),
    };
    // 损坏的 RAW 元数据（声明尺寸与实际数据不符）是真实存在的。
    // 在这里挡住，后续所有索引就都是安全的，不会 panic。
    if !samples.covers(sample_count) {
        return Err(DecodeError::corrupt(format!(
            "相机原片声明的尺寸（{}×{}，每像素 {cpp} 分量）与实际数据（{} 个采样）不符，文件可能被截断",
            raw.width,
            raw.height,
            samples.len()
        )));
    }

    let [top, right, bottom, left] = raw.crops;
    let crop_width = raw.width.saturating_sub(left.saturating_add(right)).max(1);
    let crop_height = raw.height.saturating_sub(top.saturating_add(bottom)).max(1);

    // 尺寸上限必须在**去马赛克之前**检查：一亿像素的原片插值后是 400 MB，
    // 等分配完再发现超限就来不及了。
    let width = u32::try_from(crop_width)
        .map_err(|_| DecodeError::corrupt("相机原片宽度超出可表示范围"))?;
    let height = u32::try_from(crop_height)
        .map_err(|_| DecodeError::corrupt("相机原片高度超出可表示范围"))?;
    limits.check_dimensions(width, height)?;

    let calibration = calibration_for(&raw, matches!(raw.data, RawImageData::Float(_)));

    let rgba8 = if cpp >= 3 {
        // 极少数机型（例如 Canon 的 sRAW）直接把每像素三个分量存下来，没有马赛克可言。
        interleaved_rgb_to_rgba8(
            samples,
            raw.width,
            (left, top),
            crop_width,
            crop_height,
            &calibration,
        )
    } else {
        let view = SensorView {
            samples,
            stride: raw.width,
            origin: (left, top),
            width: crop_width,
            height: crop_height,
        };
        if raw.is_monochrome() {
            mono_to_rgba8(view, &calibration)
        } else {
            // 注意这里传的是**原始** CFA 而不是 `raw.cropped_cfa()`：
            // `SensorView` 用的是整幅传感器上的绝对坐标，而 `cropped_cfa()`
            // 已经把裁剪位移烘进去了。两者同时使用会「偏移两次」，颜色全错。
            bayer_to_rgba8(view, &raw.cfa, &calibration)
        }
    };

    let exif = super::exif::from_path(src);

    Ok(ImageData {
        format: ImageFormat::Raw,
        frames: vec![Frame::new(width, height, rgba8, 0)],
        orientation: resolve_orientation(&raw, exif.as_ref().map(|data| data.orientation)),
        exif: build_summary(&raw, exif.map(|data| data.summary)),
        supersample: 1.0,
    })
}

/// 逐通道标定参数。
fn calibration_for(raw: &RawImage, is_float: bool) -> ChannelCalibration {
    if is_float {
        // 浮点 DNG 给出的已经是归一化的线性值，黑电平为 0、满量程为 1，
        // 换算到 16 位整数量纲就是 0 / 65535。
        return ChannelCalibration {
            black: [0.0; 3],
            range: [FLOAT_FULL_SCALE; 3],
            gain: [1.0; 3],
        };
    }

    let black = [
        raw.blacklevels[0] as f32,
        raw.blacklevels[1] as f32,
        raw.blacklevels[2] as f32,
    ];
    let mut range = [1.0f32; 3];
    for (channel, slot) in range.iter_mut().enumerate() {
        let white = raw.whitelevels[channel] as f32;
        // 有的机型白电平为 0（元数据缺失），此时退化为「不缩放」而不是整张图变白。
        let span = white - black[channel];
        *slot = if span >= 1.0 { span } else { FLOAT_FULL_SCALE };
    }

    ChannelCalibration {
        black,
        range,
        gain: white_balance_gains(&raw.wb_coeffs),
    }
}

/// 白平衡系数 → 以绿色为基准的三通道增益。
///
/// `rawloader` 给出的是 dcraw 口径的 `pre_mul`（RGBE 顺序、最小值通常为 1），
/// 除以绿色分量即可得到「绿 = 1、红蓝通常大于 1」的增益。
fn white_balance_gains(coefficients: &[f32; 4]) -> [f32; 3] {
    let rgb = [coefficients[0], coefficients[1], coefficients[2]];

    // 元数据缺失或被写坏时，宁可不做白平衡，也不要按垃圾系数把画面染成一片绿。
    if rgb.iter().any(|value| !value.is_finite() || *value <= 0.0) {
        return [1.0; 3];
    }

    let reference = rgb[1];
    let mut gains = [1.0f32; 3];
    for (channel, slot) in gains.iter_mut().enumerate() {
        let gain = rgb[channel] / reference;
        *slot = if gain.is_finite() {
            gain.clamp(GAIN_RANGE.0, GAIN_RANGE.1)
        } else {
            1.0
        };
    }
    gains
}

/// 决定图像方向。
///
/// 优先用 RAW 容器自己写的方向；它没写（`Unknown`）时才退到 EXIF，
/// 最后再退到「无需纠正」。方向和像素是绑定的，猜错的代价是整张图躺倒，
/// 所以这里宁可保守也要求有依据。
fn resolve_orientation(raw: &RawImage, exif: Option<Orientation>) -> Orientation {
    match raw.orientation {
        RawOrientation::Unknown => exif.unwrap_or(Orientation::Normal),
        known => map_orientation(known),
    }
}

fn map_orientation(orientation: RawOrientation) -> Orientation {
    match orientation {
        RawOrientation::Normal => Orientation::Normal,
        RawOrientation::HorizontalFlip => Orientation::FlipHorizontal,
        RawOrientation::Rotate180 => Orientation::Rotate180,
        RawOrientation::VerticalFlip => Orientation::FlipVertical,
        RawOrientation::Transpose => Orientation::Transpose,
        RawOrientation::Rotate90 => Orientation::Rotate90,
        RawOrientation::Transverse => Orientation::Transverse,
        RawOrientation::Rotate270 => Orientation::Rotate270,
        // 上面已经单独处理过；这里再兜一次，保证函数是全函数、不会遗漏。
        RawOrientation::Unknown => Orientation::Normal,
    }
}

/// 把 RAW 自带的厂商/机型补进 EXIF 摘要。
///
/// RAW 容器的厂商字段往往比 EXIF 更规范（`clean_make` / `clean_model`），
/// 但曝光的读取仍然要依赖 EXIF，所以两者是**互补**而不是二选一。
fn build_summary(raw: &RawImage, exif: Option<ExifSummary>) -> Option<ExifSummary> {
    let mut summary = exif.unwrap_or_default();

    if summary.camera_make.is_none() {
        summary.camera_make = non_empty(&raw.clean_make).or_else(|| non_empty(&raw.make));
    }
    if summary.camera_model.is_none() {
        summary.camera_model = non_empty(&raw.clean_model).or_else(|| non_empty(&raw.model));
    }

    if summary.is_empty() {
        None
    } else {
        Some(summary)
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 把 rawloader 的错误映射成我们的错误类型。
///
/// `RawLoaderError` 只有一个字符串字段，没有可匹配的结构。这里按它的措辞区分
/// 「这台机型的原片我还读不了」与「文件本身坏了」—— 两者对用户意味着完全不同的下一步：
/// 前者要换工具转格式，后者要重新拷贝文件。
fn raw_failure(error: &rawloader::RawLoaderError) -> DecodeError {
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();

    if lower.contains("decoder") || lower.contains("unsupported") {
        DecodeError::unsupported(
            Some(ImageFormat::Raw),
            format!(
                "{message}。这台相机/这种原片格式尚未被支持，可先用相机自带软件或 Adobe DNG Converter 转成 DNG / JPEG。"
            ),
        )
    } else {
        DecodeError::corrupt(format!("相机原片解析失败：{message}"))
    }
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case(extension))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小的 `RawImage`，只关心标定相关的字段。
    fn sample_raw(blacklevels: [u16; 4], whitelevels: [u16; 4], wb_coeffs: [f32; 4]) -> RawImage {
        RawImage {
            make: "TestMake".to_string(),
            model: "TestModel".to_string(),
            clean_make: "TestMake".to_string(),
            clean_model: "TestModel".to_string(),
            width: 4,
            height: 4,
            cpp: 1,
            wb_coeffs,
            whitelevels,
            blacklevels,
            xyz_to_cam: [[0.0; 3]; 4],
            cfa: rawloader::CFA::new("RGGB"),
            crops: [0, 0, 0, 0],
            blackareas: Vec::new(),
            orientation: RawOrientation::Normal,
            data: RawImageData::Integer(vec![0; 16]),
        }
    }

    #[test]
    fn white_balance_is_normalised_to_green() {
        // 绿色为基准（1.0），红蓝各放大一倍。
        let gains = white_balance_gains(&[2.0, 1.0, 2.0, 0.0]);
        assert!((gains[1] - 1.0).abs() < 1e-6);
        assert!((gains[0] - 2.0).abs() < 1e-6);
        assert!((gains[2] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn broken_white_balance_falls_back_to_no_op() {
        // 全零、负数、非有限值都必须退化成「不做白平衡」，
        // 而不是产出一张被染色的图。
        for broken in [[0.0f32; 4], [-1.0, 1.0, 2.0, 0.0], [f32::NAN, 1.0, 2.0, 0.0]] {
            assert_eq!(white_balance_gains(&broken), [1.0; 3], "{broken:?}");
        }
    }

    #[test]
    fn absurd_gains_are_clamped_instead_of_tinting_the_image() {
        let gains = white_balance_gains(&[100.0, 1.0, 0.01, 0.0]);
        assert_eq!(gains[0], GAIN_RANGE.1);
        assert_eq!(gains[2], GAIN_RANGE.0);
    }

    #[test]
    fn black_level_is_subtracted_from_the_white_range() {
        let raw = sample_raw([1000, 2000, 3000, 0], [11_000, 12_000, 13_000, 0], [1.0; 4]);
        let calibration = calibration_for(&raw, false);
        assert_eq!(calibration.black, [1000.0, 2000.0, 3000.0]);
        assert_eq!(calibration.range, [10_000.0; 3]);
    }

    #[test]
    fn missing_white_level_does_not_blow_out_the_image() {
        // 白电平为 0（元数据缺失）时不能把有效范围算成 0，
        // 否则除法会退化，整张图要么全白要么全黑。
        let raw = sample_raw([0; 4], [0; 4], [1.0; 4]);
        let calibration = calibration_for(&raw, false);
        for channel in 0..3 {
            assert!(
                calibration.range[channel] >= 1.0,
                "第 {channel} 个通道的有效范围不应为 0：{:?}",
                calibration.range
            );
        }
    }

    #[test]
    fn float_dng_uses_normalised_levels() {
        // 浮点 DNG 的数值已经是归一化的线性值，黑/白电平由我们自己定死，
        // 不能去读那些对浮点数据没有意义的整数标签。
        let raw = sample_raw([1234; 4], [4321; 4], [0.5, 1.0, 2.0, 0.0]);
        let calibration = calibration_for(&raw, true);
        assert_eq!(calibration.black, [0.0; 3]);
        assert_eq!(calibration.range, [FLOAT_FULL_SCALE; 3]);
        assert_eq!(calibration.gain, [1.0; 3]);
    }

    #[test]
    fn cr3_gets_an_actionable_message() {
        let error = decode_raw(Path::new("IMG_0001.CR3"), &DecodeLimits::default())
            .expect_err("CR3 目前不应能解码");
        assert!(matches!(error, DecodeError::Unsupported { .. }));
        let message = error.user_message();
        assert!(message.contains("CR3"), "{message}");
        assert!(message.contains("DNG"), "提示应给出下一步：{message}");
    }

    #[test]
    fn probe_accepts_both_raw_and_plain_tiff_headers() {
        let decoder = RawDecoder;
        // CR2：能在内容层确定的原片。
        assert!(decoder.probe(b"II\x2a\x00\x10\x00\x00\x00CR\x02\x00"));
        // 普通 TIFF 头：NEF/ARW/DNG 都长这样，必须自荐，
        // 否则一个被改名的 DNG 就永远救不回来。
        assert!(decoder.probe(b"II\x2a\x00\x08\x00\x00\x00"));
        // 而 PNG 不该被抢走。
        assert!(!decoder.probe(b"\x89PNG\r\n\x1a\n"));
    }
}
