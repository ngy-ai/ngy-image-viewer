//! 文件关联：把本程序登记为图片扩展名的打开方式。
//!
//! # 这一层解决什么问题
//!
//! 「设置关联格式，之后双击图片就用本程序打开」。它碰的是 Windows 注册表，
//! 因而和其它文件能力一样：**要么成功，要么给出一个能直接展示给用户的错误**。
//!
//! # 生效规则（实测得到，不是照文档推测的）
//!
//! Windows 决定「双击一个扩展名用哪个程序」，按下面的优先级取第一个非空的：
//!
//! ```text
//! 1. HKCU\...\Explorer\FileExts\.<ext>\UserChoice      ← 有哈希保护，程序写不进去
//! 2. HKCU\Software\Classes\.<ext> 的默认值             ← 我们能写
//! 3. HKLM\Software\Classes\.<ext> 的默认值             ← 需要管理员，我们不碰
//! ```
//!
//! 在一台真实机器上（58 个候选扩展名）实测的结论：
//!
//! | 现象 | 实测证据 |
//! | --- | --- |
//! | 第 2 条写得进去，确实生效 | `.qoi` / `.jxl` 本来是「系统问你要用哪个程序」，写入后立刻变成我们指定的程序 |
//! | **第 1 条存在时，第 2 条被完全压住** | `.png`（被 WPS 占着）、`.heic`（被美图占着）写入后系统仍然用原来那个 |
//! | 本机 **26 / 58 个扩展名带 UserChoice**（44%） | 见 `read_all` 在真实机器上的输出 |
//! | 覆盖第 2 条会顶掉别的来源，**必须备份才能还原** | `.jp2` 原本由 SumatraPDF 认领（经 `OpenWithProgids`），写入后掉成了「没有程序认领」 |
//!
//! 所以「设置之后一定双击就用本程序」**做不到全量** —— 这是 Windows 的设计，
//! 不是实现缺陷。本模块的应对是把三件事都做全，并如实表达：
//!
//! 1. 让程序出现在「打开方式」列表与系统「默认应用」的候选里（这部分 100% 有效）；
//! 2. 对**没有 UserChoice** 的扩展名直接写默认值，双击立即生效；
//! 3. 被 UserChoice 压住的那些，在界面上如实标成「已登记，但系统默认是别的程序」，
//!    而不是假装设置成功了 —— 用户可以据此去系统设置里改，那时程序已经在候选列表里。
//!
//! # 「去系统设置里改」这一步，由程序直接送达
//!
//! 被 UserChoice 压住的扩展名，程序**改不动**。这不是实现缺陷，是系统的设计：
//! 用户的默认选择存在 `FileExts\.<ext>\UserChoice`，值带哈希校验；自 2024 年 2 月
//! 累积更新起，内核驱动 `UCPD.sys`（User Choice Protection Driver）还会直接拦截
//! 对它的写入。微软的官方口径是「默认程序只能在系统 UI 里由用户改」。
//!
//! 所以能做的最后一步是把用户**送到**那个 UI 上，并直接翻到本程序那一页：
//! [`open_defaults_settings`] 打开 `ms-settings:defaultapps?registeredAppUser=…`，
//! 用户在那里点一次「设置默认值」，全部格式一次性切过来。这是受支持范围内
//! 能做到的极限，也是浏览器们如今的标准做法（它们同样只能把用户领到这一页）。
//!
//! 覆盖前会把原来的默认值备份到 `HKCU\Software\ngy-image-viewer\PreviousDefaults`，
//! 取消关联时原样还回去（WPS 抢关联时在 `.png` 下留的 `ksobak` 值也是同一套思路）。
//! 因此「关联 → 取消关联」是一个可逆操作。
//!
//! # 为什么用 winreg 而不是手写 FFI
//!
//! 项目的 `unsafe` 只允许出现在 `decode/wic.rs`。`winreg` 是纯安全 Rust 的注册表封装，
//! 用它就不必为了几十个 Win32 调用新开一处 `unsafe`。
//!
//! # 线程边界
//!
//! 读一次全部候选扩展名会做上百次注册表查询，写一个扩展名会做几次写入 ——
//! 都在毫秒级，但它们**不该出现在渲染循环里**。调用方（`ui/view.rs`）只在
//! 用户主动打开面板、主动勾选时调用，不在每帧路径上。

use std::fmt;

/// 本程序在注册表里的 ProgID。所有扩展名的默认值最终指向它。
///
/// 用包名而不是另取一个名字：`Reload` 时它必须与 `[[bin]]` 的名字一致，
/// 而注册表里还有一处按 exe 文件名登记的项（见 [`APP_EXE`]），两处对不上
/// 会让「打开方式」列表里出现一个点不动的条目。
const PROG_ID: &str = "ngy-image-viewer.image";

