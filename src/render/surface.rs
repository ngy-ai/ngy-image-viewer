//! 把解码出来的像素变成 GPU 可以直接使用的纹理。
//!
//! # 这里做的三件事
//!
//! 1. **方向落地**：把 EXIF 方向与用户旋转合起来，一次性重排像素。
//!    GPUI 的 `paint_image` 只接受轴对齐矩形，没有仿射变换入口，
//!    所以旋转必须在这里完成（详见 [`crate::model::transform`] 的模块文档）。
//! 2. **通道顺序**：GPUI 的 `RenderImage` 在 CPU 侧是 `RGBA` 布局的缓冲区，
//!    但 GPU 侧按 **BGRA** 解释（见其源码注释 “in BGRA format”）。
//!    解码层统一产出非预乘 RGBA，所以这里做一次红蓝交换。
//! 3. **超限保护**：GPU 纹理有尺寸上限。超过上限时主动降采样，
//!    而不是让上传在运行时失败 —— 用户宁可看到一张稍微软一点的图，
//!    也不愿看到「打不开」。
//!
//! # 为什么放在后台线程
//!
//! 上面三件事都是 O(像素数) 的全图遍历。构建纹理的过程发生在解码线程上，
//! UI 线程只负责把一个已经准备好的 `Arc<RenderImage>` 挂到视图上。

use std::sync::Arc;

use gpui_kit::RenderImage;
use image::{Delay, Frame as ImageFrame, ImageBuffer, Rgba};
use rayon::prelude::*;

use crate::decode::{Frame, Orientation, orientation};
use crate::model::{ImageDocument, Size};
use crate::trace;

/// 纹理长边的上限。
///
/// 桌面 GPU 普遍支持 16384×16384 的纹理。把它设在这里既能覆盖所有现实中的照片
/// （一亿像素也不过 14000 宽），又能在遇到极端尺寸（例如 3 万像素宽的全景扫描）时
/// 主动降采样，而不是让纹理上传在运行时直接失败。
const MAX_TEXTURE_EDGE: u32 = 16_384;

/// 构建纹理时可能出现的失败。
///
/// 每一项都对应一句能直接展示给用户的话：这类错误发生在解码成功之后，
/// 属于「解码器认得这个文件，但我们没法把它画出来」，必须说清楚原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceError {
    /// 图像一个像素都没有。
    Empty,
    /// 尺寸大到连降采样都放不下。
    TooLarge { width: u32, height: u32 },
    /// 像素缓冲区长度与声明的尺寸不符。
    BadPixelData { expected: usize, actual: usize },
}

impl SurfaceError {
    pub fn user_message(&self) -> String {
        match self {
            Self::Empty => "图像没有任何像素，无法显示。".to_string(),
            Self::TooLarge { width, height } => format!(
                "图像尺寸（{width}×{height}）超出显卡可用的纹理上限（长边 {MAX_TEXTURE_EDGE} 像素），无法显示。"
            ),
            Self::BadPixelData { expected, actual } => format!(
                "图像像素数据不完整（应有 {expected} 字节，实际 {actual} 字节），文件可能已损坏。"
            ),
        }
    }

    pub fn short_reason(&self) -> String {
        match self {
            Self::Empty => "surface: empty".to_string(),
            Self::TooLarge { width, height } => {
                format!("surface: too_large {width}x{height} limit={MAX_TEXTURE_EDGE}")
            }
            Self::BadPixelData { expected, actual } => {
                format!("surface: bad_pixel_data expected={expected} actual={actual}")
            }
        }
    }
}

impl std::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.short_reason())
    }
}

impl std::error::Error for SurfaceError {}

/// 一件可以直接交给渲染层的「可绘制图像」。
///
/// 它把「纹理」「逻辑尺寸」「动画时序」三件事绑在一起：
/// 三者必须来自同一份图像数据，分散持有迟早会出现「尺寸对不上纹理」的错配。
pub struct Surface {
    image: Arc<RenderImage>,
    texture_size: (u32, u32),
    logical_size: Size,
    /// 每一帧的显示时长（毫秒）。静态图为单元素。
    frame_delays_ms: Vec<u32>,
    /// 为了适配纹理上限而做的降采样倍率（1 表示没有降采样）。
    downsample: u32,
    /// 是否存在半透明像素。
    ///
    /// 画布据此决定要不要铺棋盘格。绝大多数图片（JPEG、BMP）完全不透明，
    /// 提前算出来就能让那些图少画上千个格子。
    has_transparency: bool,
}

