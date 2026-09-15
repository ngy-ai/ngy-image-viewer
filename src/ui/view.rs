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
//! | F11 | 切换全屏（全屏时标题栏 / 工具栏 / 状态栏一概不画，只剩图像） |
//! | +/− / 0 / 1 | 放大 / 缩小 / 适应 / 1:1 |
//! | R / Shift+R | 顺时针 / 逆时针旋转 90° |
//! | H / V | 水平 / 垂直翻转 |
//! | I | 信息面板 |
//! | ↑ / ↓ | 同目录里的上一个 / 下一个图片（目录在后台异步读取） |
//! | Ctrl+O / Ctrl+C / Ctrl+S | 打开 / 复制 / 另存为 |
//! | 标题栏菜单 | 文件 / 编辑 / 视图，全部动作与上表同一份实现 |
//! | 拖入文件 | 直接打开 |
//!
//! 上表里的每一条快捷键都由 [`crate::ui::command::KEY_BINDINGS`] 定义，
//! 菜单右侧显示的提示也取自同一张表 —— 两处不可能对不上。
//! 这也意味着**新增一个动作只需要改那一个文件**，这里只会多出一行 `match` 分支。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use gpui_kit::*;

use crate::decode::{ImageData, orientation};
use crate::fs_ops::Preference;
use crate::fs_ops::file_ops::{self, Bitmap};
use crate::fs_ops::neighbors::NeighborsTask;
use crate::input::{PanGesture, zoom_factor_from_lines, zoom_factor_from_pixels};
use crate::model::{ImageDocument, Size as ImageSize, Vec2, ViewTransform, ZoomMode};
use crate::open_job::{OpenTask, OpenOutcome};
use crate::perf;
use crate::render::{Surface, ViewportConfig, ViewportSlot, viewport};
use crate::trace;
use crate::ui::command::{Command, command_for_keystroke};
use crate::ui::theme::{self, Skin};
use crate::ui::{menu, panels};

