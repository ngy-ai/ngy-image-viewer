//! EXIF 方向的**像素级**纠正。
//!
//! 为什么需要它：旋转变换在渲染层需要变换矩阵支持，而这层能力在部分渲染后端上
//! 并不存在（GPUI 就没有公开的绘制变换 API）。为了让「旋转 / 翻转 / EXIF 摆正」
//! 这三件事在任何后端上都一定能做对，这里提供一份与渲染层完全无关的实现。
//!
//! 定位是「正确性兜底」而非「首选路径」：
//! - 渲染层能走变换时走变换（零拷贝、可交互）；
//! - 走不了时退回这里的像素重排（多一次整图拷贝，但结果与矩阵路径逐像素一致）。
//!
//! 两条路径必须产出相同结果，否则会出现「导出的文件和屏幕上看到的不一样」这种
//! 极难排查的问题，所以本模块带完整单元测试。

use super::types::Orientation;

/// 对 RGBA8 像素应用方向变换。
///
/// 返回 `None` 表示该方向为「无需纠正」，调用方可以直接沿用原数据 —— 
/// 显式返回 `None` 而不是原样拷贝一遍，是因为 99% 的图片方向都是 1，
/// 那条路径上不应该有任何多余的内存分配。
pub fn apply(
    width: u32,
    height: u32,
    rgba8: &[u8],
    orientation: Orientation,
) -> Option<(u32, u32, Vec<u8>)> {
    if orientation.is_identity() {
        return None;
    }

    let (src_w, src_h) = (width as usize, height as usize);
    debug_assert_eq!(
        rgba8.len(),
        src_w * src_h * 4,
        "像素数据长度与尺寸不符，调用方有 bug"
    );

    let (dst_w, dst_h) = if orientation.swaps_axes() {
        (src_h, src_w)
    } else {
        (src_w, src_h)
    };

    let mut out = vec![0u8; dst_w * dst_h * 4];
    for y in 0..src_h {
        for x in 0..src_w {
            let (dx, dy) = map_pixel(x, y, src_w, src_h, orientation);
            let src = (y * src_w + x) * 4;
            let dst = (dy * dst_w + dx) * 4;
            out[dst..dst + 4].copy_from_slice(&rgba8[src..src + 4]);
        }
    }
    Some((dst_w as u32, dst_h as u32, out))
}

/// 原图坐标 → 结果图坐标。
///
/// 语义遵循 EXIF 定义（“把存储的图像怎样摆放才是正的”），
/// 与 `image` crate 的 `Orientation` 完全一致。
fn map_pixel(x: usize, y: usize, w: usize, h: usize, orientation: Orientation) -> (usize, usize) {
    match orientation {
        Orientation::Normal => (x, y),
        Orientation::FlipHorizontal => (w - 1 - x, y),
        Orientation::Rotate180 => (w - 1 - x, h - 1 - y),
        Orientation::FlipVertical => (x, h - 1 - y),
        // 沿主对角线镜像。
        Orientation::Transpose => (y, x),
        // 顺时针 90°：左列变成顶行。
        Orientation::Rotate90 => (h - 1 - y, x),
        // 沿副对角线镜像。
        Orientation::Transverse => (h - 1 - y, w - 1 - x),
        // 顺时针 270°。
        Orientation::Rotate270 => (y, w - 1 - x),
    }
}

