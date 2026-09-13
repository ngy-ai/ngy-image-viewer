//! 一张已打开图片的领域状态。
//!
//! # 这层解决什么问题
//!
//! 解码层交出来的 [`ImageData`] 只是「像素 + 元数据」，它回答不了上层真正关心的问题：
//!
//! - 屏幕上到底该显示多大？（EXIF 方向会把宽高对调，SVG 有超采样倍率）
//! - 用户点了「顺时针旋转」之后，方向该怎么和 EXIF 方向叠加？
//! - 纹理该按什么尺寸构建？
//!
//! 把这些问题收敛到一个类型里，UI 层就只需要问「逻辑尺寸是多少」「纹理尺寸是多少」，
//! 不必在好几处各自推导一遍方向与倍率 —— 那种分散推导正是「屏幕上是正的、
//! 另存为导出却是歪的」这类问题的温床。
//!
//! # 方向的叠加顺序
//!
//! **先按 EXIF 摆正，再应用用户的旋转/翻转。** 顺序写反了会在「旋转过的竖拍照片」
//! 上立刻看出错位。`Orientation::then` 的名字就是为了让这个顺序在代码里一眼可读：
//! `exif.then(user)` 读作「先 EXIF，然后用户操作」。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::decode::{DecodeResult, ExifSummary, Frame, ImageData, ImageFormat, Orientation};
use crate::model::transform::Size;
use crate::open_job::{FileStat, OpenOutcome};
use crate::trace;

/// 一张已打开图片的完整领域状态。
pub struct ImageDocument {
    path: PathBuf,
    file: Option<FileStat>,
    data: ImageData,
    decode_ms: f64,
    total_ms: f64,
    /// 用户在 EXIF 方向**之上**叠加的旋转 / 翻转。
    user_orientation: Orientation,
}

impl ImageDocument {
    /// 把一次后台打开任务的结果转成文档。
    ///
    /// 失败时把 [`DecodeError`] 原样返回：错误文案是解码层精心写好的，
    /// 在这一层重新包装只会把「可操作的提示」变成一句无用的转述。
    pub fn from_outcome(outcome: OpenOutcome) -> DecodeResult<Self> {
        let OpenOutcome {
            path,
            file,
            result,
            decode_ms,
            total_ms,
        } = outcome;

        let document = Self {
            path,
            file,
            data: result?,
            decode_ms,
            total_ms,
            user_orientation: Orientation::Normal,
        };

        // 这一幕把「解码层的像素事实」翻译成「渲染层要用的尺寸」，是最容易在
        // 方向 / 超采样上出现「宽高对调」或「除以零」的地方，所以把三个尺寸都打出来。
        trace::step(
            "document",
            format!(
                "文档就绪：{} 纹理尺寸={}×{} 逻辑尺寸={:.2}×{:.2} EXIF 方向={:?} 超采样={}",
                document.path.display(),
                document.texture_size().0,
                document.texture_size().1,
                document.logical_size().width,
                document.logical_size().height,
                document.exif_orientation(),
                document.data.supersample(),
            ),
        );
        Ok(document)
    }

    // ---- 文件 ----

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 文件名。拿不到时退化为完整路径，而不是显示空白。
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    pub fn byte_len(&self) -> Option<u64> {
        self.file.as_ref().map(|stat| stat.byte_len)
    }

    pub fn modified(&self) -> Option<SystemTime> {
        self.file.as_ref().and_then(|stat| stat.modified)
    }

    /// 纯解码耗时（毫秒）。状态栏右侧展示它，让用户对「为什么这张图慢」有据可依。
    pub fn decode_ms(&self) -> f64 {
        self.decode_ms
    }

    /// 从点击到出像素的总耗时（毫秒）。
    pub fn total_ms(&self) -> f64 {
        self.total_ms
    }

    // ---- 格式与元数据 ----

    pub fn format(&self) -> ImageFormat {
        self.data.format
    }

