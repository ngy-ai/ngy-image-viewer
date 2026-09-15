//! 用户偏好：皮肤选择，以及它在磁盘上的往返。
//!
//! # 这一层解决什么问题
//!
//! 需求是「两套皮肤，默认跟随系统」。默认跟随系统意味着**大部分用户永远不需要
//! 打开这个文件**，但一旦有人手动选了深色，这个选择必须活过重启 —— 否则
//! 「我明明设过」会变成一个每次开机都要重来一遍的烦人事。
//!
//! # 与 UI 解耦
//!
//! 本模块不引用任何 gpui 类型，只做「字符串进，字符串出」。
//! 它读写的 [`Preference`] 用 [`Polarity`](crate::ui::theme::Polarity) 表达，
//! 而那个类型是纯数据 —— 于是「无法识别的值该落到哪一档」这类判断可以在没有
//! 窗口的情况下被单测钉死，这正是这个文件存在的意义。
//!
//! # 两条硬性约定
//!
//! 1. **绝不在关键路径上做 IO。** 本模块只在两个时刻被调用：启动时读一次
//!    （在创建窗口之前，与解码并行），以及用户主动改选时写一次（异步线程）。
//!    渲染循环里**从不**触碰磁盘 —— 「双击即见图」的 305ms 里没有留给 IO 的余地。
//! 2. **坏文件绝不让程序起不来。** 配置缺失、无法解析、写了一半，全部静默退回
//!    默认值（跟随系统）。偏好配置是便利功能，不是程序的运行前提。
//!
//! # 文件格式
//!
//! 自制的极简 `key=value`，不为了一行配置引一个序列化依赖：
//!
//! ```text
//! # ngy-image-viewer 配置文件。删除本文件即恢复「跟随系统」。
//! skin=dark
//! ```
//!
//! 未知的键会被忽略而不是报错：将来加设置项时，旧版本程序读到新文件
//! 不该出现任何异常表现。

use std::path::{Path, PathBuf};

use crate::ui::theme::Polarity;

/// 「皮肤」这一项在磁盘上的键名。
const SKIN_KEY: &str = "skin";

/// 配置文件名（放在用户配置目录下的子目录里）。
const CONFIG_DIR: &str = "ngy-image-viewer";
const CONFIG_FILE: &str = "config";

/// 用户对皮肤的选择。**三态**，因为「跟随系统」本身是一个独立的选择，
/// 不是「深色」或「浅色」的同义词。
///
/// 把它做成三态而不是「一个可空的 Polarity」，是为了让「用户没表态」
/// 与「用户明确选了深色」在类型上就分得开 —— 二者在 `set_appearance`
/// 上的正确做法是相反的（前者要 `None` 清除覆盖，后者要传具体值），
/// 用一个 `Option` 之外的类型表达会让调用处多一次容易写错的判断。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Preference {
    /// 跟随操作系统（默认）。
    #[default]
    System,
    /// 用户点名要某一套。
    Fixed(Polarity),
}

impl Preference {
    /// 这个偏好下应该用哪套皮肤。
    ///
    /// `system` 是平台当下报的外观 —— 由调用方每帧从窗口读，
    /// 本函数不缓存它，因此系统主题一变，下一次调用就返回新的皮肤。
    pub fn skin_for(self, system: Polarity) -> &'static crate::ui::theme::Skin {
        match self {
            Self::System => system.skin(),
            Self::Fixed(polarity) => polarity.skin(),
        }
    }

    /// 用户是否明确指定了皮肤（即系统外观此刻是否**不该**生效）。
    pub fn is_overriding_system(self) -> bool {
        matches!(self, Self::Fixed(_))
    }

    /// 菜单里这一项要不要显示勾选态。
    pub fn matches(self, polarity: Polarity) -> bool {
        self == Self::Fixed(polarity)
    }

    /// 写盘用的值。
    fn encode(self) -> Option<&'static str> {
        match self {
            // 「跟随系统」不落盘：删掉这一行就等于跟随系统，
            // 而写一个 `skin=system` 会让「用户没配过」与「用户配了跟随」
            // 在文件上长得一样，前者才可以被将来的迁移规则识别。
            Self::System => None,
            Self::Fixed(polarity) => Some(polarity.key()),
        }
    }

    /// 从配置文本解出偏好。任何不认识的内容都退回「跟随系统」。
    fn decode(text: &str) -> Self {
        for line in text.lines() {
            let line = line.trim();
            // 注释与空行。
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() != SKIN_KEY {
                continue;
            }
            if let Some(polarity) = Polarity::from_key(value) {
                return Self::Fixed(polarity);
            }
        }
        Self::System
    }
}

/// 从磁盘读一次偏好。
///
/// 任何失败（文件不存在、读不动、内容不认识）都返回默认的
/// [`Preference::System`]，**不报错也不留痕** —— 对一个便利配置来说，
/// 「读不到就用默认」是正确行为，把用户拖进一个错误对话框才是错的。
pub fn load() -> Preference {
    match config_path() {
        Some(path) => load_from(&path),
        None => Preference::System,
    }
}

fn load_from(path: &Path) -> Preference {
    match std::fs::read_to_string(path) {
        Ok(text) => Preference::decode(&text),
        Err(_) => Preference::System,
    }
}

