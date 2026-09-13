//! 视图变换：从「图像坐标」到「窗口坐标」的全部数学。
//!
//! # 为什么这里没有旋转
//!
//! 最初的设计是用一个仿射矩阵把旋转交给渲染层完成。实际核实 GPUI 的绘制 API 后发现：
//! `Window::paint_image(bounds, image_bounds, ...)` **只接受轴对齐的矩形**，
//! 整个 crate 里没有任何公开的仿射变换入口。因此旋转与翻转改为在**像素层**完成
//! （见 [`crate::decode::orientation`]），本模块只负责缩放与平移。
//!
//! 这个取舍并不亏：像素层的旋转同样只是一次整图拷贝，却换来一条更硬的性质 ——
//! 「屏幕上看到的」与「另存为导出的」逐像素一致，不存在方向对不上的隐患。
//!
//! # 坐标系约定
//!
//! - **图像坐标**：原点在图像左上角，向右向下为正，单位是「图像逻辑像素」
//!   （SVG 的超采样倍率已在 `ImageData` 层面扣除）。
//! - **窗口坐标**：原点在视口（画布）左上角，向右向下为正，单位是逻辑点。
//!
//! 两者之间只有缩放与平移，所以整块数学就是一条直线：
//!
//! ```text
//! 内容尺寸 = 图像尺寸 × scale
//! 内容原点 = (视口尺寸 - 内容尺寸) / 2 + pan
//! 窗口点   = 内容原点 + 图像点 × scale
//! ```
//!
//! # 为什么整块逻辑都能单测
//!
//! 这里不引用任何 UI 类型，输入输出都是本模块自己的 `Vec2` / `Size` / `Rect`。
//! 「滚轮缩放时光标下的那个像素必须钉在原地」这类最容易出错的性质，
//! 因此可以写成精确的断言，而不是靠肉眼在窗口里看。

use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

/// 缩放下限。
///
/// 1/512 已经覆盖「一亿像素的扫描件塞进一个小窗口」这种极端情况；
/// 再往下就没有可看的信息了，继续缩只会放大数值误差。
pub const MIN_SCALE: f32 = 1.0 / 512.0;

/// 缩放上限。256 倍时一颗像素已经占满大半屏幕，再放大只会暴露插值伪影。
pub const MAX_SCALE: f32 = 256.0;

/// 滚轮每一档的缩放倍率。
///
/// 1.15 是「连续滚动时能明显感觉到变化，又不会一档跳太远」的经验值。
pub const ZOOM_STEP: f32 = 1.15;

/// 二维向量（同时用作点与位移）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// 两个分量都是有限数。
    ///
    /// 所有对外暴露的入口都用它筛一遍输入：一个 NaN 的鼠标坐标只要进了
    /// `pan`，之后所有坐标都会变成 NaN，表现为「图片凭空消失」，极难定位。
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }

    pub fn length(self) -> f32 {
        self.x.hypot(self.y)
    }
}

impl Add for Vec2 {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl Sub for Vec2 {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl AddAssign for Vec2 {
    fn add_assign(&mut self, rhs: Self) {
        self.x += rhs.x;
        self.y += rhs.y;
    }
}

impl SubAssign for Vec2 {
    fn sub_assign(&mut self, rhs: Self) {
        self.x -= rhs.x;
        self.y -= rhs.y;
    }
}

impl Neg for Vec2 {
    type Output = Self;

    fn neg(self) -> Self {
        Self::new(-self.x, -self.y)
    }
}

/// 二维尺寸。宽高恒为非负才有意义。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const ZERO: Self = Self {
        width: 0.0,
        height: 0.0,
    };

    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    /// 任一边为零、为负或非有限，都视为「空」。
    ///
    /// 用 `>` 而不是 `>=`：零尺寸的图像没有任何可显示的像素，
    /// 让它走「空」的分支可以顺带避免后续的除零。
    pub fn is_empty(self) -> bool {
        !(self.width > 0.0 && self.height > 0.0)
    }

    pub fn aspect_ratio(self) -> Option<f32> {
        if self.is_empty() {
            None
        } else {
            Some(self.width / self.height)
        }
    }

    pub fn scaled(self, factor: f32) -> Self {
        Self::new(self.width * factor, self.height * factor)
    }
}

/// 轴对齐矩形。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub origin: Vec2,
    pub size: Size,
}

impl Rect {
    pub fn new(origin: Vec2, size: Size) -> Self {
        Self { origin, size }
    }

    pub fn from_center(center: Vec2, size: Size) -> Self {
        Self {
            origin: Vec2::new(center.x - size.width / 2.0, center.y - size.height / 2.0),
            size,
        }
    }

    pub fn center(&self) -> Vec2 {
        Vec2::new(
            self.origin.x + self.size.width / 2.0,
            self.origin.y + self.size.height / 2.0,
        )
    }

    pub fn max(&self) -> Vec2 {
        Vec2::new(
            self.origin.x + self.size.width,
            self.origin.y + self.size.height,
        )
    }