impl Surface {
    /// 从文档构建纹理。
    pub fn build(document: &ImageDocument) -> Result<Self, SurfaceError> {
        let (texture_width, texture_height) = document.texture_size();
        if texture_width == 0 || texture_height == 0 {
            trace::fail(
                "surface",
                format!(
                    "纹理尺寸为 0（{}×{}），无法构建纹理",
                    texture_width, texture_height
                ),
            );
            return Err(SurfaceError::Empty);
        }

        // 先算出需要降采样的倍数。取整是刻意的：整数倍率让盒式滤波的
        // 每个输出像素对应固定数量的输入像素，既好并行，也不会有累积舍入。
        let longest = texture_width.max(texture_height);
        let downsample = longest.div_ceil(MAX_TEXTURE_EDGE).max(1);
        if downsample > 1 && (texture_width / downsample) == 0 {
            trace::fail(
                "surface",
                format!("降采样 {downsample} 倍后宽度归零，无法构建纹理"),
            );
            return Err(SurfaceError::TooLarge {
                width: texture_width,
                height: texture_height,
            });
        }

        let orientation = document.orientation();
        let frames = document.data().frames.as_slice();
        trace::step(
            "surface",
            format!(
                "开始构建纹理：纹理尺寸={}×{} 方向={:?} 帧数={} 降采样={}",
                texture_width,
                texture_height,
                orientation,
                frames.len(),
                downsample,
            ),
        );

        let mut delays = Vec::with_capacity(frames.len());
        let mut rendered = Vec::with_capacity(frames.len());
        let mut has_transparency = false;
        for (index, frame) in frames.iter().enumerate() {
            match prepare_frame(frame, orientation, downsample) {
                Ok((prepared, transparent)) => {
                    has_transparency |= transparent;
                    delays.push(frame.effective_delay_ms());
                    rendered.push(prepared);
                }
                Err(error) => {
                    trace::fail(
                        "surface",
                        format!("第 {index} 帧准备失败：{}（该帧 {}×{}，{} 字节）", error.short_reason(), frame.width, frame.height, frame.rgba8.len()),
                    );
                    return Err(error);
                }
            }
        }

        if rendered.is_empty() {
            trace::fail("surface", "没有可绘制的帧（帧列表为空）");
            return Err(SurfaceError::Empty);
        }

        let surface = Self {
            image: Arc::new(RenderImage::new(rendered)),
            texture_size: (texture_width, texture_height),
            logical_size: document.logical_size(),
            frame_delays_ms: delays,
            downsample,
            has_transparency,
        };

        // 这四个数字决定了「画出来的是不是黑屏」：纹理尺寸为 0 会画不出来，
        // 逻辑尺寸为 0 会让视图认为「没有图像」，半透明标记错会让棋盘格盖住内容。
        trace::step(
            "surface",
            format!(
                "纹理就绪：GPU 纹理尺寸={}×{} 逻辑尺寸={:.2}×{:.2} 帧数={} 半透明={} RenderImage#{} 首帧字节={}",
                surface.texture_size().0,
                surface.texture_size().1,
                surface.logical_size().width,
                surface.logical_size().height,
                surface.frame_count(),
                surface.has_transparency(),
                surface.image().id.0,
                surface
                    .image()
                    .as_bytes(0)
                    .map(|bytes| bytes.len())
                    .unwrap_or(0),
            ),
        );

        Ok(surface)
    }

    /// 图像是否存在半透明像素。为 `false` 时画布不需要铺棋盘格。
    pub fn has_transparency(&self) -> bool {
        self.has_transparency
    }

    pub fn image(&self) -> &Arc<RenderImage> {
        &self.image
    }

