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
//!
//! # 两种「多个内容」：动图的帧 与 多页文档的页
//!
//! 它们共用同一套像素管线，但**组织方式必须不同**：
//!
//! - **动图**：全部帧进**同一张**多帧纹理，绘制时按帧号取。帧是同一幅画的不同时刻，
//!   尺寸一致，也总要一起用。
//! - **多页文档**：**每页一张**单帧纹理，随页面解码逐张追加。
//!
//! 分页为什么不能也塞进一张多帧纹理：`RenderImage` 构造之后不可变，而它的 `id`
//! 正是 GPU 纹理上传的缓存键（见 gpui 的 `RenderImageParams`）。每次补页都重建一张
//! 含全部页的纹理，会让**先前每一页都被重新上传** —— 翻到第 N 页就是 O(N²) 的上传量，
//! 而补页本身看起来只该多做一页的功。还有一个更硬的理由：几十页扫描件塞进一张纹理，
//! 光显存就要按 GB 算；分成一张张之后，没翻到的页根本不占显存。

use std::sync::Arc;

use gpui_kit::RenderImage;
use image::{Delay, Frame as ImageFrame, ImageBuffer, Rgba};
use rayon::prelude::*;

use crate::decode::{Frame, Orientation, orientation};
use crate::model::ImageDocument;
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
    /// 往一张非多页纹理上追加页 —— 内部调用点用错了，不会走到用户面前。
    NotPaged,
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
            Self::NotPaged => "内部错误：这份文档不是多页文档。".to_string(),
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
            Self::NotPaged => "surface: not_paged".to_string(),
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
/// 注意**逻辑尺寸不在这里** —— 它由 [`ImageDocument`] 算（当前页的尺寸除以超采样倍率），
/// 而「当前是第几页」只有文档知道。这里留着它就会有两个「当前页」，
/// 迟早出现「状态栏写着第 3 页、画布按第 1 页的尺寸摆」。
///
/// # 为什么可以 `Clone`
///
/// 补一页要拿到 `&mut self`，而视图持有的是 `Arc<Surface>` —— 上一帧的绘制闭包
/// 可能还引用着同一个 `Arc`，于是 `Arc::get_mut` 会失败。有了 `Clone` 就能用
/// `Arc::make_mut`：它只在真的被共享时才克隆，而克隆的是几张纹理的 `Arc`
/// 与一小段元数据，**像素一个字节都不拷**。
#[derive(Clone)]
pub struct Surface {
    textures: Textures,
    /// 每一帧的显示时长（毫秒）。只有动画用得到；静态图与多页文档是空的。
    frame_delays_ms: Vec<u32>,
    /// 是否存在半透明像素。
    ///
    /// 画布据此决定要不要铺棋盘格。绝大多数图片（JPEG、BMP）完全不透明，
    /// 提前算出来就能让那些图少画上千个格子。
    ///
    /// 多页文档里它是**所有已载入页**的并集：后来追加的一页若有透明像素，
    /// 这里会翻转成真，此前不透明的那几页跟着多画一层格子 ——
    /// 代价是几毫秒的绘制，换来的是不必在每次翻页时重算一次。
    has_transparency: bool,
}

/// 纹理的两种组织方式。为什么不能统一，见模块文档。
#[derive(Clone)]
enum Textures {
    /// 动图与静态图：一张纹理内含全部帧，按帧号取。
    Frames(Content),
    /// 多页文档：每页一张单帧纹理，随解码逐张追加。
    Pages(Vec<Content>),
}

/// 一张纹理，以及它承载内容的几何信息。
#[derive(Clone)]
struct Content {
    image: Arc<RenderImage>,
    /// 纹理的像素尺寸（已应用方向、已扣除降采样）。
    texture_size: (u32, u32),
}

/// 一次绘制要用的纹理。
///
/// 把「哪张纹理」与「纹理内的第几帧」一起交出去，是因为这两者必须一致：
/// 分开取的话很容易出现「拿第 1 张纹理的第 3 帧」这种越界，而 `paint_image`
/// 内部会直接索引帧数组 —— 那是 panic，不是黑屏。
pub struct TextureRef {
    pub image: Arc<RenderImage>,
    /// 要画的帧在这张纹理内部的序号：动图是帧号，多页文档恒为 0。
    pub frame: usize,
    /// 纹理像素尺寸（已应用方向、已扣除降采样）。
    pub size: (u32, u32),
}

