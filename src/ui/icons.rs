//! 工具栏图标：编译期内联的 SVG 字节。
//!
//! 每个图标都以 `currentColor` 描边，运行时由按钮的 `text_color` 着色，
//! 因此自动跟随「禁用 / 选中 / 常态」三态 —— 不必为同一动作维护多套配色。
//!
//! 渲染走 `Svg::data`：字节在编译期就嵌入二进制，首帧期间只做一次光栅化，
//! GPUI 按字节哈希缓存纹理，之后每帧重建元素树也不会重新解码，对启动速度零持续影响。

pub const FIT: &[u8] = include_bytes!("icons/fit.svg");
pub const ACTUAL: &[u8] = include_bytes!("icons/actual.svg");
pub const ROTATE_CCW: &[u8] = include_bytes!("icons/rotate_ccw.svg");
pub const ROTATE_CW: &[u8] = include_bytes!("icons/rotate_cw.svg");
pub const FLIP_H: &[u8] = include_bytes!("icons/flip_h.svg");
pub const FLIP_V: &[u8] = include_bytes!("icons/flip_v.svg");
pub const COPY: &[u8] = include_bytes!("icons/copy.svg");
pub const SAVE_AS: &[u8] = include_bytes!("icons/save_as.svg");
pub const RENAME: &[u8] = include_bytes!("icons/rename.svg");
pub const TRASH: &[u8] = include_bytes!("icons/trash.svg");
pub const INFO: &[u8] = include_bytes!("icons/info.svg");

#[cfg(test)]
mod tests {
    use resvg::tiny_skia::{Pixmap, Transform};
    use resvg::usvg::{Options, Tree};

    /// (名字, 字节)。加图标时这里漏一个，测试就不会替它把门。
    const ALL: &[(&str, &[u8])] = &[
        ("fit", super::FIT),
        ("actual", super::ACTUAL),
        ("rotate_ccw", super::ROTATE_CCW),
        ("rotate_cw", super::ROTATE_CW),
        ("flip_h", super::FLIP_H),
        ("flip_v", super::FLIP_V),
        ("copy", super::COPY),
        ("save_as", super::SAVE_AS),
        ("rename", super::RENAME),
        ("trash", super::TRASH),
        ("info", super::INFO),
    ];

    /// 图标是内联字节、锁屏时截不到图，用这条测试代替肉眼验收：
    /// 每个 SVG 必须能解析、声明正尺寸、且光栅化后留有可见像素。
    ///
    /// 光栅化路径与运行时一致（usvg 解析 + resvg 渲染），所以这里过了，
    /// 工具栏上就一定能画出来。`currentColor` 在 usvg 里默认解成黑色，
    /// 因此着色不影响「有没有像素」这个断言。
    #[test]
    fn every_icon_parses_and_leaves_visible_pixels() {
        for (name, bytes) in ALL {
            let tree = Tree::from_data(bytes, &Options::default())
                .unwrap_or_else(|error| panic!("图标 {name} 解析失败：{error}"));
            let size = tree.size();
            assert!(
                size.width() > 0.0 && size.height() > 0.0,
                "图标 {name} 声明了非法尺寸"
            );

            let side = 64u32;
            let mut pixmap = Pixmap::new(side, side).expect("64×64 画布分配失败");
            resvg::render(
                &tree,
                Transform::from_scale(
                    side as f32 / size.width(),
                    side as f32 / size.height(),
                ),
                &mut pixmap.as_mut(),
            );

            // tiny-skia 存的是预乘像素，alpha 通道在每 4 字节的第 4 位。
            let painted = pixmap
                .data()
                .chunks_exact(4)
                .filter(|pixel| pixel[3] > 0)
                .count();
            assert!(painted > 0, "图标 {name} 光栅化后没有可见像素（图形是空的）");
        }
    }
}
