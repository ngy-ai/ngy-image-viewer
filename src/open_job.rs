//! 后台打开任务：从「拿到路径」到「像素可用」的全过程都发生在 UI 线程之外。
//!
//! # 为什么单独一个模块
//!
//! 解码层是纯函数式的（`Path` 进、`ImageData` 出），不涉及时间；
//! 而「什么时候开始解码、什么时候算完、等多长时间"」是**产品目标**，不是解码细节。
//! 把这件事集中在一个模块里，才能让「感知速度优先」这条原则有唯一落点：
//!
//! ```text
//! main(): 解析 argv ──┬─→ OpenTask::spawn()   读文件 + 解码      ┐
//!                     └─→ app::run()          GPUI 平台初始化   ┘  两条线并行
//! ```
//!
//! 关键在于**并行**：GPUI 的平台初始化有几百毫秒的硬开销（已在基线里量化过），
//! 如果先解码再启动界面，用户就要等两段耗时之和；并行之后，感知等待
//! 接近两者中的较大值。这是本方案里「感知速度」的核心机制。
//!
//! # 时序保证
//!
//! 任务从 [`OpenTask::spawn`] 那一刻起就拥有独立线程，直到结果被交给 UI 线程为止，
//! 期间**不碰任何 GPUI 类型**。UI 侧只需按自己的节奏 `poll()`。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::decode::{self, DecodeError, DecodeLimits, DecodeResult, ImageData};
use crate::perf;
use crate::trace;

/// 文件级信息快照（与磁盘当时的状态一致）。
///
/// 顺序很重要：必须在解码**之前**取，否则「边解码边被改写」的文件
/// 会给出与内容对不上的大小。
#[derive(Clone, Debug)]
pub struct FileStat {
    pub byte_len: u64,
    pub modified: Option<SystemTime>,
}

/// 一次打开任务的最终结果。
#[derive(Debug)]
pub struct OpenOutcome {
    pub path: PathBuf,
    /// 读文件失败时为 `None`。
    pub file: Option<FileStat>,
    /// 成功给出像素与元数据，失败给出可直接展示给用户的错误。
    pub result: DecodeResult<ImageData>,
    /// 纯解码耗时（含格式探测），不含进程启动。
    pub decode_ms: f64,
    /// 从 `spawn` 到出结果的总耗时 —— 衡量「感知速度」的关键数字。
    pub total_ms: f64,
}

impl OpenOutcome {
    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }

    /// 单行日志，用于「解码耗时 / 探测到的格式 / 失败原因」这条关键路径。
    pub fn log_line(&self) -> String {
        match &self.result {
            Ok(data) => format!(
                "[open] {} -> {} {}x{} 帧数={} 方向={:?} 解码 {:.2} ms 总计 {:.2} ms",
                self.path.display(),
                data.format.display_name(),
                data.width(),
                data.height(),
                data.frame_count(),
                data.orientation,
                self.decode_ms,
                self.total_ms,
            ),
            Err(error) => format!(
                "[open] {} -> 失败（{}）解码 {:.2} ms 总计 {:.2} ms",
                self.path.display(),
                error.short_reason(),
                self.decode_ms,
                self.total_ms,
            ),
        }
    }
}

/// 后台打开任务句柄。
///
/// 持有接收端；结果只会被交付一次。
pub struct OpenTask {
    path: PathBuf,
    receiver: Receiver<OpenOutcome>,
    started: Instant,
    limits: DecodeLimits,
    /// 结果已交付：之后 `poll` 一律返回 `None`，避免重复触发 UI 更新。
    settled: bool,
    /// 连线程都创建不出来时的兜底结果。
    pre_failed: Option<OpenOutcome>,
}

impl OpenTask {
    /// 立刻启动后台线程读取并解码。
    ///
    /// 调用点是 `main()`，越早越好 —— 它要和 GPUI 几百毫秒的平台初始化抢时间。
    pub fn spawn(path: impl Into<PathBuf>) -> Self {
        Self::spawn_with_limits(path, DecodeLimits::default())
    }

