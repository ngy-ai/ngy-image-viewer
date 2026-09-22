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
    /// 可以浏览的页数；不分页的格式恒为 1。
    ///
    /// 它从「文件里有多少页」起步，补页因上限停下时会被收敛到已解码的页数 ——
    /// 这样界面永远不会把用户送到一页永远解不出来的空白页上（见 [`Self::append_page`]）。
    pages: usize,
    /// 当前浏览的是第几页（从 0 开始）。
    ///
    /// **永远指向一个已经解码的页**（见 [`Self::goto_page`]）。这一点是刻意的：
    /// 当前页一旦可以指向不存在的帧，`logical_size()` / `texture_size()` 就必须
    /// 凭空编一个尺寸出来，而「状态栏写着第 3 页、画布上却是第 1 页」正是这类错误里
    /// 最难被用户说清、也最难被开发者复现的一种。
    page: usize,
    /// 载入预算。生产路径永远是 [`PageBudget::default`]；测试把它调到很小，
    /// 好让「触到上限之后界面怎么办」这条路径真的被走一遍。
    budget: PageBudget,
}

/// 一份多页文档最多载入多少页。
///
/// 它与 [`crate::decode::DecodeLimits::max_frames`] 管的是两件事：那个上限在解码**单个
/// 文件**的时候生效，而这里是「用户一页页翻过去、每翻一页就多留一页」的累积量。
/// 后者不设上限的话，一本 800 页的扫描件就会被翻成一次内存耗尽。
pub const MAX_LOADED_PAGES: usize = 512;

/// 多页文档累计像素占用的上限（字节）。
///
/// 页与动图的帧很不一样：一页往往就是一整张扫描件（A4 300dpi 约 33 MB RGBA），
/// 十几页就能吃掉一张显卡的显存。触到上限就停下并告诉用户，而不是把机器拖死。
pub const MAX_PAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// 多页文档的载入预算。
///
/// 独立成一个类型而不是两个常量直接比较，是为了让上限**可注入**：真正触到 512 页 /
/// 2 GiB 需要造出几十 GB 的内存，那样的测试跑不起来，而「差一页」恰恰是这类判断里
/// 最容易写错的地方。测试用一个很小的预算就能把同一条代码路径走一遍。
#[derive(Clone, Copy, Debug)]
pub struct PageBudget {
    pub max_pages: usize,
    pub max_bytes: u64,
}

impl Default for PageBudget {
    fn default() -> Self {
        Self {
            max_pages: MAX_LOADED_PAGES,
            max_bytes: MAX_PAGE_BYTES,
        }
    }
}

impl PageBudget {
    /// 还能不能收下这一页。
    ///
    /// `decoded` 是**已经载入**的页数（也等于新页将要落在的序号）。
    pub fn admit(
        &self,
        decoded: usize,
        used_bytes: u64,
        incoming_bytes: u64,
    ) -> Result<(), PageRejection> {
        if decoded >= self.max_pages {
            return Err(PageRejection::PageLimit {
                limit: self.max_pages,
            });
        }
        // 用 saturating 加法：累计量真溢出 u64 时应当判「超了」，而不是回绕成
        // 「还剩很多空间」—— 后者会让一份恶意构造的文件一路加到内存耗尽。
        let used = used_bytes.saturating_add(incoming_bytes);
        if used > self.max_bytes {
            return Err(PageRejection::MemoryLimit {
                used,
                limit: self.max_bytes,
            });
        }
        Ok(())
    }
}

/// 补页被拒绝的原因。
///
/// 分两种**性质完全不同**的情况，调用方要区别对待：页序错位是内部状态问题，
/// 安静丢掉即可；触到上限则必须停下来并告诉用户，否则表现就是「翻到后面几页是空白」。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageRejection {
    /// 收到的不是紧接着已有页之后的那一页。
    OutOfOrder,
    /// 累计像素占用达到上限，后面的页不再载入。
    MemoryLimit { used: u64, limit: u64 },
    /// 页数达到上限。
    PageLimit { limit: usize },
}

