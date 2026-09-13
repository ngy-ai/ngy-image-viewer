//! 视觉常量：颜色、尺寸、动效时长。
//!
//! # 为什么集中在一处
//!
//! 「沉浸式深色 + 内容优先」这件事的成败，几乎全在于**界面元素是否足够退让**：
//! 画布要够黑、表面要够暗、描边要够淡、文字对比度要够低。
//! 一旦这些值散落在各个文件里，某处多写了一点亮度就会让整个界面
//! 从「高级」变成「花」。集中定义也让调整风格只需要改一个文件。
//!
//! # 色板
//!
//! | 用途 | 值 | 说明 |
//! | --- | --- | --- |
//! | 画布 | `#0B0C0F` | 近黑但不纯黑：纯黑与图像的暗部糊在一起，反而看不出边界 |
//! | 表面 | `#17191E` | 工具栏与状态栏 |
//! | 表面浮起 | `#1F2228` | 悬停态 |
//! | 表面强调 | `#262A31` | 按下态与分隔 |
//! | 主色 | `#4C8DFF` | 唯一的高饱和色，只用在「当前选中/主要动作」上 |
//! | 正文 | `#E9EBEF` | 不用纯白：纯白在深色底上会刺眼 |

use gpui_kit::{Hsla, rgb};

/// 画布背景（近黑）。
pub fn canvas() -> Hsla {
    rgb(0x0B0C0F).into()
}

/// 工具栏 / 状态栏的表面色。
pub fn surface() -> Hsla {
    rgb(0x17191E).into()
}

/// 悬停态表面色。
pub fn surface_hover() -> Hsla {
    rgb(0x1F2228).into()
}

/// 按下态 / 强调表面色。
pub fn surface_active() -> Hsla {
    rgb(0x262A31).into()
}

/// 主色（当前选中、主要动作）。
pub fn primary() -> Hsla {
    rgb(0x4C8DFF).into()
}

/// 主色的悬停态（更亮）。
pub fn primary_hover() -> Hsla {
    rgb(0x6AA1FF).into()
}

/// 主色的按下态（更深）。
pub fn primary_active() -> Hsla {
    rgb(0x2F6BE0).into()
}

/// 主要文字。
pub fn text() -> Hsla {
    rgb(0xE9EBEF).into()
}

/// 次要文字（信息面板的标签、状态栏）。
pub fn text_muted() -> Hsla {
    rgb(0xA2A8B4).into()
}

/// 最弱的文字与图标（分隔、占位、禁用）。
pub fn text_faint() -> Hsla {
    rgb(0x6C727E).into()
}

/// 成功（复制完成、另存成功）。
pub fn success() -> Hsla {
    rgb(0x34D399).into()
}

/// 警告。
pub fn warning() -> Hsla {
    rgb(0xFBBF24).into()
}

/// 错误（解码失败、操作失败）。
pub fn danger() -> Hsla {
    rgb(0xF87171).into()
}

/// 信息（加载中、一般提示）。
pub fn info() -> Hsla {
    rgb(0x60A5FA).into()
}

/// 细描边。用极低的对比度把区块分开，而不是画一条明显的线。
pub fn border() -> Hsla {
    rgb(0x262A31).into()
}

/// 棋盘格的浅色与深色。
///
/// 刻意压得极淡：棋盘格的作用是「说明这块是透明的」，
/// 而不是抢走图像本身的注意力。对比度过高会让半透明 PNG 看起来发灰。
pub fn checkerboard() -> (Hsla, Hsla) {
    (rgb(0x1A1C21).into(), rgb(0x141519).into())
}

/// 图标按钮的边长（逻辑点）。
pub const BUTTON_SIZE: f32 = 32.0;

/// 标题栏高度。
///
/// 刻意比工具栏（44）矮一档：这一行承载的是「窗口身份 + 菜单 + 窗口按钮」，
/// 用户只在需要时才看它一眼，而这是一个**看图**的程序 —— 纵向空间要尽量留给图像。
pub const TITLE_BAR_HEIGHT: f32 = 34.0;

/// 标题栏的左右内边距。
pub const TITLE_BAR_PADDING: f32 = 8.0;

/// 标题栏上应用标记占用的宽度。
pub const APP_MARK_WIDTH: f32 = 28.0;

/// 菜单标签的固定宽度。
///
/// 固定而不是随文字自适应：下拉浮层是绝对定位的横向排布层，它靠「先放一块与标题栏
/// 等宽的占位」来对齐自己所属的标签（见 `ui/menu.rs`）。宽度一旦随文字变化，
/// 对齐就只能依赖渲染后的测量结果 —— 浮层会晚一帧到位，切换菜单时还会抖一下。
/// 三个菜单名都是两个汉字，等宽不会浪费空间。
pub const MENU_LABEL_WIDTH: f32 = 56.0;

/// 下拉菜单面板的宽度。
pub const MENU_PANEL_WIDTH: f32 = 208.0;

/// 下拉菜单里一行的高度。
pub const MENU_ITEM_HEIGHT: f32 = 26.0;

/// macOS 红绿灯按钮占用的宽度。
///
/// 那三个按钮由系统绘制，既不能隐藏也不能移走，只能把标题栏左侧的内容让开。
pub const TRAFFIC_LIGHTS_WIDTH: f32 = 72.0;

/// 标题栏上单个窗口按钮（最小化 / 最大化 / 关闭）的宽度。
pub const WINDOW_BUTTON_WIDTH: f32 = 44.0;

/// 工具栏高度。
pub const TOOLBAR_HEIGHT: f32 = 44.0;

/// 状态栏高度。
pub const STATUS_BAR_HEIGHT: f32 = 28.0;

/// EXIF 信息面板宽度。
pub const INFO_PANEL_WIDTH: f32 = 320.0;

/// 按钮悬停高亮的过渡时长（毫秒）。
///
/// 120ms 是「能感觉到反馈，又不会让操作显得迟钝」的经验值。
pub const HOVER_TRANSITION_MS: u64 = 120;

/// 图像淡入时长（毫秒）。
pub const FADE_IN_MS: u64 = 180;

/// 面板滑入滑出时长（毫秒）。与淡入共用同一条缓动曲线，保证节奏统一。
pub const PANEL_TRANSITION_MS: u64 = 200;