impl Surface {
    /// 从文档构建纹理。
    ///
    /// 多页文档只构建**已经解码的页** —— 打开时通常只有第 1 页，其余由
    /// [`Self::append_page`] 逐页补上。
    pub fn build(document: &ImageDocument) -> Result<Self, SurfaceError> {
        let orientation = document.orientation();
        let frames = document.data().frames.as_slice();
        let paged = document.is_paged();

        if frames.is_empty() {
            trace::fail("surface", "没有可绘制的帧（帧列表为空）");
            return Err(SurfaceError::Empty);
        }

        trace::step(
            "surface",
            format!(
                "开始构建纹理：{} 方向={:?} 已解内容数={} 多页文档={}",
                document.path().display(),
                orientation,
                frames.len(),
                paged,
            ),
        );

        let (textures, frame_delays_ms, has_transparency) = if paged {
            let mut pages: Vec<Content> = Vec::with_capacity(frames.len());
            let mut has_transparency = false;
            for (index, frame) in frames.iter().enumerate() {
                let prepared = prepare_single(frame, orientation).map_err(|error| {
                    trace::fail(
                        "surface",
                        format!(
                            "第 {} 页准备失败：{}（该页 {}×{}，{} 字节）",
                            index + 1,
                            error.short_reason(),
                            frame.width,
                            frame.height,
                            frame.rgba8.len(),
                        ),
                    );
                    error
                })?;
                has_transparency |= prepared.has_transparency;
                pages.push(prepared.content);
            }
            (Textures::Pages(pages), Vec::new(), has_transparency)
        } else {
            // 动图 / 静态图：全部帧进同一张纹理。降采样倍率必须对**所有帧**一致 ——
            // 一张纹理里混进不同尺寸的帧时 `RenderImage` 不会报错，
            // 要到绘制那一刻才炸。
            let (texture_width, texture_height) = document.texture_size();
            if texture_width == 0 || texture_height == 0 {
                trace::fail(
                    "surface",
                    format!("纹理尺寸为 0（{texture_width}×{texture_height}），无法构建纹理"),
                );
                return Err(SurfaceError::Empty);
            }
            let downsample = downsample_for(texture_width, texture_height);

            let mut delays = Vec::with_capacity(frames.len());
            let mut rendered = Vec::with_capacity(frames.len());
            let mut has_transparency = false;
            let mut size = (0, 0);
            for (index, frame) in frames.iter().enumerate() {
                match prepare_frame(frame, orientation, downsample) {
                    Ok((image_frame, frame_size, transparent)) => {
                        has_transparency |= transparent;
                        if index == 0 {
                            size = frame_size;
                        }
                        delays.push(frame.effective_delay_ms());
                        rendered.push(image_frame);
                    }
                    Err(error) => {
                        trace::fail(
                            "surface",
                            format!(
                                "第 {index} 帧准备失败：{}（该帧 {}×{}，{} 字节）",
                                error.short_reason(),
                                frame.width,
                                frame.height,
                                frame.rgba8.len(),
                            ),
                        );
                        return Err(error);
                    }
                }
            }
            if rendered.is_empty() {
                trace::fail("surface", "没有可绘制的帧（帧列表为空）");
                return Err(SurfaceError::Empty);
            }

            let content = Content {
                image: Arc::new(RenderImage::new(rendered)),
                texture_size: size,
            };
            (Textures::Frames(content), delays, has_transparency)
        };

        let surface = Self {
            textures,
            frame_delays_ms,
            has_transparency,
        };

        // 这几个数字决定了「画出来的是不是黑屏」：纹理尺寸为 0 会画不出来，
        // 逻辑尺寸为 0 会让视图认为「没有图像」，内容数为 0 会让绘制无处下手。
        let first = surface.texture(0).map(|texture| texture.size).unwrap_or((0, 0));
        trace::step(
            "surface",
            format!(
                "纹理就绪：首个纹理={}×{} 可绘制内容数={} 多页文档={} 逻辑尺寸={:.2}×{:.2} 半透明={}",
                first.0,
                first.1,
                surface.frame_count(),
                surface.is_paged(),
                document.logical_size().width,
                document.logical_size().height,
                surface.has_transparency(),
            ),
        );

        Ok(surface)
    }

    /// 收下**新解码的一页**，返回它的序号。
    ///
    /// 只对多页文档有意义。这一步是 O(这一页的像素)：已经画过的页不会被重新准备，
    /// 也不会被重新上传 —— 上一页的纹理原封不动。
    pub fn append_page(
        &mut self,
        frame: &Frame,
        orientation: Orientation,
    ) -> Result<usize, SurfaceError> {
        let Textures::Pages(pages) = &mut self.textures else {
            trace::fail("surface", "往非多页纹理上追加页：调用点用错了");
            return Err(SurfaceError::NotPaged);
        };

        let prepared = prepare_single(frame, orientation)?;
        self.has_transparency |= prepared.has_transparency;
        let texture_size = prepared.content.texture_size;
        pages.push(prepared.content);
        let index = pages.len() - 1;

        trace::step(
            "surface",
            format!(
                "第 {} 页纹理就绪：{}×{} 已就绪页数={}",
                index + 1,
                texture_size.0,
                texture_size.1,
                pages.len(),
            ),
        );
        Ok(index)
    }