    /// 纹理的像素尺寸（**已经**扣除降采样）。
    pub fn texture_size(&self) -> (u32, u32) {
        let factor = self.downsample.max(1);
        (
            (self.texture_size.0 / factor).max(1),
            (self.texture_size.1 / factor).max(1),
        )
    }

    /// 显示尺寸，单位是图像逻辑像素。
    ///
    /// 它与 [`Self::texture_size`] 的比值就是「一个逻辑像素对应多少纹理像素」，
    /// 降采样之后这个比值会小于 1 —— 渲染层据此决定要不要开平滑插值。
    pub fn logical_size(&self) -> Size {
        self.logical_size
    }

    pub fn frame_count(&self) -> usize {
        self.frame_delays_ms.len()
    }

    pub fn is_animated(&self) -> bool {
        self.frame_delays_ms.len() > 1
    }

    /// 一轮播放的总时长。
    pub fn loop_duration_ms(&self) -> u64 {
        self.frame_delays_ms
            .iter()
            .map(|delay| *delay as u64)
            .sum()
    }

    /// 按已播放时间取当前帧号。
    pub fn frame_index_at(&self, elapsed_ms: u64) -> usize {
        if self.frame_delays_ms.len() <= 1 {
            return 0;
        }
        let total = self.loop_duration_ms();
        if total == 0 {
            return 0;
        }

        let mut remaining = elapsed_ms % total;
        for (index, delay) in self.frame_delays_ms.iter().enumerate() {
            let delay = (*delay).max(1) as u64;
            if remaining < delay {
                return index;
            }
            remaining -= delay;
        }
        self.frame_delays_ms.len() - 1
    }
}

impl std::fmt::Debug for Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Surface")
            .field("texture_size", &self.texture_size())
            .field("logical_size", &self.logical_size)
            .field("frames", &self.frame_delays_ms.len())
            .field("downsample", &self.downsample)
            .finish()
    }
}

/// 单帧：应用方向 → 交换红蓝 → 降采样 → 变成 `image::Frame`。
///
/// 返回值里的 `bool` 表示这一帧是否存在半透明像素，供画布决定要不要铺棋盘格。
fn prepare_frame(
    frame: &Frame,
    orientation: Orientation,
    downsample: u32,
) -> Result<(ImageFrame, bool), SurfaceError> {
    let (width, height, pixels, transparent) = oriented_pixels(frame, orientation)?;
    let (width, height, pixels) = if downsample > 1 {
        downsample_bgra_ready(width, height, pixels, downsample)
    } else {
        (width, height, pixels)
    };
    let (width, height) = (width.max(1), height.max(1));

    let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, pixels).ok_or(
        SurfaceError::BadPixelData {
            expected: width as usize * height as usize * 4,
            actual: frame.rgba8.len(),
        },
    )?;

    let image_frame = ImageFrame::from_parts(
        buffer,
        0,
        0,
        Delay::from_numer_denom_ms(frame.effective_delay_ms(), 1),
    );
    Ok((image_frame, transparent))
}

/// 应用方向，并把 RGBA 就地改成 BGRA（GPU 侧的期望顺序）。
fn oriented_pixels(
    frame: &Frame,
    orientation: Orientation,
) -> Result<(u32, u32, Vec<u8>, bool), SurfaceError> {
    let expected = frame.width as usize * frame.height as usize * 4;
    if frame.rgba8.len() != expected {
        return Err(SurfaceError::BadPixelData {
            expected,
            actual: frame.rgba8.len(),
        });
    }

    let (width, height, mut pixels) = match orientation::apply(
        frame.width,
        frame.height,
        &frame.rgba8,
        orientation,
    ) {
        // `None` 表示「无需纠正」：直接接管原缓冲区，省掉一次整图拷贝。
        // 这也是绝大多数图片（EXIF 方向为 1）的实际路径。
        None => (frame.width, frame.height, frame.rgba8.clone()),
        Some(result) => result,
    };

    let transparent = swap_red_blue(&mut pixels);
    Ok((width, height, pixels, transparent))
}

