//! gpui-kit 应用启动骨架。
//!
//! 严格遵循 gpui-kit 官方约定，这几条踩错一条就会白屏或丢事件：
//!
//! 1. `application().with_assets(...)` 构建应用；
//! 2. `app.run` 闭包中**尽早**调用 `gpui_kit::init(cx)`；
//! 3. 窗口的第一层必须是 `Root` 包裹；
//! 4. 视图若要弹系统级浮层，需要在 `render` 末尾追加 `Root` 的对应层。
//!
//! # 启动时序（本项目的核心机制）
//!
//! ```text
//! main(): 解析 argv ──┬─→ OpenTask::spawn()   读文件 + 解码      ┐
//!                     └─→ app::run()          GPUI 平台初始化   ┘  两条线并行
//! ```
//!
//! 关键在于**并行**：GPUI 的平台初始化有约 300ms 的固定开销（已在基线中量化），
//! 若先解码再启动界面，用户要等两段耗时之和；并行之后，感知等待接近两者的较大值。
//! 实测一张 6000×4000 的 PNG 解码约 59ms —— 也就是说，界面起来的时候图早就好了，
//! 首帧可以直接呈现图像，用户完全看不到加载过程。
//!
//! 这个文件只负责「把并行的两条线接起来」，不承载任何界面逻辑。

use std::path::PathBuf;

use gpui_kit::component::*;
use gpui_kit::*;

use crate::decode::DecodeLimits;
use crate::open_job::OpenTask;
use crate::perf;
use crate::trace;
use crate::ui::ImageViewerView;
use crate::ui::command::APP_NAME;

/// 启动参数。来自文件管理器双击或命令行。
#[derive(Clone, Debug, Default)]
pub struct AppOptions {
    /// 待打开的图片路径；无参数时进入空状态。
    pub path: Option<PathBuf>,
    /// 解码资源上限。
    pub limits: DecodeLimits,
}

impl AppOptions {
    /// 解析命令行。只取第一个看起来是路径的参数，行为可预测。
    pub fn from_args() -> Self {
        let mut options = AppOptions {
            limits: DecodeLimits::default(),
            ..AppOptions::default()
        };
        for arg in std::env::args_os().skip(1) {
            let arg = PathBuf::from(arg);
            let text = arg.to_string_lossy();
            // 预留开关位置：`-` 开头的参数暂不当作文件路径。
            if text.starts_with('-') {
                continue;
            }
            if options.path.is_none() {
                options.path = Some(arg);
            }
        }
        options
    }
}

/// 诊断开关：在 `gpui_kit::init` 之前先枚举一次系统字体并计时。
///
/// 背景：曾经怀疑冷启动的几百毫秒来自 `gpui_kit::init` 里的全系统字体枚举。
/// 实测否定了这个猜测（`all_font_names()` 仅 0.2ms，真正的开销在进入 `app.run`
/// 回调**之前**的 GPUI 平台初始化）。保留这个开关是为了以后回归时能快速复测，
/// 而不是重新走一遍排查。
///
/// 仅在 `NGY_DIAG_FONTS=1` 时生效，默认零影响。
fn maybe_diag_font_enum(cx: &mut App) {
    if std::env::var("NGY_DIAG_FONTS").as_deref() != Ok("1") {
        return;
    }
    let start = perf::elapsed_ms();
    let names = cx.text_system().all_font_names();
    perf::log_always(&format!(
        "[diag] all_font_names() -> {} 个字体，耗时 {:.2} ms（调用时刻 {:.2} ms）",
        names.len(),
        perf::elapsed_ms() - start,
        start
    ));
}

/// 窗口选项。
///
/// 唯一改了默认值的是 `titlebar.appears_transparent`：让系统不再绘制标题栏，
/// 改由 `ui/menu.rs` 自绘一条与界面同为深色的标题栏 + 菜单栏。保留系统标题栏的话，
/// 那条浅色的系统栏会横在沉浸式深色界面之上，一眼就能看出「不是这个程序的一部分」；
/// 而且我们也没有别的地方可以安放菜单栏（塞进工具栏会和缩放/旋转按钮混成一片）。
///
/// 代价是 Windows 上要自己画最小化 / 最大化 / 关闭，并把它们声明成对应的
/// [`WindowControlArea`] 才能拿回系统行为；平台差异见 `ui/menu.rs`。
fn window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            // 任务栏与 Alt+Tab 里显示的名字。打开图片后会由视图改成文件名。
            title: Some(APP_NAME.into()),
            appears_transparent: true,
            // macOS 的红绿灯按钮位置。自绘标题栏只有 34pt 高，系统默认位置会把按钮顶到最上面，
            // 这里显式给一个视觉居中的位置；其它平台忽略这个字段。
            traffic_light_position: Some(point(px(10.0), px(11.0))),
        }),
        ..WindowOptions::default()
    }
}

