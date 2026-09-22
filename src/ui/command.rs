//! 菜单与命令的数据层：动作清单、菜单结构、快捷键表、按键解析。
//!
//! # 为什么不和渲染放在一起
//!
//! 这个文件**不引用任何 gpui 类型**，理由与 [`crate::ui::format`] 完全相同：
//! 需要被断言的东西必须能跑单元测试，而带 `#[test]` 的模块一旦 `use gpui_kit::*`
//! 就会撞上测试宏展开的递归上限。
//!
//! # 它解决的核心问题：菜单上写着的快捷键不能是假的
//!
//! 「菜单里写着 Ctrl+S，按下去却没反应」是这类界面最容易出现、又最难在评审里被
//! 看出来的缺陷，根因是**快捷键文案与键盘分发各写了一份**。这里把两者收敛到同一张
//! [`KEY_BINDINGS`]：视图的按键分发查它，菜单右侧的提示也从它取。测试逐条按下每个
//! 按键，断言解出来的正是它声明的动作 —— 只要表里出现笔误，测试就红。

/// 应用名。
///
/// 自绘标题栏之后这个名字要在界面上出现两处（标题栏上的文字与窗口标题），
/// 定义一次，避免两处写法慢慢分叉。
pub const APP_NAME: &str = "ngy-image-viewer";

/// 一个动作。菜单项与键盘快捷键最终都落到它上面。
///
/// 动作的粒度按「用户会怎么描述这一步」来切：旋转分顺逆时针，缩放分放大缩小，
/// 而不是压成一个 `Zoom(delta)` —— 菜单里要写的中文、菜单项的启用条件都依赖这个粒度。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    Open,
    /// 同目录里的上一个图片（↑ / ←）。
    PreviousFile,
    /// 同目录里的下一个图片（↓ / →）。
    NextFile,
    /// 多页文档里的上一页（PageUp）。单页文档里没有意义。
    PreviousPage,
    /// 多页文档里的下一页（PageDown）。
    NextPage,
    SaveAs,
    Rename,
    DeleteToTrash,
    CopyToClipboard,
    FitToWindow,
    ActualSize,
    ZoomIn,
    ZoomOut,
    RotateClockwise,
    RotateCounterClockwise,
    FlipHorizontal,
    FlipVertical,
    ToggleInfoPanel,
    ToggleFullscreen,
    /// 皮肤：跟随系统外观。
    SkinFollowSystem,
    /// 皮肤：固定深色。
    SkinDark,
    /// 皮肤：固定浅色。
    SkinLight,
    /// 文件关联：挑出双击时要用本程序打开的格式。
    AssociateFormats,
    Quit,
}

