//! 打开图片全链路的调试日志（「每一幕」）。
//!
//! # 为什么单独一个模块，而不是塞进 `perf`
//!
//! `perf` 回答的是「花了多久」，它默认完全静默，且只在**成功**路径上有观察价值。
//! 本模块回答的是另一个问题：**走到了哪一步、那一步看到的数据是什么、哪一步失败了**。
//! 排查「打开图片后界面全黑、控制台却没有任何报错」时，需要的正是后者。
//!
//! # 两条硬性约定
//!
//! 1. **失败永远输出**（[`fail`]），不受开关影响。
//!    黑屏之所以难查，根因是 GPUI 有若干绘制 API 在「画了，但什么都没画出来」时
//!    返回 `Ok(())`（例如可见区域为空就提前返回）。这类「静默的没画出来」必须
//!    由我们自己判定，然后当成失败打出来 —— 否则它永远是一片沉默。
//! 2. **阶段日志默认只在 debug 构建输出**（[`step`]），release 下需显式 `NGY_TRACE=1`。
//!    绘制路径每帧都会跑，release 里无条件格式化字符串会直接吃掉「跟手」这条产品要求。
//!
//! # 同一处反复失败只报一次
//!
//! 绘制是每帧执行的，一处持续存在的失败会以每秒上百次的频率刷屏，把日志从
//! 「线索」变成「噪音」。因此 `fail` 与 `step_once` 都按 key 去重，只留第一条。

use std::collections::HashSet;
use std::fmt;
use std::sync::{Mutex, OnceLock};

use crate::perf;

static ENABLED: OnceLock<bool> = OnceLock::new();
/// 已经报告过的 key。用于「同一处失败／同一幕只报一次」。
static REPORTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// 阶段日志是否开启。
///
/// 规则：`debug` 构建默认开启（开发期就是要看得见），`release` 默认关闭。
/// 两种情况都可以用 `NGY_TRACE` 显式覆盖：`1` / `true` 强制开启，`0` / `false` 强制关闭。
/// 发布版没有控制台，开了它照样会落到 `perf` 的日志文件里。
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| match std::env::var("NGY_TRACE").as_deref() {
        Ok("1") | Ok("true") | Ok("TRUE") => true,
        Ok("0") | Ok("false") | Ok("FALSE") => false,
        _ => cfg!(debug_assertions),
    })
}

/// 记录一幕：某一步做完了，它看到的数据是什么。
///
/// 只在 [`enabled`] 为真时输出。**不要**把它放在逐帧执行的路径上（那里用 [`step_once`]）。
pub fn step(tag: &str, message: impl fmt::Display) {
    if !enabled() {
        return;
    }
    perf::log_always(&format!("[trace:{tag}] {message}"));
}

/// 只报一次的一幕。key 相同则后续调用静默。
///
/// 给逐帧路径用：绘制每帧都会跑，但「画布尺寸是 1600×900」这种事实只需要说一次。
pub fn step_once(key: &str, tag: &str, message: impl fmt::Display) {
    if !enabled() || !first_time(key) {
        return;
    }
    perf::log_always(&format!("[trace:{tag}] {message}"));
}

/// 失败。**永远输出**，并且同一处只输出第一条。
///
/// 这是本模块存在的核心理由：一个看不见的失败，比一个会崩的失败危险得多。
pub fn fail(tag: &str, message: impl fmt::Display) {
    if !first_time(&format!("fail:{tag}")) {
        return;
    }
    perf::log_always(&format!("[fail:{tag}] {message}"));
}

/// 同一处失败是否第一次出现。
fn first_time(key: &str) -> bool {
    let mut guard = match REPORTED.lock() {
        Ok(guard) => guard,
        // 锁中毒说明上一次报告时 panic 了。日志功能绝不能因此拖垮程序：
        // 退化成「每次都报」，宁可重复也不要沉默。
        Err(poisoned) => poisoned.into_inner(),
    };
    let set = guard.get_or_insert_with(HashSet::new);
    set.insert(key.to_string())
}

/// 把一串字节按十六进制打出来，用于核对文件头这类「肉眼比对才看得出对不对」的数据。
///
/// 只取前 `max` 个字节：日志的用途是比对不是存档。
pub fn hex_preview(bytes: &[u8], max: usize) -> String {
    let shown = bytes.len().min(max);
    let mut out = String::with_capacity(shown * 3 + 16);
    for (index, byte) in bytes[..shown].iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        let _ = fmt::Write::write_fmt(&mut out, format_args!("{byte:02X}"));
    }
    if bytes.len() > shown {
        let _ = fmt::Write::write_fmt(&mut out, format_args!(" …（共 {} 字节）", bytes.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_preview_truncates_and_marks_the_real_length() {
        assert_eq!(hex_preview(&[0x89, 0x50, 0x4E, 0x47], 4), "89 50 4E 47");
        assert_eq!(hex_preview(&[0x89, 0x50, 0x4E, 0x47], 2), "89 50 …（共 4 字节）");
        assert_eq!(hex_preview(&[], 8), "");
    }

    /// 去重是「日志不被刷屏」的唯一保险：绘制路径每帧都会走到报告点，
    /// 不去重的话一处持续失败会以每秒上百次的频率把日志变成噪音。
    #[test]
    fn repeated_keys_are_only_reported_once() {
        // 用测试专属的 key，避免与其它用例共享同一份全局状态。
        let key = "unit-test-dedup/repeated_keys_are_only_reported_once";
        assert!(first_time(key), "第一次应当报告");
        assert!(!first_time(key), "同一处不应重复报告");
        assert!(!first_time(key));

        // 换一个 key 必须重新报告：不同位置的问题不能被前一处「吃掉」。
        assert!(first_time("unit-test-dedup/another-site"));
    }
}
