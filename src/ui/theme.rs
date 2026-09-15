//! 视觉常量：两套皮肤（深色 / 浅色）、尺寸、动效时长。
//!
//! # 为什么集中在一处
//!
//! 「内容优先」这件事的成败，几乎全在于**界面元素是否足够退让**：
//! 画布要够沉、表面要比画布略亮一档、描边要够淡、文字对比度要够低。
//! 一旦这些值散落在各个文件里，某处多写了一点亮度就会让整个界面
//! 从「高级」变成「花」。集中定义也让调整风格只需要改一个文件。
//!
//! # 两套皮肤是「两列颜色」，不是「两套代码」
//!
//! 深色与浅色**唯一**的区别是颜色取值，布局、尺寸、动效一概相同。所以这里
//! 用同一个 [`Skin`] 结构体承载两列值（[`Dark`](Skin::DARK) / [`Light`](Skin::LIGHT)），
//! 而不是让每个绘制函数各自分支 —— 后者会让「漏了浅色那一支」变成一种
//! 只在某个界面上才看得见的缺陷（而且是白底白字这种最难自查的形态）。
//!
//! 新增一个颜色时，编译器会**强制**你把两套皮肤都填上：`Skin` 的字段没有默认值，
//! 少一个字段就编译不过。
//!
//! # 深色皮肤：沉浸式
//!
//! | 用途 | 值 | 说明 |
//! | --- | --- | --- |
//! | 画布 | `#0B0C0F` | 近黑但不纯黑：纯黑与图像的暗部糊在一起，反而看不出边界 |
//! | 表面 | `#17191E` | 工具栏与状态栏 |
//! | 表面浮起 | `#1F2228` | 悬停态 |
//! | 表面强调 | `#262A31` | 按下态与分隔 |
//! | 主色 | `#4C8DFF` | 唯一的高饱和色，只用在「当前选中/主要动作」上 |
//! | 正文 | `#E9EBEF` | 不用纯白：纯白在深色底上会刺眼 |
//!
//! # 浅色皮肤：同样的层级关系，方向相反
//!
//! 浅色**不是**把深色的值取反。要保住的是「层级关系」而不是「数值关系」：
//!
//! | 用途 | 值 | 说明 |
//! | --- | --- | --- |
//! | 画布 | `#E9EBEF` | 不纯白：纯白与图像的亮部糊在一起，看不出边界（对称于深色的「不纯黑」） |
//! | 表面 | `#F7F8FA` | 比画布**亮**：深色下表面比画布亮，浅色下同样要亮，这样「界面浮在画布上」的关系才成立 |
//! | 表面浮起 | `#EDEFF3` | 悬停态 |
//! | 表面强调 | `#E1E4EA` | 按下态与分隔 |
//! | 主色 | `#3B7BEA` | 比深色的 `#4C8DFF` 压暗一档：浅底上同亮度的蓝字读不清 |
//! | 正文 | `#1B1D22` | 不用纯黑：纯黑在浅色底上同样刺眼 |
//!
//! 状态色（成功 / 警告 / 错误 / 信息）在浅色下一律**压暗**：
//! 深色皮肤里它们是「发光的提示」，浅色里同样的亮度会糊在白底上看不见。

use std::sync::LazyLock;

use gpui_kit::{Hsla, WindowAppearance, rgb};

/// 一套皮肤的极性。
///
/// 只有两个值 —— [`WindowAppearance`] 有四个（还分 vibrant 与非 vibrant），
/// 但对配色而言 `VibrantDark` 就是深色、`VibrantLight` 就是浅色。
/// 把「四选一」收敛成「二选一」放在这里，是为了让所有下游代码
/// （视图、配置项、菜单文案）都只面对两个分支。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Polarity {
    /// 深色。
    #[default]
    Dark,
    /// 浅色。
    Light,
}

impl Polarity {
    /// 从平台给的外观判定极性。
    ///
    /// 缺省（[`WindowAppearance::Light`]）归浅色：它是 `Default`，也是
    /// 「读不到系统设置」时最安全的答案 —— 浅色底上万一配色没跟上，
    /// 深色文字仍然看得见；反过来则是白底白字。
    pub fn of(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
        }
    }

    /// 配置里记的名字（写盘用）。
    pub fn key(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    /// 配置里记的名字（读盘用）。无法识别时返回 `None`。
    pub fn from_key(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }

    /// 菜单里显示的名字。
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "深色",
            Self::Light => "浅色",
        }
    }

    pub fn skin(self) -> &'static Skin {
        match self {
            Self::Dark => &DARK,
            Self::Light => &LIGHT,
        }
    }
}