    /// 图像是否存在半透明像素。为 `false` 时画布不需要铺棋盘格。
    pub fn has_transparency(&self) -> bool {
        self.has_transparency
    }

    /// 可绘制内容的个数：动图是帧数，多页文档是**已解码**的页数。
    pub fn frame_count(&self) -> usize {
        match &self.textures {
            Textures::Frames(_) => self.frame_delays_ms.len(),
            Textures::Pages(pages) => pages.len(),
        }
    }

    /// 是不是「按时间自动播放的动图」。
    ///
    /// 多页文档即使已经补了十几页也**不是**动画：页由用户翻，不该自己动。
    /// 界面靠这一位决定要不要持续请求重绘帧。
    pub fn is_animated(&self) -> bool {
        matches!(self.textures, Textures::Frames(_)) && self.frame_delays_ms.len() > 1
    }

    /// 是不是「多页文档」。判据取自纹理自身的组织方式，而不是回头去问文档 ——
    /// 两者一旦不一致，界面会按「页」的逻辑去画一张按「帧」组织的纹理。
    pub fn is_paged(&self) -> bool {
        matches!(self.textures, Textures::Pages(_))
    }

    /// 第 `index` 个内容的纹理。未解码返回 `None`。
    pub fn texture(&self, index: usize) -> Option<TextureRef> {
        match &self.textures {
            Textures::Frames(content) => {
                // 帧号必须同时落在时长表内：`RenderImage` 只看自己的帧数组，
                // 时长表若比它长，这里就会给出一个「对得上时长、对不上纹理」的帧号。
                if index >= self.frame_delays_ms.len() {
                    return None;
                }
                Some(TextureRef {
                    image: content.image.clone(),
                    frame: index,
                    size: content.texture_size,
                })
            }
            Textures::Pages(pages) => {
                let content = pages.get(index)?;
                Some(TextureRef {
                    image: content.image.clone(),
                    frame: 0,
                    size: content.texture_size,
                })
            }
        }
    }

    /// 一轮播放的总时长。
    pub fn loop_duration_ms(&self) -> u64 {
        self.frame_delays_ms
            .iter()
            .map(|delay| *delay as u64)
            .sum()
    }

    /// 按已播放时间取当前帧号。
    ///
    /// 只对动图有意义；多页文档里时长表是空的，恒返回 0（页序号由用户决定）。
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
            .field("contents", &self.frame_count())
            .field("paged", &self.is_paged())
            .field("animated", &self.is_animated())
            .finish()
    }
}

/// 长边超限时需要的整数降采样倍率，恒 ≥ 1。
///
/// 取整是刻意的：整数倍率让盒式滤波的每个输出像素对应固定数量的输入像素，
/// 既好并行，也不会有累积舍入。
///
/// 每个内容各算一次（而不是整份文档共用一个）：多页 TIFF 的各页尺寸可以不同，
/// 用第一页的倍率去处理后面更大的页，会让纹理超过 GPU 上限 ——
/// 而那种情况在 `paint_image` 那里只会安静地什么都画不出来。
fn downsample_for(width: u32, height: u32) -> u32 {
    width.max(height).div_ceil(MAX_TEXTURE_EDGE).max(1)
}

/// 一帧（或一页）准备好的结果。
struct Prepared {
    content: Content,
    has_transparency: bool,
}

/// 单帧路径：一帧 → 一张单帧纹理。多页文档的每一页都走它。
fn prepare_single(frame: &Frame, orientation: Orientation) -> Result<Prepared, SurfaceError> {
    if frame.width == 0 || frame.height == 0 {
        return Err(SurfaceError::Empty);
    }
    let downsample = downsample_for(frame.width, frame.height);
    let (image_frame, size, has_transparency) = prepare_frame(frame, orientation, downsample)?;
    Ok(Prepared {
        content: Content {
            image: Arc::new(RenderImage::new([image_frame])),
            texture_size: size,
        },
        has_transparency,
    })
}