impl Command {
    /// 菜单里显示的名字。带省略号的表示点下去还会再弹一个对话框。
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "打开…",
            Self::PreviousFile => "上一个文件",
            Self::NextFile => "下一个文件",
            Self::PreviousPage => "上一页",
            Self::NextPage => "下一页",
            Self::SaveAs => "另存为…",
            Self::Rename => "重命名…",
            Self::DeleteToTrash => "移到回收站",
            Self::CopyToClipboard => "复制图像",
            Self::FitToWindow => "适应窗口",
            Self::ActualSize => "实际大小（1:1）",
            Self::ZoomIn => "放大",
            Self::ZoomOut => "缩小",
            Self::RotateClockwise => "顺时针旋转 90°",
            Self::RotateCounterClockwise => "逆时针旋转 90°",
            Self::FlipHorizontal => "水平翻转",
            Self::FlipVertical => "垂直翻转",
            Self::ToggleInfoPanel => "EXIF 信息面板",
            Self::ToggleFullscreen => "全屏",
            Self::SkinFollowSystem => "跟随系统",
            Self::SkinDark => "深色",
            Self::SkinLight => "浅色",
            Self::AssociateFormats => "设置关联格式…",
            Self::Quit => "退出",
        }
    }

    /// 没有打开图片时，这个动作是否还有意义。
    ///
    /// 「打开」「全屏」「皮肤」「设置关联格式」「退出」不依赖当前文档；其余动作都作用在图像上，没有图的时候
    /// 点它们只会静默地什么都不发生 —— 那比显示成灰色更让人困惑，因此菜单里这些项
    /// 会被渲染成禁用态。键盘入口在视图里用同一个判断挡一次，两条入口不会走偏。
    pub fn needs_image(self) -> bool {
        !matches!(
            self,
            // 皮肤是「窗口长什么样」，与画布内容无关：空状态下一样该能改。
            // 文件关联改的是「这个程序认识哪些文件」，同样与当前文档无关。
            Self::Open
                | Self::ToggleFullscreen
                | Self::SkinFollowSystem
                | Self::SkinDark
                | Self::SkinLight
                | Self::AssociateFormats
                | Self::Quit
        )
    }

    /// 是否只在**多页文档**里才有意义。
    ///
    /// 与 [`needs_image`](Self::needs_image) 是独立的一维：那个问「现在有没有图」，
    /// 这个问「这张图有没有别的页」。混进一个判断会让单页图片上的「下一页」
    /// 也亮着 —— 点下去什么都不发生，比灰着更让人困惑；而「灰色」在这里
    /// 恰好是一个有用的信息：这份文件就一页。
    pub fn needs_pages(self) -> bool {
        matches!(self, Self::PreviousPage | Self::NextPage)
    }

    /// 当前平台上这个动作做不做得到。
    ///
    /// 与 [`needs_image`](Self::needs_image) 分开：那个问的是「现在有没有可作用的对象」，
    /// 这个问的是「这台机器上这件事根本做不做得到」。前者会随文档状态变化，
    /// 后者在进程的生命周期里是恒定的 —— 混成一个判断会让菜单在两种灰之间失去区别。
    ///
    /// 目前只有文件关联受限：它读写 Windows 注册表，别的系统还没有实现
    /// （macOS 要在打包时声明文档类型，Linux 要写 `.desktop` + `xdg-mime`）。
    /// 做不到的项置灰，比点下去再弹一句「做不到」早一步告诉用户。
    pub fn is_available(self) -> bool {
        match self {
            Self::AssociateFormats => crate::fs_ops::associations::is_supported(),
            _ => true,
        }
    }

    /// 菜单右侧的快捷键提示；没有绑定按键时返回空串。
    ///
    /// 从这里取而不是在菜单里手写字符串，是这一层存在的全部理由：
    /// 文案与[按键表](KEY_BINDINGS)是同一份数据的两种呈现，不可能对不上。
    pub fn shortcut_label(self) -> String {
        match KEY_BINDINGS.iter().find(|binding| binding.command == self) {
            Some(binding) if binding.control => {
                format!("{}{}", primary_modifier(), binding.display)
            }
            Some(binding) => binding.display.to_string(),
            None => String::new(),
        }
    }

    /// 这个菜单项代表的皮肤选择，是否就是当前生效的那一个。
    ///
    /// 皮肤是**三选一**（跟随系统 / 深色 / 浅色），菜单需要把当前生效的那项标出来，
    /// 否则用户点了「深色」之后无法确认到底生效了没有 —— 而深色与浅色的区别
    /// 本身就够明显，反倒是「我到底是在跟随系统还是固定深色」看不出来。
    ///
    /// 非皮肤命令一律返回假：它们没有「选中」这个状态。
    ///
    /// 参数用 `crate::fs_ops::Preference` 而不是这里的类型 —— 偏好的**存储形态**
    /// 归 `fs_ops` 管，本模块只回答「这一项是不是当前那项」。
    pub fn is_selected_skin(self, preference: crate::fs_ops::Preference) -> bool {
        use crate::fs_ops::Preference;
        match self {
            Self::SkinFollowSystem => preference == Preference::System,
            Self::SkinDark => preference.matches(crate::ui::theme::Polarity::Dark),
            Self::SkinLight => preference.matches(crate::ui::theme::Polarity::Light),
            _ => false,
        }
    }
}