    pub fn spawn_with_limits(path: impl Into<PathBuf>, limits: DecodeLimits) -> Self {
        let path = path.into();
        let started = Instant::now();
        let (sender, receiver) = mpsc::channel();
        let worker_path = path.clone();

        // 用裸线程而不是线程池：只有一个任务，池化只会增加启动延迟。
        let spawned = thread::Builder::new()
            .name("ngy-open".to_string())
            .spawn(move || {
                let outcome = run(&worker_path, limits, started);
                // 接收端可能已被丢弃（窗口关了、用户又拖了别的图），忽略发送失败。
                let _ = sender.send(outcome);
            });

        match &spawned {
            Ok(_) => trace::step(
                "open",
                format!("已派生解码线程：{}（时刻 {:.2} ms）", path.display(), started.elapsed().as_secs_f64() * 1000.0),
            ),
            Err(error) => trace::fail(
                "open",
                format!("无法创建解码线程：{error}（界面会显示这一条而不是一直转圈）"),
            ),
        }

        // 线程都创建不出来属于极端情况，但也不能让窗口永远停在加载态：
        // 把失败预先存成一个「已就绪的结果」，`poll` 会优先交付它。
        let pre_failed = spawned.err().map(|error| OpenOutcome {
            path: path.clone(),
            file: None,
            result: Err(DecodeError::corrupt(format!("无法创建解码线程：{error}"))),
            decode_ms: 0.0,
            total_ms: started.elapsed().as_secs_f64() * 1000.0,
        });

        Self {
            path,
            receiver,
            started,
            limits,
            settled: false,
            pre_failed,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn limits(&self) -> &DecodeLimits {
        &self.limits
    }

    /// 从 `spawn` 到现在过了多久。用于「还在加载中」时显示进度文案。
    pub fn elapsed_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }

    /// 结果是否已经交付过。
    ///
    /// UI 侧用它判断「要不要继续挂轮询」：已经交付过的任务再 `poll` 只会得到 `None`，
    /// 继续循环就变成了空转。
    pub fn is_settled(&self) -> bool {
        self.settled || self.pre_failed.is_some()
    }

    /// 非阻塞取结果。
    ///
    /// 拿到结果后本任务即视为结束，再调用返回 `None`。
    /// 调用方必须处理返回的 `Some`，否则结果会被丢弃。
    pub fn poll(&mut self) -> Option<OpenOutcome> {
        if let Some(outcome) = self.pre_failed.take() {
            self.settled = true;
            return Some(outcome);
        }
        if self.settled {
            return None;
        }
        let outcome = match self.receiver.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return None,
            // 线程结束了却没发结果（理论上不该发生）：当成一次解码失败处理，
            // 好过让界面一直转圈。
            Err(TryRecvError::Disconnected) => worker_died(&self.path, self.elapsed_ms()),
        };
        self.settled = true;
        Some(outcome)
    }

    /// 阻塞等待结果。用于命令行/测试等「同步取一张图」的场景。
    ///
    /// 与 [`OpenTask::poll`] 一样，取到结果后任务即结束。
    pub fn wait(&mut self) -> OpenOutcome {
        let outcome = match self.receiver.recv() {
            Ok(outcome) => outcome,
            Err(_) => worker_died(&self.path, self.elapsed_ms()),
        };
        self.settled = true;
        outcome
    }

    /// 限时等待。超时返回 `None`，任务继续在后台跑。
    pub fn wait_timeout(&mut self, timeout: Duration) -> Option<OpenOutcome> {
        let outcome = match self.receiver.recv_timeout(timeout) {
            Ok(outcome) => Some(outcome),
            Err(RecvTimeoutError::Timeout) => return None,
            Err(RecvTimeoutError::Disconnected) => {
                Some(worker_died(&self.path, self.elapsed_ms()))
            }
        };
        self.settled = true;
        outcome
    }
}

/// 线程主体。任何路径都必须产出 `OpenOutcome`（含失败），不允许 panic 逃逸。
fn run(path: &Path, limits: DecodeLimits, started: Instant) -> OpenOutcome {
    let file = stat(path);
    trace::step(
        "open",
        format!(
            "后台线程启动：{} 文件大小={:?} 修改时间={:?}",
            path.display(),
            file.as_ref().map(|stat| stat.byte_len),
            file.as_ref().and_then(|stat| stat.modified),
        ),
    );

    let decode_started = Instant::now();
    let result = decode::decode_path(path, &limits);
    let decode_ms = decode_started.elapsed().as_secs_f64() * 1000.0;

    let outcome = OpenOutcome {
        path: path.to_path_buf(),
        file,
        result,
        decode_ms,
        total_ms: started.elapsed().as_secs_f64() * 1000.0,
    };

    // 只打这一条：探测到的格式、解码耗时、失败原因都在里面，
    // 且不涉及像素内容。未开启打点时是一次静态 bool 判断。
    if perf::enabled() {
        perf::log_always(&outcome.log_line());
    }

    match &outcome.result {
        Ok(data) => trace::step(
            "open",
            format!(
                "解码完成：{} {}×{} 帧数={} 方向={:?} 像素总字节={}",
                data.format.display_name(),
                data.primary().width,
                data.primary().height,
                data.frames.len(),
                data.orientation,
                data.frames.iter().map(|frame| frame.rgba8.len()).sum::<usize>(),
            ),
        ),
        Err(error) => trace::fail(
            "open",
            format!(
                "打开失败：{} → {}（用户可见文案：{}）",
                path.display(),
                error.short_reason(),
                error.user_message(),
            ),
        ),
    }

    outcome
}