/// 包名，同时就是可执行文件名（`Cargo.toml` 的 `[[bin]] name`）。
const APP_EXE: &str = env!("CARGO_PKG_NAME");

/// 一个扩展名相对本程序的状态。**三态**，因为界面上要表达三种不同的处境。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AssocState {
    /// 没登记过。这个格式与本程序没关系。
    None,
    /// 已登记为打开方式，但系统当前用的是别的程序 —— 用户还需要去系统设置里切。
    ///
    /// 最常见的成因是 [`UserChoice`](self) 指向了别的程序（本机 44% 的扩展名如此）。
    Registered,
    /// 系统当前就用本程序打开。双击立即生效。
    Default,
}

impl AssocState {
    /// 界面上给用户看的说法。用「已关联 / 已登记 / 未关联」三个不同词，
    /// 而不是「已/否」两个 —— 后者会把「还差一步」和「完全没做」混成一样。
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "未关联",
            Self::Registered => "已登记",
            Self::Default => "已关联",
        }
    }

    /// 是否算「勾上」了。两种已登记的状态都算：用户点一下的意图是「让这个程序处理它」，
    /// 我们已经尽力了，剩下的一步（系统设置里切换）不该让勾选看起来像没生效。
    pub fn is_marked(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// 一个扩展名的关联现状。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Association {
    /// 不带点的小写扩展名，例如 `png`。
    pub extension: String,
    pub state: AssocState,
}

impl Association {
    /// 带点的写法，用于日志与界面。
    pub fn dotted(&self) -> String {
        format!(".{}", self.extension)
    }
}

/// 关联操作的失败类型。分类标准与 [`crate::fs_ops::FileOpError`] 一致：
/// 用户看到之后该做的事不同，就必须是不同的变体。
#[derive(Clone, Debug)]
pub enum AssocError {
    /// 当前系统不支持在程序内改文件关联。
    ///
    /// Windows 之外只有 macOS 与 Linux：前者必须打包时在 `Info.plist` 里声明
    /// 文档类型（运行时改不了），后者要写 `.desktop` 并调 `xdg-mime`。
    /// 两者都还没做，与其假装成功，不如明说。
    UnsupportedPlatform,
    /// 取不到自己的可执行文件路径 —— 注册表里要写绝对路径，没有它无从下手。
    ExecutableUnavailable { message: String },
    /// 注册表读写失败（权限、键被锁、值类型不符）。
    Registry {
        /// 出问题的扩展名（不带点）。整体登记失败时为 `None`。
        extension: Option<String>,
        message: String,
    },
    /// 打不开系统「默认应用」设置页（一般是 URI 处理器起不来）。
    ///
    /// 与 [`Registry`](Self::Registry) 分开：那一个是「注册表没写进去」，
    /// 这一个是「写进去了、但没能把用户领到系统 UI 上」，用户该做的事不一样。
    LaunchFailed { message: String },
}

impl AssocError {
    /// 面向用户：说清发生了什么 + 下一步能做什么。
    pub fn user_message(&self) -> String {
        match self {
            Self::UnsupportedPlatform => {
                "当前系统不支持在程序里修改文件关联。macOS 需要在打包时声明支持的文档类型；\
                 Linux 可以手动写 .desktop 文件并调用 xdg-mime。"
                    .to_string()
            }
            Self::ExecutableUnavailable { .. } => {
                "无法确定本程序的位置，因此没法写入文件关联。请从安装目录启动本程序后重试。"
                    .to_string()
            }
            Self::LaunchFailed { .. } => {
                "无法自动打开系统「默认应用」设置页。请手动打开「设置 → 应用 → 默认应用」，\
                 找到本程序那一项后点「设置默认值」。"
                    .to_string()
            }
            Self::Registry {
                extension: Some(extension),
                ..
            } => format!(
                "写入 .{extension} 的关联时失败。可能是注册表被安全软件保护了，\
                 稍后重试或改用系统「默认应用」设置。"
            ),
            Self::Registry {
                extension: None, ..
            } => "写入文件关联失败。可能是注册表被安全软件保护了，稍后重试即可。".to_string(),
        }
    }

    /// 面向日志：短、单行、不引导用户。
    pub fn short_reason(&self) -> String {
        match self {
            Self::UnsupportedPlatform => format!(
                "unsupported platform ({})",
                std::env::consts::OS
            ),
            Self::ExecutableUnavailable { message } => {
                format!("current_exe unavailable: {message}")
            }
            Self::LaunchFailed { message } => {
                format!("open default-apps settings: {message}")
            }
            Self::Registry {
                extension,
                message,
            } => match extension {
                Some(extension) => format!("registry .{extension}: {message}"),
                None => format!("registry: {message}"),
            },
        }
    }
}