/// 启动应用。阻塞直到窗口关闭。
///
/// `task` 是 `main()` 在解析完参数后立刻启动的后台解码任务；
/// 它此刻**很可能已经完成**（GPUI 的平台初始化比常见图片的解码慢得多），
/// 因此这里的第一动作是先捞一次结果，再决定要不要挂心跳。
pub fn run(options: AppOptions, mut task: Option<OpenTask>) -> anyhow::Result<()> {
    perf::mark("app_run_begin");
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        // 关键分界点：`app.run` 回调被调用的时刻，等于 GPUI 平台初始化完成的时刻。
        // 进程启动到这里之间的耗时是 GPUI 的固定开销（平台 / 文字系统 / 事件循环），
        // 与我们的代码无关，也无法通过延迟初始化绕过 —— 基线里已经量化过。
        perf::mark("gpui_platform_ready");

        maybe_diag_font_enum(cx);

        // gpui-kit 要求：必须在最早期初始化主题与全局配置。
        gpui_kit::init(cx);
        perf::mark("gpui_kit_init_done");

        // 建视图之前先同步看一眼后台解码是否已经完成。
        //
        // 这一步是「首帧即图像」的关键：GPUI 的平台初始化有约 300ms 的固定开销，
        // 而绝大多数图片在这段时间里早就解码完了。如果非要挂轮询等第一次 tick，
        // 首帧就只能显示加载占位，用户会看到一次不必要的闪烁与跳变。
        // 实测（6000×4000 PNG，解码 59ms）：加这一步之前图像晚首帧 7ms 上屏，加之后为 0。
        let prefetched = task.as_mut().and_then(|task| task.poll());
        perf::mark("prefetch_checked");
        trace::step(
            "app",
            format!(
                "启动期预取解码结果：{}",
                match prefetched.as_ref() {
                    Some(outcome) => format!("已就绪（{}）", outcome.log_line()),
                    None => "尚未就绪，交给视图轮询（窗口会在「正在打开」状态下显示占位）".to_string(),
                },
            ),
        );

        let view = cx.new(|cx| {
            let mut view = ImageViewerView::new(cx);
            match prefetched {
                Some(outcome) => view.apply(outcome),
                // 还没有结果：把任务交给视图，它会在每次渲染前轮询一次。
                None => {
                    if let Some(path) = options.path.as_ref() {
                        if let Some(task) = task.take() {
                            view.attach_task(path, task);
                        }
                    }
                }
            }
            view
        });

        let window = cx.open_window(window_options(), |window, cx| {
            // 约定：每个窗口的第一层必须是 Root。
            cx.new(|cx| Root::new(view.clone(), window, cx))
        });
        perf::mark("window_opened");

        let Ok(_window) = window else {
            perf::log_always("[fatal] 创建窗口失败，应用退出");
            trace::fail("app", "创建窗口失败，应用退出（界面上什么都看不到）");
            return;
        };
        trace::step("app", "窗口已创建，进入渲染循环");

        // 这里刻意**没有**「等解码完成」的轮询循环。
        //
        // 启动时分两种情形，两种都不需要外部唤醒：
        // - 解码已经完成（绝大多数）—— 上面那次 `poll()` 就把它取走了，视图不持有任务，
        //   首帧直接出图，一帧多余的重绘都不会有；
        // - 还没完成 —— 视图持有任务，它会在每次渲染里轮询一次，并在任务未完成时
        //   自己请求下一帧（见 `ImageViewerView::needs_animation_frame`）。
        //
        // 之所以把这件事收进视图，是因为「打开」不止发生在启动时：拖入文件、菜单里的
        // 「打开…」都会新起一个任务。心跳只有一份、且长在视图里，这几条路径才不会
        // 有的能等到结果、有的永远停在「正在打开」。
    });

    Ok(())
}
