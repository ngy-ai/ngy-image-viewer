//! 去马赛克：把「每个像素只有一种颜色」的传感器马赛克还原成完整 RGB 位图。
//!
//! # 为什么自己写
//!
//! `rawloader` 的职责边界是「从各家差异极大的 RAW 容器里正确取出 CFA 原始采样与标定参数」，
//! 它**不做**去马赛克 —— 那属于渲染管线的选择（双线性 / AHD / AMaZE 各有取舍）。
//! 这里采用双线性：实现简单、行为可预测、不会产生算法特有的伪影，
//! 对「打开看一眼」的看图场景完全够用，而且容易并行。
//!
//! # 管线
//!
//! ```text
//! 原始采样(u16 / f32)
//!   → 逐通道黑电平相减、白电平归一化   （把不同通道的量纲拉平）
//!   → 双线性插值补出缺失的两个颜色通道  （3×3 邻域同色均值）
//!   → 白平衡增益                       （以绿色为基准）
//!   → sRGB 伽马编码 → u8
//! ```
//!
//! 组合成「一个通道一个缩放系数」的做法（`gain / range`，`black` 单独减）
//! 与 dcraw 的 `scale_mul` 思路一致：先线性化，再增益，最后统一编码。

use rayon::prelude::*;
use rawloader::CFA;

/// 传感器采样源的统一视图。
///
/// `rawloader` 会按格式给出 16 位整数（绝大多数）或 32 位浮点（部分 DNG）。
/// 浮点数据存的已经是归一化的线性值，这里换算到 16 位整数的量纲，
/// 让上层的黑/白电平运算只需要一套代码。
#[derive(Clone, Copy)]
pub enum SensorSamples<'a> {
    Integer(&'a [u16]),
    Float(&'a [f32]),
}

impl SensorSamples<'_> {
    #[inline]
    pub fn get(&self, index: usize) -> f32 {
        match self {
            Self::Integer(data) => data[index] as f32,
            Self::Float(data) => data[index] * 65535.0,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Integer(data) => data.len(),
            Self::Float(data) => data.len(),
        }
    }

    /// 采样数与「宽 × 高 × 每像素分量数」是否自洽。
    ///
    /// 自洽检查放在这里而不是让后续索引越界 panic：损坏的 RAW 元数据
    /// （声明尺寸与实际数据不符）是真实存在的，必须变成一条可读的错误。
    pub fn covers(&self, count: usize) -> bool {
        self.len() >= count
    }
}

/// 传感器数据的一个矩形子视图。
///
/// 之所以要「视图」而不是直接切一个 `&[u16]`：去马赛克必须知道每个像素在
/// **整幅传感器**里的绝对坐标，否则 CFA 的颜色判断全会错位。
#[derive(Clone, Copy)]
pub struct SensorView<'a> {
    pub samples: SensorSamples<'a>,
    /// 一整行有多少个采样（未裁剪时的行宽）。
    pub stride: usize,
    /// 子视图左上角在整幅传感器中的坐标。
    pub origin: (usize, usize),
    pub width: usize,
    pub height: usize,
}

impl SensorView<'_> {
    #[inline]
    fn sample(&self, x: usize, y: usize) -> f32 {
        self.samples
            .get((self.origin.1 + y) * self.stride + self.origin.0 + x)
    }

    /// 该点在整幅传感器中的 CFA 颜色索引。用绝对坐标，裁剪不会改变颜色判断。
    #[inline]
    fn color_at(&self, cfa: &CFA, x: usize, y: usize) -> usize {
        cfa.color_at(self.origin.1 + y, self.origin.0 + x)
    }
}

/// 逐通道标定：把原始采样换算成线性 0..1 所需的全部系数。
#[derive(Clone, Copy, Debug)]
pub struct ChannelCalibration {
    /// 黑电平（该通道「全黑」时的读数）。
    pub black: [f32; 3],
    /// 白电平与黑电平之差，即该通道的有效动态范围。
    pub range: [f32; 3],
    /// 白平衡增益，已经以绿色（索引 1）为基准归一化。
    pub gain: [f32; 3],
}