/// 主修饰键在菜单里的写法。
///
/// Windows / Linux 用 `Ctrl`，macOS 按系统惯例用 `⌘`。按键表里只记「要不要按主修饰键」，
/// 具体写成什么字交给这里 —— 否则每条绑定都要为两个平台各写一份文案。
fn primary_modifier() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl+"
    }
}

/// 键序列里的一个节点：具体动作，或一条分隔线。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    Item(Command),
    /// 分组用的横线。分组的意义是让用户更快找到「那一类」动作，
    /// 而不是把动作排成一长串等距的条目。
    Separator,
}

/// 一个菜单。
pub struct Menu {
    pub label: &'static str,
    pub entries: &'static [Entry],
}

/// 菜单栏的结构。
///
/// 四个菜单，每一项都真的能执行：这一行占的是图像的纵向空间，
/// 塞进「帮助」却点不出帮助内容（本产品没有独立窗口去承载）只会让这条栏变得更廉价。
///
/// 分工按「这个动作作用在什么上」划：**文件 / 编辑 / 视图** 作用于当前这张图，
/// **工具** 作用于程序自身（现在只有文件关联）。把程序设置混进「文件」菜单会让
/// 那个菜单变得不可预测 —— 里面每一项都该是「对现在这张图做点什么」。
pub const MENUS: &[Menu] = &[
    Menu {
        label: "文件",
        entries: &[
            Entry::Item(Command::Open),
            Entry::Item(Command::SaveAs),
            Entry::Item(Command::Rename),
            Entry::Item(Command::DeleteToTrash),
            Entry::Separator,
            Entry::Item(Command::Quit),
        ],
    },
    Menu {
        label: "编辑",
        entries: &[Entry::Item(Command::CopyToClipboard)],
    },
    Menu {
        label: "视图",
        entries: &[
            // 翻页放在最前：一份 30 页的扫描件打开之后，用户接下来要做的事就是往下翻。
            // 这两项只在多页文档里才亮（见 `Command::needs_pages`）——
            // 单页图片上它们灰着，那本身也是「这份文件只有一页」的一个说明。
            Entry::Item(Command::PreviousPage),
            Entry::Item(Command::NextPage),
            Entry::Separator,
            Entry::Item(Command::FitToWindow),
            Entry::Item(Command::ActualSize),
            Entry::Separator,
            Entry::Item(Command::ZoomIn),
            Entry::Item(Command::ZoomOut),
            Entry::Separator,
            Entry::Item(Command::RotateClockwise),
            Entry::Item(Command::RotateCounterClockwise),
            Entry::Item(Command::FlipHorizontal),
            Entry::Item(Command::FlipVertical),
            Entry::Separator,
            Entry::Item(Command::ToggleInfoPanel),
            Entry::Separator,
            // 皮肤三项是一组互斥的单选：放在同一个分组里，与上面的命令动作分开。
            // 「跟随系统」排第一，因为它是默认值 —— 用户进来第一眼要看到的是
            // 「现在是什么状态」，而不是「我可以改成什么」。
            Entry::Item(Command::SkinFollowSystem),
            Entry::Item(Command::SkinDark),
            Entry::Item(Command::SkinLight),
            Entry::Separator,
            Entry::Item(Command::ToggleFullscreen),
        ],
    },
    Menu {
        label: "工具",
        entries: &[Entry::Item(Command::AssociateFormats)],
    },
];

/// 一条快捷键绑定。
pub struct Binding {
    /// 会被解析成这个动作的按键。一个动作可以有多个等价键
    /// （`=` 与 `+` 在多数键盘上是同一个物理键的上下档）。
    pub keys: &'static [&'static str],
    /// 是否要求按住主修饰键（Ctrl / ⌘）。
    pub control: bool,
    /// 是否**要求**按住 Shift。
    ///
    /// 为假只表示「不要求」，不表示「不允许」：`+` 在物理上就是 Shift+`=`，
    /// 若把不要求当成不允许，放大就只有在数字键盘上才按得出来。
    pub shift: bool,
    /// 菜单右侧显示的按键名（不含主修饰键，它由 [`primary_modifier`] 补上）。
    pub display: &'static str,
    pub command: Command,
}

