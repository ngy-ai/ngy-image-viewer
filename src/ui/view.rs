//! 主视图：把文档、视图变换与渲染层接起来，并承接全部交互。
//!
//! # 状态很少，因为大部分逻辑不在这一层
//!
//! 这个视图只持有三类东西：
//!
//! 1. **文档**（[`ImageDocument`]）—— 打开的是什么；
//! 2. **视图变换**（[`ViewTransform`]）—— 现在是第几倍、平移到哪；
//! 3. **临时状态** —— 后台任务、拖拽手势、面板开合、浮层。
//!
//! 「缩放怎么算」「方向怎么叠」「像素怎么上屏」都不在这里，
//! 因此这个文件读起来就是一份交互清单，而不是一团数学。
//!
//! # 交互清单
//!
//! | 输入 | 行为 |
//! | --- | --- |
//! | 滚轮 | 以光标为锚点缩放 |
//! | 按住左键拖动 | 平移 |
//! | 双击 | 在「适应窗口」与「1:1」之间切换（带 180ms 缓动） |
//! | ESC | 关掉展开的菜单；没有菜单时退出全屏 |
//! | F11 | 切换全屏 |
//! | +/− / 0 / 1 | 放大 / 缩小 / 适应 / 1:1 |
//! | R / Shift+R | 顺时针 / 逆时针旋转 90° |
//! | H / V | 水平 / 垂直翻转 |
//! | I | 信息面板 |
//! | Ctrl+O / Ctrl+C / Ctrl+S | 打开 / 复制 / 另存为 |
//! | 标题栏菜单 | 文件 / 编辑 / 视图，全部动作与上表同一份实现 |
//! | 拖入文件 | 直接打开 |
//!
//! 上表里的每一条快捷键都由 [`crate::ui::command::KEY_BINDINGS`] 定义，
//! 菜单右侧显示的提示也取自同一张表 —— 两处不可能对不上。
//! 这也意味着**新增一个动作只需要改那一个文件**，这里只会多出一行 `match` 分支。

use std::sync::Arc;
use std::time::Instant;

use gpui_kit::*;

use crate::decode::{ImageData, orientation};
use crate::fs_ops::file_ops::{self, Bitmap};
use crate::input::{PanGesture, zoom_factor_from_lines, zoom_factor_from_pixels};
use crate::model::{ImageDocument, Size as ImageSize, Vec2, ViewTransform, ZoomMode};
use crate::open_job::{OpenTask, OpenOutcome};
use crate::perf;
use crate::render::{Surface, ViewportConfig, ViewportSlot, viewport};
use crate::trace;
use crate::ui::command::{Command, command_for_keystroke};
use crate::ui::{menu, panels, theme};

/// 浮层的类型，决定配色。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastKind {
    fn color(self) -> Hsla {
        match self {
            Self::Info => theme::info(),
            Self::Success => theme::success(),
            Self::Warning => theme::warning(),
            Self::Error => theme::danger(),
        }
    }
}

struct Toast {
    text: String,
    kind: ToastKind,
    shown_at: Instant,
}

/// 浮层停留时长。
const TOAST_LIFETIME_MS: u64 = 2000;

/// 视图当前处于哪个阶段。
enum Phase {
    /// 没有指定文件。
    Empty,
    /// 正在解码。
    Loading { name: String },
    /// 已就绪。
    Ready,
    /// 解码失败，带着可直接展示的文案。
    Failed { message: String, detail: String },
}

/// 主视图。
pub struct ImageViewerView {
    document: Option<ImageDocument>,
    surface: Option<Arc<Surface>>,
    phase: Phase,
    /// 后台打开任务。就绪或失败后置空。
    task: Option<OpenTask>,

    transform: ViewTransform,
    viewport: ViewportSlot,
    /// 上一次渲染时画布的尺寸，用来判断「窗口变了没有」。
    last_viewport: ImageSize,

    pan: PanGesture,

    info_open: bool,
    focus_handle: FocusHandle,

    /// 当前展开的菜单下标（[`crate::ui::command::MENUS`] 的索引）。
    ///
    /// 放在视图里而不是菜单组件内部：ESC 关菜单、点空白处关菜单、横向划过切换菜单
    /// 这三条路径分别落在键盘处理与浮层里，状态必须由它们共同的上级持有。
    menu: Option<usize>,