impl Default for ChannelCalibration {
    fn default() -> Self {
        Self {
            black: [0.0; 3],
            range: [1.0; 3],
            gain: [1.0; 3],
        }
    }
}

impl ChannelCalibration {
    /// 原始采样 → 线性 0..1。
    ///
    /// 增益之后直接 clamp：高光处被增益放大的通道会削顶，
    /// 表现为「高光偏色」，这是不做完整色彩管理时最常见也最容易接受的取舍 ——
    /// 反过来（为了不削顶而整体压暗）会让整张照片发灰，更不可接受。
    #[inline]
    fn linearize(&self, channel: usize, raw: f32) -> f32 {
        let range = self.range[channel];
        let value = if range > 0.0 {
            (raw - self.black[channel]) / range
        } else {
            0.0
        };
        (value * self.gain[channel]).clamp(0.0, 1.0)
    }
}

/// 拜耳（或多色）马赛克 → RGBA8。
///
/// 双线性插值：像素自己的颜色直接用原值，缺失的两个通道取 3×3 邻域内
/// 所有「该颜色」像素的均值。对标准 RGGB/BGGR 等 2×2 阵列，
/// 这等价于教科书上的双线性插值；对 6×6 / 12×12 的复杂阵列（X-Trans 等）
/// 也依然能得到一张颜色正确的图，只是锐度略逊于专用算法。
pub fn bayer_to_rgba8(
    view: SensorView<'_>,
    cfa: &CFA,
    calibration: &ChannelCalibration,
) -> Vec<u8> {
    let (width, height) = (view.width, view.height);
    let mut out = vec![0u8; width * height * 4];

    out.par_chunks_exact_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..width {
                let mut sum = [0.0f32; 3];
                let mut count = [0u32; 3];

                accumulate(
                    &mut sum,
                    &mut count,
                    view.color_at(cfa, x, y),
                    view.sample(x, y),
                );

                for dy in -1isize..=1 {
                    for dx in -1isize..=1 {
                        if dy == 0 && dx == 0 {
                            continue;
                        }
                        let nx = x as isize + dx;
                        let ny = y as isize + dy;
                        if nx < 0 || ny < 0 || nx >= width as isize || ny >= height as isize {
                            continue;
                        }
                        let (nx, ny) = (nx as usize, ny as usize);
                        accumulate(
                            &mut sum,
                            &mut count,
                            view.color_at(cfa, nx, ny),
                            view.sample(nx, ny),
                        );
                    }
                }

                let base = x * 4;
                for channel in 0..3 {
                    let mean = if count[channel] > 0 {
                        sum[channel] / count[channel] as f32
                    } else {
                        // 该通道在邻域内一次都没出现（只可能发生在极罕见的多色 CFA 上）。
                        // 用其它通道的均值兜底，至少不会留下一块纯黑。
                        mean_of_present(&sum, &count)
                    };
                    row[base + channel] = srgb_encode(calibration.linearize(channel, mean));
                }
                row[base + 3] = 255;
            }
        });

    out
}

/// 单通道（黑白传感器）→ RGBA8。
pub fn mono_to_rgba8(
    view: SensorView<'_>,
    calibration: &ChannelCalibration,
) -> Vec<u8> {
    let (width, height) = (view.width, view.height);
    let mut out = vec![0u8; width * height * 4];

    out.par_chunks_exact_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..width {
                let value = srgb_encode(calibration.linearize(0, view.sample(x, y)));
                let base = x * 4;
                row[base] = value;
                row[base + 1] = value;
                row[base + 2] = value;
                row[base + 3] = 255;
            }
        });

    out
}