    pub fn contains(&self, point: Vec2) -> bool {
        point.x >= self.origin.x
            && point.y >= self.origin.y
            && point.x <= self.origin.x + self.size.width
            && point.y <= self.origin.y + self.size.height
    }
}

/// 缩放模式的来源。
///
/// 它决定「窗口尺寸变化时该怎么办」：适应窗口要重新计算，
/// 而 1:1 与用户手动缩放的倍率都必须保持不变。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZoomMode {
    /// 图像完整装进视口。
    #[default]
    Fit,
    /// 1 个图像像素 = 1 个设备像素。
    Actual,
    /// 用户手动缩放出来的倍率。
    Free,
}

/// 当前的视图状态：缩放倍率 + 相对视口中心的平移。
///
/// 用「相对视口中心的平移」而不是「内容左上角的绝对坐标」来表达位置，
/// 是因为前者在窗口缩放时自动保持居中，不需要任何补偿逻辑。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewTransform {
    scale: f32,
    pan: Vec2,
    mode: ZoomMode,
}

impl Default for ViewTransform {
    fn default() -> Self {
        Self {
            scale: 1.0,
            pan: Vec2::ZERO,
            mode: ZoomMode::Fit,
        }
    }
}

impl ViewTransform {
    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn pan(&self) -> Vec2 {
        self.pan
    }

    pub fn mode(&self) -> ZoomMode {
        self.mode
    }

    pub fn is_at_min_scale(&self) -> bool {
        self.scale <= MIN_SCALE
    }

    pub fn is_at_max_scale(&self) -> bool {
        self.scale >= MAX_SCALE
    }

    /// 状态栏与工具栏展示的缩放百分比。
    ///
    /// **100% 的定义是「1 个图像像素 = 1 个设备像素」**，而不是
    /// 「1 个图像像素 = 1 个逻辑点」。在 200% 缩放的屏幕上，两者差一倍 ——
    /// 用户按下 1:1 时期待的显然是前者。
    pub fn zoom_percent(&self, pixel_ratio: f32) -> f32 {
        self.scale * pixel_ratio.max(f32::MIN_POSITIVE) * 100.0
    }

    /// 「适应窗口」的倍率。图像或视口为空时返回 `None`。
    pub fn fit_scale(image: Size, viewport: Size) -> Option<f32> {
        if image.is_empty() || viewport.is_empty() {
            return None;
        }
        let horizontal = viewport.width / image.width;
        let vertical = viewport.height / image.height;
        Some(clamp_scale(horizontal.min(vertical)))
    }

    /// 切到「适应窗口」。图像会比视口小时**会**被放大 —— 这是该模式的字面含义，
    /// 也是各平台看图工具的通行行为。
    pub fn fit(&mut self, image: Size, viewport: Size) {
        self.scale = Self::fit_scale(image, viewport).unwrap_or(1.0);
        self.pan = Vec2::ZERO;
        self.mode = ZoomMode::Fit;
    }

    /// 「适应窗口」的等价变换，但**不修改自身**。
    ///
    /// 给画布用：绘制时才能拿到视口的真实尺寸，而那时已经不能再改视图状态了
    /// （绘制闭包拿不到 `&mut self`）。就地求值可以让第一帧就落在正确的位置，
    /// 不必等视图下一帧把尺寸回传上来才发现「适应窗口」还没生效。
    pub fn fitted(image: Size, viewport: Size) -> Self {
        let mut result = Self::default();
        result.fit(image, viewport);
        result
    }

    /// 切到「1:1 原尺寸」。
    pub fn actual_size(&mut self, pixel_ratio: f32, image: Size, viewport: Size) {
        self.scale = clamp_scale(1.0 / pixel_ratio.max(f32::MIN_POSITIVE));
        self.pan = Vec2::ZERO;
        self.mode = ZoomMode::Actual;
        self.constrain(image, viewport);
    }

    /// 在「适应窗口」与「1:1」之间切换。双击与快捷键都用它。
    ///
    /// 判据是**当前是否已经是适应窗口的倍率**，而不是 `mode`：
    /// 用户手动缩放到恰好等于适应倍率时，双击的直觉仍是「切到 1:1」。
    pub fn toggle_fit_and_actual(&mut self, pixel_ratio: f32, image: Size, viewport: Size) {
        let fits = Self::fit_scale(image, viewport)
            .map(|fit| (self.scale - fit).abs() <= fit * 0.01)
            .unwrap_or(false);

        if self.mode == ZoomMode::Fit || fits {
            self.actual_size(pixel_ratio, image, viewport);
        } else {
            self.fit(image, viewport);
        }
    }

    /// 视口尺寸变化后的处理。
    ///
    /// 只有「适应窗口」需要重算；另外两种模式的倍率是用户的明确选择，必须保持。
    /// 平移量是相对视口中心的，因此三种模式下都不需要额外补偿。
    pub fn apply_viewport_change(&mut self, image: Size, viewport: Size) {
        if self.mode == ZoomMode::Fit {
            self.fit(image, viewport);
        } else {
            self.constrain(image, viewport);
        }
    }