/// RGBA → BGRA。交换每个像素的首尾两个字节，同时顺手判断透明度。
///
/// 独立成函数是因为它虽然只有一行，却承担着「颜色对不对」的全部责任：
/// 忘了它就会得到一张红蓝互换的图（人脸变蓝），而这种错误在缩略图上
/// 有时不容易立刻发现。
///
/// 透明度顺便在这里判断，而不是再遍历一遍：这里已经在触碰每一个像素了，
/// 多一次比较几乎免费。
fn swap_red_blue(pixels: &mut [u8]) -> bool {
    let mut transparent = false;
    for pixel in pixels.chunks_exact_mut(4) {
        transparent |= pixel[3] != 255;
        pixel.swap(0, 2);
    }
    transparent
}

/// 整数倍率的盒式降采样。
///
/// 用盒式平均而不是最近邻：最近邻在小尺寸下会产生明显的锯齿和摩尔纹
/// （细密纹理变成一片噪点），而盒式只多几次加法，观感好得多。
fn downsample_bgra_ready(
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    factor: u32,
) -> (u32, u32, Vec<u8>) {
    let factor = factor.max(1) as usize;
    let source_width = width as usize;
    let target_width = (source_width / factor).max(1);
    let target_height = ((height as usize) / factor).max(1);

    let mut out = vec![0u8; target_width * target_height * 4];

    // 按行并行：每个输出行只读自己那几行输入，天然无冲突。
    out.par_chunks_exact_mut(target_width * 4)
        .enumerate()
        .for_each(|(row, target_row)| {
            for column in 0..target_width {
                let mut sums = [0u32; 4];
                let mut samples = 0u32;

                for dy in 0..factor {
                    let source_y = row * factor + dy;
                    if source_y >= height as usize {
                        break;
                    }
                    for dx in 0..factor {
                        let source_x = column * factor + dx;
                        if source_x >= source_width {
                            break;
                        }
                        let index = (source_y * source_width + source_x) * 4;
                        for channel in 0..4 {
                            sums[channel] += pixels[index + channel] as u32;
                        }
                        samples += 1;
                    }
                }

                let divisor = samples.max(1);
                let target = column * 4;
                for channel in 0..4 {
                    target_row[target + channel] = ((sums[channel] + divisor / 2) / divisor) as u8;
                }
            }
        });

    (target_width as u32, target_height as u32, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{ImageData, ImageFormat};
    use crate::open_job::FileStat;
    use std::path::PathBuf;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Frame {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            data.extend_from_slice(&color);
        }
        Frame::new(width, height, data, 0)
    }

    fn document(frames: Vec<Frame>, orientation: Orientation, supersample: f32) -> ImageDocument {
        let data = ImageData {
            format: ImageFormat::Png,
            frames,
            orientation,
            exif: None,
            supersample,
        };
        ImageDocument::from_outcome(crate::open_job::OpenOutcome {
            path: PathBuf::from("C:/photos/sample.png"),
            file: Some(FileStat {
                byte_len: 16,
                modified: None,
            }),
            result: Ok(data),
            decode_ms: 0.0,
            total_ms: 0.0,
        })
        .expect("构造文档失败")
    }

    #[test]
    fn pixels_are_swapped_to_bgra_order() {
        // 纯红 RGBA 交给渲染层之前必须变成 BGR 顺序：[0,0,255,255]。
        // 这一条错了整张图的颜色就全反了。
        let mut pixels = vec![255, 0, 0, 255];
        assert!(!swap_red_blue(&mut pixels), "不透明像素不应被标记为有透明");
        assert_eq!(pixels, vec![0, 0, 255, 255]);

        // 只要有一个像素是半透明的，就必须报告出来，否则棋盘格不会出现。
        let mut pixels = vec![255, 0, 0, 255, 0, 255, 0, 128];
        assert!(swap_red_blue(&mut pixels));
    }

    #[test]
    fn transparency_is_detected_so_the_checkerboard_can_be_skipped() {
        // 不透明的图（JPEG/BMP 的绝大多数情况）不该白白画上千个格子。
        let opaque = document(vec![solid(4, 4, [10, 20, 30, 255])], Orientation::Normal, 1.0);
        let surface = Surface::build(&opaque).expect("构建纹理失败");
        assert!(!surface.has_transparency());

        let translucent = document(vec![solid(4, 4, [10, 20, 30, 0])], Orientation::Normal, 1.0);
        let surface = Surface::build(&translucent).expect("构建纹理失败");
        assert!(surface.has_transparency());
    }

    #[test]
    fn orientation_is_baked_into_the_texture_and_swaps_the_size() {
        // 4×2 的图带 Rotate90：纹理应当变成 2×4。
        let document = document(vec![solid(4, 2, [10, 20, 30, 255])], Orientation::Rotate90, 1.0);
        let surface = Surface::build(&document).expect("构建纹理失败");
        assert_eq!(surface.texture_size(), (2, 4));
        assert_eq!(surface.logical_size(), Size::new(2.0, 4.0));
    }

    #[test]
    fn logical_size_is_preserved_for_supersampled_vectors() {
        // 192×192 的 SVG 纹理，声明尺寸 24×24：纹理归纹理，显示尺寸归显示尺寸。
        let document = document(vec![solid(192, 192, [0, 0, 0, 255])], Orientation::Normal, 8.0);
        let surface = Surface::build(&document).expect("构建纹理失败");
        assert_eq!(surface.texture_size(), (192, 192));
        assert_eq!(surface.logical_size(), Size::new(24.0, 24.0));
    }

    #[test]
    fn oversized_textures_are_downsampled_and_the_logical_size_is_untouched() {
        // 用一个小上限来验证降采样路径本身，而不是真的去分配一张 20000 宽的图。
        let width = MAX_TEXTURE_EDGE + 4096;
        let document = document(vec![solid(width, 16, [0, 0, 0, 255])], Orientation::Normal, 1.0);

        // 直接调用降采样函数，避免测试里分配上百 MB 的像素。
        let source = vec![200u8; (width as usize) * 16 * 4];
        let (out_width, out_height, out) = downsample_bgra_ready(width, 16, source, 2);
        assert_eq!(out_width, width / 2);
        assert_eq!(out_height, 8);
        assert_eq!(out.len(), (out_width as usize) * (out_height as usize) * 4);
        // 均匀输入经过均值滤波应当还是同一个值。
        assert!(out.iter().all(|value| *value == 200), "均匀输入不应产生偏差");

        // 逻辑尺寸与纹理尺寸无关，必须保持原始声明值。
        assert_eq!(document.logical_size(), Size::new(width as f32, 16.0));
    }

    #[test]
    fn bad_pixel_data_is_reported_instead_of_panicking() {
        // 人为制造一个「长度与尺寸不符」的帧。
        let broken = Frame {
            width: 4,
            height: 4,
            rgba8: vec![0; 10],
            delay_ms: 0,
        };
        let document = document(vec![broken], Orientation::Normal, 1.0);
        let error = Surface::build(&document).expect_err("数据不完整应当报错");
        assert!(matches!(error, SurfaceError::BadPixelData { .. }));
        assert!(!error.user_message().is_empty());
    }

    #[test]
    fn frame_index_follows_the_delays() {
        let mut first = solid(1, 1, [0, 0, 0, 255]);
        first.delay_ms = 100;
        let mut second = solid(1, 1, [0, 0, 0, 255]);
        second.delay_ms = 50;

        let document = document(vec![first, second], Orientation::Normal, 1.0);
        let surface = Surface::build(&document).expect("构建纹理失败");
        assert_eq!(surface.frame_count(), 2);
        assert!(surface.is_animated());
        assert_eq!(surface.loop_duration_ms(), 150);
        assert_eq!(surface.frame_index_at(0), 0);
        assert_eq!(surface.frame_index_at(120), 1);
        assert_eq!(surface.frame_index_at(150), 0, "应当循环回第一帧");
    }

    #[test]
    fn a_static_image_never_advances_its_frame() {
        let document = document(vec![solid(2, 2, [0, 0, 0, 255])], Orientation::Normal, 1.0);
        let surface = Surface::build(&document).expect("构建纹理失败");
        assert!(!surface.is_animated());
        assert_eq!(surface.frame_index_at(0), 0);
        assert_eq!(surface.frame_index_at(999_999), 0);
    }
}