/// 已经是交错 RGB 的传感器数据（少数机型把 sRAW 直接存成每像素 3 分量）→ RGBA8。
///
/// 这条路径**不做**双线性：每个像素本来就有完整的三个通道，插值只会凭空糊掉细节。
/// `stride` 与 `origin` 的含义同 [`SensorView`]（以**像素**为单位，不是采样数）。
pub fn interleaved_rgb_to_rgba8(
    samples: SensorSamples<'_>,
    stride: usize,
    origin: (usize, usize),
    width: usize,
    height: usize,
    calibration: &ChannelCalibration,
) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 4];

    out.par_chunks_exact_mut(width * 4)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..width {
                let source = ((origin.1 + y) * stride + origin.0 + x) * 3;
                let base = x * 4;
                for channel in 0..3 {
                    let raw = samples.get(source + channel);
                    row[base + channel] = srgb_encode(calibration.linearize(channel, raw));
                }
                row[base + 3] = 255;
            }
        });

    out
}

#[inline]
fn accumulate(sum: &mut [f32; 3], count: &mut [u32; 3], channel: usize, value: f32) {
    // 颜色索引 3 是某些传感器的「额外通道」（RGBE 里的 E），不参与可见光三通道。
    if channel < 3 {
        sum[channel] += value;
        count[channel] += 1;
    }
}

fn mean_of_present(sum: &[f32; 3], count: &[u32; 3]) -> f32 {
    let mut total = 0.0;
    let mut present = 0u32;
    for channel in 0..3 {
        if count[channel] > 0 {
            total += sum[channel] / count[channel] as f32;
            present += 1;
        }
    }
    if present > 0 {
        total / present as f32
    } else {
        // 整个 3×3 邻域都没有任何可见光采样，只能给黑。
        0.0
    }
}