/// 逆变换。用于「撤销一次旋转」这类操作，避免再写一遍方向表。
pub fn inverse(orientation: Orientation) -> Orientation {
    match orientation {
        Orientation::Normal => Orientation::Normal,
        Orientation::FlipHorizontal => Orientation::FlipHorizontal,
        Orientation::Rotate180 => Orientation::Rotate180,
        Orientation::FlipVertical => Orientation::FlipVertical,
        Orientation::Transpose => Orientation::Transpose,
        Orientation::Transverse => Orientation::Transverse,
        Orientation::Rotate90 => Orientation::Rotate270,
        Orientation::Rotate270 => Orientation::Rotate90,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2×2 的图像，四个像素分别是 R/G/B/A 四种纯色，方便肉眼与断言核对。
    fn quad() -> (u32, u32, Vec<u8>) {
        let pixels: [[u8; 4]; 4] = [
            [255, 0, 0, 255], // 左上 红
            [0, 255, 0, 255], // 右上 绿
            [0, 0, 255, 255], // 左下 蓝
            [255, 255, 0, 255], // 右下 黄
        ];
        let mut data = Vec::with_capacity(16);
        for p in pixels {
            data.extend_from_slice(&p);
        }
        (2, 2, data)
    }

    fn pixel_at(data: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let index = ((y * width + x) * 4) as usize;
        [
            data[index],
            data[index + 1],
            data[index + 2],
            data[index + 3],
        ]
    }

    #[test]
    fn identity_returns_none_to_avoid_copying() {
        let (w, h, data) = quad();
        assert!(apply(w, h, &data, Orientation::Normal).is_none());
    }

    #[test]
    fn rotate90_moves_left_column_to_top_row() {
        // 2×1：左 A 右 B。顺时针 90° 后应变成 1×2：上 A 下 B。
        let data = vec![255, 0, 0, 255, 0, 255, 0, 255];
        let (w, h, out) = apply(2, 1, &data, Orientation::Rotate90).unwrap();
        assert_eq!((w, h), (1, 2));
        assert_eq!(pixel_at(&out, 1, 0, 0), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&out, 1, 0, 1), [0, 255, 0, 255]);
    }

    #[test]
    fn rotate90_on_quad_is_visually_correct() {
        let (w, h, data) = quad();
        let (dw, dh, out) = apply(w, h, &data, Orientation::Rotate90).unwrap();
        assert_eq!((dw, dh), (2, 2));
        // 顺时针 90°：左下(蓝) → 左上；左上(红) → 右上；右下(黄) → 左下；右上(绿) → 右下。
        assert_eq!(pixel_at(&out, 2, 0, 0), [0, 0, 255, 255]);
        assert_eq!(pixel_at(&out, 2, 1, 0), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&out, 2, 0, 1), [255, 255, 0, 255]);
        assert_eq!(pixel_at(&out, 2, 1, 1), [0, 255, 0, 255]);
    }

    #[test]
    fn rotate180_is_point_symmetric() {
        let (w, h, data) = quad();
        let (dw, dh, out) = apply(w, h, &data, Orientation::Rotate180).unwrap();
        assert_eq!((dw, dh), (2, 2));
        assert_eq!(pixel_at(&out, 2, 0, 0), pixel_at(&data, 2, 1, 1));
        assert_eq!(pixel_at(&out, 2, 1, 1), pixel_at(&data, 2, 0, 0));
    }

    #[test]
    fn flips_are_involutions() {
        let (w, h, data) = quad();
        for orientation in [Orientation::FlipHorizontal, Orientation::FlipVertical] {
            let (dw, dh, once) = apply(w, h, &data, orientation).unwrap();
            let (_, _, twice) = apply(dw, dh, &once, orientation).unwrap();
            assert_eq!(twice, data, "{orientation:?} 两次应回到原图");
        }
    }

    #[test]
    fn every_orientation_is_reverted_by_its_inverse() {
        let (w, h, data) = quad();
        for exif in 1..=8u8 {
            let orientation = Orientation::from_exif(exif);
            if orientation.is_identity() {
                continue;
            }
            let (dw, dh, once) = apply(w, h, &data, orientation).unwrap();
            let (_, _, twice) = apply(dw, dh, &once, inverse(orientation)).unwrap();
            assert_eq!(twice, data, "EXIF {exif} 的方向与其逆变换应能还原");
        }
    }

    #[test]
    fn swaps_axes_matches_actual_output_shape() {
        let (w, h, data) = quad();
        for exif in 1..=8u8 {
            let orientation = Orientation::from_exif(exif);
            let result = apply(w, h, &data, orientation);
            match result {
                None => assert!(!orientation.swaps_axes()),
                Some((dw, dh, _)) => {
                    if orientation.swaps_axes() {
                        assert_eq!((dw, dh), (h, w));
                    } else {
                        assert_eq!((dw, dh), (w, h));
                    }
                }
            }
        }
    }

    #[test]
    fn non_square_image_keeps_all_pixels() {
        // 3×1 的横条旋转 90°，应变成 1×3，且每个像素都还在。
        let mut data = Vec::new();
        for value in 1u8..=3 {
            data.extend_from_slice(&[value, value, value, 255]);
        }
        let (dw, dh, out) = apply(3, 1, &data, Orientation::Rotate270).unwrap();
        assert_eq!((dw, dh), (1, 3));
        let mut values: Vec<u8> = (0..3).map(|y| pixel_at(&out, 1, 0, y)[0]).collect();
        values.sort_unstable();
        assert_eq!(values, vec![1, 2, 3]);
    }
}