    /// 上一次同步给窗口的标题。
    ///
    /// 自绘标题栏之后，「任务栏 / Alt+Tab 里显示什么」只剩 `set_window_title` 一条通道，
    /// 而它会一路走到平台层 —— 先比一次，避免每帧都来一次跨 FFI 往返。
    window_title: Option<String>,

    /// 动画：动图当前帧与开始播放的时刻。
    frame_index: usize,
    animation_started: Instant,

    toasts: Vec<Toast>,

    /// 首帧是否已经呈现过图像（用于打点，只报一次）。
    reported_first_frame: bool,
}

impl ImageViewerView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            document: None,
            surface: None,
            phase: Phase::Empty,
            task: None,
            transform: ViewTransform::default(),
            viewport: ViewportSlot::default(),
            last_viewport: ImageSize::ZERO,
            pan: PanGesture::default(),
            info_open: false,
            focus_handle: cx.focus_handle(),
            menu: None,
            window_title: None,
            frame_index: 0,
            animation_started: Instant::now(),
            toasts: Vec::new(),
            reported_first_frame: false,
        }
    }

    /// 装载一次打开的结果。
    ///
    /// 两条路径都用它：
    /// - **启动时就已经拿到结果**（绝大多数情况）—— GPUI 的平台初始化有几百毫秒
    ///   固定开销，而常见图片在那段时间里早就解码完了。走这条路时首帧直接呈现图像，
    ///   用户看不到任何占位与跳变，这是「双击即见图」的关键；
    /// - 后台任务完成时由轮询线程回调。
    pub fn apply(&mut self, outcome: OpenOutcome) {
        self.accept(outcome);
    }

    /// 装载一个后台打开任务。结果会在后续渲染中被轮询取走。
    pub fn attach_task(&mut self, path: &std::path::Path, task: OpenTask) {
        trace::step(
            "view",
            format!("挂载后台任务：{}（进入「正在打开」）", path.display()),
        );
        self.phase = Phase::Loading {
            name: file_name_of(path),
        };
        self.task = Some(task);
    }

    fn accept(&mut self, outcome: OpenOutcome) {
        trace::step("view", format!("收到打开结果：{}", outcome.log_line()));
        match ImageDocument::from_outcome(outcome) {
            Ok(document) => {
                match Surface::build(&document) {
                    Ok(surface) => {
                        self.transform = ViewTransform::default();
                        self.transform.fit(document.logical_size(), self.last_viewport);
                        self.document = Some(document);
                        self.surface = Some(Arc::new(surface));
                        self.phase = Phase::Ready;
                        self.pan.end();
                        self.frame_index = 0;
                        self.animation_started = Instant::now();
                        trace::step(
                            "view",
                            format!(
                                "进入 Ready：初始倍率={:.4} 平移=({:.2},{:.2}) 画布={:.2}×{:.2}（画布为 0 时下一帧会重算适应窗口）",
                                self.transform.scale(),
                                self.transform.pan().x,
                                self.transform.pan().y,
                                self.last_viewport.width,
                                self.last_viewport.height,
                            ),
                        );
                    }
                    Err(error) => {
                        self.document = None;
                        self.surface = None;
                        self.phase = Phase::Failed {
                            message: error.user_message(),
                            detail: error.short_reason(),
                        };
                        trace::fail(
                            "view",
                            format!("纹理构建失败，界面转为失败态：{}", error.short_reason()),
                        );
                    }
                }
            }
            Err(error) => {
                self.document = None;
                self.surface = None;
                self.phase = Phase::Failed {
                    message: error.user_message(),
                    detail: error.short_reason(),
                };
                trace::fail(
                    "view",
                    format!("文档构建失败，界面转为失败态：{}", error.short_reason()),
                );
            }
        }
    }

    /// 每次渲染前把后台任务的结果取回来。
    ///
    /// 这是任务结果**唯一**的出口：调用点只有渲染一处，因此「加载中 → 就绪」这段时序
    /// 不会散落在几个地方。（启动骨架那边曾经另有一份定时轮询，已经删掉 ——
    /// 两条驱动路径并存时，最容易出问题的就是其中一条慢慢失效而没人发现。）
    fn pump_task(&mut self) {
        let Some(task) = self.task.as_mut() else {
            return;
        };
        let Some(outcome) = task.poll() else {
            return;
        };
        self.task = None;
        self.accept(outcome);
    }

    // ---- 供面板调用的动作 ----

    /// 「适应窗口」的等价变换，用于工具栏高亮判断。
    fn fits(&self) -> bool {
        let Some(document) = self.document.as_ref() else {
            return true;
        };
        match ViewTransform::fit_scale(document.logical_size(), self.last_viewport) {
            Some(fit) => (self.transform.scale() - fit).abs() <= fit * 0.01,
            None => true,
        }
    }

    fn zoom_percent(&self, window: &Window) -> f32 {
        self.transform.zoom_percent(pixel_ratio_of(window))
    }

    fn with_image<F>(&mut self, action: F)
    where
        F: FnOnce(&mut Self, ImageSize, ImageSize),
    {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let logical = document.logical_size();
        let viewport = self.last_viewport;
        action(self, logical, viewport);
    }

    pub fn set_fit_mode(&mut self, _cx: &mut Context<Self>) {
        self.with_image(|this, logical, viewport| this.transform.fit(logical, viewport));
    }

    pub fn set_actual_mode(&mut self, _cx: &mut Context<Self>) {
        self.with_image(|this, logical, viewport| {
            this.transform.actual_size(1.0, logical, viewport)
        });
    }

    pub fn zoom_in(&mut self, _cx: &mut Context<Self>) {
        self.with_image(|this, logical, viewport| {
            this.transform.zoom_by(crate::model::transform::ZOOM_STEP, logical, viewport)
        });
    }

    pub fn zoom_out(&mut self, _cx: &mut Context<Self>) {
        self.with_image(|this, logical, viewport| {
            this.transform
                .zoom_by(1.0 / crate::model::transform::ZOOM_STEP, logical, viewport)
        });
    }

    pub fn toggle_info_panel(&mut self, _cx: &mut Context<Self>) {
        self.info_open = !self.info_open;
    }

    // ---- 菜单 ----

    /// 点菜单标签：展开，或收起已经展开的那一个。
    pub fn toggle_menu(&mut self, index: usize, cx: &mut Context<Self>) {
        self.menu = if self.menu == Some(index) {
            None
        } else {
            Some(index)
        };
        cx.notify();
    }

    /// 鼠标划过菜单标签：只有在已经有菜单展开时才切换。
    ///
    /// 「开着菜单时横向划过即切换」是菜单的既定习惯；反过来（没展开时划过就展开）
    /// 会让鼠标一经过标题栏就弹出一堆面板。
    pub fn hover_menu(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.menu.is_some() && self.menu != Some(index) {
            self.menu = Some(index);
            cx.notify();
        }
    }

    pub fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        }
    }

    /// 执行一个动作。菜单项与键盘快捷键共用这一条路径。
    ///
    /// 两条入口共用而不是各写一份，是为了让「菜单里能点、快捷键按不动」这类
    /// 不一致从一开始就不可能出现。
    pub fn run_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        // 菜单的既定语义：点完一项就收起。键盘触发时它本来就是收起的。
        self.menu = None;

        // 没有打开图片时，作用在图像上的动作无处施加。菜单里这些项已经是禁用态，
        // 这里再挡一次是给键盘入口准备的同一个判断。
        if command.needs_image() && self.document.is_none() {
            // 上面那行已经可能把菜单收起来了，所以这里仍然要重绘一次。
            cx.notify();
            return;
        }

        match command {
            Command::Open => self.open_dialog(cx),
            Command::SaveAs => self.save_as(cx),
            Command::Rename => self.rename(cx),
            Command::DeleteToTrash => self.delete_to_trash(cx),
            Command::CopyToClipboard => self.copy_to_clipboard(cx),
            Command::FitToWindow => self.set_fit_mode(cx),
            Command::ActualSize => self.set_actual_mode(cx),
            Command::ZoomIn => self.zoom_in(cx),
            Command::ZoomOut => self.zoom_out(cx),
            Command::RotateClockwise => self.rotate_clockwise(cx),
            Command::RotateCounterClockwise => self.rotate_counter_clockwise(cx),
            Command::FlipHorizontal => self.flip_horizontal(cx),
            Command::FlipVertical => self.flip_vertical(cx),
            Command::ToggleInfoPanel => self.toggle_info_panel(cx),
            Command::ToggleFullscreen => window.toggle_fullscreen(),
            // 退出整个应用而不是关掉这个窗口：本程序只有这一个窗口，
            // 两者在当前实现下等价，但「退出」是用户按下菜单项时的心智模型。
            Command::Quit => cx.quit(),
        }

        cx.notify();
    }

    /// 「打开…」：走系统文件对话框，再复用「拖入文件」那条路径。
    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        // `None` 表示用户取消 —— 取消不是错误，什么都不做即可。
        let Some(path) = file_ops::pick_open_path() else {
            return;
        };
        self.open_path(path, cx);
    }

    /// 把「现在在看什么」同步给窗口标题。
    ///
    /// 自绘标题栏之后系统标题栏没有了，任务栏与 Alt+Tab 列表里的名字就只剩这一条通道，
    /// 所以它必须在**文档变化后**立刻更新，而不是只在启动时设一次。
    fn sync_window_title(&mut self, window: &mut Window) {
        // 与标题栏中间显示的是同一段文字：两处若各写一份格式，
        // 迟早会出现「标题栏写着 A、任务栏写着 B」这种没人会去比对的不一致。
        let title = menu::title_text(self.document.as_ref());
        if self.window_title.as_deref() == Some(title.as_str()) {
            return;
        }
        window.set_window_title(&title);
        self.window_title = Some(title);
    }

    fn rotate(&mut self, clockwise: bool) {
        let Some(document) = self.document.as_mut() else {
            return;
        };
        if clockwise {
            document.rotate_clockwise();
        } else {
            document.rotate_counter_clockwise();
        }
        self.rebuild_surface();
        // 旋转会交换宽高，原来的平移量已经没有意义了；重新适应一次，
        // 让用户立刻看到完整的图，而不是一张被裁掉一半的图。
        self.refit();
    }

    pub fn rotate_clockwise(&mut self, _cx: &mut Context<Self>) {
        self.rotate(true);
    }

    pub fn rotate_counter_clockwise(&mut self, _cx: &mut Context<Self>) {
        self.rotate(false);
    }

    pub fn flip_horizontal(&mut self, _cx: &mut Context<Self>) {
        if let Some(document) = self.document.as_mut() {
            document.flip_horizontal();
        }
        self.rebuild_surface();
    }

    pub fn flip_vertical(&mut self, _cx: &mut Context<Self>) {
        if let Some(document) = self.document.as_mut() {
            document.flip_vertical();
        }
        self.rebuild_surface();
    }

    /// 按当前方向重建纹理。
    fn rebuild_surface(&mut self) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        match Surface::build(document) {
            Ok(surface) => self.surface = Some(Arc::new(surface)),
            Err(error) => self.push_toast(error.user_message(), ToastKind::Error),
        }
    }

    fn refit(&mut self) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        self.transform.fit(document.logical_size(), self.last_viewport);
    }

    /// 当前显示的像素（方向已应用），非预乘 RGBA8。
    ///
    /// 剪贴板与另存为都要的是「屏幕上看到的那张图」，
    /// 而不是文件里存的原始朝向 —— 用户旋转过之后另存，期待的就是旋转后的结果。
    fn displayed_pixels(&self) -> Option<(u32, u32, Vec<u8>)> {
        let document = self.document.as_ref()?;
        let frame = document.primary();
        let orientation = document.orientation();

        match orientation::apply(frame.width, frame.height, &frame.rgba8, orientation) {
            Some(result) => Some(result),
            // `None` 表示方向为「无需纠正」，直接用原数据。
            None => Some((frame.width, frame.height, frame.rgba8.clone())),
        }
    }

    pub fn copy_to_clipboard(&mut self, _cx: &mut Context<Self>) {
        let Some((width, height, pixels)) = self.displayed_pixels() else {
            return;
        };
        let bitmap = Bitmap {
            width,
            height,
            rgba8: &pixels,
        };
        match file_ops::copy_to_clipboard(&bitmap) {
            Ok(()) => self.push_toast("已复制到剪贴板", ToastKind::Success),
            Err(error) => self.push_toast(error.user_message(), ToastKind::Error),
        }
    }

    pub fn save_as(&mut self, _cx: &mut Context<Self>) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let (suggested, _) = file_ops::suggest_save_name(document.path(), document.format());
        let directory = document.path().parent().map(|path| path.to_path_buf());
        let Some(destination) = file_ops::pick_save_path(&suggested, directory.as_deref()) else {
            return;
        };

        let Some((width, height, pixels)) = self.displayed_pixels() else {
            return;
        };
        let bitmap = Bitmap {
            width,
            height,
            rgba8: &pixels,
        };
        match file_ops::save_bitmap(&bitmap, &destination) {
            Ok(()) => self.push_toast(
                format!("已另存为 {}", destination.display()),
                ToastKind::Success,
            ),
            Err(error) => self.push_toast(error.user_message(), ToastKind::Error),
        }
    }

    pub fn rename(&mut self, _cx: &mut Context<Self>) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let current = document.path().to_path_buf();
        let Some(destination) = file_ops::pick_rename_path(&current) else {
            return;
        };
        match file_ops::rename(&current, &destination) {
            Ok(()) => {
                // 重命名后必须让文档指向新路径，否则「再点一次重命名」
                // 会去操作一个已经不存在的老路径。
                if let Some(document) = self.document.as_mut() {
                    document.replace_path(destination.clone());
                }
                self.push_toast(format!("已重命名为 {}", file_name_of(&destination)), ToastKind::Success);
            }
            Err(error) => self.push_toast(error.user_message(), ToastKind::Error),
        }
    }

    pub fn delete_to_trash(&mut self, _cx: &mut Context<Self>) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let path = document.path().to_path_buf();
        match file_ops::delete_to_trash(&path) {
            Ok(()) => {
                // 文件已经进了回收站，继续显示它只会让下一步操作全部失败。
                // 清空到空状态，并明确告诉用户去了哪里。
                self.document = None;
                self.surface = None;
                self.phase = Phase::Empty;
                self.push_toast("已移入回收站（可从回收站还原）", ToastKind::Success);
            }
            Err(error) => self.push_toast(error.user_message(), ToastKind::Error),
        }
    }

    pub fn open_path(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        trace::step("view", format!("打开路径：{}", path.display()));
        // 先建任务再借用路径：`OpenTask::spawn` 会拿走所有权，
        // 而 `attach_task` 需要路径来显示「正在打开 xxx」。
        let task = OpenTask::spawn(path.clone());
        self.attach_task(&path, task);
        cx.notify();
    }

    fn push_toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toasts.push(Toast {
            text: text.into(),
            kind,
            shown_at: Instant::now(),
        });
    }

    /// 取当前仍应显示的浮层，并顺手清掉过期的。
    fn live_toasts(&mut self) -> &[Toast] {
        let now = Instant::now();
        self.toasts.retain(|toast| {
            let shown_for = now.duration_since(toast.shown_at).as_millis() as u64;
            shown_for < TOAST_LIFETIME_MS
        });
        &self.toasts
    }

    /// 动图播放：按已播时间算出当前帧。
    fn advance_animation(&mut self) {
        let Some(surface) = self.surface.as_ref() else {
            return;
        };
        if !surface.is_animated() {
            self.frame_index = 0;
            return;
        }
        let elapsed = self.animation_started.elapsed().as_millis() as u64;
        self.frame_index = surface.frame_index_at(elapsed);
    }

    /// 是否需要继续请求下一帧。
    ///
    /// 除了动图与浮层寿命，**后台任务未完成时也必须持续请求帧**：任务结果只能在
    /// [`Self::pump_task`] 里被取回，而那要等下一次渲染；没有人会替我们安排下一帧。
    /// 漏掉这一条的表现是「拖进来一张大图，界面一直停在『正在打开』」——
    /// 直到用户碰一下鼠标（触发重绘）才突然显示出来，看起来像是随机抽风。
    ///
    /// 首帧的快速路径（启动时就已解码完）不受影响：那时根本没有任务，一帧都不会多请求。
    /// 这也是本函数可以无条件跟随重绘节奏的原因 —— 它只在真的有事要做时才为真。
    fn needs_animation_frame(&mut self) -> bool {
        let animating = self
            .surface
            .as_ref()
            .map(|surface| surface.is_animated())
            .unwrap_or(false);
        animating || !self.toasts.is_empty() || self.task.is_some()
    }

    // ---- 交互 ----

    /// 滚轮缩放：以光标为锚点。
    fn on_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let factor = match event.delta {
            ScrollDelta::Lines(point) => zoom_factor_from_lines(point.y),
            ScrollDelta::Pixels(point) => zoom_factor_from_pixels(f32::from(point.y)),
        };

        let cursor = Vec2::new(f32::from(event.position.x), f32::from(event.position.y));
        // 事件坐标是相对窗口的，画布从标题栏 + 工具栏下方开始，这里换算到画布局部坐标。
        // 少减一项的表现是缩放锚点整体偏移一个标题栏的高度 —— 用户说不出哪里不对，
        // 只会觉得"缩放时图像在往一边跑"。
        let cursor = Vec2::new(
            cursor.x,
            cursor.y - theme::TITLE_BAR_HEIGHT - theme::TOOLBAR_HEIGHT,
        );

        self.with_image(|this, logical, viewport| {
            this.transform.zoom_at(cursor, factor, logical, viewport);
        });
        cx.notify();
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 双击在「适应窗口」与「1:1」之间切换。
        // 用 `click_count` 而不是自己数时间：平台已经处理了双击间隔与位移容差，
        // 自己数只会在触摸板与高延迟鼠标上时灵时不灵。
        if event.click_count >= 2 {
            self.pan.end();
            let pixel_ratio = pixel_ratio_of(window);
            self.with_image(|this, logical, viewport| {
                this.transform
                    .toggle_fit_and_actual(pixel_ratio, logical, viewport);
            });
            cx.notify();
            return;
        }

        self.pan
            .begin(Vec2::new(f32::from(event.position.x), f32::from(event.position.y)));
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(delta) = self
            .pan
            .update(Vec2::new(f32::from(event.position.x), f32::from(event.position.y)))
        else {
            return;
        };
        self.with_image(|this, logical, viewport| {
            this.transform.pan_by(delta, logical, viewport);
        });
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();

        // ESC 有两条含义，取决于菜单是否展开：先收起菜单，再谈退出全屏。
        // 顺序反过来的话，开着菜单按 ESC 会连带退出全屏，用户会以为按错了键。
        if key == "escape" {
            if self.menu.is_some() {
                self.close_menu(cx);
            } else {
                window.toggle_fullscreen();
                cx.notify();
            }
            return;
        }

        // 按键到动作的映射只有一份，在 `command` 模块里，菜单上的提示也从那里取。
        // 这里只负责把平台给的修饰键状态翻译成三个布尔值。
        let control = event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
        let shift = event.keystroke.modifiers.shift;

        if let Some(command) = command_for_keystroke(key, control, shift) {
            self.run_command(command, window, cx);
        }
    }

    fn on_file_drop(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        // 一次拖入多个文件时不追问「要开哪个」——那会打断「拖进来就想看」的直觉。
        // 取第一个支持的即可，其余忽略。
        let Some(path) = paths.0.iter().next().cloned() else {
            return;
        };
        self.open_path(path, cx);
    }

    /// 占位层：加载中、失败与空状态都走这里。
    fn placeholder(&self, window: &Window) -> AnyElement {
        let (headline, detail, accent) = match &self.phase {
            Phase::Empty => (
                "把图片拖进来，或从文件管理器双击一张图".to_string(),
                format!("也可以执行 {} <图片路径>", env_program_name()),
                theme::text_muted(),
            ),
            Phase::Loading { name } => {
                let elapsed = self
                    .task
                    .as_ref()
                    .map(|task| task.elapsed_ms())
                    .unwrap_or_default();
                (
                    format!("正在打开 {name}"),
                    format!("已用时 {elapsed:.0} ms"),
                    theme::info(),
                )
            }
            Phase::Failed { message, detail } => (message.clone(), detail.clone(), theme::danger()),
            Phase::Ready => return div().into_any_element(),
        };

        let ratio = pixel_ratio_of(window);

        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .absolute()
            .inset_0()
            .child(
                div()
                    .text_color(accent)
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(headline),
            )
            .child(
                div()
                    .max_w(px(520.0))
                    .text_center()
                    .text_color(theme::text_muted())
                    .text_size(px(12.0))
                    .child(detail),
            )
            .child(
                div()
                    .text_color(theme::text_faint())
                    .text_size(px(10.0))
                    .child(format!("屏幕缩放 {ratio:.2}×")),
            )
            .into_any_element()
    }

    /// 浮层：底部居中，2 秒后自动淡出。
    fn toast_layer(&mut self) -> AnyElement {
        let toasts = self.live_toasts();
        let Some(latest) = toasts.last() else {
            return div().into_any_element();
        };

        div()
            .absolute()
            .bottom(px(48.0))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(
                div()
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(theme::surface_active())
                    .border_1()
                    .border_color(latest.kind.color())
                    .text_color(theme::text())
                    .text_size(px(12.0))
                    .child(latest.text.clone()),
            )
            .into_any_element()
    }

    /// 画布区块：图像 + 占位层 + 浮层。
    fn canvas_area(&mut self, window: &Window) -> AnyElement {
        let logical = self
            .document
            .as_ref()
            .map(|document| document.logical_size())
            .unwrap_or(ImageSize::ZERO);

        let placeholder = self.placeholder(window);
        let toast = self.toast_layer();

        let mut area = div()
            .flex_1()
            .relative()
            .overflow_hidden()
            .bg(theme::canvas())
            .child(viewport(ViewportConfig {
                surface: self.surface.clone(),
                logical_size: logical,
                transform: self.transform,
                frame_index: self.frame_index,
                slot: self.viewport.clone(),
                background: theme::canvas(),
                checker: theme::checkerboard(),
            }))
            .child(placeholder);

        if self.info_open {
            area = area.child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .child(panels::info_panel(self.document.as_ref())),
            );
        }

        area.child(toast).into_any_element()
    }
}