    pub fn format_label(&self) -> &'static str {
        self.data.format.display_name()
    }

    pub fn exif(&self) -> Option<&ExifSummary> {
        self.data.exif.as_ref()
    }

    pub fn data(&self) -> &ImageData {
        &self.data
    }

    // ---- 帧与动画 ----

    pub fn frame_count(&self) -> usize {
        self.data.frame_count()
    }

    pub fn is_animated(&self) -> bool {
        self.data.is_animated()
    }

    pub fn primary(&self) -> &Frame {
        self.data.primary()
    }

    /// 按序号取帧。越界时退化为第一帧 —— 播放循环里的一帧错位不值得让整张图消失。
    pub fn frame(&self, index: usize) -> &Frame {
        self.data
            .frames
            .get(index)
            .unwrap_or_else(|| self.data.primary())
    }

    pub fn frame_index_at(&self, elapsed_ms: u64) -> usize {
        self.data.frame_index_at(elapsed_ms)
    }

    pub fn loop_duration_ms(&self) -> u64 {
        self.data.loop_duration_ms()
    }

    // ---- 方向 ----

    /// EXIF 给出的方向（未经用户修改）。
    pub fn exif_orientation(&self) -> Orientation {
        self.data.orientation
    }

    /// 用户在 EXIF 之上叠加的方向。
    pub fn user_orientation(&self) -> Orientation {
        self.user_orientation
    }

    /// 最终要施加到原始像素上的方向。
    pub fn orientation(&self) -> Orientation {
        self.data.orientation.then(self.user_orientation)
    }

    /// 用户是否做过旋转 / 翻转。
    ///
    /// 另存为与关闭确认都要用它判断「有没有需要特意提醒用户的变化」。
    /// 注意它**不包含** EXIF 方向：EXIF 是文件自带的，不算用户的改动。
    pub fn has_user_orientation(&self) -> bool {
        !self.user_orientation.is_identity()
    }

    pub fn rotate_clockwise(&mut self) {
        self.user_orientation = self.user_orientation.then(Orientation::Rotate90);
    }

    pub fn rotate_counter_clockwise(&mut self) {
        self.user_orientation = self.user_orientation.then(Orientation::Rotate270);
    }

    /// 水平翻转（沿竖直中轴镜像）。
    pub fn flip_horizontal(&mut self) {
        self.user_orientation = self.user_orientation.then(Orientation::FlipHorizontal);
    }

    /// 垂直翻转（沿水平中轴镜像）。
    pub fn flip_vertical(&mut self) {
        self.user_orientation = self.user_orientation.then(Orientation::FlipVertical);
    }

    pub fn reset_orientation(&mut self) {
        self.user_orientation = Orientation::Normal;
    }

    /// 更新文档指向的文件路径。重命名之后必须调用它。
    ///
    /// 不调用的话，文档会继续指向那个已经不存在的旧路径 ——
    /// 接着再点一次「重命名」或「另存为」，全部都会失败，而且失败理由
    /// （「文件不存在」）与用户刚做的操作看起来毫无关系。
    pub fn replace_path(&mut self, path: PathBuf) {
        self.path = path;
    }

    // ---- 尺寸 ----

    /// 纹理的像素尺寸：把最终方向应用到原始像素之后的实际像素数。
    ///
    /// 渲染层按它构建 `RenderImage`，也决定了旋转按钮要做什么样的像素重排。
    pub fn texture_size(&self) -> (u32, u32) {
        let frame = self.data.primary();
        if self.orientation().swaps_axes() {
            (frame.height, frame.width)
        } else {
            (frame.width, frame.height)
        }
    }

    /// 逻辑尺寸：状态栏显示、适应窗口、1:1 所使用的尺寸。
    ///
    /// 与纹理尺寸的差别只有 SVG 的超采样倍率 —— 一个 24×24 的矢量图
    /// 纹理可能是 192×192，但它「就是」24×24 那么大。
    pub fn logical_size(&self) -> Size {
        let (width, height) = self.texture_size();
        let supersample = self.data.supersample() as f32;
        Size::new(width as f32 / supersample, height as f32 / supersample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{DecodeLimits, ImageData};
    use crate::model::transform::Size;

    fn document(width: u32, height: u32, orientation: Orientation, supersample: f32) -> ImageDocument {
        let frame = Frame::new(width, height, vec![0; (width * height * 4) as usize], 0);
        let mut data = ImageData::single(ImageFormat::Png, frame);
        data.orientation = orientation;
        data.supersample = supersample;

        ImageDocument {
            path: PathBuf::from("C:/photos/sample.png"),
            file: Some(FileStat {
                byte_len: 4096,
                modified: None,
            }),
            data,
            decode_ms: 1.0,
            total_ms: 2.0,
            user_orientation: Orientation::Normal,
        }
    }

    #[test]
    fn logical_size_follows_exif_orientation() {
        let doc = document(400, 100, Orientation::Normal, 1.0);
        assert_eq!(doc.logical_size(), Size::new(400.0, 100.0));

        // 竖拍照片：EXIF 说"顺时针 90° 才是正的"，逻辑尺寸随之变成 100×400。
        let doc = document(400, 100, Orientation::Rotate90, 1.0);
        assert_eq!(doc.logical_size(), Size::new(100.0, 400.0));
        assert_eq!(doc.texture_size(), (100, 400));
    }

    #[test]
    fn supersample_shrinks_the_logical_size_but_not_the_texture() {
        // 一个 192×192 的 SVG 纹理，声明尺寸是 24×24。
        let doc = document(192, 192, Orientation::Normal, 8.0);
        assert_eq!(doc.texture_size(), (192, 192));
        assert_eq!(doc.logical_size(), Size::new(24.0, 24.0));
    }

    #[test]
    fn user_rotation_stacks_on_top_of_exif() {
        let mut doc = document(400, 100, Orientation::Rotate90, 1.0);
        // EXIF 已把 400×100 摆成 100×400；用户再顺时针转 90° → 400×100。
        doc.rotate_clockwise();
        assert_eq!(doc.logical_size(), Size::new(400.0, 100.0));
        assert_eq!(doc.orientation(), Orientation::Rotate90.then(Orientation::Rotate90));
        assert!(doc.has_user_orientation());
    }

    #[test]
    fn four_clockwise_turns_return_to_the_start() {
        let mut doc = document(400, 100, Orientation::Rotate90, 1.0);
        let before = doc.orientation();
        for _ in 0..4 {
            doc.rotate_clockwise();
        }
        assert_eq!(doc.orientation(), before);
        assert!(!doc.has_user_orientation(), "转四圈应当回到未被用户修改的状态");
    }

    #[test]
    fn flip_is_an_involution_and_survives_a_full_turn() {
        let mut doc = document(400, 100, Orientation::Normal, 1.0);
        doc.flip_horizontal();
        let flipped = doc.orientation();
        doc.flip_horizontal();
        assert_eq!(doc.orientation(), Orientation::Normal);
        assert!(!doc.has_user_orientation());

        // 镜像 + 一整圈旋转不等于恒等 —— 这一点常被写错。
        doc.flip_horizontal();
        doc.rotate_clockwise();
        doc.rotate_clockwise();
        assert_ne!(doc.orientation(), Orientation::Normal);
        doc.rotate_clockwise();
        doc.rotate_clockwise();
        assert_eq!(doc.orientation(), flipped);
    }

    #[test]
    fn reset_clears_only_the_user_side() {
        let mut doc = document(400, 100, Orientation::Rotate90, 1.0);
        doc.rotate_clockwise();
        doc.flip_vertical();
        doc.reset_orientation();

        assert_eq!(doc.user_orientation(), Orientation::Normal);
        assert_eq!(doc.orientation(), Orientation::Rotate90, "EXIF 方向不应被重置");
    }

    #[test]
    fn file_name_falls_back_to_the_full_path() {
        let mut doc = document(4, 4, Orientation::Normal, 1.0);
        assert_eq!(doc.file_name(), "sample.png");
        doc.path = PathBuf::from("C:/");
        assert!(!doc.file_name().is_empty(), "拿不到文件名时也不能返回空串");
    }

    #[test]
    fn frame_access_is_out_of_bounds_safe() {
        let doc = document(4, 4, Orientation::Normal, 1.0);
        // 越界取帧应当退化为第一帧，而不是 panic —— 播放循环里宁可错一帧。
        assert_eq!(doc.frame(999).width, doc.primary().width);
    }

    #[test]
    fn limits_are_not_consulted_here() {
        // 文档层不做任何尺寸判断：那是解码层的职责，
        // 这里只保证「拿到什么就如实描述什么」。
        let _ = DecodeLimits::default();
        let doc = document(10_000, 10_000, Orientation::Normal, 1.0);
        assert_eq!(doc.texture_size(), (10_000, 10_000));
    }
}
