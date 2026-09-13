//! 启动与解码性能打点。
//!
//! 设计原则：默认完全静默（零 I/O、零输出），只有设置环境变量 `NGY_PERF=1`
//! 时才记录。原因是本项目的核心指标就是启动耗时，打点本身不能成为负担。
//!
//! 启用后记录会同时写入 stderr 与 `%TEMP%/ngy-image-viewer-perf.log`，
//! 因为发布版在 Windows 上是 `windows_subsystem = "windows"`，没有控制台可看。

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 时间零点，由 `init()` 在 `main` 最开始处设置。
static START: OnceLock<Instant> = OnceLock::new();
/// 已记录的打点：(标签, 相对进程启动的毫秒数)。
static RECORDS: Mutex<Vec<(String, f64)>> = Mutex::new(Vec::new());
static ENABLED: OnceLock<bool> = OnceLock::new();

/// 是否开启了打点（`NGY_PERF=1`）。
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var("NGY_PERF").as_deref(),
            Ok("1") | Ok("true") | Ok("TRUE")
        )
    })
}

/// 基准采集模式下的自动退出时长（毫秒），由环境变量 `NGY_BENCH_MS` 指定。
///
/// 为什么需要它：GUI 进程会一直运行到窗口被关闭，自动化采集启动耗时的时候
/// 没有交互窗口可用，于是提供一个开关让应用在采集完成后自行退出。
/// 未设置该变量时返回 `None`，对正常使用零影响。
pub fn bench_exit_ms() -> Option<u64> {
    std::env::var("NGY_BENCH_MS").ok()?.parse().ok()
}

/// 必须在 `main` 的第一行调用：确立时间零点。
pub fn init() {
    let _ = START.set(Instant::now());
    if bench_exit_ms().is_some() {
        // 基准运行：清空上一轮日志，避免历史数据混入本次结果。
        let _ = std::fs::remove_file(log_path());
    }
    if enabled() {
        mark("process_start");
    }
}

/// 在基准采集模式下安排一次自动退出，确保日志落盘后再结束进程。
pub fn schedule_bench_exit() {
    let Some(ms) = bench_exit_ms() else {
        return;
    };
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        log_always(&summary());
        log_always(&format!("[bench] 采集完成，自动退出（NGY_BENCH_MS={ms}）"));
        std::process::exit(0);
    });
}

/// 相对进程启动已过去的毫秒数。
pub fn elapsed_ms() -> f64 {
    START
        .get()
        .map_or(0.0, |start| start.elapsed().as_secs_f64() * 1000.0)
}

/// 记录一个阶段耗时点。未开启打点时立即返回，无任何开销。
pub fn mark(label: &str) {
    if !enabled() {
        return;
    }
    let ms = elapsed_ms();
    if let Ok(mut records) = RECORDS.lock() {
        records.push((label.to_string(), ms));
    }
    let line = format!("{ms:>9.2} ms  {label}");
    eprintln!("[perf] {line}");
    append_log(&line);
}

/// 已记录的打点快照。
pub fn records() -> Vec<(String, f64)> {
    RECORDS.lock().map(|r| r.clone()).unwrap_or_default()
}

/// 生成人类可读的启动耗时摘要，用于验收对比。
pub fn summary() -> String {
    let mut out = String::from("启动耗时摘要：\n");
    for (label, ms) in records() {
        let _ = writeln!(out, "  {ms:>9.2} ms  {label}");
    }
    out
}

/// 无条件写一行日志（用于致命错误，即使未开启打点也要留下现场）。
pub fn log_always(line: &str) {
    eprintln!("{line}");
    append_log(line);
}

/// 日志文件位置。
///
/// 默认写在临时目录，但允许用 `NGY_PERF_LOG` 覆盖 —— 自动化采集时
/// 把日志放进工作目录里的 `target/` 下面，就不必去别处翻文件，
/// 也不会和上一次采集的结果混在一起。
fn log_path() -> PathBuf {
    match std::env::var_os("NGY_PERF_LOG") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => std::env::temp_dir().join("ngy-image-viewer-perf.log"),
    }
}

fn append_log(line: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(file, "{line}");
    }
}
