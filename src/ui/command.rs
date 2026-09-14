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
    /// 同目录里的上一个图片（↑）。
    PreviousFile,
    /// 同目录里的下一个图片（↓）。
    NextFile,
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
    Quit,
}

impl Command {
    /// 菜单里显示的名字。带省略号的表示点下去还会再弹一个对话框。
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "打开…",
            Self::PreviousFile => "上一个文件",
            Self::NextFile => "下一个文件",
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
            Self::Quit => "退出",
        }
    }

    /// 没有打开图片时，这个动作是否还有意义。
    ///
    /// 「打开」「全屏」「退出」不依赖当前文档；其余动作都作用在图像上，没有图的时候
    /// 点它们只会静默地什么都不发生 —— 那比显示成灰色更让人困惑，因此菜单里这些项
    /// 会被渲染成禁用态。键盘入口在视图里用同一个判断挡一次，两条入口不会走偏。
    pub fn needs_image(self) -> bool {
        !matches!(self, Self::Open | Self::ToggleFullscreen | Self::Quit)
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
/// 只有三个菜单，且每一项都真的能执行：这一行占的是图像的纵向空间，
/// 塞进「帮助」却点不出帮助内容（本产品没有独立窗口去承载）只会让这条栏变得更廉价。
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
            Entry::Item(Command::ToggleFullscreen),
        ],
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
    // 上下方向键 = 同目录导航。这是看图器的本能操作：看第一张之前手已经
    // 放在了方向键上，所以不带任何修饰键。邻居列表在后台线程里异步准备，
    // 没就绪时按下会有提示（见 view.rs 的 open_neighbor）。
    Binding {
        keys: &["up"],
        control: false,
        shift: false,
        display: "↑",
        command: Command::PreviousFile,
    },
    Binding {
        keys: &["down"],
        control: false,
        shift: false,
        display: "↓",
        command: Command::NextFile,
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

        // 反向：所有动作里只有「重命名」「移到回收站」「退出」没有快捷键 ——
        // 前两个要弹对话框 / 有破坏性，后者交给系统的 Alt+F4。
        // 这条断言的作用是把「哪些动作没有快捷键」变成一个需要显式改动的决定。
        let without_shortcut: Vec<Command> = menu_commands()
            .into_iter()
            .filter(|command| command.shortcut_label().is_empty())
            .collect();
        assert_eq!(
            without_shortcut,
            vec![
                Command::Rename,
                Command::DeleteToTrash,
                Command::Quit
            ]
        );
    }

    #[test]
    fn window_level_actions_do_not_require_an_open_image() {
        // 这三分支是菜单禁用态的唯一依据，写错的表现是「没打开图片时连打开都点不动」。
        for command in menu_commands() {
            let expected = !matches!(
                command,
                Command::Open | Command::ToggleFullscreen | Command::Quit
            );
            assert_eq!(command.needs_image(), expected, "{command:?}");
        }
    }

    #[test]
    fn menu_bar_is_shallow_and_correctly_grouped() {
        assert_eq!(MENUS.len(), 3);
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
        assert_eq!(labels, vec!["文件", "编辑", "视图"]);
    }
}