/// 线性光 → sRGB 编码值。
///
/// 相机输出的是线性光，直接当 sRGB 显示会整体偏暗、对比度失真；
/// 这里做标准的 sRGB 传递函数（含低亮度处的线性段，避免暗部噪声被过度放大）。
fn srgb_encode(linear: f32) -> u8 {
    let linear = linear.clamp(0.0, 1.0);
    let encoded = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calibration() -> ChannelCalibration {
        ChannelCalibration::default()
    }

    #[test]
    fn srgb_encoding_matches_reference_points() {
        assert_eq!(srgb_encode(0.0), 0);
        assert_eq!(srgb_encode(1.0), 255);
        // 中灰 0.5 线性光对应约 188 的 sRGB 码值（而不是 128）。
        let mid = srgb_encode(0.5);
        assert!((186..=190).contains(&mid), "实际得到 {mid}");
    }

    #[test]
    fn uniform_gray_bayer_stays_gray() {
        // 一片均匀的传感器读数，去马赛克后应当还是均匀的灰，
        // 且三个通道完全一致 —— 这是「黑电平/白电平/增益」没有串色的最低验收。
        let width = 8;
        let height = 8;
        let samples: Vec<u16> = vec![8192; width * height];
        let cfa = CFA::new("RGGB");
        let view = SensorView {
            samples: SensorSamples::Integer(&samples),
            stride: width,
            origin: (0, 0),
            width,
            height,
        };

        let rgba = bayer_to_rgba8(view, &cfa, &calibration());
        for pixel in rgba.chunks_exact(4) {
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[1], pixel[2]);
            assert_eq!(pixel[3], 255);
        }
        assert!(rgba[0] > 0, "均匀灰不应被压成纯黑");
    }

    #[test]
    fn cfa_color_indices_drive_the_channels() {
        // 构造一个只有红色通道亮的 2×2 阵列：RGGB 的 R 在第 (0,0) 与 (0,2) 位置。
        // 去马赛克后红通道必须显著高于另外两个，否则说明颜色索引用错了位置。
        let (width, height) = (4, 4);
        let mut samples = vec![0u16; width * height];
        let cfa = CFA::new("RGGB");
        for y in 0..height {
            for x in 0..width {
                if cfa.color_at(y, x) == 0 {
                    samples[y * width + x] = 60000;
                }
            }
        }

        let view = SensorView {
            samples: SensorSamples::Integer(&samples),
            stride: width,
            origin: (0, 0),
            width,
            height,
        };
        let rgba = bayer_to_rgba8(view, &cfa, &calibration());

        let center = (2 * width + 2) * 4;
        assert!(
            rgba[center] > 200,
            "红通道应当接近饱和，实际 {}",
            rgba[center]
        );
        assert!(
            rgba[center + 1] < 200 && rgba[center + 2] < 200,
            "绿蓝通道应明显较低，实际 {} / {}",
            rgba[center + 1],
            rgba[center + 2]
        );
    }

    #[test]
    fn cropped_view_keeps_absolute_color_positions() {
        // 裁剪后的视图必须用**绝对坐标**判断 CFA 颜色。
        // 这里裁掉最左一列：新视图的 (0,0) 在整幅里其实是 (1,0)，颜色应当由绿变红。
        let (width, height) = (4, 4);
        let samples: Vec<u16> = (0..(width * height) as u16).collect();
        let cfa = CFA::new("RGGB");

        let full = bayer_to_rgba8(
            SensorView {
                samples: SensorSamples::Integer(&samples),
                stride: width,
                origin: (0, 0),
                width,
                height,
            },
            &cfa,
            &calibration(),
        );
        let cropped = bayer_to_rgba8(
            SensorView {
                samples: SensorSamples::Integer(&samples),
                stride: width,
                origin: (1, 0),
                width: width - 1,
                height,
            },
            &cfa,
            &calibration(),
        );

        // 裁剪后第一行的第一个像素，应当等于原图第一行的第二个像素。
        assert_eq!(&cropped[0..3], &full[4..7]);
    }

    #[test]
    fn black_and_white_levels_are_applied_per_channel() {
        let calibration = ChannelCalibration {
            black: [1000.0, 2000.0, 3000.0],
            range: [10_000.0; 3],
            gain: [1.0; 3],
        };
        // 每个通道刚好落在自己的中点上，线性化后都应当是 0.5。
        assert!((calibration.linearize(0, 6000.0) - 0.5).abs() < 1e-6);
        assert!((calibration.linearize(1, 7000.0) - 0.5).abs() < 1e-6);
        assert!((calibration.linearize(2, 8000.0) - 0.5).abs() < 1e-6);
        // 低于黑电平要夹到 0，而不是产生负值。
        assert_eq!(calibration.linearize(0, 0.0), 0.0);
    }

    #[test]
    fn interleaved_rgb_path_does_not_interpolate() {
        // 4 个像素，每像素 3 分量。输出必须与原值一一对应，不能互相污染。
        let samples = [65535u16, 0, 0, 0, 65535, 0, 0, 0, 65535, 65535, 65535, 65535];
        let rgba =
            interleaved_rgb_to_rgba8(SensorSamples::Integer(&samples), 2, (0, 0), 2, 2, &calibration());
        assert_eq!(&rgba[0..3], &[255, 0, 0]);
        assert_eq!(&rgba[4..7], &[0, 255, 0]);
        assert_eq!(&rgba[8..11], &[0, 0, 255]);
        assert_eq!(&rgba[12..15], &[255, 255, 255]);
    }

    #[test]
    fn interleaved_rgb_path_honours_the_crop_origin() {
        // 3×1 的面板，裁掉最左一列后应当从第 2 个像素开始。
        let samples = [
            65535u16, 0, 0, // 红
            0, 65535, 0, // 绿
            0, 0, 65535, // 蓝
        ];
        let rgba = interleaved_rgb_to_rgba8(
            SensorSamples::Integer(&samples),
            3,
            (1, 0),
            2,
            1,
            &calibration(),
        );
        assert_eq!(&rgba[0..3], &[0, 255, 0], "第一列应当被裁掉");
        assert_eq!(&rgba[4..7], &[0, 0, 255]);
    }

    #[test]
    fn mono_path_replicates_the_single_channel() {
        let samples = [0u16, 32768, 65535, 16384];
        let rgba = mono_to_rgba8(
            SensorView {
                samples: SensorSamples::Integer(&samples),
                stride: 2,
                origin: (0, 0),
                width: 2,
                height: 2,
            },
            &calibration(),
        );
        for pixel in rgba.chunks_exact(4) {
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[1], pixel[2]);
            assert_eq!(pixel[3], 255);
        }
        assert_eq!(rgba[0], 0);
        assert_eq!(rgba[12], 255);
    }
}