impl Render for ImageViewerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 1. 先把后台结果收回来 —— 这一帧就能显示图像，而不是等到下一帧。
        self.pump_task();

        // 2. 画布尺寸可能刚刚变化（首帧时它还是零），据此重算「适应窗口」。
        let measured = self.viewport.get();
        if measured != self.last_viewport && !measured.is_empty() {
            self.last_viewport = measured;
            if let Some(document) = self.document.as_ref() {
                self.transform
                    .apply_viewport_change(document.logical_size(), measured);
            }
            trace::step(
                "view",
                format!(
                    "画布尺寸变化 → {:.2}×{:.2}，重算变换：模式={:?} 倍率={:.4}",
                    measured.width,
                    measured.height,
                    self.transform.mode(),
                    self.transform.scale(),
                ),
            );
        }

        // 2b. 状态自检：Ready 却缺文档或纹理，是「黑屏但没有任何报错」的另一种形态。
        //     正常路径走不到这里，所以一旦出现就必须留痕。
        if matches!(self.phase, Phase::Ready) && (self.document.is_none() || self.surface.is_none()) {
            trace::fail(
                "view",
                format!(
                    "状态不一致：处于 Ready，但 文档={} 纹理={} —— 画布会画不出内容",
                    self.document.is_some(),
                    self.surface.is_some(),
                ),
            );
        }

        // 3. 动画推进（动图帧、浮层寿命）。
        self.advance_animation();

        // 4. 打点：首帧就带着图像是「双击即见图」的直接量化指标。
        if !self.reported_first_frame && matches!(self.phase, Phase::Ready) {
            self.reported_first_frame = true;
            perf::mark("first_frame_with_image");
            trace::step(
                "view",
                format!(
                    "首帧带图：纹理={} 帧号={} 倍率={:.4} 画布={:.2}×{:.2}",
                    self.surface.is_some(),
                    self.frame_index,
                    self.transform.scale(),
                    self.last_viewport.width,
                    self.last_viewport.height,
                ),
            );
        }

        // 5. 标题跟着当前文档走（任务栏 / Alt+Tab 里显示的名字）。
        self.sync_window_title(window);

        // 键盘事件要先有焦点才会派发到这一层。窗口里只有这一个视图，
        // 主动取一次焦点是最稳妥的做法。
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle, cx);
        }

        if self.needs_animation_frame() {
            window.request_animation_frame();
        }

        // 浮层要用 `&mut self`，而工具栏只要 `&self`；先取完不可变的读数，
        // 避免在同一段代码里同时借可变与不可变。
        let zoom_percent = self.zoom_percent(window);
        let fits = self.fits();
        let view_handle = cx.entity();

        // 先构建画布区（需要 `&mut self`），再构建其它区块（只读 `&self`）。
        // 顺序反过来的话，工具栏持有的不可变借用会与画布区的可变借用冲突。
        let area = self.canvas_area(window);

        let title_bar = menu::title_bar(self.document.as_ref(), self.menu, &view_handle, window);
        let toolbar = panels::toolbar(
            self.document.as_ref(),
            zoom_percent,
            fits,
            self.info_open,
            &view_handle,
        );
        let status = panels::status_bar(self.document.as_ref(), zoom_percent);
        let menu_layer = menu::menu_layer(self.menu, self.document.is_some(), &view_handle);

        div()
            .flex()
            .flex_col()
            .size_full()
            // `relative` 是下拉浮层的前提：它用绝对定位挂在根容器上，
            // 需要一个明确的包含块，否则会退化成相对窗口定位。
            .relative()
            .bg(theme::canvas())
            .text_color(theme::text())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.on_file_drop(paths, cx)
            }))
            .child(title_bar)
            .child(toolbar)
            .child(
                // 这一层**必须**是 flex 容器，否则里面的 `area` 高度会塌成 0。
                //
                // `area` 用 `flex_1()` 撑满，而 `flex-grow` 只在 flex 容器里生效。
                // 缺了 `.flex()` 时这一层退化成普通块级容器，`area` 的高度就由内容决定：
                // 它唯一的子元素是绝对定位的画布（不占空间），于是高度算出来是 0，
                // 画布随之变成 `宽 × 0`。`paint_image` 在可见区域为空时返回的是
                // `Ok(())`，所以整个过程**没有任何报错**，界面只是一片黑。
                //
                // 之所以不改成给 `area` 加 `size_full()`：那依赖父级高度已经确定，
                // 而这里父级的高度正是由 `flex_1` 决定的 —— 显式声明 flex 容器
                // 才让「谁分配空间、谁撑满」这条链路是可读的。
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .relative()
                    .on_scroll_wheel(cx.listener(|this, event, _window, cx| {
                        this.on_scroll(event, cx)
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event, window, cx| {
                            this.on_mouse_down(event, window, cx)
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event, _window, cx| {
                        this.on_mouse_move(event, cx)
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _event, _window, _cx| this.pan.end()),
                    )
                    .child(area),
            )
            .child(status)
            // 下拉浮层必须是最后一个孩子：GPUI 按树序绘制，后画的盖住先画的。
            // 放在标题栏里（它的逻辑归属处）会被后画的画布整块盖住。
            .child(menu_layer)
    }
}

/// 从窗口取设备像素比（HiDPI 缩放）。
///
/// 「1:1」与缩放百分比都要用它：在 200% 的屏幕上，一个逻辑点等于两个设备像素，
/// 不换算的话「100%」会显示成两倍大小。
fn pixel_ratio_of(window: &Window) -> f32 {
    let ratio = f32::from(window.scale_factor());
    if ratio.is_finite() && ratio > 0.0 {
        ratio
    } else {
        1.0
    }
}

fn file_name_of(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn env_program_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "ngy-image-viewer".to_string())
}

/// 让 `ImageData` 与 `ZoomMode` 的导入在类型推导中有落点。
#[allow(dead_code)]
fn type_anchors(data: &ImageData, mode: ZoomMode) -> (usize, ZoomMode) {
    (data.frame_count(), mode)
}
