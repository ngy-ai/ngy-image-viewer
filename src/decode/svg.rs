//! SVG 矢量图解码（usvg 解析 + resvg 栅格化，全部纯 Rust）。
//!
//! # 矢量图为什么也要「解码成位图」
//!
//! 我们的渲染层最终画的是 GPU 纹理，纹理必须有确定的分辨率。
//! 真正的矢量渲染需要把「按当前缩放重新光栅化」接进渲染循环，这不属于本期范围。
//! 因此这里的策略是：**一次性光栅化到比声明尺寸更高的分辨率**，
//! 让后续的缩放由 GPU 采样承担 —— 放大到「适应窗口」时依然清晰，
//! 而代价只是一次有界的额外内存。
//!
//! 逻辑尺寸（状态栏显示、1:1、适应窗口所用的尺寸）仍然是 SVG 自己声明的尺寸，
//! 像素尺寸与逻辑尺寸的比值通过 [`ImageData::supersample`] 告知渲染层。
//!
//! # 字体
//!
//! SVG 里的 `<text>` 要真正渲染出字，必须有一份系统字体库。
//! 扫描系统字体是**一次性**开销（几百毫秒量级），所以这里用一个进程级缓存，
//! 只有真的遇到 SVG 时才会去扫，不会拖慢启动。

use std::path::Path;
use std::sync::{Arc, OnceLock};

use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{Options, Tree, fontdb};

use super::Decoder;
use super::types::{DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat, Orientation};

/// 光栅化的目标长边。
///
/// 取 1600 的理由：绝大多数 SVG（图标、徽标、示意图）声明尺寸都在 24–512 之间，
/// 1600 足以覆盖「在 4K 显示器上适应窗口」的场景，而单张位图最多约 10 MB。
const TARGET_LONG_EDGE: f32 = 1600.0;

/// 超采样倍率上限。倍率再往上收益急剧下降，内存却按平方增长。
const MAX_SUPERSAMPLE: f32 = 8.0;

/// 单个 SVG 文件的大小上限。
///
/// 矢量图是文本，正常体积在几 KB 到几 MB。64 MiB 已经极其宽松，
/// 存在的意义是避免一个损坏或被恶意构造的「超大 SVG」把内存吃光。
const MAX_SVG_BYTES: u64 = 64 * 1024 * 1024;

pub struct SvgDecoder;

impl Decoder for SvgDecoder {
    fn id(&self) -> &'static str {
        "resvg"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Svg]
    }

    fn probe(&self, head: &[u8]) -> bool {
        super::sniff::sniff_magic(head) == Some(ImageFormat::Svg)
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_svg(src, limits)
    }
}

/// 进程级共享的系统字体库。
///
/// 之所以共享而不是每次新建：`load_system_fonts()` 要遍历系统字体目录并解析字体元数据，
/// 连续打开多张 SVG 时重复扫描是纯浪费。`OnceLock` 保证只扫一次，且首次访问才触发。
fn shared_fontdb() -> Arc<fontdb::Database> {
    static FONTS: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
    FONTS
        .get_or_init(|| {
            let mut database = fontdb::Database::new();
            database.load_system_fonts();
            Arc::new(database)
        })
        .clone()
}

/// 光栅化方案：最终位图尺寸 + 使用的缩放倍率。
#[derive(Debug, Clone, Copy)]
struct RasterPlan {
    width: u32,
    height: u32,
    /// 像素尺寸 ÷ 逻辑尺寸。
    scale: f32,
}

