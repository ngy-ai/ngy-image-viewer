//! 界面用的纯格式化函数。
//!
//! 单独一个文件、**不引用任何 UI 类型**：这些函数的输出直接决定用户读到什么，
//! 属于「必须精确」的那一类，因此要能被单元测试钉死。
//! （放在 `panels.rs` 里也可以，但那个文件引入了整个 gpui 命名空间，
//! 测试宏展开会撞到递归上限 —— 分层清楚顺带也解决了这个问题。）

/// 缩放百分比的显示文案。
///
/// 小于 10% 时保留一位小数：把 4% 和 5% 都显示成整数会让用户分不清
/// 「到底缩了没有」；而 100% 以上再带小数只是噪音。
pub fn zoom_text(percent: f32) -> String {
    if !percent.is_finite() {
        return "—".to_string();
    }
    if percent < 10.0 {
        format!("{percent:.1}%")
    } else {
        format!("{:.0}%", percent)
    }
}

/// 文件大小的可读文案。
///
/// 用 1024 进制（与系统文件管理器显示的「KB」一致），并按量级切换单位 ——
/// 否则会出现「1258291.2 KB」这种没法读的数字。
pub fn bytes_text(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return "—".to_string();
    };
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes = bytes as f64;

    if bytes >= GB {
        format!("{:.2} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes / KB)
    } else {
        format!("{bytes:.0} B")
    }
}

/// 加载耗时的文案。
pub fn duration_text(milliseconds: f64) -> String {
    if !milliseconds.is_finite() || milliseconds < 0.0 {
        return String::new();
    }
    if milliseconds >= 1000.0 {
        format!("加载 {:.2} s", milliseconds / 1000.0)
    } else {
        format!("加载 {milliseconds:.0} ms")
    }
}

/// 尺寸文案。
pub fn size_text(width: u32, height: u32) -> String {
    format!("{width} × {height}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_text_switches_precision_below_ten_percent() {
        assert_eq!(zoom_text(100.0), "100%");
        assert_eq!(zoom_text(4.0), "4.0%");
        assert_eq!(zoom_text(9.5), "9.5%");
        assert_eq!(zoom_text(12.4), "12%");
        // 数值异常时宁可显示占位符，也不要显示 "NaN%"
        assert_eq!(zoom_text(f32::NAN), "—");
    }

    #[test]
    fn byte_text_uses_readable_units() {
        assert_eq!(bytes_text(Some(512)), "512 B");
        assert_eq!(bytes_text(Some(2048)), "2.0 KB");
        assert_eq!(bytes_text(Some(5 * 1024 * 1024)), "5.0 MB");
        assert_eq!(bytes_text(Some(3 * 1024 * 1024 * 1024)), "3.00 GB");
        // 拿不到文件大小时不能显示 "0 B" —— 那会让人以为文件是空的。
        assert_eq!(bytes_text(None), "—");
    }

    #[test]
    fn duration_text_switches_to_seconds() {
        assert_eq!(duration_text(59.4), "加载 59 ms");
        assert_eq!(duration_text(1500.0), "加载 1.50 s");
        // 非法值不显示任何东西，免得在状态栏留下一句 "加载 NaN ms"。
        assert_eq!(duration_text(f64::NAN), "");
        assert_eq!(duration_text(-1.0), "");
    }

    #[test]
    fn size_text_is_dimension_ordered() {
        assert_eq!(size_text(6000, 4000), "6000 × 4000");
    }
}