/// 全部快捷键。**这是唯一的事实来源**：键盘分发与菜单提示都读它。
pub const KEY_BINDINGS: &[Binding] = &[
    Binding {
        keys: &["o"],
        control: true,
        shift: false,
        display: "O",
        command: Command::Open,
    },
    // 方向键 = 同目录导航，上下与左右四个键都认。这是看图器的本能操作：看第一张
    // 之前手已经放在了方向键上，所以不带任何修饰键。两对方向键是**并存**而不是
    // 谁取代谁 —— 竖着看图时手习惯按上下，横排版式与单手操作时习惯按左右；
    // 砍掉一半只会让那一半人的肌肉记忆失灵，而代价只是 `keys` 里多一个键名。
    // 邻居列表在后台线程里异步准备，没就绪时按下会有提示（见 view.rs 的 open_neighbor）。
    Binding {
        keys: &["up", "left"],
        control: false,
        shift: false,
        display: "↑ / ←",
        command: Command::PreviousFile,
    },
    Binding {
        keys: &["down", "right"],
        control: false,
        shift: false,
        display: "↓ / →",
        command: Command::NextFile,
    },
    // 翻页与换文件是两条独立的轴，所以用两对不同的键：方向键换文件，
    // PageUp / PageDown 翻页。合并成一个动作会把「看下一张图」与
    // 「看这份扫描件的下一页」混为一谈，而这两件事的期待完全不同 ——
    // 前者要换掉整份文档，后者只是同一份文档的下一页。
    Binding {
        keys: &["pageup"],
        control: false,
        shift: false,
        display: "PageUp",
        command: Command::PreviousPage,
    },
    Binding {
        keys: &["pagedown"],
        control: false,
        shift: false,
        display: "PageDown",
        command: Command::NextPage,
    },
    Binding {
        keys: &["s"],
        control: true,
        shift: false,
        display: "S",
        command: Command::SaveAs,
    },
    Binding {
        keys: &["c"],
        control: true,
        shift: false,
        display: "C",
        command: Command::CopyToClipboard,
    },
    // 模式切换同时支持带修饰键与不带：前者是「记住了快捷键」的人用的，
    // 后者是「随手试一下」的人用的，两个都被原有交互验证过，一并保留。
    Binding {
        keys: &["0"],
        control: true,
        shift: false,
        display: "0",
        command: Command::FitToWindow,
    },
    Binding {
        keys: &["1"],
        control: true,
        shift: false,
        display: "1",
        command: Command::ActualSize,
    },
    Binding {
        keys: &["0"],
        control: false,
        shift: false,
        display: "0",
        command: Command::FitToWindow,
    },
    Binding {
        keys: &["1"],
        control: false,
        shift: false,
        display: "1",
        command: Command::ActualSize,
    },
    Binding {
        keys: &["=", "+"],
        control: false,
        shift: false,
        display: "+",
        command: Command::ZoomIn,
    },
    Binding {
        keys: &["-", "_"],
        control: false,
        shift: false,
        display: "−",
        command: Command::ZoomOut,
    },
    Binding {
        keys: &["r"],
        control: false,
        shift: false,
        display: "R",
        command: Command::RotateClockwise,
    },
    Binding {
        keys: &["r"],
        control: false,
        shift: true,
        display: "Shift+R",
        command: Command::RotateCounterClockwise,
    },
    Binding {
        keys: &["h"],
        control: false,
        shift: false,
        display: "H",
        command: Command::FlipHorizontal,
    },
    Binding {
        keys: &["v"],
        control: false,
        shift: false,
        display: "V",
        command: Command::FlipVertical,
    },
    Binding {
        keys: &["i"],
        control: false,
        shift: false,
        display: "I",
        command: Command::ToggleInfoPanel,
    },
    Binding {
        keys: &["f11"],
        control: false,
        shift: false,
        display: "F11",
        command: Command::ToggleFullscreen,
    },
];