    /// 以 `cursor` 为锚点缩放：光标下的那个图像点必须钉在原地。
    ///
    /// 这是交互手感的核心。若改成以视口中心为锚点，每次滚轮都会把用户
    /// 正在看的细节推走，用起来会非常别扭。
    pub fn zoom_at(&mut self, cursor: Vec2, factor: f32, image: Size, viewport: Size) {
        if !cursor.is_finite() || !factor.is_finite() || factor <= 0.0 {
            return;
        }
        if image.is_empty() || viewport.is_empty() {
            return;
        }

        let anchor = self.screen_to_image(cursor, image, viewport);
        self.rescale(self.scale * factor, anchor, cursor, image, viewport);
    }

    /// 以视口中心为锚点缩放。快捷键与工具栏按钮用它（此时没有光标位置）。
    pub fn zoom_by(&mut self, factor: f32, image: Size, viewport: Size) {
        let center = Vec2::new(viewport.width / 2.0, viewport.height / 2.0);
        self.zoom_at(center, factor, image, viewport);
    }

    /// 直接设定倍率，保持图像中心不动。
    pub fn set_scale(&mut self, scale: f32, image: Size, viewport: Size) {
        let center = Vec2::new(viewport.width / 2.0, viewport.height / 2.0);
        let anchor = self.screen_to_image(center, image, viewport);
        self.rescale(scale, anchor, center, image, viewport);
    }

    /// 设定倍率，并把图像上的 `anchor` 点固定在窗口的 `keep_at` 位置。
    fn rescale(&mut self, scale: f32, anchor: Vec2, keep_at: Vec2, image: Size, viewport: Size) {
        self.scale = clamp_scale(scale);
        // 改完倍率后重新算锚点落在哪里，把差量补进平移量。
        // 这样「锚点不动」是由构造保证的，而不是靠调用方维护。
        let landed = self.image_to_screen(anchor, image, viewport);
        self.pan += keep_at - landed;
        self.mode = ZoomMode::Free;
        self.constrain(image, viewport);
    }

    /// 拖拽平移。
    pub fn pan_by(&mut self, delta: Vec2, image: Size, viewport: Size) {
        if !delta.is_finite() {
            return;
        }
        self.pan += delta;
        self.constrain(image, viewport);
    }

    /// 回到「图像居中」。不清除倍率，只归零平移。
    pub fn center(&mut self) {
        self.pan = Vec2::ZERO;
    }

    /// 把平移量夹到「图像不会被拖出视野」的范围内。
    ///
    /// 两种情况的限值其实是同一个式子 `|内容尺寸 - 视口尺寸| / 2`，
    /// 只是含义不同：
    /// - 内容比视口**大**：可以拖到某条边与视口边缘对齐，但不能继续拖走；
    /// - 内容比视口**小**：可以在视口内自由移动，但不能整张移出视野。
    ///
    /// 之所以不对「比视口小」的情况强制居中，是因为那会破坏缩放的锚点不变性 ——
    /// 用户在小图上滚轮放大时，图像会不受控地往中间跳。
    pub fn constrain(&mut self, image: Size, viewport: Size) {
        if image.is_empty() || viewport.is_empty() {
            self.pan = Vec2::ZERO;
            return;
        }
        let content = self.content_size(image);
        self.pan.x = constrain_axis(content.width, viewport.width, self.pan.x);
        self.pan.y = constrain_axis(content.height, viewport.height, self.pan.y);
    }

    /// 内容在窗口中的矩形。
    pub fn content_rect(&self, image: Size, viewport: Size) -> Rect {
        let size = self.content_size(image);
        let origin = Vec2::new(
            (viewport.width - size.width) / 2.0 + self.pan.x,
            (viewport.height - size.height) / 2.0 + self.pan.y,
        );
        Rect::new(origin, size)
    }

    pub fn content_size(&self, image: Size) -> Size {
        image.scaled(self.scale)
    }

    /// 图像坐标 → 窗口坐标。
    pub fn image_to_screen(&self, point: Vec2, image: Size, viewport: Size) -> Vec2 {
        let origin = self.content_rect(image, viewport).origin;
        Vec2::new(
            origin.x + point.x * self.scale,
            origin.y + point.y * self.scale,
        )
    }

    /// 窗口坐标 → 图像坐标。
    ///
    /// 结果可能落在图像之外（用户把光标移到画布空白处），调用方自行判断是否需要夹取。
    pub fn screen_to_image(&self, point: Vec2, image: Size, viewport: Size) -> Vec2 {
        let origin = self.content_rect(image, viewport).origin;
        let scale = self.scale.max(f32::MIN_POSITIVE);
        Vec2::new((point.x - origin.x) / scale, (point.y - origin.y) / scale)
    }
}

fn clamp_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(MIN_SCALE, MAX_SCALE)
    } else {
        1.0
    }
}

/// 单轴上的平移限值。
///
/// 内容比视口大时是「可拖动的余量」，比视口小时是「可移动的余量」，
/// 数值上都是两者之差的绝对值的一半 —— 所以这里不需要分支。
fn constrain_axis(content: f32, viewport: f32, pan: f32) -> f32 {
    let limit = (content - viewport).abs() / 2.0;
    pan.clamp(-limit, limit)
}