impl fmt::Display for AssocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 与其它层一致：Display 走 short_reason，日志里不会出现面向用户的引导语。
        f.write_str(&self.short_reason())
    }
}

impl std::error::Error for AssocError {}

/// 当前平台是否支持在程序内改文件关联。
///
/// 界面用它决定菜单项可不可点：不支持时置灰，比点下去弹一句「做不到」更早一步。
pub fn is_supported() -> bool {
    cfg!(windows)
}

/// 系统「默认应用」设置页的深链，直接定位到本程序。
///
/// `?registeredAppUser=` 后面跟的是 `HKCU\Software\RegisteredApplications` 里的
/// **值名**（不是数据）。我们写进去的值名就是 [`APP_EXE`]，所以这里直接用它。
///
/// 该查询参数自 Windows 11 21H2 / 22H2 的 2023-04 累积更新、以及 23H2 及以后可用；
/// 更早的系统会忽略它，退化成「默认应用」列表页 —— 不会出错，只是少走一步。
///
/// 值名不做 URI 转义：crate 名只含 `[A-Za-z0-9._-]`，天然是 URI 安全字符
/// （有测试钉住这个前提，免得将来换成带空格的名字而悄悄破坏深链）。
pub fn settings_page_uri() -> String {
    format!("ms-settings:defaultapps?registeredAppUser={APP_EXE}")
}

/// 打开系统「默认应用」设置页并定位到本程序。
///
/// 先把本程序登记齐（幂等）再跳转：设置页里本程序那一页只会列出
/// `Capabilities\FileAssociations` 里登记过的格式，漏登记的格式即使点了
/// 「设置默认值」也切不过来。
///
/// 这是「让本程序成为双击打开方式」在受支持范围内的最后一步，理由见模块文档。
pub fn open_defaults_settings() -> Result<(), AssocError> {
    #[cfg(windows)]
    {
        windows_impl::open_defaults_settings()
    }
    #[cfg(not(windows))]
    {
        Err(AssocError::UnsupportedPlatform)
    }
}

/// 候选扩展名 = 「打开」对话框认识的那一份。
///
/// 刻意复用同一份列表而不是另立一套：两处一旦分叉，就会出现
/// 「能打开却关联不了」或「关联了却打不开」这种自相矛盾的状态。
pub fn candidates() -> &'static [&'static str] {
    crate::fs_ops::file_ops::IMAGE_EXTENSIONS
}

/// 读一个扩展名的现状。
///
/// 判定用的三个输入就是上面「生效规则」里的那三级优先级，外加
/// 「我们有没有出现在 `OpenWithProgids` 里」——后者决定了「已登记」这一态。
pub fn read_one(extension: &str) -> Association {
    #[cfg(windows)]
    let state = windows_impl::read_state(extension);
    #[cfg(not(windows))]
    let state = {
        let _ = extension;
        AssocState::None
    };

    Association {
        extension: extension.to_ascii_lowercase(),
        state,
    }
}

/// 读全部候选扩展名的现状。
///
/// 不支持关联的平台返回 [`AssocError::UnsupportedPlatform`]，
/// 而不是一个空列表 —— 空列表会被界面画成「全都未关联」，那是在撒谎。
pub fn read_all() -> Result<Vec<Association>, AssocError> {
    #[cfg(windows)]
    {
        Ok(candidates()
            .iter()
            .map(|extension| read_one(extension))
            .collect())
    }
    #[cfg(not(windows))]
    {
        Err(AssocError::UnsupportedPlatform)
    }
}

/// 把扩展名关联到本程序（`associated = true`）或解除关联（`false`）。
///
/// 返回操作之后的实际状态：关联成功但被系统默认应用设置压住时，返回的是
/// [`AssocState::Registered`] 而不是 `Default` —— 调用方据此如实告诉用户
/// 「还差一步」，而不是报一句成功。
pub fn set(extension: &str, associated: bool) -> Result<AssocState, AssocError> {
    #[cfg(windows)]
    {
        windows_impl::set(extension, associated)?;
        Ok(windows_impl::read_state(extension))
    }
    #[cfg(not(windows))]
    {
        let _ = (extension, associated);
        Err(AssocError::UnsupportedPlatform)
    }
}