impl PageRejection {
    /// 给用户看的一句话。`OutOfOrder` 不该被展示，所以也只给内部用的说明。
    pub fn user_message(&self) -> Option<String> {
        match self {
            Self::OutOfOrder => None,
            Self::MemoryLimit { used, limit } => Some(format!(
                "多页文档已载入 {:.1} GB，达到 {:.1} GB 的载入上限，其余页未载入",
                *used as f64 / 1_073_741_824.0,
                *limit as f64 / 1_073_741_824.0,
            )),
            Self::PageLimit { limit } => {
                Some(format!("多页文档超过 {limit} 页的载入上限，其余页未载入"))
            }
        }
    }
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
            page_count,
            decode_ms,
            total_ms,
        } = outcome;

        // 解码器交出来的像素一定看得见，所以可浏览页数取两者的较大值：
        // 多页 TIFF 打开时是「已解 1 帧、共 N 页」，而动画格式永远是「N 帧、1 页」。
        let data = result?;
        let pages = page_count.max(data.frame_count());

        let document = Self {
            path,
            file,
            data,
            decode_ms,
            total_ms,
            user_orientation: Orientation::Normal,
            pages,
            page: 0,
            budget: PageBudget::default(),
        };

        // 这一幕把「解码层的像素事实」翻译成「渲染层要用的尺寸」，是最容易在
        // 方向 / 超采样上出现「宽高对调」或「除以零」的地方，所以把三个尺寸都打出来。
        // 「已解几页 / 共几页」也一并打。
        trace::step(
            "document",
            format!(
                "文档就绪：{} 纹理尺寸={}×{} 逻辑尺寸={:.2}×{:.2} EXIF 方向={:?} 超采样={} 已解页数={} 总页数={} 多页文档={}",
                document.path.display(),
                document.texture_size().0,
                document.texture_size().1,
                document.logical_size().width,
                document.logical_size().height,
                document.exif_orientation(),
                document.data.supersample(),
                document.decoded_pages(),
                document.pages(),
                document.is_paged(),
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

    // ---- 内容：帧（动图）与页（多页文档） ----

    /// 已解码到内存里的**帧数**。多页文档里它就等于已解码的页数。
    ///
    /// 注意它回答的不是「这份文件有多少内容」—— 那个问题由 [`Self::pages`] 回答。
    /// 多页 TIFF 刚打开时这里是 1，而 `pages()` 可能是 30。
    pub fn frame_count(&self) -> usize {
        self.data.frame_count()
    }

    /// 是不是「按时间自动播放的动图」。判据里带格式维度，理由见
    /// [`ImageData::is_animated`]。
    pub fn is_animated(&self) -> bool {
        self.data.is_animated()
    }

    /// 第一帧 / 第一页（用户打开这份文件时默认看到的那一个）。
    pub fn primary(&self) -> &Frame {
        self.data.primary()
    }

    /// 当前浏览页的像素。
    ///
    /// `page` 只可能指向已解码的页（见 [`Self::goto_page`]），所以这里不会落空；
    /// 仍然写成 `get` + 兜底而不是索引，是因为[解码层](crate::decode)把
    /// 「绝不 panic」定成了硬要求，模型层不该在它上面开一个例外。
    pub fn current(&self) -> &Frame {
        self.data
            .frames
            .get(self.page)
            .unwrap_or_else(|| self.data.primary())
    }

    /// 按序号取一个内容（动图的帧 / 多页文档的页）。
    ///
    /// 未解码的内容返回 `None`，**绝不退化为第一页**。旧实现是「越界就给第一帧」，
    /// 那对播放循环或许无伤大雅，对多页文档却是错的：「点下一页点出一张已经看过的图」
    /// 会被当成界面卡了，而画面上明明有图。
    pub fn content(&self, index: usize) -> Option<&Frame> {
        self.data.frames.get(index)
    }

    pub fn frame_index_at(&self, elapsed_ms: u64) -> usize {
        self.data.frame_index_at(elapsed_ms)
    }

    pub fn loop_duration_ms(&self) -> u64 {
        self.data.loop_duration_ms()
    }

    // ---- 页 ----

    /// 这份文件一共有多少页可以浏览；不分页的格式恒为 1。
    pub fn pages(&self) -> usize {
        self.pages
    }

    /// 是不是「多页文档」。
    ///
    /// 与「动图」是互斥的两种东西：动图的多个 frame 挂在时间轴上（自动播放），
    /// 多页文档的多个 frame 是并列的页（由用户翻）。同一个 `frames` 数组被两种
    /// 语义共用，所以每一处都要问清楚自己要的是哪一种。
    pub fn is_paged(&self) -> bool {
        self.pages > 1 && !self.data.format.may_be_animated()
    }

    /// 已经解码到内存里的页数。它只会增长，且不大于 [`Self::pages`]。
    pub fn decoded_pages(&self) -> usize {
        self.data.frames.len()
    }

    /// 当前浏览的是第几页（从 0 开始）。
    pub fn current_page(&self) -> usize {
        self.page
    }

    /// 第 `index` 页是否已经解码。未解码的页画不出来，界面要单独提示。
    pub fn has_page(&self, index: usize) -> bool {
        index < self.data.frames.len()
    }

    /// 切到第 `index` 页。只有在它**已经解码**时才会成功，返回是否发生了切换。
    ///
    /// 刻意不支持「先切过去、等它解码」：那样 `logical_size()` 与 `texture_size()`
    /// 就得为一个不存在的帧编个尺寸出来，而状态栏会抢先显示新页码 ——
    /// 用户看到的将是「页码变了、画还是上一张」。
    pub fn goto_page(&mut self, index: usize) -> bool {
        if !self.is_paged() || !self.has_page(index) || index == self.page {
            return false;
        }
        self.page = index;
        true
    }

    /// 翻 `delta` 页（`-1` 上一页 / `+1` 下一页）。已经在首尾时不动，返回是否翻动了。
    pub fn step_page(&mut self, delta: isize) -> bool {
        if !self.is_paged() {
            return false;
        }
        let target = self
            .page
            .saturating_add_signed(delta)
            .min(self.pages.saturating_sub(1));
        self.goto_page(target)
    }

    /// 收下一页的解码结果。
    ///
    /// 页必须**按序**到达（`index` 恰好是下一个空位）：补页任务可能被丢弃、重发，
    /// 但绝不会乱序，所以乱序到达意味着内部状态出了问题 —— 那要报出来，
    /// 而不是把页塞到一个错误的位置上（那会让「第 3 页」永远显示第 5 页的内容）。
    ///
    /// 触到页数或内存上限时返回 `Err`，并把 [`Self::pages`] 收敛到已载入的部分：
    /// 界面于是最多只能翻到最后一页真实存在的页，不会把用户送到一页永远解不出来的空白页。
    pub fn append_page(&mut self, index: usize, frame: Frame) -> Result<(), PageRejection> {
        if index != self.data.frames.len() {
            return Err(PageRejection::OutOfOrder);
        }
        let used: u64 = self
            .data
            .frames
            .iter()
            .map(|frame| frame.byte_len() as u64)
            .sum();
        if let Err(rejection) =
            self.budget
                .admit(self.data.frames.len(), used, frame.byte_len() as u64)
        {
            self.pages = self.data.frames.len().max(1);
            return Err(rejection);
        }
        self.data.frames.push(frame);
        Ok(())
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
    ///
    /// 指的是**当前页**：一份多页 TIFF 的几页可以有各自的尺寸（扫描件常见），
    /// 而屏幕上此刻只有一页。
    pub fn texture_size(&self) -> (u32, u32) {
        let frame = self.current();
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
    ///
    /// 同样指的是当前页。界面上所有显示尺寸的地方（状态栏、信息面板、
    /// 适应窗口）都经由这里，于是翻页时它们会一起跟着变，不会有谁还停在第一页。
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
            pages: 1,
            page: 0,
            budget: PageBudget::default(),
        }
    }

    /// 一份多页文档：已解码 `sizes` 描述的这几页，文件里一共有 `pages` 页。
    ///
    /// 格式用 TIFF —— 只有「非动画格式 + 多页」才会被 [`ImageDocument::is_paged`] 认作
    /// 多页文档，换成 GIF 就变成动图了。
    fn paged_document(sizes: &[(u32, u32)], pages: usize) -> ImageDocument {
        let frames: Vec<Frame> = sizes
            .iter()
            .map(|(width, height)| {
                Frame::new(*width, *height, vec![0; (*width * *height * 4) as usize], 0)
            })
            .collect();

        ImageDocument {
            path: PathBuf::from("C:/scans/multi.tiff"),
            file: Some(FileStat {
                byte_len: 4096,
                modified: None,
            }),
            data: ImageData {
                format: ImageFormat::Tiff,
                frames,
                orientation: Orientation::Normal,
                exif: None,
                supersample: 1.0,
            },
            decode_ms: 1.0,
            total_ms: 2.0,
            user_orientation: Orientation::Normal,
            pages,
            page: 0,
            budget: PageBudget::default(),
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

    // ---- 页 ----

    /// 多页 TIFF 补页之后帧数会长到 2 以上，但它不是动图。
    ///
    /// 这条断言守的是本功能最容易翻车的地方：把「页」当成「帧」，
    /// 于是打开一份 30 页的扫描件就开始自动翻页，而用户按「下一页」时永远追不上。
    #[test]
    fn pages_are_not_animation_frames() {
        let doc = paged_document(&[(4, 4), (4, 4)], 2);
        assert_eq!(doc.frame_count(), 2);
        assert!(doc.is_paged());
        assert!(!doc.is_animated(), "多页文档不该被当成动图自动播放");

        // 反过来：真的动图（GIF 多帧）不该被当成多页文档。
        let mut animated = document(4, 4, Orientation::Normal, 1.0);
        animated.data.format = ImageFormat::Gif;
        animated.data.frames.push(Frame::new(4, 4, vec![0; 64], 100));
        animated.pages = 2;
        assert!(animated.is_animated());
        assert!(!animated.is_paged(), "动图的多帧不是页");
    }

    #[test]
    fn content_of_a_page_that_is_not_decoded_does_not_fall_back() {
        // 已解前两页，文件里共 5 页。取第 3 页（下标 2）必须拿到 None ——
        // 「给不出就悄悄给第 1 页」在这里是最难发现的错：画面上明明有图，
        // 用户只会以为自己点错了。
        let mut doc = paged_document(&[(4, 4), (6, 6)], 5);
        assert!(doc.content(0).is_some());
        assert!(doc.content(1).is_some());
        assert!(doc.content(2).is_none());

        // 同上：切到一个还没解码的页必须失败，并且当前页不动。
        assert!(!doc.goto_page(2), "未解码的页不能成为当前页");
        assert_eq!(doc.current_page(), 0);
        assert!(doc.goto_page(1));
        assert_eq!(doc.current_page(), 1);
    }

    #[test]
    fn stepping_saturates_at_both_ends() {
        let mut doc = paged_document(&[(4, 4), (4, 4), (4, 4)], 3);

        assert!(!doc.step_page(-1), "第一页再往前没有页");
        assert_eq!(doc.current_page(), 0);
        assert!(doc.step_page(1));
        assert_eq!(doc.current_page(), 1);
        assert!(doc.step_page(1));
        assert_eq!(doc.current_page(), 2);
        assert!(!doc.step_page(1), "最后一页再往后没有页");
        assert_eq!(doc.current_page(), 2);

        // 不分页的格式怎么翻都不动 —— 键盘上按 PageDown 不该有任何副作用。
        let mut plain = document(4, 4, Orientation::Normal, 1.0);
        assert!(!plain.step_page(1));
        assert!(!plain.step_page(-1));
        assert_eq!(plain.current_page(), 0);
    }

    /// 每一页可以有各自的尺寸，而尺寸相关的一切都跟着当前页走。
    ///
    /// 扫描件里「正文页 A4、插页 A3」很常见。若尺寸只认第一页，
    /// 翻到插页时会被拉伸成 A4 的比例 —— 而画面上看不出「被拉伸了」，
    /// 只觉得那张图有点糊。
    #[test]
    fn size_follows_the_current_page() {
        let mut doc = paged_document(&[(100, 200), (300, 50)], 2);
        assert_eq!(doc.texture_size(), (100, 200));
        assert_eq!(doc.logical_size(), Size::new(100.0, 200.0));

        assert!(doc.goto_page(1));
        assert_eq!(doc.texture_size(), (300, 50));
        assert_eq!(doc.logical_size(), Size::new(300.0, 50.0));

        // EXIF 方向仍然叠在「当前页」的像素上。
        let mut doc = paged_document(&[(100, 200), (300, 50)], 2);
        doc.data.orientation = Orientation::Rotate90;
        assert!(doc.goto_page(1));
        assert_eq!(doc.texture_size(), (50, 300));
    }

    #[test]
    fn appending_pages_advances_the_decoded_count() {
        let mut doc = paged_document(&[(4, 4)], 3);
        assert_eq!(doc.decoded_pages(), 1);

        doc.append_page(1, Frame::new(4, 4, vec![0; 64], 0))
            .expect("正常追加不该失败");
        assert_eq!(doc.decoded_pages(), 2);
        assert!(doc.has_page(1));
        // 追加不改变当前页：用户还在第 1 页上。
        assert_eq!(doc.current_page(), 0);

        doc.append_page(2, Frame::new(4, 4, vec![0; 64], 0))
            .expect("正常追加不该失败");
        assert_eq!(doc.decoded_pages(), 3);
        assert_eq!(doc.pages(), 3, "没有触到上限时页数保持文件里报的值");
    }

    /// 乱序到达的页必须报出来，而不是被塞到错误的位置上。
    ///
    /// 塞错位置的后果是「第 3 页永远显示第 5 页的内容」，而且文件越大越难对出来。
    #[test]
    fn out_of_order_pages_are_rejected_without_a_user_facing_message() {
        let mut doc = paged_document(&[(4, 4)], 5);
        let rejection = doc
            .append_page(3, Frame::new(4, 4, vec![0; 64], 0))
            .expect_err("跳页追加应当被拒绝");
        assert_eq!(rejection, PageRejection::OutOfOrder);
        assert_eq!(doc.decoded_pages(), 1, "被拒绝的页不该进内存");
        // 这是内部状态问题，不该弹给用户看 —— 用户对此无能为力。
        assert!(rejection.user_message().is_none());
    }

    /// 预算判据的边界值：正好用满时放行，多一个就拒。
    #[test]
    fn the_page_budget_stops_exactly_at_both_limits() {
        let full = PageBudget {
            max_pages: 3,
            max_bytes: 1000,
        };

        // 页数：第 3 页（下标 2）放行，第 4 页就拒。
        assert!(full.admit(2, 0, 0).is_ok());
        assert_eq!(full.admit(3, 0, 0), Err(PageRejection::PageLimit { limit: 3 }));

        // 内存：恰好等于上限时放行（是「超过」才拒），再多一个字节就拒。
        assert!(full.admit(0, 990, 10).is_ok());
        match full.admit(0, 1000, 1) {
            Err(PageRejection::MemoryLimit { used, limit }) => {
                assert_eq!(used, 1001);
                assert_eq!(limit, 1000);
            }
            other => panic!("期望内存上限，实际为 {other:?}"),
        }

        // 累计量溢出 u64 也不能回绕成「还剩很多空间」。
        assert!(full.admit(0, u64::MAX, u64::MAX).is_err());
    }

    /// 触到上限之后，可浏览页数要收敛到已载入的部分。
    ///
    /// 不收敛的话，界面会继续把用户送到一页永远解不出来的空白页上 ——
    /// 那看起来像卡死，而不是「到上限了」。这里用一个极小的预算把真实路径走一遍。
    #[test]
    fn hitting_a_limit_shrinks_the_browsable_page_count() {
        let mut doc = paged_document(&[(4, 4)], 900);
        doc.budget = PageBudget {
            max_pages: 2,
            max_bytes: u64::MAX,
        };

        // 第 2 页还能收下（已载入 1 页，下标 1 < 上限 2）。
        doc.append_page(1, Frame::new(4, 4, vec![0; 64], 0))
            .expect("第 2 页应当能收下");
        assert_eq!(doc.decoded_pages(), 2);

        // 第 3 页触到页数上限：拒绝，并且可浏览页数收敛到 2。
        let rejection = doc
            .append_page(2, Frame::new(4, 4, vec![0; 64], 0))
            .expect_err("超过页数上限应当被拒");
        assert_eq!(rejection, PageRejection::PageLimit { limit: 2 });
        assert!(rejection.user_message().is_some(), "触到上限必须告诉用户");
        assert_eq!(doc.pages(), 2, "页数收敛到已载入的页数");
        assert_eq!(doc.decoded_pages(), 2);
        assert!(doc.goto_page(1));
        assert!(!doc.step_page(1), "收敛之后不该还能翻到不存在的页");
    }

    /// 内存预算触到之后同样收敛，且报出的用量包含被拒的那一页。
    #[test]
    fn the_memory_limit_also_shrinks_the_page_count() {
        let mut doc = paged_document(&[(4, 4)], 900);
        // 每页 4×4×4 = 64 字节；已载入 1 页占 64，预算给 100：第 2 页会让它到 128。
        doc.budget = PageBudget {
            max_pages: usize::MAX,
            max_bytes: 100,
        };

        let rejection = doc
            .append_page(1, Frame::new(4, 4, vec![0; 64], 0))
            .expect_err("超过内存上限应当被拒");
        match rejection {
            PageRejection::MemoryLimit { used, limit } => {
                assert_eq!(used, 128);
                assert_eq!(limit, 100);
            }
            other => panic!("期望内存上限，实际为 {other:?}"),
        }
        assert_eq!(doc.pages(), 1, "页数收敛到已载入的页数");
    }

    #[test]
    fn rejection_messages_say_what_happened_in_chinese() {
        let memory = PageRejection::MemoryLimit {
            used: 3 * 1024 * 1024 * 1024,
            limit: 2 * 1024 * 1024 * 1024,
        };
        let message = memory.user_message().expect("内存上限要给用户一句解释");
        assert!(message.contains("3.0 GB"), "实际文案：{message}");
        assert!(message.contains("未载入"), "实际文案：{message}");

        let pages = PageRejection::PageLimit { limit: 512 };
        let message = pages.user_message().expect("页数上限要给用户一句解释");
        assert!(message.contains("512"), "实际文案：{message}");
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