fn decode_svg(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    let metadata = std::fs::metadata(src).map_err(|error| DecodeError::io(src, error))?;
    if metadata.len() == 0 {
        return Err(DecodeError::corrupt("文件为空（0 字节）"));
    }
    if metadata.len() > MAX_SVG_BYTES {
        return Err(DecodeError::corrupt(format!(
            "SVG 文件过大（{} MB），超过 {} MB 的解析上限",
            metadata.len() / (1024 * 1024),
            MAX_SVG_BYTES / (1024 * 1024)
        )));
    }

    let bytes = std::fs::read(src).map_err(|error| DecodeError::io(src, error))?;

    let mut options = Options::default();
    options.fontdb = shared_fontdb();
    // 让 SVG 里 `<image href="sibling.png">` 这样的相对引用能解析到文件旁边，
    // 否则内嵌位图会静默变成空白。
    options.resources_dir = src.parent().map(Path::to_path_buf);

    // SVGZ（gzip 压缩的 SVG）由 usvg 内部按 magic bytes 自动识别并解压，这里不用管。
    let tree = Tree::from_data(&bytes, &options)
        .map_err(|error| DecodeError::corrupt(format!("SVG 解析失败：{error}")))?;

    let size = tree.size();
    let logical_width = size.width();
    let logical_height = size.height();
    if !logical_width.is_finite() || !logical_height.is_finite() {
        return Err(DecodeError::corrupt("SVG 声明的尺寸不是有效数值"));
    }

    let plan = plan_raster(logical_width, logical_height, limits);

    let mut pixmap = Pixmap::new(plan.width, plan.height).ok_or_else(|| {
        DecodeError::corrupt(format!(
            "无法为 SVG 分配 {}×{} 的绘制画布",
            plan.width, plan.height
        ))
    })?;

    resvg::render(
        &tree,
        Transform::from_scale(plan.scale, plan.scale),
        &mut pixmap.as_mut(),
    );

    let rgba8 = unpremultiply(pixmap.data());

    Ok(ImageData {
        format: ImageFormat::Svg,
        frames: vec![Frame::new(plan.width, plan.height, rgba8, 0)],
        // 光栅化时就直接按声明尺寸正着画，不存在 EXIF 方向。
        orientation: Orientation::Normal,
        exif: None,
        // 像素 ÷ 这个倍率 = 逻辑尺寸，也就是 SVG 自己声明的尺寸。
        // 逻辑尺寸由它反推而不是直接存下来，是为了让「显示尺寸」与
        // 「实际光栅化的像素」永远自洽 —— 取整误差只会在 1 像素以内。
        supersample: plan.scale,
    })
}

/// 决定光栅化到什么尺寸。
///
/// 与位图解码器不同，这里**不会**因为「声明尺寸超过上限」就拒绝整张图，
/// 而是把声明的逻辑尺寸收敛到上限之内再继续。理由：位图超限意味着真的解不开，
/// 而矢量图不存在这个问题 —— 一张声明 100000×100000 的 SVG 完全可以画成
/// 8192×8192 的位图正常显示。
///
/// 收敛顺序：先把逻辑尺寸压进单边上限（这也让后续比例计算不会被天文数字或
/// 极端长宽比搅乱），再按「目标长边」求出期望倍率，最后用单边、总像素、
/// 人为上限三重约束把它压下来。倍率允许小于 1（声明尺寸太大时按比例缩小光栅化）。
fn plan_raster(logical_width: f32, logical_height: f32, limits: &DecodeLimits) -> RasterPlan {
    // `f32::min` 在遇到 NaN 时返回另一个操作数，顺带把「尺寸是 NaN」这种异常也兜住了。
    let logical_width = logical_width.min(limits.max_width as f32).max(1.0);
    let logical_height = logical_height.min(limits.max_height as f32).max(1.0);
    let long_edge = logical_width.max(logical_height);

    // 期望倍率：把长边推到目标值。下限 1 表示「默认不做无意义的缩小」，
    // 真正的缩小由下面的上限约束决定。
    let desired = (TARGET_LONG_EDGE / long_edge).clamp(1.0, MAX_SUPERSAMPLE);

    // 上限三重约束：单边不越界、总像素不越界、倍率不超人为上限。
    let by_edge = (limits.max_width as f64 / logical_width as f64)
        .min(limits.max_height as f64 / logical_height as f64);
    let by_pixels =
        (limits.max_pixels as f64 / (logical_width as f64 * logical_height as f64)).sqrt();
    let upper_bound = by_edge.min(by_pixels).min(MAX_SUPERSAMPLE as f64);

    // 下限：保证两个方向的光栅尺寸都至少是 1 像素。
    // 超出上限时不再强行兜底，而是让 scale 取到上限 —— 上面的逻辑尺寸收敛
    // 已经保证了 `lower_bound <= upper_bound`，这里只是把不变量写清楚。
    let lower_bound = (1.0 / logical_width as f64).max(1.0 / logical_height as f64);
    let scale = (desired as f64)
        .min(upper_bound)
        .max(lower_bound.min(upper_bound))
        .max(f64::MIN_POSITIVE) as f32;

    let width = (logical_width * scale).round().max(1.0) as u32;
    let height = (logical_height * scale).round().max(1.0) as u32;

    RasterPlan {
        width,
        height,
        scale,
    }
}