/// 单帧：应用方向 → 交换红蓝 → 降采样 → 变成 `image::Frame`。
///
/// 返回的第二项是**处理之后**的纹理尺寸（方向可能交换宽高，降采样会缩小），
/// 第三项表示这一帧是否存在半透明像素，供画布决定要不要铺棋盘格。
fn prepare_frame(
    frame: &Frame,
    orientation: Orientation,
    downsample: u32,
) -> Result<(ImageFrame, (u32, u32), bool), SurfaceError> {
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
    Ok((image_frame, (width, height), transparent))
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
    use crate::model::Size;
    use crate::open_job::{FileStat, OpenOutcome};
    use std::path::PathBuf;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Frame {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            data.extend_from_slice(&color);
        }
        Frame::new(width, height, data, 0)
    }

    fn open_outcome(
        format: ImageFormat,
        frames: Vec<Frame>,
        orientation: Orientation,
        supersample: f32,
        page_count: usize,
    ) -> OpenOutcome {
        OpenOutcome {
            path: PathBuf::from("C:/photos/sample.png"),
            file: Some(FileStat {
                byte_len: 16,
                modified: None,
            }),
            result: Ok(ImageData {
                format,
                frames,
                orientation,
                exif: None,
                supersample,
            }),
            page_count,
            decode_ms: 0.0,
            total_ms: 0.0,
        }
    }

    /// 普通文档：走真实的打开路径构造，好让 `pages` 也与线上一致。
    fn document(frames: Vec<Frame>, orientation: Orientation, supersample: f32) -> ImageDocument {
        ImageDocument::from_outcome(open_outcome(
            ImageFormat::Png,
            frames,
            orientation,
            supersample,
            1,
        ))
        .expect("构造文档失败")
    }

    /// 多页文档：格式必须是 TIFF（非动画格式）才会被认作「多页」。
    /// 已解码的是 `frames`，而文件里一共有 `pages` 页。
    fn paged_document(frames: Vec<Frame>, pages: usize) -> ImageDocument {
        ImageDocument::from_outcome(open_outcome(
            ImageFormat::Tiff,
            frames,
            Orientation::Normal,
            1.0,
            pages,
        ))
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
        assert_eq!(surface.texture(0).expect("应有第 0 帧").size, (2, 4));
        // 逻辑尺寸由文档给出（它负责超采样倍率与当前页）。
        assert_eq!(document.logical_size(), Size::new(2.0, 4.0));
    }

    #[test]
    fn logical_size_is_preserved_for_supersampled_vectors() {
        // 192×192 的 SVG 纹理，声明尺寸 24×24：纹理归纹理，显示尺寸归显示尺寸。
        let document = document(vec![solid(192, 192, [0, 0, 0, 255])], Orientation::Normal, 8.0);
        let surface = Surface::build(&document).expect("构建纹理失败");
        assert_eq!(surface.texture(0).expect("应有第 0 帧").size, (192, 192));
        assert_eq!(document.logical_size(), Size::new(24.0, 24.0));
    }

    #[test]
    fn oversized_textures_are_downsampled() {
        let width = MAX_TEXTURE_EDGE + 4096;
        // 倍率按长边算，与方向无关。
        assert_eq!(downsample_for(width, 16), 2);
        assert_eq!(downsample_for(16, width), 2);
        assert_eq!(downsample_for(16, 16), 1);
        assert_eq!(downsample_for(MAX_TEXTURE_EDGE, 16), 1);
        assert_eq!(downsample_for(MAX_TEXTURE_EDGE + 1, 16), 2);

        // 直接调用降采样函数，避免测试里分配上百 MB 的像素。
        let source = vec![200u8; (width as usize) * 16 * 4];
        let (out_width, out_height, out) = downsample_bgra_ready(width, 16, source, 2);
        assert_eq!(out_width, width / 2);
        assert_eq!(out_height, 8);
        assert_eq!(out.len(), (out_width as usize) * (out_height as usize) * 4);
        // 均匀输入经过均值滤波应当还是同一个值。
        assert!(out.iter().all(|value| *value == 200), "均匀输入不应产生偏差");
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
        assert!(!surface.is_paged());
        assert_eq!(surface.loop_duration_ms(), 150);
        assert_eq!(surface.frame_index_at(0), 0);
        assert_eq!(surface.frame_index_at(120), 1);
        assert_eq!(surface.frame_index_at(150), 0, "应当循环回第一帧");
        // 两帧共用同一张纹理，帧号在纹理内部。
        assert_eq!(surface.texture(0).expect("第 0 帧").image.id, surface.texture(1).expect("第 1 帧").image.id);
        assert_eq!(surface.texture(1).expect("第 1 帧").frame, 1);
    }

    #[test]
    fn a_static_image_never_advances_its_frame() {
        let document = document(vec![solid(2, 2, [0, 0, 0, 255])], Orientation::Normal, 1.0);
        let surface = Surface::build(&document).expect("构建纹理失败");
        assert!(!surface.is_animated());
        assert_eq!(surface.frame_index_at(0), 0);
        assert_eq!(surface.frame_index_at(999_999), 0);
    }

    // ---- 多页 ----

    /// 多页文档里每页各拿一张纹理，并且**补页不会重建已画过的页**。
    ///
    /// 「不重建」是这条测试的重点：`RenderImage` 的 id 就是 GPU 上传的缓存键，
    /// 重建意味着已经看过的页被整批重新上传 —— 翻到第 N 页就是 O(N²)。
    #[test]
    fn pages_get_their_own_textures_and_earlier_ones_are_untouched() {
        let mut document = paged_document(vec![solid(4, 4, [0, 0, 0, 255])], 3);
        let mut surface = Surface::build(&document).expect("构建纹理失败");

        assert!(surface.is_paged());
        assert!(!surface.is_animated(), "多页文档不是动图，不该自己翻页");
        assert_eq!(surface.frame_count(), 1);
        let first_id = surface.texture(0).expect("第 1 页应当可画").image.id;

        // 补第 2 页：尺寸与第 1 页不同，好顺带确认每页按自己的尺寸建纹理。
        let second = solid(8, 6, [255, 255, 255, 255]);
        document
            .append_page(1, second.clone())
            .expect("追加第 2 页失败");
        surface
            .append_page(&second, document.orientation())
            .expect("补第 2 页纹理失败");

        assert_eq!(surface.frame_count(), 2);
        assert_eq!(
            surface.texture(0).expect("第 1 页应当还在").image.id,
            first_id,
            "第 1 页的纹理不该被重建（重建会让它重新上传一次）"
        );
        let second_ref = surface.texture(1).expect("第 2 页应当可画");
        assert_eq!(second_ref.size, (8, 6), "每页按自己的尺寸建纹理");
        assert_eq!(second_ref.frame, 0, "一页就是一张单帧纹理");
        assert_ne!(
            second_ref.image.id, first_id,
            "两页必须是两张纹理：共用一张就得整张重建"
        );

        // 还没补的页取不到 —— 界面据此显示「正在解码」，而不是错画相邻页。
        assert!(surface.texture(2).is_none());
    }

    /// 补页之后半透明标记要跟着翻转，否则第 2 页的透明像素会被画成黑块。
    #[test]
    fn transparency_is_updated_when_a_later_page_has_alpha() {
        let mut document = paged_document(vec![solid(4, 4, [10, 20, 30, 255])], 2);
        let mut surface = Surface::build(&document).expect("构建纹理失败");
        assert!(!surface.has_transparency());

        let translucent = solid(4, 4, [10, 20, 30, 0]);
        document
            .append_page(1, translucent.clone())
            .expect("追加失败");
        surface
            .append_page(&translucent, document.orientation())
            .expect("补页失败");
        assert!(
            surface.has_transparency(),
            "后补的一页有透明像素时，棋盘格必须跟着出现"
        );
    }

    /// 往非多页纹理上追加页必须报错，而不是安静地什么都不做。
    #[test]
    fn appending_a_page_to_a_non_paged_surface_is_an_error() {
        let document = document(vec![solid(4, 4, [0, 0, 0, 255])], Orientation::Normal, 1.0);
        let mut surface = Surface::build(&document).expect("构建纹理失败");
        let error = surface
            .append_page(&solid(4, 4, [0, 0, 0, 255]), Orientation::Normal)
            .expect_err("非多页纹理不该接受页");
        assert_eq!(error, SurfaceError::NotPaged);
        assert!(!error.user_message().is_empty());
    }

    /// 补页时方向也要叠上去 —— 用户转过 90° 之后翻页，新页必须也是转过的。
    #[test]
    fn appended_pages_honour_the_users_orientation() {
        let mut document = paged_document(vec![solid(4, 2, [0, 0, 0, 255])], 2);
        document.rotate_clockwise();
        let mut surface = Surface::build(&document).expect("构建纹理失败");
        assert_eq!(surface.texture(0).expect("第 1 页").size, (2, 4));

        let second = solid(4, 2, [0, 0, 0, 255]);
        document.append_page(1, second.clone()).expect("追加失败");
        surface
            .append_page(&second, document.orientation())
            .expect("补页失败");
        assert_eq!(
            surface.texture(1).expect("第 2 页").size,
            (2, 4),
            "补页也必须应用用户的旋转，否则第 2 页会横过来"
        );
    }
}