/// 给定注册表里的四处读数，判定一个扩展名的状态。
///
/// 抽成纯函数是为了能单测：真实判定要读注册表，而注册表操作没法在单元测试里
/// 安全地跑（会改掉跑测试那台机器的真实关联）。这个函数把**判定规则**与
/// **读注册表**分开，规则部分于是可以被完整覆盖 —— 包括那 44% 被 UserChoice
/// 压住的情形，它是整个模块最容易想当然写错的地方。
///
/// 参数按优先级从高到低：
/// 1. `user_choice` —— `FileExts\.<ext>\UserChoice\ProgId`（系统保护的，程序写不进）；
/// 2. `hkcu_default` —— `HKCU\Software\Classes\.<ext>` 的默认值（我们写的就是它）；
/// 3. `hklm_default` —— `HKLM\Software\Classes\.<ext>` 的默认值（系统级默认程序）；
/// 4. `listed` —— 本程序是否已出现在 `OpenWithProgids` 里。
fn classify(
    user_choice: Option<&str>,
    hkcu_default: Option<&str>,
    hklm_default: Option<&str>,
    listed: bool,
) -> AssocState {
    // 空串等同于没有值：注册表里 `.heic` 这类键下常常存在一个空字符串的默认值，
    // 它不表示任何程序认领（实测：`AssocQueryString` 对它是「没有程序认领」）。
    fn first_claimed(value: Option<&str>) -> Option<&str> {
        value.filter(|text| !text.is_empty())
    }

    let effective = first_claimed(user_choice)
        .or_else(|| first_claimed(hkcu_default))
        .or_else(|| first_claimed(hklm_default));

    if effective == Some(PROG_ID) {
        AssocState::Default
    } else if listed {
        // 有别的程序占着，或在候选列表里等着用户去系统设置里选。
        AssocState::Registered
    } else {
        AssocState::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「已关联」只在真正生效时成立。
    #[test]
    fn our_prog_id_at_the_top_of_the_chain_means_associated() {
        assert_eq!(
            classify(None, Some(PROG_ID), None, true),
            AssocState::Default
        );
        assert_eq!(
            classify(Some(PROG_ID), Some(PROG_ID), None, true),
            AssocState::Default
        );
        // 我们只在系统级默认里（HKLM）也算 —— 安装包写进去的情形。
        assert_eq!(
            classify(None, None, Some(PROG_ID), true),
            AssocState::Default
        );
    }

    /// UserChoice 压住我们写的那一层 —— 实测里 `.png` / `.heic` 就是这个样子。
    /// 这时必须是「已登记」而不是「已关联」：后者会让用户以为设置成功了。
    #[test]
    fn a_foreign_user_choice_leaves_us_registered_but_not_default() {
        assert_eq!(
            classify(Some("WPS.PIC.png"), None, Some("pngfile"), true),
            AssocState::Registered
        );
        assert_eq!(
            classify(Some("WPS.PIC.png"), Some(PROG_ID), None, true),
            AssocState::Registered,
            "UserChoice 优先级最高，我们写了 HKCU\\Classes 也没用"
        );
    }

    /// 系统级默认程序占着，但我们在候选列表里 —— 用户还能从「打开方式」里选我们。
    #[test]
    fn a_foreign_system_default_still_counts_as_listed() {
        assert_eq!(
            classify(None, None, Some("Paint.Picture"), true),
            AssocState::Registered
        );
    }

    /// 完全不认识这个格式：既没有程序认领，我们也没登记。
    #[test]
    fn nothing_claims_it_and_we_are_not_listed() {
        assert_eq!(classify(None, None, None, false), AssocState::None);
    }

    /// 空字符串不是认领。注册表里常见 `.heic` 这类「有默认值但为空」的键，
    /// 把它当成认领会得出「系统已指定程序」的错误结论。
    #[test]
    fn empty_strings_do_not_claim_anything() {
        assert_eq!(classify(Some(""), Some(""), Some(""), false), AssocState::None);
        assert_eq!(
            classify(Some(""), Some(""), Some(""), true),
            AssocState::Registered
        );
        // 空串不能挡住下面那一级的真实值。
        assert_eq!(
            classify(None, Some(""), Some(PROG_ID), true),
            AssocState::Default
        );
    }

    /// 用户把默认程序改回我们之后，状态要跟着回来。
    #[test]
    fn a_user_choice_pointing_at_us_is_associated() {
        assert_eq!(
            classify(Some(PROG_ID), Some(PROG_ID), Some("pngfile"), true),
            AssocState::Default
        );
    }

    #[test]
    fn states_report_themselves_honestly() {
        assert!(!AssocState::None.is_marked());
        assert!(AssocState::Registered.is_marked());
        assert!(AssocState::Default.is_marked());
        assert_eq!(AssocState::None.label(), "未关联");
        assert_eq!(AssocState::Registered.label(), "已登记");
        assert_eq!(AssocState::Default.label(), "已关联");
    }

    /// 候选列表就是「打开」对话框那一份，两处不允许分叉。
    #[test]
    fn candidates_come_from_the_single_shared_list() {
        assert_eq!(candidates(), crate::fs_ops::file_ops::IMAGE_EXTENSIONS);
        assert!(candidates().contains(&"png"));
        // 全是小写、不带点 —— 注册表路径是按这个形态拼的。
        for extension in candidates() {
            assert!(!extension.starts_with('.'), "{extension} 不该带点");
            assert_eq!(
                *extension,
                extension.to_ascii_lowercase(),
                "{extension} 应当是小写"
            );
        }
    }

    /// 错误文案必须区分「本平台不支持」与「注册表写失败」——
    /// 前者是死路（要换平台），后者是可以重试的。
    #[test]
    fn errors_say_what_to_do_next() {
        let unsupported = AssocError::UnsupportedPlatform.user_message();
        assert!(unsupported.contains("不支持"));

        let registry = AssocError::Registry {
            extension: Some("png".into()),
            message: "access denied".into(),
        };
        assert!(registry.user_message().contains(".png"));
        assert!(registry.short_reason().contains("access denied"));
        // Display 走 short_reason，日志里不该出现面向用户的引导语。
        assert_eq!(registry.to_string(), registry.short_reason());
    }

    #[test]
    fn association_reports_its_dotted_form() {
        let association = Association {
            extension: "png".into(),
            state: AssocState::Default,
        };
        assert_eq!(association.dotted(), ".png");
    }

    /// 平台能力查询与实现必须一致：`is_supported` 为真时 `read_all` 不该报不支持。
    #[test]
    fn the_platform_flag_matches_the_implementation() {
        match read_all() {
            Ok(associations) => {
                assert!(is_supported());
                assert_eq!(associations.len(), candidates().len());
            }
            Err(AssocError::UnsupportedPlatform) => assert!(!is_supported()),
            Err(other) => panic!("读取关联状态失败：{other}"),
        }
    }

    /// 深链指向的就是我们写进 `RegisteredApplications` 的那个值名 ——
    /// 系统「默认应用」页靠它把用户翻到本程序那一页，写错一个字就翻不过去。
    #[test]
    fn the_settings_deep_link_names_our_registered_application() {
        assert_eq!(
            settings_page_uri(),
            format!("ms-settings:defaultapps?registeredAppUser={APP_EXE}")
        );
        assert!(settings_page_uri().starts_with("ms-settings:defaultapps?registeredAppUser="));
    }

    /// 深链里的值名没有做 URI 转义，前提是它只含 URI 安全字符。
    /// crate 名天然满足；这条断言把这个前提变成本文档里能被保护的一部分 ——
    /// 将来若把 `APP_EXE` 换成带空格的名字，深链会在这里先红，而不是在用户机器上静默失效。
    #[test]
    fn our_registered_name_needs_no_uri_escaping() {
        assert!(
            APP_EXE
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')),
            "{APP_EXE} 含 URI 需要转义的字符，深链要补转义"
        );
    }

    /// 打不开设置页时，要给出「手动去哪个页面」的退路 —— 而不是只说失败了。
    #[test]
    fn a_failed_launch_points_at_the_manual_route() {
        let error = AssocError::LaunchFailed {
            message: "spawn failed".into(),
        };
        assert!(error.user_message().contains("默认应用"));
        assert!(error.short_reason().contains("spawn failed"));
        assert_eq!(error.to_string(), error.short_reason());
    }
}

/// Windows 上的实际读写。注册表路径集中在这里，便于对照上面的生效规则。
#[cfg(windows)]
mod windows_impl {
    use std::path::PathBuf;

    use winreg::RegKey;
    use winreg::RegValue;
    use winreg::enums::{
        HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_NONE,
    };

    use super::{APP_EXE, AssocError, AssocState, PROG_ID};

    /// 扩展名与 ProgID 都住在 `HKCU\Software\Classes` 下。
    const CLASSES: &str = r"Software\Classes";

    /// 被我们覆盖掉的原默认值备份在这里，取消关联时还回去。
    const PREVIOUS_DEFAULTS: &str = r"Software\ngy-image-viewer\PreviousDefaults";

    /// 让本程序出现在系统「默认应用」页面的能力声明。
    const CAPABILITIES: &str = r"Software\ngy-image-viewer\Capabilities";

    /// 系统「默认应用」页面读的那个索引。
    const REGISTERED_APPLICATIONS: &str = r"Software\RegisteredApplications";

    fn hkcu() -> RegKey {
        RegKey::predef(HKEY_CURRENT_USER)
    }

    /// 从一个**已经打开**的键里读默认值，并把错误如实抛出去。
    ///
    /// 与 [`default_value`] 的分工：那个是「读现状」，读不到就当没有（只读展示路径，
    /// 少显示一档状态比整个面板报错好）；这个专供**写入路径** —— 调用方要按
    /// 「当前值是不是我们写的」决定要不要还回备份，把权限错误当成 `None`
    /// 会静默走错分支，表现为「取消关联没生效，但一点动静都没有」。
    fn default_of(key: &RegKey) -> std::io::Result<Option<String>> {
        match key.get_value::<String, _>("") {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn hklm() -> RegKey {
        RegKey::predef(HKEY_LOCAL_MACHINE)
    }

    fn registry_error(extension: Option<&str>, error: std::io::Error) -> AssocError {
        AssocError::Registry {
            extension: extension.map(str::to_string),
            message: error.to_string(),
        }
    }

    /// 本程序可执行文件的绝对路径。注册表里必须是绝对路径 ——
    /// 写相对路径或只写文件名，系统会在自己的目录里找不到它。
    fn executable() -> Result<PathBuf, AssocError> {
        let path = std::env::current_exe().map_err(|error| AssocError::ExecutableUnavailable {
            message: error.to_string(),
        })?;
        // `current_exe` 给的是 \\?\ 前缀的扩展路径，注册表里用普通的反斜杠路径更稳
        // （命令行解析对 \\?\ 前缀的容忍度在各版本上不一致）。
        Ok(PathBuf::from(
            path.to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_string(),
        ))
    }

    /// 读一个扩展名的默认值（`None` = 键不存在或没有默认值）。
    fn default_value(root: &RegKey, path: &str) -> Option<String> {
        let key = root.open_subkey_with_flags(path, KEY_READ).ok()?;
        key.get_value::<String, _>("").ok()
    }

    /// 系统实际会用哪个 ProgID：UserChoice > HKCU\Classes > HKLM\Classes。
    fn effective_prog_id(extension: &str) -> (Option<String>, Option<String>, Option<String>) {
        let user_choice = hkcu()
            .open_subkey_with_flags(
                format!(
                    r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.{extension}\UserChoice"
                ),
                KEY_READ,
            )
            .ok()
            .and_then(|key| key.get_value::<String, _>("ProgId").ok());

        let hkcu_default = default_value(&hkcu(), &format!(r"{CLASSES}\.{extension}"));
        let hklm_default = default_value(&hklm(), &format!(r"{CLASSES}\.{extension}"));

        (user_choice, hkcu_default, hklm_default)
    }

    /// 本程序是否已经出现在 `OpenWithProgids` 里。
    ///
    /// 用 `get_raw_value` 而不是 `get_value::<T>`：那个值的类型是 `REG_NONE`（空），
    /// `winreg` 没有为它实现任何 `FromRegValue` —— 但**存在性**才是这里要问的，
    /// 值的类型与内容都无所谓。
    fn listed_as_open_with(extension: &str) -> bool {
        hkcu()
            .open_subkey_with_flags(
                format!(r"{CLASSES}\.{extension}\OpenWithProgids"),
                KEY_READ,
            )
            .ok()
            .is_some_and(|key| key.get_raw_value(PROG_ID).is_ok())
    }

    pub fn read_state(extension: &str) -> AssocState {
        let (user_choice, hkcu_default, hklm_default) = effective_prog_id(extension);
        super::classify(
            user_choice.as_deref(),
            hkcu_default.as_deref(),
            hklm_default.as_deref(),
            listed_as_open_with(extension),
        )
    }

    /// 写入 ProgID 本身，以及「打开方式」列表与系统设置需要的三处登记。
    ///
    /// 幂等：每次关联都写一遍，代价是几次注册表写入，换来的是
    /// 「程序被移动过、原来的登记失效了」这类情况能自愈。
    fn ensure_registered(exe: &PathBuf) -> Result<(), AssocError> {
        let command = format!("\"{}\" \"%1\"", exe.display());

        // 1. ProgID：双击时系统实际执行的就是这里的命令。
        let (prog_id, _) = hkcu()
            .create_subkey(format!(r"{CLASSES}\{PROG_ID}"))
            .map_err(|error| registry_error(None, error))?;
        prog_id
            .set_value("", &format!("图片（{APP_EXE}）"))
            .map_err(|error| registry_error(None, error))?;
        let (icon, _) = prog_id
            .create_subkey("DefaultIcon")
            .map_err(|error| registry_error(None, error))?;
        icon.set_value("", &format!("\"{}\",0", exe.display()))
            .map_err(|error| registry_error(None, error))?;
        let (open, _) = prog_id
            .create_subkey(r"shell\open\command")
            .map_err(|error| registry_error(None, error))?;
        open.set_value("", &command)
            .map_err(|error| registry_error(None, error))?;

        // 2. Applications\<exe>：资源管理器在「打开方式」里按可执行文件找我们。
        let (application, _) = hkcu()
            .create_subkey(format!(r"{CLASSES}\Applications\{APP_EXE}.exe"))
            .map_err(|error| registry_error(None, error))?;
        let (app_open, _) = application
            .create_subkey(r"shell\open\command")
            .map_err(|error| registry_error(None, error))?;
        app_open
            .set_value("", &command)
            .map_err(|error| registry_error(None, error))?;

        // 3. Capabilities + RegisteredApplications：让本程序出现在
        //    系统「设置 → 应用 → 默认应用」的候选列表里。UserChoice 压住我们时，
        //    这是用户唯一能一键切回来的入口，不能省。
        let (registered, _) = hkcu()
            .create_subkey(REGISTERED_APPLICATIONS)
            .map_err(|error| registry_error(None, error))?;
        registered
            .set_value(APP_EXE, &CAPABILITIES)
            .map_err(|error| registry_error(None, error))?;

        let (capabilities, _) = hkcu()
            .create_subkey(CAPABILITIES)
            .map_err(|error| registry_error(None, error))?;
        capabilities
            .set_value("ApplicationName", &APP_EXE)
            .map_err(|error| registry_error(None, error))?;
        capabilities
            .set_value(
                "ApplicationDescription",
                &format!("{APP_EXE}：双击即见图的图片查看器"),
            )
            .map_err(|error| registry_error(None, error))?;

        Ok(())
    }

    /// 登记一个扩展名：进候选列表 + 抢占默认值（原来的值先备份）。
    fn associate(extension: &str, exe: &PathBuf) -> Result<(), AssocError> {
        let error = |error: std::io::Error| registry_error(Some(extension), error);

        // 「打开方式」列表：值的类型是 REG_NONE（空），只有名字有意义。
        let (extension_key, _) = hkcu()
            .create_subkey(format!(r"{CLASSES}\.{extension}"))
            .map_err(error)?;
        let (open_with, _) = extension_key.create_subkey("OpenWithProgids").map_err(error)?;
        open_with
            .set_raw_value(
                PROG_ID,
                &RegValue {
                    bytes: Vec::new(),
                    vtype: REG_NONE,
                },
            )
            .map_err(error)?;

        // 系统「默认应用」页面按扩展名列出每个程序，读的是这里。
        let (capabilities, _) = hkcu().create_subkey(CAPABILITIES).map_err(error)?;
        let (file_associations, _) = capabilities.create_subkey("FileAssociations").map_err(error)?;
        file_associations
            .set_value(&format!(".{extension}"), &PROG_ID)
            .map_err(error)?;

        let (application, _) = hkcu()
            .create_subkey(format!(r"{CLASSES}\Applications\{APP_EXE}.exe"))
            .map_err(error)?;
        let (supported, _) = application.create_subkey("SupportedTypes").map_err(error)?;
        supported
            .set_value(&format!(".{extension}"), &String::new())
            .map_err(error)?;

        // 覆盖前先备份原来的默认值 —— 没有它，「取消关联」就还不会去，
        // 用户原本的默认程序会被我们永久顶掉（实测 `.jp2` 就会从
        // SumatraPDF 掉成「没有程序认领」）。
        let current = default_of(&extension_key).map_err(error)?;
        if let Some(current) = current.as_deref().filter(|value| {
            !value.is_empty() && *value != PROG_ID
        }) {
            let (previous, _) = hkcu().create_subkey(PREVIOUS_DEFAULTS).map_err(error)?;
            previous
                .set_value(&format!(".{extension}"), &current)
                .map_err(error)?;
        }

        extension_key.set_value("", &PROG_ID).map_err(error)?;

        // 到这里注册表已经改完。系统会不会**立刻**用上新值，取决于它自己有没有
        // 监听到 `Software\Classes` 的变化 —— 实测在本机是立刻生效的
        // （`.qoi` / `.jxl` 写完再查就已经指向本程序）。
        //
        // 若将来发现某些机器上「设置完要重启资源管理器才认」，标准做法是在这里补一次
        // `SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, null, null)`。
        // 那会是本项目 `decode/wic.rs` 之外的第二处 `unsafe`，在拿到确凿证据之前不引入。
        let _ = exe;
        Ok(())
    }

    /// 撤掉一个扩展名的登记，并把默认值还给原来的程序。
    fn disassociate(extension: &str) -> Result<(), AssocError> {
        let error = |error: std::io::Error| registry_error(Some(extension), error);

        // 候选列表里的登记：只删我们这一条，别人的一条都不动。
        if let Ok(open_with) = hkcu().open_subkey_with_flags(
            format!(r"{CLASSES}\.{extension}\OpenWithProgids"),
            KEY_SET_VALUE,
        ) {
            let _ = open_with.delete_value(PROG_ID);
        }

        if let Ok(capabilities) =
            hkcu().open_subkey_with_flags(format!(r"{CAPABILITIES}\FileAssociations"), KEY_SET_VALUE)
        {
            let _ = capabilities.delete_value(&format!(".{extension}"));
        }

        if let Ok(supported) = hkcu().open_subkey_with_flags(
            format!(r"{CLASSES}\Applications\{APP_EXE}.exe\SupportedTypes"),
            KEY_SET_VALUE,
        ) {
            let _ = supported.delete_value(&format!(".{extension}"));
        }

        // 默认值：只有它确实是我们时才动。用户后来自己改成了别的程序，
        // 那属于用户的决定，不该被我们的「取消关联」覆盖掉。
        //
        // 权限必须读、写都给：下面要先读一次确认「当前值确实是我们写的」。
        // 只开 `KEY_SET_VALUE` 的话这次读会以 `ERROR_ACCESS_DENIED` 失败，
        // 被 `.ok()` 吞成 `None`，于是**取消关联永远不还回备份** —— 而表面上
        // 一切正常（候选列表清掉了、提示也弹了），这正是最难查的一类失效。
        // 实测就是这么暴露的：点两次之后 `.png` 的默认值仍然指着我们。
        let extension_key = hkcu()
            .open_subkey_with_flags(
                format!(r"{CLASSES}\.{extension}"),
                KEY_READ | KEY_SET_VALUE,
            )
            .map_err(error)?;
        let current = default_of(&extension_key).map_err(error)?;
        if current.as_deref() == Some(PROG_ID) {
            let previous = hkcu()
                .open_subkey_with_flags(PREVIOUS_DEFAULTS, KEY_READ)
                .ok()
                .and_then(|key| key.get_value::<String, _>(&format!(".{extension}")).ok());

            match previous {
                Some(previous) if !previous.is_empty() => {
                    extension_key.set_value("", &previous).map_err(error)?;
                }
                // 原来就没有默认程序（`.qoi` 这类），删掉值即回到原状。
                _ => {
                    extension_key.delete_value("").map_err(error)?;
                }
            }

            if let Ok(store) =
                hkcu().open_subkey_with_flags(PREVIOUS_DEFAULTS, KEY_SET_VALUE)
            {
                let _ = store.delete_value(&format!(".{extension}"));
            }
        }

        Ok(())
    }

    pub fn set(extension: &str, associated: bool) -> Result<(), AssocError> {
        let extension = extension.to_ascii_lowercase();
        if associated {
            let exe = executable()?;
            ensure_registered(&exe)?;
            associate(&extension, &exe)
        } else {
            disassociate(&extension)
        }
    }

    /// 打开系统「默认应用」页并定位到本程序。见模块文档与 [`super::settings_page_uri`]。
    pub fn open_defaults_settings() -> Result<(), AssocError> {
        // 先补登记：设置页里本程序那一页只列已登记的格式，没登记就先跳过去，
        // 用户在那页看不到本程序（或看到它却没有任何格式可设），等于白跳。
        let exe = executable()?;
        ensure_registered(&exe)?;
        launch(&super::settings_page_uri())
    }

    /// 交给系统的 URI 处理器打开一个 `ms-settings:` 深链。
    ///
    /// 走 `cmd /C start` 而不是 `ShellExecute`：后者要 FFI，而本项目的 `unsafe`
    /// 只允许出现在 `decode/wic.rs`。`CREATE_NO_WINDOW` 是必须的 —— 不加会在
    /// 每次跳转时闪一个控制台黑框。
    fn launch(uri: &str) -> Result<(), AssocError> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        std::process::Command::new("cmd")
            .arg("/C")
            .arg("start")
            // `start` 会把**第一个带引号的参数**当成窗口标题。给一个空标题，
            // 否则 URI 会被它吃掉、变成新窗口的标题，什么也不会打开。
            .arg("")
            // 显式给 URI 加引号：Windows 上 Rust 默认只给「含空格」的参数加引号，
            // 而这个 URI 不含空格、会被裸传 —— 裸传虽然也成立，但带引号才是 `start`
            // 的标准用法，也免得将来值名里冒出特殊字符时踩坑。
            .raw_arg(format!("\"{uri}\""))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|error| AssocError::LaunchFailed {
                message: error.to_string(),
            })
    }
}