fn stat(path: &Path) -> Option<FileStat> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(FileStat {
        byte_len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn worker_died(path: &Path, total_ms: f64) -> OpenOutcome {
    OpenOutcome {
        path: path.to_path_buf(),
        file: None,
        result: Err(DecodeError::corrupt("解码线程意外结束，未能产出结果")),
        decode_ms: 0.0,
        total_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ngy-open-job-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 手写一张 2×2 的 PNG，避免测试依赖任何二进制样本文件。
    fn write_png(path: &Path) {
        use image::{ImageBuffer, Rgba};
        let image: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(2, 2, |x, y| Rgba([(x * 100) as u8, (y * 100) as u8, 40, 255]));
        image.save(path).expect("写入测试 PNG 失败");
    }

    #[test]
    fn successful_open_reports_pixels_and_timing() {
        let dir = temp_dir("ok");
        let path = dir.join("ok.png");
        write_png(&path);

        let mut task = OpenTask::spawn(&path);
        let outcome = task.wait();

        // 先做所有「借用 outcome」的断言，再把它拆开取像素，避免部分移动。
        assert!(outcome.is_ok(), "应能解出图片：{}", outcome.log_line());
        assert!(outcome.file.is_some());
        assert!(outcome.decode_ms >= 0.0);
        assert!(outcome.total_ms >= outcome.decode_ms);
        assert!(outcome.log_line().contains("PNG"));

        let data = outcome.result.expect("应能解出图片");
        assert_eq!(data.format, crate::decode::ImageFormat::Png);
        assert_eq!((data.width(), data.height()), (2, 2));
        assert_eq!(data.frame_count(), 1);
    }

    #[test]
    fn poll_is_idempotent_after_result_delivered() {
        let dir = temp_dir("poll");
        let path = dir.join("poll.png");
        write_png(&path);

        let mut task = OpenTask::spawn(&path);
        // 阻塞等到线程发完，再 poll：此时结果必定就绪。
        let first = task.wait_timeout(Duration::from_secs(5));
        assert!(first.is_some(), "解码不应超过 5 秒");
        // 结果已被上面的 wait_timeout 取走，poll 必须给出 None 而不是挂起，
        // 更不能把「通道已断开」误判成一次新的失败。
        assert!(task.poll().is_none());
        assert!(task.poll().is_none());
    }

    /// 覆盖 UI 实际使用的路径：反复 `poll` 直到结果出现，之后永远返回 `None`。
    #[test]
    fn poll_yields_result_exactly_once() {
        let dir = temp_dir("poll-loop");
        let path = dir.join("once.png");
        write_png(&path);

        let mut task = OpenTask::spawn(&path);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(outcome) = task.poll() {
                assert!(outcome.is_ok(), "{}", outcome.log_line());
                break;
            }
            assert!(Instant::now() < deadline, "5 秒内应完成解码");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(task.poll().is_none(), "结果只应交付一次");
    }

    #[test]
    fn missing_file_becomes_a_result_not_a_panic() {
        let dir = temp_dir("missing");
        let path = dir.join("does-not-exist.png");

        let mut task = OpenTask::spawn(&path);
        let outcome = task.wait();
        assert!(!outcome.is_ok());
        assert!(outcome.log_line().contains("失败"));

        match outcome.result.unwrap_err() {
            DecodeError::Io { .. } => {}
            other => panic!("期望 Io 错误，实际为 {other:?}"),
        }
    }

    #[test]
    fn elapsed_keeps_growing_while_worker_runs() {
        let dir = temp_dir("elapsed");
        let path = dir.join("elapsed.png");
        write_png(&path);

        let task = OpenTask::spawn(&path);
        let first = task.elapsed_ms();
        std::thread::sleep(Duration::from_millis(20));
        assert!(task.elapsed_ms() >= first);
    }
}