/// 把偏好写到磁盘。
///
/// 由视图在用户改选后**在后台线程上**调用（见 `view.rs` 的 `set_preference`）：
/// 一次写盘是几毫秒，但把它放在渲染循环里就是几毫秒的卡顿，
/// 而这类卡顿会被归因成「这个看图器有点钝」。
///
/// 返回值只用于打点：写失败不是致命问题（下次启动会退回跟随系统），
/// 但值得在日志里留一行，免得「设了没记住」变成一个无法复盘的现象。
pub fn store(preference: Preference) -> std::io::Result<()> {
    let Some(path) = config_path() else {
        return Err(std::io::Error::other("找不到用户配置目录"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, encode_document(preference))
}

/// 生成要写入的完整文本。
///
/// 独立成函数是为了能单测：写盘的部分碰真实文件系统，往返测试要的是这段纯逻辑。
fn encode_document(preference: Preference) -> String {
    let mut text = String::from(
        "# ngy-image-viewer 配置文件\n\
         # 删除本文件即恢复默认（跟随系统外观）。\n",
    );
    // 「跟随系统」时只留注释，不写这一行 —— 空文件与「全注释文件」
    // 在读回来时是同一个结果（都是 `Preference::System`），写成注释更直白，
    // 也让「用户没配过」在文件上保持缺席。
    if let Some(value) = preference.encode() {
        text.push_str(&format!("{SKIN_KEY}={value}\n"));
    }
    text
}

/// 配置文件的完整路径。
///
/// 平台目录由 `dirs` 那类 crate 提供会更省事，但为了一行路径引一个依赖不划算：
/// Windows 读 `APPDATA`、macOS / Linux 读 `XDG_CONFIG_HOME` 或 `~/.config`，
/// 三段 `cfg` 就够，且没有额外依赖与版本风险。
fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join(CONFIG_DIR).join(CONFIG_FILE))
}

fn config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        // `APPDATA` 指向 `C:\Users\<用户>\AppData\Roaming`，
        // 是 Windows 上放用户级配置的既定位置。
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 没配过的文件 → 跟随系统。
    #[test]
    fn missing_or_empty_config_follows_the_system() {
        assert_eq!(Preference::decode(""), Preference::System);
        assert_eq!(
            Preference::decode("# 只有注释\n\n   \n"),
            Preference::System
        );
    }

    /// 两条路径必须往返一致：写出来再读回去是同一个偏好。
    ///
    /// 这条通过意味着 `encode` 与 `decode` 的键名、大小写、空白处理没有分叉。
    #[test]
    fn preferences_survive_a_round_trip() {
        for preference in [
            Preference::System,
            Preference::Fixed(Polarity::Dark),
            Preference::Fixed(Polarity::Light),
        ] {
            let text = encode_document(preference);
            assert_eq!(
                Preference::decode(&text),
                preference,
                "往返后偏好变了，写入的文本是：\n{text}"
            );
        }
    }

    /// 值大小写与周围空白都不该影响解析。
    #[test]
    fn values_are_parsed_leniently() {
        assert_eq!(
            Preference::decode("skin=DARK\n"),
            Preference::Fixed(Polarity::Dark)
        );
        assert_eq!(
            Preference::decode("  skin = Light  \n"),
            Preference::Fixed(Polarity::Light)
        );
    }

    /// 不认识的键与不认识的值都不能让解析失败 —— 一律退回跟随系统。
    ///
    /// 这一条是「将来加设置项」的护栏：旧版本程序读到新版本写的文件时，
    /// 正确行为是忽略不懂的部分，而不是变成空窗口。
    #[test]
    fn unknown_content_falls_back_instead_of_failing() {
        assert_eq!(Preference::decode("skin=blue\n"), Preference::System);
        assert_eq!(Preference::decode("zoom=200\n"), Preference::System);
        assert_eq!(Preference::decode("这不是配置\n"), Preference::System);
        assert_eq!(Preference::decode("skin\n"), Preference::System);
        // 前面的垃圾不该挡住后面正确的行。
        assert_eq!(
            Preference::decode("垃圾行\nskin=light\n"),
            Preference::Fixed(Polarity::Light)
        );
    }

    /// 「用户明确选了皮肤」与「跟随系统」在这个类型上是可区分的 ——
    /// 视图正是靠它决定要不要给平台设 override。
    #[test]
    fn only_an_explicit_choice_overrides_the_system() {
        assert!(!Preference::System.is_overriding_system());
        assert!(Preference::Fixed(Polarity::Dark).is_overriding_system());
        assert!(Preference::Fixed(Polarity::Light).is_overriding_system());
    }

    /// 皮肤取用：跟随系统时用平台给的那套，明确指定时无视平台。
    ///
    /// 这条是「手动覆盖生效」与「跟随系统真的跟随」的共同判据。
    #[test]
    fn skin_selection_respects_the_preference() {
        let dark = Preference::Fixed(Polarity::Dark).skin_for(Polarity::Light);
        assert!(std::ptr::eq(dark, &*crate::ui::theme::DARK));
        let light = Preference::Fixed(Polarity::Light).skin_for(Polarity::Dark);
        assert!(std::ptr::eq(light, &*crate::ui::theme::LIGHT));
        // 跟随系统：平台说是深色就是深色，说是浅色就是浅色。
        assert!(std::ptr::eq(
            Preference::System.skin_for(Polarity::Dark),
            &*crate::ui::theme::DARK
        ));
        assert!(std::ptr::eq(
            Preference::System.skin_for(Polarity::Light),
            &*crate::ui::theme::LIGHT
        ));
    }
}