/// 预乘 RGBA → 非预乘 RGBA。
///
/// tiny-skia 的 `Pixmap` 存的是**预乘**像素（为的是合成时的正确性），
/// 而整条解码链的约定是**非预乘**（与 `image` crate 的 `into_rgba8` 口径一致）。
/// 这一层必须转回来，否则半透明 SVG 会整体偏暗。
fn unpremultiply(premultiplied: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; premultiplied.len()];

    for (source, target) in premultiplied
        .chunks_exact(4)
        .zip(out.chunks_exact_mut(4))
    {
        let alpha = source[3];
        match alpha {
            // 全透明像素的颜色本身没有意义，统一归零，
            // 免得留下「有颜色但完全看不见」的脏数据。
            0 => {}
            // 不透明像素无需换算，直接搬。
            255 => target.copy_from_slice(source),
            alpha => {
                let divisor = alpha as u32;
                // 四舍五入到最近的整数，避免连续两次量化带来的系统性偏暗。
                let restore = |channel: u8| -> u8 {
                    (((channel as u32 * 255) + divisor / 2) / divisor).min(255) as u8
                };
                target[0] = restore(source[0]);
                target[1] = restore(source[1]);
                target[2] = restore(source[2]);
                target[3] = alpha;
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_vector_is_upsampled_for_zoom_quality() {
        let plan = plan_raster(24.0, 24.0, &DecodeLimits::default());
        // 24×24 的图标会被放大到倍率上限。
        assert_eq!((plan.width, plan.height), (192, 192));
        assert!((plan.scale - 8.0).abs() < 1e-6);
    }

    #[test]
    fn large_vector_is_not_upscaled() {
        // 4000×3000 的矢量已经比目标长边大，保持原样，不做无意义的放大。
        let plan = plan_raster(4000.0, 3000.0, &DecodeLimits::default());
        assert_eq!((plan.width, plan.height), (4000, 3000));
        assert!((plan.scale - 1.0).abs() < 1e-6);
    }

    #[test]
    fn aspect_ratio_survives_rounding() {
        let plan = plan_raster(100.0, 33.0, &DecodeLimits::default());
        let ratio_in = 100.0 / 33.0;
        let ratio_out = plan.width as f32 / plan.height as f32;
        assert!(
            (ratio_in - ratio_out).abs() < 0.05,
            "长宽比被破坏了：{ratio_in} → {ratio_out}"
        );
    }

    #[test]
    fn oversized_declaration_is_converged_instead_of_rejected() {
        // 声明 1000000×1 这种离谱尺寸：收敛到单边上限，而不是拒绝整张图，
        // 也不会在窄边上产出 0 像素。
        let limits = DecodeLimits::default();
        let plan = plan_raster(1_000_000.0, 1.0, &limits);
        assert_eq!(plan.height, 1, "{plan:?}");
        assert_eq!(plan.width, limits.max_width, "{plan:?}");
        // 逻辑尺寸由「像素 ÷ 倍率」反推，必须是一个稳定、非零的值。
        let logical_width = plan.width as f32 / plan.scale;
        let logical_height = plan.height as f32 / plan.scale;
        assert!((logical_width - limits.max_width as f32).abs() < 1.0);
        assert!((logical_height - 1.0).abs() < 0.01);
    }

    #[test]
    fn pixel_budget_downscales_without_losing_the_aspect_ratio() {
        let limits = DecodeLimits::default();
        // 20000×20000 = 4 亿像素，超出 1.2 亿的预算，必须缩到预算之内。
        let plan = plan_raster(20_000.0, 20_000.0, &limits);
        let pixels = plan.width as u64 * plan.height as u64;
        assert!(pixels <= limits.max_pixels, "{plan:?} → {pixels} 像素");
        assert_eq!(plan.width, plan.height);
        // 缩的是光栅分辨率，不是显示尺寸：逻辑尺寸仍然是声明的 20000×20000。
        let logical = plan.width as f32 / plan.scale;
        assert!((logical - 20_000.0).abs() < 2.0, "{logical}");
    }

    #[test]
    fn limits_are_respected() {
        let limits = DecodeLimits {
            max_width: 512,
            max_height: 512,
            max_pixels: 512 * 512,
            ..DecodeLimits::default()
        };
        let plan = plan_raster(24.0, 24.0, &limits);
        assert!(plan.width <= limits.max_width, "{plan:?}");
        assert!(plan.height <= limits.max_height, "{plan:?}");
        assert!(plan.width as u64 * plan.height as u64 <= limits.max_pixels);
    }

    #[test]
    fn unpremultiply_restores_straight_alpha() {
        // 半透明纯红：预乘后是 (128,0,0,128)，还原应回到 (255,0,0,128)。
        let premultiplied = vec![128, 0, 0, 128];
        assert_eq!(unpremultiply(&premultiplied), vec![255, 0, 0, 128]);

        // 全透明像素颜色归零。
        assert_eq!(unpremultiply(&[10, 20, 30, 0]), vec![0, 0, 0, 0]);

        // 不透明像素原样保留。
        assert_eq!(unpremultiply(&[10, 20, 30, 255]), vec![10, 20, 30, 255]);
    }
}