/// 浮层的类型，决定配色。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastKind {
    fn color(self, skin: &Skin) -> Hsla {
        match self {
            Self::Info => skin.info,
            Self::Success => skin.success,
            Self::Warning => skin.warning,
            Self::Error => skin.danger,
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

/// 全屏切换后，最多再持续请求几帧。
///
/// Windows 上 `toggle_fullscreen` 是**异步投递**的（`gpui-pre-windows` 里走
/// `executor.spawn`），平台的全屏标志不会在调用的当下翻面。界面完全由那个标志
/// 驱动，所以这段时间必须保持重绘，否则切换会停在旧样子上 —— 直到用户碰一下
/// 鼠标触发重绘才突然变，表现为「按了 F11 但界面过了一会儿才动」。
///
/// 3 帧足够跨过一次事件循环；它不是对延迟的预估，而是「万一平台压根没切、
/// 或本平台不支持全屏」时的兜底：数到零就停，界面如实反映平台状态，
/// 不会为了等一个永远不会来的状态而空转。
const FULLSCREEN_SETTLE_FRAMES: u8 = 3;

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

    /// 同目录邻居（上一个 / 下一个图片）。由 `neighbors_task` 在后台算出。
    neighbors: Option<crate::fs_ops::Neighbors>,
    /// 后台目录扫描任务。就绪后置空。
    ///
    /// 与解码一样在独立线程上跑：一个装了几万张图的文件夹可以慢到上百毫秒，
    /// 绝不能让它在「双击即见图」的关键路径上。首帧不等它，↑/↓ 在它就绪前
    /// 按下只会得到一句「正在读取目录」的提示。
    neighbors_task: Option<NeighborsTask>,

    /// 邻居的预解码结果：路径 → 已解码（尚未上屏）的 `OpenOutcome`。
    ///
    /// 切换时若目标路径命中，直接 `accept`，跳过最慢的「读文件 + 解码」，
    /// 只剩纹理上传，几乎是瞬时切换。键用路径，是因为命中判断拿的就是
    /// 目标文件的路径。
    preloaded: HashMap<PathBuf, OpenOutcome>,
    /// 进行中的邻居预解码任务（上一张 / 下一张）。`neighbors` 就绪后派生，
    /// 完成后由 `pump_preloads` 把结果收进 `preloaded`。文档一变就整体作废，
    /// 不该让上一张图的预解码结果被错当成下一张图的邻居。
    preload_tasks: Option<(Option<OpenTask>, Option<OpenTask>)>,

    /// 正在等待用户通过系统对话框（打开 / 另存为 / 重命名）做选择的接收端。
    ///
    /// 系统对话框在自己的线程上跑模态循环，主线程只在这里存下接收端、继续泵
    /// GPUI 事件循环；结果由 `pump_dialog` 在渲染循环里取回。这样对话框打开期间
    /// 把焦点切回主窗口，窗口照常响应，不会被系统标成「未响应」。
    pending_dialog: Option<PendingDialog>,

    transform: ViewTransform,
    viewport: ViewportSlot,
    /// 上一次渲染时画布的尺寸，用来判断「窗口变了没有」。
    last_viewport: ImageSize,
    /// 刚打开一张图，但画布尺寸还没测到，初始视图尚未定下来。
    ///
    /// 初始视图要在「1:1」与「适应窗口」之间二选一，判据需要视口尺寸。冷启动时
    /// 打开结果早于首帧绘制（画布尺寸还是 0），只能先按 1:1 摆好并挂起这一位，
    /// 等下一帧读到真实尺寸再定（见 `apply_initial_view`）。
    initial_view_pending: bool,

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

    /// 启动时就带着图片路径（即文件管理器里双击了这张图）。
    ///
    /// 它只回答一个问题：**打开图片之后要不要把界面收起来**。
    ///
    /// - 带路径启动 —— 用户点的是「一张图」，窗口就该是那张图：标题栏不画菜单、
    ///   底部不画状态栏，纵向空间全给图像；
    /// - 不带路径启动 —— 用户是「先开程序，再选图」。这时界面是完整的，
    ///   选完图也必须保持完整：凭空少掉两栏会像是程序出了错，而想要全屏
    ///   本来就有 F11 这一条明路，不必替他做主。
    ///
    /// 启动那一刻定死，之后不再改变：它描述的是「这次会话是怎么开始的」，
    /// 不是一个随当前文档漂移的状态 —— 否则「双击打开后按 Ctrl+O 换一张」
    /// 会让界面在两种形态之间跳一下。
    immersive: bool,

    /// 窗口当前是否全屏。**每帧从平台读一次**（见 `refresh_fullscreen`），
    /// 不在切换的那一刻就地翻转：自持的那份会在异步切换落地前与平台打架。
    fullscreen: bool,
    /// 刚请求过全屏切换、平台状态还没翻面时，剩下的持续重绘帧数。
    fullscreen_pending: u8,

    /// 首帧是否已经呈现过图像（用于打点，只报一次）。
    reported_first_frame: bool,

    /// 用户对皮肤的偏好（跟随系统 / 固定深色 / 固定浅色）。
    ///
    /// 它**不是**「当前用的是哪套皮肤」—— 后者每帧由 [`current_skin`](Self::current_skin)
    /// 从「这个偏好 + 平台当下报的外观」现算。理由与 `fullscreen` 完全相同：
    /// 系统外观是平台状态，自己缓存一份就会在「用户改了系统主题」与「平台把新值
    /// 送过来」之间那段时间里与之打架，表现是界面停在旧配色上直到用户碰一下鼠标。
    ///
    /// 手动选了深色之后，这个偏好在本次会话里就一直压着系统值，
    /// 直到用户重新选「跟随系统」。
    preference: Preference,
}

/// 正在等待用户通过系统对话框做选择的一项。
///
/// 系统对话框（打开 / 另存为 / 重命名）在自己的线程上跑模态循环，主线程通过
/// 轮询接收端取结果 —— 因此主线程在对话框打开期间始终在泵 GPUI 消息循环，
/// 切回主窗口也不会被系统标记为「未响应」。
enum PendingDialog {
    /// 「打开」：结果直接走 `open_path`。
    Open(Receiver<Option<PathBuf>>),
    /// 「另存为」：结果出来时把当前文档编码保存到该路径。
    SaveAs(Receiver<Option<PathBuf>>),
    /// 「重命名」：必须记住「被改名的源文件」是谁，否则改名会去动一个
    /// 可能已经不存在的老路径。
    Rename {
        current: PathBuf,
        rx: Receiver<Option<PathBuf>>,
    },
}

impl ImageViewerView {
    /// `immersive`：启动时命令行是否带了图片路径（文件管理器双击）。含义见字段文档。
    pub fn new(cx: &mut Context<Self>, immersive: bool) -> Self {
        Self {
            document: None,
            surface: None,
            phase: Phase::Empty,
            task: None,
            neighbors: None,
            neighbors_task: None,
            preloaded: HashMap::new(),
            preload_tasks: None,
            pending_dialog: None,
            transform: ViewTransform::default(),
            viewport: ViewportSlot::default(),
            last_viewport: ImageSize::ZERO,
            initial_view_pending: false,
            pan: PanGesture::default(),
            info_open: false,
            focus_handle: cx.focus_handle(),
            menu: None,
            window_title: None,
            frame_index: 0,
            animation_started: Instant::now(),
            toasts: Vec::new(),
            immersive,
            fullscreen: false,
            fullscreen_pending: 0,
            reported_first_frame: false,
            // 默认「跟随系统」。真正的偏好在建窗之前由 `app.rs` 读一次，
            // 经 `set_loaded_preference` 送进来 —— 这里再读一次盘是多余的 IO，
            // 而且会给「视图的偏好」与「平台已经切过去的外观」制造一个可能不一致的
            // 中间态。默认值必须与 `Preference::default()` 一致。
            preference: Preference::default(),
        }
    }

    /// 覆盖从磁盘读来的偏好。
    ///
    /// 「读配置」这件事必须在**建窗之前**发生：视图构造时窗口还不存在，
    /// 而平台需要知道要不要把外观覆盖成用户选的那一套，否则窗口会先以系统配色
    /// 呈现一帧再跳成用户选的配色。`app.rs` 读一次交给这里，视图不再读第二次。
    pub fn set_loaded_preference(&mut self, preference: Preference) {
        self.preference = preference;
    }

    /// 用户当前的皮肤偏好（供 `app.rs` 决定要不要给平台设外观覆盖）。
    pub fn preference(&self) -> Preference {
        self.preference
    }

    /// 当下该用哪套皮肤。
    ///
    /// **每帧现算，不缓存**：`system` 由调用方从窗口（或 `App`）读来，
    /// 因此「系统切了主题」与「用户改了偏好」两条路径都能立刻反映到画面上，
    /// 不需要任何一个自持的「现在是深色还是浅色」布尔。
    fn current_skin(&self, system: WindowAppearance) -> &'static Skin {
        self.preference.skin_for(theme::Polarity::of(system))
    }

    /// 换一套皮肤偏好：写盘、通知平台、请求重绘。
    ///
    /// 三件事都在这里做，因为三个入口（菜单三项）共用它，
    /// 漏掉任何一步都会表现为「点了没反应」或「这次有效下次忘了」。
    fn set_preference(&mut self, preference: Preference, cx: &mut Context<Self>) {
        if self.preference == preference {
            // 重复点同一项：什么都不做。既避免多余的写盘，
            // 也避免给平台反复设同一个覆盖值（那会让窗口再走一次 DWM 重配）。
            return;
        }
        self.preference = preference;
        trace::step(
            "theme",
            format!(
                "皮肤偏好 → {}（{}）",
                match preference {
                    Preference::System => "跟随系统",
                    Preference::Fixed(polarity) => polarity.label(),
                },
                if preference.is_overriding_system() {
                    "不再读取系统外观"
                } else {
                    "读系统外观"
                },
            ),
        );
        // 写盘放到后台线程：一次 `write` 是几毫秒，但把它放在渲染循环里
        // 就是几毫秒的卡顿，而这类卡顿会被归因成「这个看图器有点钝」。
        // 写失败不致命（下次启动退回跟随系统），但要在日志里留一行 ——
        // 否则「设了没记住」会变成一个无法复盘的现象。
        std::thread::spawn(move || {
            if let Err(error) = crate::fs_ops::settings::store(preference) {
                trace::fail("theme", format!("皮肤偏好写盘失败：{error}"));
            }
        });
        // 平台侧的外观覆盖：只有 macOS 真的实现了它（`set_window_appearance`），
        // 其它平台是空实现。这里仍然调用 —— 让「窗口的系统级外观」与「我们自绘的
        // 配色」在支持的平台上保持一致（例如 macOS 的红绿灯按钮、滚动条配色），
        // 而在不支持的平台上它是一次无害的空调用，自绘配色照常生效。
        cx.set_window_appearance(match preference {
            Preference::System => None,
            Preference::Fixed(theme::Polarity::Dark) => Some(WindowAppearance::Dark),
            Preference::Fixed(theme::Polarity::Light) => Some(WindowAppearance::Light),
        });
        cx.notify();
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
        // 旧目录的邻居列表对新图没有意义，先清掉；新列表由 accept 成功后重新派发。
        self.neighbors = None;
    }

    fn accept(&mut self, outcome: OpenOutcome) {
        trace::step("view", format!("收到打开结果：{}", outcome.log_line()));
        match ImageDocument::from_outcome(outcome) {
            Ok(document) => {
                match Surface::build(&document) {
                    Ok(surface) => {
                        // 打开图片的起点是「1:1 优先，装不下就适应窗口」，判据见
                        // `ViewTransform::initial`。这里先挂一个「还没定」的占位：
                        // 选哪种要看画布尺寸与屏幕缩放，而此刻 `last_viewport` 还是
                        // 0×0（冷启动时打开早于首帧绘制），屏幕缩放更是只有窗口知道。
                        // 占位期间由绘制层就地求值（同一个 `initial`），所以画面从
                        // 第一帧起就是最终的样子；视图层量到画布尺寸后
                        // （`apply_initial_view`）再把状态定下来。
                        self.transform = ViewTransform::pending();
                        self.initial_view_pending = true;
                        // 邻居列表与首帧并行：目录枚举在自己的线程上跑，
                        // 这里只是派发，不等待 —— 首帧时刻一分都不让。
                        // 路径要先取走：document 马上被移进 self.document。
                        let doc_path = document.path().to_path_buf();
                        self.document = Some(document);
                        self.surface = Some(Arc::new(surface));
                        self.phase = Phase::Ready;
                        self.pan.end();
                        self.frame_index = 0;
                        self.animation_started = Instant::now();
                        self.neighbors_task = Some(NeighborsTask::spawn(doc_path));
                        // 当前文档变了：旧邻居列表、旧预加载都基于上一张图，留着
                        // 只会让「再按一次方向键」算出错误的邻居。先作废，等这次
                        // 的目录扫描完成后再预加载新邻居。两条入口（普通打开 /
                        // 命中预加载）都走这里，所以统一在 `accept` 里复位最稳。
                        self.neighbors = None;
                        self.preloaded.clear();
                        self.preload_tasks = None;
                        trace::step(
                            "view",
                            format!(
                                "进入 Ready：模式={:?} 初始倍率={:.4} 平移=({:.2},{:.2}) 画布={:.2}×{:.2}（初始视图待定={}，待定期间由绘制层就地求值）",
                                self.transform.mode(),
                                self.transform.scale(),
                                self.transform.pan().x,
                                self.transform.pan().y,
                                self.last_viewport.width,
                                self.last_viewport.height,
                                self.initial_view_pending,
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

    /// 为刚打开的那张图定下初始视图。
    ///
    /// 只在「打开了新图、但画布尺寸还没测到」时起作用：冷启动时打开结果早于首帧绘制，
    /// `last_viewport` 还是 0×0，`ViewTransform::initial` 无从判断装不装得下，只能先挂
    /// 一个 [`ZoomMode::Pending`] 占位。这里补上画布尺寸与屏幕缩放后的那次判定。
    ///
    /// 判定函数与绘制层就地求值用的是同一个 `initial`，所以画面不会因为这次判定而变 ——
    /// 它只是把「待定」这个状态落定，好让缩放读数、工具栏高亮这些读 `scale()` 的地方
    /// 拿到真实值。
    fn apply_initial_view(&mut self, window: &Window) {
        if !self.initial_view_pending {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let viewport = self.last_viewport;
        if viewport.is_empty() {
            // 画布还没量到尺寸，继续等下一帧。图像没上屏前用户也无从操作，
            // 所以这一位不会被晾在这里太久。
            return;
        }

        let image = document.logical_size();
        self.initial_view_pending = false;
        self.transform = ViewTransform::initial(pixel_ratio_of(window), image, viewport);
        trace::step(
            "view",
            format!(
                "初始视图判定：图像 {:.0}×{:.0} 画布 {:.0}×{:.0} → 模式={:?} 倍率={:.4}",
                image.width,
                image.height,
                viewport.width,
                viewport.height,
                self.transform.mode(),
                self.transform.scale(),
            ),
        );
    }

    /// 每次渲染前把系统对话框的结果取回来（与 `pump_task` 同一条时序约定）。
    ///
    /// 关键：取结果用的是 `try_recv()` —— 主线程**从不**在这里阻塞，即使对话框还开着、
    /// 用户还没选，也只是把 `pending_dialog` 放回原位等下一帧。这正是「打开文件对话框时
    /// 切回主窗口不会卡死」的根因修复：主线程一直空闲在事件循环里，随时能响应
    /// WM_PAINT / 鼠标 / 键盘。
    fn pump_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_dialog.take() else {
            return;
        };
        // 只问接收端有没有消息，不 `recv()` —— 否则主线程又被卡住。
        let received = match &pending {
            PendingDialog::Open(rx)
            | PendingDialog::SaveAs(rx)
            | PendingDialog::Rename { rx, .. } => rx.try_recv(),
        };
        match received {
            Ok(Some(path)) => {
                // `pending` 已移出 `self`，这里按类型分派后续动作，互不借用。
                match pending {
                    PendingDialog::Open(_) => self.open_path(path, cx),
                    PendingDialog::SaveAs(_) => self.perform_save_as(path),
                    PendingDialog::Rename { current, .. } => self.perform_rename(current, path),
                }
            }
            // 用户取消：什么都不做，丢弃即可。
            Ok(None) => {}
            // 还没选完：放回原位，下一帧再问。
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.pending_dialog = Some(pending);
            }
            // 对话框线程异常退出（极少）：等同取消。
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
        cx.notify();
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

    /// 每次渲染前把目录扫描的结果取回来（与 `pump_task` 同一条时序约定）。
    fn pump_neighbors(&mut self) {
        let Some(task) = self.neighbors_task.as_mut() else {
            return;
        };
        let Some(neighbors) = task.poll() else {
            return;
        };
        self.neighbors_task = None;
        self.neighbors = Some(neighbors);
        // 目录就绪：马上开始预解码邻居，下一次切换就能省掉最慢的「读文件 + 解码」。
        // 首帧仍然不等它 —— 这里只是派发后台任务，不阻塞当前帧。
        self.spawn_preloads();
    }

    /// 目录就绪后立刻预解码上一张 / 下一张。
    ///
    /// 与 [`crate::open_job::OpenTask`] 同款：解码在独立线程上跑，只做「读文件 +
    /// 解码」，不含纹理上传（纹理要在有窗口上下文时由 `accept` → `Surface::build` 做）。
    /// 所以切换命中后只剩上屏，几乎瞬时。
    fn spawn_preloads(&mut self) {
        let Some(neighbors) = self.neighbors.as_ref() else {
            return;
        };
        // 邻居结果只交付一次，不该重复派生两批任务。
        if self.preload_tasks.is_some() {
            return;
        }
        let previous = neighbors.previous.clone().map(OpenTask::spawn);
        let next = neighbors.next.clone().map(OpenTask::spawn);
        self.preload_tasks = Some((previous, next));
    }

    /// 把后台预解码的结果收进缓存（与 `pump_task` 同一条时序约定：
    /// 结果只在渲染循环里被取回，且取一次即止）。
    fn pump_preloads(&mut self) {
        let Some(tasks) = self.preload_tasks.as_mut() else {
            return;
        };
        let mut all_done = true;
        // 上一张 / 下一张各一个槽，完成的取出进缓存、清掉槽位。
        for slot in [&mut tasks.0, &mut tasks.1] {
            if let Some(task) = slot.as_mut() {
                if let Some(outcome) = task.poll() {
                    // 按路径索引：切换时拿目标路径来这里查，命中即用。
                    self.preloaded.insert(outcome.path.clone(), outcome);
                    *slot = None;
                } else {
                    all_done = false;
                }
            }
        }
        if all_done {
            self.preload_tasks = None;
        }
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
        // 用户已经自己动过视图了（缩放、平移、适应窗口、双击…）。
        // 初始视图的待定判定就此作废：否则下一帧拿到画布尺寸时会把他刚做的操作覆盖掉。
        self.initial_view_pending = false;
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
            Command::PreviousFile => self.open_neighbor(-1, cx),
            Command::NextFile => self.open_neighbor(1, cx),
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
            Command::ToggleFullscreen => self.toggle_fullscreen(window, cx),
            // 皮肤三项是同一件事的三个取值，共用一条路径。
            Command::SkinFollowSystem => self.set_preference(Preference::System, cx),
            Command::SkinDark => {
                self.set_preference(Preference::Fixed(theme::Polarity::Dark), cx)
            }
            Command::SkinLight => {
                self.set_preference(Preference::Fixed(theme::Polarity::Light), cx)
            }
            // 退出整个应用而不是关掉这个窗口：本程序只有这一个窗口，
            // 两者在当前实现下等价，但「退出」是用户按下菜单项时的心智模型。
            Command::Quit => cx.quit(),
        }

        cx.notify();
    }

    /// 「打开…」：走系统文件对话框，再复用「拖入文件」那条路径。
    /// 「打开…」：走系统文件对话框，再复用「拖入文件」那条路径。
    ///
    /// 非阻塞：对话框在它自己的线程上跑，主线程只存下接收端、继续泵事件循环；
    /// 用户选完（或取消）由 `pump_dialog` 在渲染循环里收结果。这样对话框打开期间
    /// 切回主窗口，主窗口照常响应，不会被系统标成「未响应」。
    fn open_dialog(&mut self, cx: &mut Context<Self>) {
        // 已经有对话框在等：不再叠第二个。用户在对话框打开期间仍能与主窗口交互，
        // 可能再次触发 Ctrl+O / 菜单项，这里挡掉。
        if self.pending_dialog.is_some() {
            return;
        }
        // 非阻塞：立刻拿回接收端，主线程继续泵 GPUI 的消息循环。
        self.pending_dialog = Some(PendingDialog::Open(file_ops::pick_open_path()));
        cx.notify();
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

    pub fn save_as(&mut self, cx: &mut Context<Self>) {
        // 对话框已开：避免叠第二个（用户在对话框打开期间仍可与主窗口交互）。
        if self.pending_dialog.is_some() {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let (suggested, _) = file_ops::suggest_save_name(document.path(), document.format());
        let directory = document.path().parent().map(|path| path.to_path_buf());
        // 非阻塞：把目标路径的获取交给后台线程，结果由 `pump_dialog` 收。
        self.pending_dialog = Some(PendingDialog::SaveAs(file_ops::pick_save_path(
            &suggested,
            directory.as_deref(),
        )));
        cx.notify();
    }

    /// 「另存为」拿到目标路径后的实际写入。从 `pump_dialog` 调，不在 `save_as` 里直接做，
    /// 是为了让系统对话框的阻塞只发生在它自己的线程上（见 `file_ops::pick_save_path`）。
    fn perform_save_as(&mut self, destination: PathBuf) {
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

    pub fn rename(&mut self, cx: &mut Context<Self>) {
        // 对话框已开：避免叠第二个。
        if self.pending_dialog.is_some() {
            return;
        }
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let current = document.path().to_path_buf();
        // 非阻塞：把目标路径的获取交给后台线程，结果由 `pump_dialog` 收。
        // 必须记住「被改名的源文件」是谁，否则对话框关闭时再去动一个
        // 可能已经不存在的老路径。
        let rx = file_ops::pick_rename_path(&current);
        self.pending_dialog = Some(PendingDialog::Rename { current, rx });
        cx.notify();
    }

    /// 「重命名」拿到目标路径后的实际移动。从 `pump_dialog` 调，不在 `rename` 里直接做，
    /// 是为了让系统对话框的阻塞只发生在它自己的线程上（见 `file_ops::pick_rename_path`）。
    fn perform_rename(&mut self, current: PathBuf, destination: PathBuf) {
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
                // 邻居与预加载都基于这张已删除的图，留着既错又占内存。
                self.neighbors = None;
                self.preloaded.clear();
                self.preload_tasks = None;
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

    /// 打开同目录里的上一个 / 下一个图片（`offset` 为 -1 或 1）。
    fn open_neighbor(&mut self, offset: isize, cx: &mut Context<Self>) {
        // 列表还没就绪：说明而不是装作没按到 —— 大目录的枚举要一点时间，
        // 用户按了没反应会以为键盘坏了。
        let Some(neighbors) = self.neighbors.as_ref() else {
            self.push_toast("正在读取目录…", ToastKind::Info);
            return;
        };
        let target = if offset < 0 {
            neighbors.previous.clone()
        } else {
            neighbors.next.clone()
        };
        let Some(path) = target else {
            self.push_toast("目录里没有其他图片", ToastKind::Info);
            return;
        };
        // 命中预加载：直接接管，省掉最慢的「读文件 + 解码」这段。
        // 文档变了，`accept` 会清掉旧邻居与旧预加载、重新扫描并预加载新邻居。
        if let Some(outcome) = self.preloaded.remove(&path) {
            trace::step("view", format!("命中预加载，直接挂载：{}", path.display()));
            self.accept(outcome);
            return;
        }
        self.open_path(path, cx);
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
        animating
            || !self.toasts.is_empty()
            || self.task.is_some()
            || self.neighbors_task.is_some()
            || self.preload_tasks.is_some()
            || self.pending_dialog.is_some()
            // 刚按过 F11 / ESC：等平台的全屏标志翻面（见 `FULLSCREEN_SETTLE_FRAMES`）。
            || self.fullscreen_pending > 0
            // 初始视图还没定：要等到下一帧才有画布尺寸可用（见 `apply_initial_view`）。
            // 少这一项时画面本身还是对的（绘制层就地求值），但缩放读数与工具栏高亮
            // 会一直停在占位的「待定」上 —— 这类"界面不跟着动"最难从现象倒推。
            || self.initial_view_pending
    }

    // ---- 全屏 ----

    /// 把平台的全屏状态读进 `self.fullscreen`。
    ///
    /// 为什么每帧读而不是在切换时自己翻一位：Windows 的 `toggle_fullscreen` 是异步
    /// 投递的，就地翻转会先于平台生效，随后平台状态落地时两者就打架了 —— 界面会是
    /// 「先藏起来、再亮回来」这种最像 bug 的表现。读平台值没有这个问题，代价只是需要
    /// 多渲染几帧来等它，由 `fullscreen_pending` 负责。
    ///
    /// 反过来这也让「平台自己改变全屏」（macOS 的绿色按钮之类）能自动跟上，
    /// 不需要额外的事件订阅 —— 而窗口事件里本来也没有全屏变化这一项。
    fn refresh_fullscreen(&mut self, window: &Window) {
        let fullscreen = window.is_fullscreen();
        // 打点：全屏相关的问题（按了没反应、界面没跟着变）只能靠这个时间线复盘 ——
        // 界面上「什么都没发生」和「发生了但画错」在截图里长得一样。
        if self.fullscreen != fullscreen {
            trace::step(
                "view",
                format!(
                    "全屏状态 → {}，界面{}",
                    if fullscreen { "全屏" } else { "窗口" },
                    if fullscreen { "隐藏标题栏与工具栏" } else { "恢复" },
                ),
            );
        }
        self.fullscreen = fullscreen;
        self.fullscreen_pending = self.fullscreen_pending.saturating_sub(1);
    }

    /// 请求一次全屏切换。
    ///
    /// 两个入口（F11 与 ESC）共用这里，免得各自处理一遍「收起菜单、保持重绘」，
    /// 也就不会出现「一个入口能用、另一个漏了某步」的不一致。
    fn toggle_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 全屏时不画下拉浮层，留着这个下标只会让退出全屏的瞬间冒出一个
        // 「本来就开着」的菜单。
        self.menu = None;
        window.toggle_fullscreen();
        self.fullscreen_pending = FULLSCREEN_SETTLE_FRAMES;
        cx.notify();
    }

    // ---- 交互 ----

    /// 滚轮缩放：以光标为锚点。
    fn on_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let factor = match event.delta {
            ScrollDelta::Lines(point) => zoom_factor_from_lines(point.y),
            ScrollDelta::Pixels(point) => zoom_factor_from_pixels(f32::from(point.y)),
        };

        let cursor = Vec2::new(f32::from(event.position.x), f32::from(event.position.y));
        // 事件坐标是相对窗口的，画布却从标题栏 + 工具栏下方开始，这里换算到画布局部坐标。
        // 少减一项的表现是缩放锚点整体偏移一个栏的高度 —— 用户说不出哪里不对，
        // 只会觉得"缩放时图像在往一边跑"。全屏时两栏都不存在，所以这个偏移必须
        // 与「画不画那两栏」用同一个判据（见 `canvas_top_offset`）。
        let cursor = Vec2::new(cursor.x, cursor.y - canvas_top_offset(self.fullscreen));

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
                self.toggle_fullscreen(window, cx);
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

    /// 占位层：空状态给一个居中的「打开图片」按钮；加载中与失败态给文字说明。
    fn placeholder(
        &self,
        skin: &Skin,
        window: &Window,
        view: &Entity<ImageViewerView>,
    ) -> AnyElement {
        // 空状态：画布正中一个「打开图片」按钮，点开系统文件对话框。
        // 按钮下方留一行极弱的提示，告诉用户还有拖入与命令行两种入口；
        // 但主操作只有一个，避免空界面上堆一堆文字。
        if matches!(self.phase, Phase::Empty) {
            let view = view.clone();
            return div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .absolute()
                .inset_0()
                .child(
                    div()
                        .px_6()
                        .py_3()
                        .rounded_lg()
                        .bg(skin.primary)
                        .text_color(skin.text)
                        .text_size(px(15.0))
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .hover(|style| style.bg(skin.primary_hover))
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            // 走与菜单里「打开…」完全相同的路径（系统对话框在独立线程弹）。
                            view.update(cx, |this, cx| this.open_dialog(cx));
                        })
                        .child("打开图片"),
                )
                .child(
                    div()
                        .text_color(skin.text_faint)
                        .text_size(px(12.0))
                        .child(format!(
                            "也可以把图片拖进来，或执行 {} <图片路径>",
                            env_program_name()
                        )),
                )
                .into_any_element();
        }

        let (headline, detail, accent) = match &self.phase {
            // 空状态已在上面单独处理，这里不会再走到。
            Phase::Empty => return div().into_any_element(),
            Phase::Loading { name } => {
                let elapsed = self
                    .task
                    .as_ref()
                    .map(|task| task.elapsed_ms())
                    .unwrap_or_default();
                (
                    format!("正在打开 {name}"),
                    format!("已用时 {elapsed:.0} ms"),
                    skin.info,
                )
            }
            Phase::Failed { message, detail } => (message.clone(), detail.clone(), skin.danger),
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
                    .text_color(skin.text_muted)
                    .text_size(px(12.0))
                    .child(detail),
            )
            .child(
                div()
                    .text_color(skin.text_faint)
                    .text_size(px(10.0))
                    .child(format!("屏幕缩放 {ratio:.2}×")),
            )
            .into_any_element()
    }

    /// 浮层：底部居中，2 秒后自动淡出。
    fn toast_layer(&mut self, skin: &Skin) -> AnyElement {
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
                    .bg(skin.surface_active)
                    .border_1()
                    .border_color(latest.kind.color(skin))
                    .text_color(skin.text)
                    .text_size(px(12.0))
                    .child(latest.text.clone()),
            )
            .into_any_element()
    }

    /// 画布区块：图像 + 占位层 + 浮层。
    fn canvas_area(
        &mut self,
        skin: &Skin,
        window: &Window,
        view: &Entity<ImageViewerView>,
    ) -> AnyElement {
        let logical = self
            .document
            .as_ref()
            .map(|document| document.logical_size())
            .unwrap_or(ImageSize::ZERO);

        let placeholder = self.placeholder(skin, window, view);
        let toast = self.toast_layer(skin);

        let mut area = div()
            .flex_1()
            .relative()
            .overflow_hidden()
            .bg(skin.canvas)
            .child(viewport(ViewportConfig {
                surface: self.surface.clone(),
                logical_size: logical,
                transform: self.transform,
                frame_index: self.frame_index,
                slot: self.viewport.clone(),
                background: skin.canvas,
                checker: skin.checker,
            }))
            .child(placeholder);

        if self.info_open {
            area = area.child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .child(panels::info_panel(skin, self.document.as_ref())),
            );
        }

        area.child(toast).into_any_element()
    }

    /// 承接画布的那一层。
    ///
    /// 它只做两件事：把画布撑满剩余空间，以及承载滚轮与拖拽。
    ///
    /// # 为什么全屏与非全屏共用它
    ///
    /// 全屏只是不画标题栏 / 工具栏，事件落点一个都不能少。两条分支各写一份的话，
    /// 漏掉某个绑定（最典型的是滚轮）不会有任何报错，只会表现为「全屏后滚轮不能用」。
    /// 共用一份绑定就没有这个空间。
    ///
    /// # 为什么必须显式声明 flex 容器
    ///
    /// `area` 用 `flex_1()` 撑满，而 `flex-grow` 只在 flex 容器里生效。缺了
    /// `.flex()` 时这一层退化成普通块级容器，`area` 的高度就由内容决定：它唯一的
    /// 子元素是绝对定位的画布（不占空间），于是高度算出来是 0，画布随之变成
    /// `宽 × 0`。`paint_image` 在可见区域为空时返回的是 `Ok(())`，所以整个过程
    /// **没有任何报错**，界面只是一片黑。
    ///
    /// 之所以不改成给 `area` 加 `size_full()`：那依赖父级高度已经确定，
    /// 而这里父级的高度正是由 `flex_1` 决定的 —— 显式声明 flex 容器
    /// 才让「谁分配空间、谁撑满」这条链路是可读的。
    fn stage(area: AnyElement, cx: &mut Context<Self>) -> AnyElement {
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
            .child(area)
            .into_any_element()
    }
}

impl Render for ImageViewerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 0. 全屏状态每帧从平台读一次。往下「画不画界面」与滚轮锚点换算都看它，
        //    所以必须在构建元素树之前刷新。
        self.refresh_fullscreen(window);

        // 0a. 皮肤也每帧现算一次。`window.appearance()` 是平台值，系统主题一变
        //     它就变了（Windows 走 `ImmersiveColorSet` 消息），因此「跟随系统」
        //     不需要任何自持的状态或额外订阅 —— 与上面那条同一个理由。
        //     手动选了皮肤的会话里，`current_skin` 直接返回固定的那一套，
        //     系统怎么变都不影响。
        let skin = self.current_skin(window.appearance());

        // 1. 先把后台结果收回来 —— 这一帧就能显示图像，而不是等到下一帧。
        self.pump_task();
        self.pump_neighbors();
        self.pump_preloads();
        // 系统对话框的结果也在每帧取回：这里只 `try_recv`，主线程从不阻塞，
        // 对话框打开期间切回主窗口也不会卡死。
        self.pump_dialog(cx);

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

        // 2a. 刚打开一张图：画布尺寸到手了，这才谈得上「装不装得下」。
        //     冷启动时打开早于首帧绘制，判定只能推迟到这一帧。
        self.apply_initial_view(window);

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
        let area = self.canvas_area(skin, window, &view_handle);
        let stage = Self::stage(area, cx);

        // 有没有打开图片，决定「用不用得上完整界面」。注意它与下面 `compact` 的区别：
        // 这一位说的是**文档状态**（有没有图可操作），`compact` 说的是**会话形态**。
        //
        // - `has_image` → 菜单项可用、状态栏有内容可填；
        // - `compact` → 收起菜单与状态栏，让纵向空间给图像。只在「启动就带图」
        //   （文件管理器双击）时为真：那是用户点名要看这一张图。从空窗口里用菜单 /
        //   按钮 / 拖入打开图片**不改界面形态** —— 用户是先开程序再选图，界面忽然
        //   少两栏会像是出错；想全屏，F11 是明路。
        let has_image = self.document.is_some();
        let compact = compact_form(has_image, self.immersive);

        let root = div()
            .flex()
            .flex_col()
            .size_full()
            // `relative` 是下拉浮层的前提：它用绝对定位挂在根容器上，
            // 需要一个明确的包含块，否则会退化成相对窗口定位。
            .relative()
            .bg(skin.canvas)
            .text_color(skin.text)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.on_file_drop(paths, cx)
            }));

        if self.fullscreen {
            // 全屏：窗口只剩画布本身。标题栏（含窗口按钮）、工具栏、状态栏、
            // 下拉浮层一概不画 —— 空状态下也一样，全屏这个东西的语义就是
            // 「把窗口整个让给内容」，哪怕内容此刻只是一个「打开图片」按钮。
            //
            // 换掉的只是**内容**：滚轮 / 拖拽 / 缩放锚点所在的 `stage` 两条分支
            // 共用同一份绑定，分开写迟早会漏掉某个事件 —— 表现就是「全屏后
            // 滚轮不能用」，而这类缺失在代码评审里几乎看不出来。
            root.child(stage).into_any_element()
        } else {
            let title_bar = menu::title_bar(
                skin,
                self.document.as_ref(),
                self.menu,
                !compact,
                &view_handle,
                window,
            );
            let toolbar = panels::toolbar(
                skin,
                self.document.as_ref(),
                zoom_percent,
                fits,
                self.info_open,
                &view_handle,
            );
            // 收起形态下菜单标签整段不画，下拉浮层也就无从展开，给一个空元素占位即可。
            let menu_layer = if compact {
                div().into_any_element()
            } else {
                menu::menu_layer(skin, self.menu, has_image, self.preference, &view_handle)
            };

            let root = root.child(title_bar).child(toolbar).child(stage);
            // 收起形态下不画底部状态栏（菜单已在标题栏里整段跳过）。
            let root = if compact {
                root
            } else {
                root.child(panels::status_bar(skin, self.document.as_ref(), zoom_percent))
            };
            // 下拉浮层必须是最后一个孩子：GPUI 按树序绘制，后画的盖住先画的。
            // 放在标题栏里（它的逻辑归属处）会被后画的画布整块盖住。
            root.child(menu_layer).into_any_element()
        }
    }
}

/// 打开图片后是否把界面收起来（标题栏不画菜单、底部不画状态栏）。
///
/// 两个条件缺一不可：
///
/// - `has_image` —— 没有图片就没什么可让位的，界面按完整形态画（画布正中是「打开图片」）；
/// - `immersive` —— **启动时就带着图片路径**（文件管理器双击）。那是用户点名要看这一张图，
///   界面让位给图像；而「先开程序、再选图」时界面本来就是完整的，选完图也必须完整 ——
///   凭空少掉两栏会像是程序出错，何况想要全屏还有 F11 这条明路。
///
/// 抽成函数是为了让「判据」与「画不画那两栏」共用一个来源：两处各写一遍，
/// 将来只会改其中一处（与 [`canvas_top_offset`] 同一个理由）。
fn compact_form(has_image: bool, immersive: bool) -> bool {
    has_image && immersive
}

/// 画布在窗口坐标系里的上边界。
///
/// 鼠标事件的坐标是相对窗口的，画布却从标题栏 + 工具栏下方开始，凡是要把窗口坐标
/// 换成画布坐标的地方都得先减掉这一段。抽成一个函数是为了让它与「画不画那两栏」
/// 用同一个判据：把两处各写一遍，将来改了界面布局就只会改其中一处，
/// 漏掉的那处表现是「缩放锚点整体偏移一个栏高」—— 用户说不出哪里不对，
/// 只觉得缩放时图像在往一边跑。
///
/// 全屏时两栏都不画，所以这里必须归零，而不是继续按常量减。
fn canvas_top_offset(fullscreen: bool) -> f32 {
    if fullscreen {
        0.0
    } else {
        theme::TITLE_BAR_HEIGHT + theme::TOOLBAR_HEIGHT
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

#[cfg(test)]
mod tests {
    // 刻意不用 `use super::*`：本模块（`view.rs`）顶部 `use gpui_kit::*`，
    // 而开着 `test-support` 时那个 glob 里含 GPUI 自己的 `test` 宏，经 glob 传下来
    // 会遮蔽内置的 `#[test]`，报的是「recursion limit reached while expanding
    // `#[test]`」这种看不出根因的错。按需显式导入即可。
    use super::canvas_top_offset;
    use super::compact_form;
    use crate::ui::theme;

    /// 全屏下画布上边界归零 —— 与非全屏时相差正好一个「标题栏 + 工具栏」。
    ///
    /// 这两件事是一体的：界面不画那两栏了，坐标换算也必须跟着归零。任何一面走偏，
    /// 结果都是滚轮缩放的锚点整体偏一个栏高，而画面上看不出任何异常。
    #[test]
    fn fullscreen_lifts_the_canvas_to_the_window_top() {
        assert_eq!(canvas_top_offset(true), 0.0);
        assert_eq!(
            canvas_top_offset(false),
            theme::TITLE_BAR_HEIGHT + theme::TOOLBAR_HEIGHT
        );
    }

    /// 只有「启动就带图」才收起界面。四种组合全钉死。
    ///
    /// 第二行是用户报过的那个缺陷：从空窗口里用菜单 / 按钮打开图片，界面必须与打开前
    /// 一样（除了画布里有图）。判据一旦被「顺手」简化回只看 `has_image`，这里立刻红。
    #[test]
    fn only_a_double_click_launch_collapses_the_interface() {
        assert!(compact_form(true, true), "双击图片打开：界面让位给图像");
        assert!(
            !compact_form(true, false),
            "先开程序再选图：界面必须保持完整，全屏交给 F11"
        );
        assert!(!compact_form(false, false), "空状态：完整界面");
        assert!(
            !compact_form(false, true),
            "带路径启动但没打开成功：错误提示要配完整界面"
        );
    }
}