/// 把一个按键事件解成动作。
///
/// 匹配顺序是**先看要求按住 Shift 的绑定，再看其余**，这一条不能反：
/// `Shift+R` 与 `R` 是两个相反的动作，若先匹配不要求 Shift 的那条，
/// `Shift+R` 会被 `R` 抢走，用户看到的是「转了反方向」——而且看起来只是有点别扭，
/// 极难自查。同理，「不要求 Shift」的绑定对 Shift 是**不敏感**的，
/// 否则 `+`（物理上等于 Shift+`=`）会匹配不到放大。
pub fn command_for_keystroke(key: &str, control: bool, shift: bool) -> Option<Command> {
    // 按键名统一小写再比：平台给的是 `R` / `F11` 这类写法，表里存的是小写。
    let key = key.to_ascii_lowercase();
    for requires_shift in [true, false] {
        // 第一轮只放行「确实按住了 Shift」的情况，第二轮才轮到对其余按键。
        if requires_shift && !shift {
            continue;
        }
        for binding in KEY_BINDINGS {
            if binding.shift != requires_shift || binding.control != control {
                continue;
            }
            if binding.keys.contains(&key.as_str()) {
                return Some(binding.command);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 菜单里出现的每个动作。
    fn menu_commands() -> Vec<Command> {
        MENUS
            .iter()
            .flat_map(|menu| menu.entries.iter())
            .filter_map(|entry| match entry {
                Entry::Item(command) => Some(*command),
                Entry::Separator => None,
            })
            .collect()
    }

    #[test]
    fn every_declared_key_combination_resolves_to_its_own_command() {
        for binding in KEY_BINDINGS {
            for key in binding.keys {
                let resolved = command_for_keystroke(key, binding.control, binding.shift);
                assert_eq!(
                    resolved,
                    Some(binding.command),
                    "按键 {key}（control={}, shift={}）应触发 {:?}",
                    binding.control,
                    binding.shift,
                    binding.command
                );
            }
        }
    }

    #[test]
    fn shifted_keys_win_over_their_unmodified_twins() {
        // 这两条是「看起来只是有点别扭」里最典型的一类：转反了方向。
        // 它必须由解析顺序保证，而不是由绑定表的书写顺序碰巧保证。
        assert_eq!(
            command_for_keystroke("r", false, true),
            Some(Command::RotateCounterClockwise)
        );
        assert_eq!(
            command_for_keystroke("R", false, false),
            Some(Command::RotateClockwise)
        );
        // 大写字母由平台原样给出，解析必须自己归一化。
        assert_eq!(
            command_for_keystroke("R", false, true),
            Some(Command::RotateCounterClockwise)
        );
    }

    #[test]
    fn unmodified_bindings_tolerate_an_incidental_shift() {
        // `+` 在多数键盘上是 Shift+`=`：不要求 Shift 不等于不允许 Shift，
        // 否则「放大」只有在小键盘上才按得出来。
        assert_eq!(command_for_keystroke("+", false, true), Some(Command::ZoomIn));
        assert_eq!(command_for_keystroke("=", false, false), Some(Command::ZoomIn));
        // `_` 是 Shift+`-`，同理。
        assert_eq!(command_for_keystroke("_", false, true), Some(Command::ZoomOut));
    }

    /// 同目录导航四个方向键都通：上下与左右是同一对动作的两个入口。
    ///
    /// 这条断言把「四个键都认」钉死，防的是将来有人把左右当成「冗余别名」删掉 ——
    /// 删掉的后果不会表现在编译期，而是某天有人横着看图时按了左右、画面毫无反应，
    /// 然后去怀疑邻居列表坏了。
    #[test]
    fn both_horizontal_and_vertical_arrows_navigate_neighbors() {
        assert_eq!(
            command_for_keystroke("up", false, false),
            Some(Command::PreviousFile)
        );
        assert_eq!(
            command_for_keystroke("left", false, false),
            Some(Command::PreviousFile)
        );
        assert_eq!(
            command_for_keystroke("down", false, false),
            Some(Command::NextFile)
        );
        assert_eq!(
            command_for_keystroke("right", false, false),
            Some(Command::NextFile)
        );
    }

    #[test]
    fn no_two_bindings_claim_the_same_keystroke() {
        let mut seen: Vec<(&str, bool, bool)> = Vec::new();
        for binding in KEY_BINDINGS {
            for key in binding.keys {
                let combo = (*key, binding.control, binding.shift);
                assert!(
                    !seen.contains(&combo),
                    "按键 {key}（control={}, shift={}）被绑定了两次",
                    binding.control,
                    binding.shift
                );
                seen.push(combo);
            }
        }
    }

    #[test]
    fn every_menu_item_advertising_a_shortcut_really_has_one() {
        for command in menu_commands() {
            let advertised = command.shortcut_label();
            if advertised.is_empty() {
                continue;
            }
            assert!(
                KEY_BINDINGS.iter().any(|binding| binding.command == command),
                "{:?} 在菜单里显示「{advertised}」，但按键表里没有它",
                command
            );
        }

        // 反向：所有动作里只有这些没有快捷键 —— 前两个要弹对话框 / 有破坏性，
        // 「退出」交给系统的 Alt+F4，皮肤三项与文件关联是低频的一次性设置
        // （系统改了主题自己会跟随，没必要时时按；文件关联更是装完设一次就不动）。
        // 这条断言的作用是把「哪些动作没有快捷键」变成一个需要显式改动的决定。
        //
        // 顺序按**菜单里出现的先后**（`menu_commands` 逐菜单收集）：「退出」在
        // 「文件」菜单末尾，所以排在「视图」菜单里的皮肤三项与「工具」菜单的文件关联之前。
        let without_shortcut: Vec<Command> = menu_commands()
            .into_iter()
            .filter(|command| command.shortcut_label().is_empty())
            .collect();
        assert_eq!(
            without_shortcut,
            vec![
                Command::Rename,
                Command::DeleteToTrash,
                Command::Quit,
                Command::SkinFollowSystem,
                Command::SkinDark,
                Command::SkinLight,
                Command::AssociateFormats
            ]
        );
    }

    #[test]
    fn window_level_actions_do_not_require_an_open_image() {
        // 这些分支是菜单禁用态的唯一依据，写错的表现是「没打开图片时连打开都点不动」，
        // 或者反过来「空窗口下能点深色但点了没反应」。
        for command in menu_commands() {
            let expected = !matches!(
                command,
                Command::Open
                    | Command::ToggleFullscreen
                    | Command::SkinFollowSystem
                    | Command::SkinDark
                    | Command::SkinLight
                    | Command::AssociateFormats
                    | Command::Quit
            );
            assert_eq!(command.needs_image(), expected, "{command:?}");
        }
    }

    /// 「本平台做不做得到」是独立的一维：它不随文档状态变化，
    /// 因此除了明确受限的那一项，其余动作在任何平台都必须可用。
    ///
    /// 这条断言防的是「顺手把 is_available 写成 needs_image 的别名」——
    /// 那会让菜单在两种灰之间失去区别，用户无从判断「先打开一张图就行」
    /// 还是「这件事在这台机器上永远做不到」。
    #[test]
    fn platform_availability_is_separate_from_the_document_state() {
        for command in menu_commands() {
            let expected = match command {
                Command::AssociateFormats => crate::fs_ops::associations::is_supported(),
                _ => true,
            };
            assert_eq!(command.is_available(), expected, "{command:?}");
        }
    }

    /// 皮肤三项正好覆盖三种偏好，且**每个偏好恰有一项被标为选中**。
    ///
    /// 这条断言防的是两类错：
    /// - 新增了一个偏好变体却忘了加菜单项（用户无法切换到它）；
    /// - `is_selected_skin` 的分支写漏或写重（菜单上出现两个 `●` 或一个都没有）。
    ///   后者是「我到底在跟随系统还是固定深色」这个疑问的直接来源。
    #[test]
    fn the_three_skin_items_are_mutually_exclusive_and_cover_every_preference() {
        use crate::fs_ops::Preference;
        use crate::ui::theme::Polarity;

        let skin_commands = [
            Command::SkinFollowSystem,
            Command::SkinDark,
            Command::SkinLight,
        ];

        // 三个偏好各自恰好命中一项。
        for preference in [
            Preference::System,
            Preference::Fixed(Polarity::Dark),
            Preference::Fixed(Polarity::Light),
        ] {
            let marked: Vec<Command> = skin_commands
                .into_iter()
                .filter(|command| command.is_selected_skin(preference))
                .collect();
            assert_eq!(
                marked.len(),
                1,
                "{preference:?} 应当恰好有一项被标为选中，实际是 {marked:?}"
            );
        }

        // 非皮肤命令永远不参与选中态，否则菜单里会冒出莫名其妙的 `●`。
        for command in menu_commands() {
            if skin_commands.contains(&command) {
                continue;
            }
            for preference in [
                Preference::System,
                Preference::Fixed(Polarity::Dark),
                Preference::Fixed(Polarity::Light),
            ] {
                assert!(
                    !command.is_selected_skin(preference),
                    "{command:?} 不是皮肤项，不该被标为选中"
                );
            }
        }
    }

    #[test]
    fn menu_bar_is_shallow_and_correctly_grouped() {
        assert_eq!(MENUS.len(), 4);
        for menu in MENUS {
            assert!(!menu.label.is_empty());
            assert!(!menu.entries.is_empty(), "{} 菜单是空的", menu.label);
            // 首尾不能是分隔线。
            assert!(!matches!(menu.entries.first(), Some(Entry::Separator)));
            assert!(!matches!(menu.entries.last(), Some(Entry::Separator)));
            // 不允许连续两条分隔线（表现为一条特别粗的线）。
            for pair in menu.entries.windows(2) {
                assert!(
                    !matches!(pair, [Entry::Separator, Entry::Separator]),
                    "{} 菜单里有连续的分隔线",
                    menu.label
                );
            }
        }
        let labels: Vec<&str> = MENUS.iter().map(|menu| menu.label).collect();
        assert_eq!(labels, vec!["文件", "编辑", "视图", "工具"]);
    }

    /// 翻页命令必须真的绑在 PageUp / PageDown 上，且只在多页文档里才有意义。
    ///
    /// 「需要翻页」与「需要图片」是两维：混在一起会让单页图片上的「下一页」
    /// 也亮着 —— 点下去什么都不发生，比灰着更让人困惑。
    #[test]
    fn page_commands_are_bound_to_page_keys_and_need_a_paged_document() {
        assert_eq!(
            command_for_keystroke("pageup", false, false),
            Some(Command::PreviousPage)
        );
        assert_eq!(
            command_for_keystroke("pagedown", false, false),
            Some(Command::NextPage)
        );
        // 带修饰键时不该触发：翻页是全键盘里最不该跟 Ctrl 组合的动作。
        assert_eq!(command_for_keystroke("pagedown", true, false), None);

        for command in menu_commands() {
            let expected = matches!(command, Command::PreviousPage | Command::NextPage);
            assert_eq!(command.needs_pages(), expected, "{command:?}");
            if expected {
                assert!(
                    command.needs_image(),
                    "{command:?} 说的是「这张图的下一页」，当然要先有图"
                );
            }
        }
    }

    /// 「工具」菜单只放程序自身的设置，不放作用于当前图片的动作。
    ///
    /// 这条断言把菜单的**语义归属**钉住：一条命令该进哪个菜单是设计决定，
    /// 不该因为「顺手」而漂移 —— 用户找「双击用什么打开」时，
    /// 会去「工具/设置」，不会去「视图」。
    #[test]
    fn the_tools_menu_holds_program_settings_only() {
        let tools = MENUS
            .iter()
            .find(|menu| menu.label == "工具")
            .expect("应当有一个「工具」菜单");
        let items: Vec<Command> = tools
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Item(command) => Some(*command),
                Entry::Separator => None,
            })
            .collect();
        assert_eq!(items, vec![Command::AssociateFormats]);
        // 程序设置与文档无关 —— 空窗口下它也必须能点。
        for command in items {
            assert!(!command.needs_image(), "{command:?} 不该要求先打开图片");
        }
    }
}