/// 一套完整的配色。
///
/// 字段与原来的无参函数一一对应 —— 每个字段的语义见模块文档的色板表。
/// 这里**只放颜色**，尺寸与时长仍在下方以常量给出：它们是「两套皮肤共用」的，
/// 放进 `Skin` 会暗示它们可以随皮肤变化，而那是没人想要的能力。
pub struct Skin {
    /// 画布背景。
    pub canvas: Hsla,
    /// 工具栏 / 状态栏 / 信息面板的表面色。
    pub surface: Hsla,
    /// 悬停态表面色。
    pub surface_hover: Hsla,
    /// 按下态 / 强调表面色。
    pub surface_active: Hsla,
    /// 主色（当前选中、主要动作）。
    pub primary: Hsla,
    /// 主色的悬停态。
    pub primary_hover: Hsla,
    /// 主色的按下态。
    pub primary_active: Hsla,
    /// 主要文字。
    pub text: Hsla,
    /// 次要文字（信息面板的标签、状态栏）。
    pub text_muted: Hsla,
    /// 最弱的文字与图标（分隔、占位、禁用）。
    pub text_faint: Hsla,
    /// 成功（复制完成、另存成功）。
    pub success: Hsla,
    /// 警告。
    pub warning: Hsla,
    /// 错误（解码失败、操作失败）。
    pub danger: Hsla,
    /// 信息（加载中、一般提示）。
    pub info: Hsla,
    /// 细描边。用极低的对比度把区块分开，而不是画一条明显的线。
    pub border: Hsla,
    /// 棋盘格的浅色与深色。
    ///
    /// 刻意压得极淡：棋盘格的作用是「说明这块是透明的」，
    /// 而不是抢走图像本身的注意力。两套皮肤都遵守这一条，
    /// 但浅色下的两个值必须**比画布暗**（深色下是比画布亮）——
    /// 方向反了棋盘格就消失了，而半透明 PNG 会看起来像纯色块。
    pub checker: (Hsla, Hsla),
}

/// 深色皮肤。
///
/// 不是 `const`：`rgb()` 与 `Rgba → Hsla` 的转换都不是 const 函数
/// （后者要算 HSL，见 gpui 的 `color.rs`）。因此两套皮肤用 `LazyLock` 在
/// 首次取用时构造一次 —— 它们以后就一直是那两个静态引用，
/// 每次取用零成本，也不会在渲染循环里反复做 HSL 换算。
///
/// 放在模块级而不是 `Skin` 的关联项：关联 `static` 在 Rust 里是不允许的
/// （关联项只能是 `const` / 函数 / 类型）。
pub static DARK: LazyLock<Skin> = LazyLock::new(|| Skin {
    canvas: rgb(0x0B0C0F).into(),
    surface: rgb(0x17191E).into(),
    surface_hover: rgb(0x1F2228).into(),
    surface_active: rgb(0x262A31).into(),
    primary: rgb(0x4C8DFF).into(),
    primary_hover: rgb(0x6AA1FF).into(),
    primary_active: rgb(0x2F6BE0).into(),
    text: rgb(0xE9EBEF).into(),
    text_muted: rgb(0xA2A8B4).into(),
    text_faint: rgb(0x6C727E).into(),
    success: rgb(0x34D399).into(),
    warning: rgb(0xFBBF24).into(),
    danger: rgb(0xF87171).into(),
    info: rgb(0x60A5FA).into(),
    border: rgb(0x262A31).into(),
    checker: (rgb(0x1A1C21).into(), rgb(0x141519).into()),
});

/// 浅色皮肤。
pub static LIGHT: LazyLock<Skin> = LazyLock::new(|| Skin {
    // 画布比表面暗：与深色皮肤同样的「表面浮在画布上」关系，
    // 只是方向反过来。纯白会与图像亮部糊在一起，所以取一档灰。
    canvas: rgb(0xE9EBEF).into(),
    surface: rgb(0xF7F8FA).into(),
    surface_hover: rgb(0xEDEFF3).into(),
    surface_active: rgb(0xE1E4EA).into(),
    // 主色压暗一档：`#4C8DFF` 在浅底上作为文字色对比度不够。
    primary: rgb(0x3B7BEA).into(),
    primary_hover: rgb(0x5B93F0).into(),
    primary_active: rgb(0x2A63C9).into(),
    text: rgb(0x1B1D22).into(),
    text_muted: rgb(0x5A6070).into(),
    text_faint: rgb(0x8A909E).into(),
    // 状态色一律压暗：深色里它们是发光提示，浅色里同样的亮度会消失。
    success: rgb(0x0E9F6E).into(),
    warning: rgb(0xB45309).into(),
    danger: rgb(0xDC2626).into(),
    info: rgb(0x2563EB).into(),
    border: rgb(0xD6DAE1).into(),
    checker: (rgb(0xDCDEE3).into(), rgb(0xD2D5DB).into()),
});

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
