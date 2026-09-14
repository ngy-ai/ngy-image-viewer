//! 目录导航：找出当前文件在同目录里的上一个 / 下一个图片。
//!
//! # 为什么单独一个模块，以及为什么是后台线程
//!
//! 目录枚举通常只要几毫秒，但一个装了几万张图的文件夹（壁纸目录、截图目录）
//! 可以慢到几十甚至几百毫秒。它绝不能挡住「双击即见图」的关键路径：
//! 所以与 [`crate::open_job::OpenTask`] 同款 —— 独立线程 + 通道 + 非阻塞
//! `poll()`，UI 侧在渲染循环里顺手取结果，取不到就继续画当前的。
//!
//! # 判定「算不算图片」
//!
//! 与「打开」对话框共用同一份扩展名列表（[`crate::fs_ops::file_ops::IMAGE_EXTENSIONS`]）。
//! 真正的格式判定仍以内容为准（`decode/sniff.rs`），这里只是排序**候选**：
//! 扩展名认识就进列表，之后打不开的话错误会照常显示 —— 与用户直接双击
//! 这个文件的行为一致，不会比那更糟。
//!
//! # 环绕
//!
//! 到头之后绕回另一端（最后一张的「下一个」是第一张），与多数看图器一致。
//! 目录里只有当前一张图时不给邻居：绕回一圈得到的还是它自己，按了没反应
//! 不如明说「没有别的图」。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use crate::fs_ops::file_ops::IMAGE_EXTENSIONS;

/// 当前文件的相邻文件。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Neighbors {
    /// 按文件名排序后当前文件的前一个。
    pub previous: Option<PathBuf>,
    /// 按文件名排序后当前文件的后一个。
    pub next: Option<PathBuf>,
}

impl Neighbors {
    /// 枚举 `path` 所在目录，算出它的前后邻居。
    ///
    /// 任何一步走不通（没有父目录、目录读不到、当前文件不在列表里）都返回
    /// 空结果而不是错误：导航是尽力而为的能力，不该为了它打断看图。
    pub fn of(path: &Path) -> Self {
        let Some(dir) = path.parent() else {
            return Self::default();
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Self::default();
        };

        let current = file_name_lower(path);
        if current.is_empty() {
            return Self::default();
        }

        let mut files: Vec<PathBuf> = entries
            .flatten()
            // 只收文件：子目录里的同名文件不该把顺序搅乱。
            .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter(|entry| is_image_file_name(&entry.file_name().to_string_lossy()))
            .map(|entry| entry.path())
            .collect();

        // 排序键用小写名：Windows / macOS 的文件名不区分大小写，
        // `a.png` 与 `B.PNG` 必须有确定的先后，不能依赖文件系统给回的顺序。
        // 路径本身保留原样 —— 大小写敏感的文件系统上，改成小写就打不开了。
        files.sort_by_key(|path| file_name_lower(path));
        files.dedup();

        let Some(index) = files.iter().position(|p| file_name_lower(p) == current) else {
            return Self::default();
        };
        if files.len() < 2 {
            return Self::default();
        }

        // 环绕：`(i - 1 + n) % n`，避免 usize 下溢。
        let n = files.len();
        Self {
            previous: Some(files[(index + n - 1) % n].clone()),
            next: Some(files[(index + 1) % n].clone()),
        }
    }
}

/// 扩展名在「打开」对话框的图片列表里吗（大小写不敏感）。
fn is_image_file_name(name: &str) -> bool {
    let Some(dot) = name.rfind('.') else {
        return false;
    };
    let extension = name[dot + 1..].to_ascii_lowercase();
    IMAGE_EXTENSIONS.contains(&extension.as_str())
}

/// 文件名的统一小写形态，用于排序与定位当前文件。
fn file_name_lower(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// 后台目录扫描任务。结构与 [`crate::open_job::OpenTask`] 同款：
/// 结果只交付一次，`poll` 永不阻塞。
pub struct NeighborsTask {
    path: PathBuf,
    receiver: Receiver<Neighbors>,
    settled: bool,
    /// 连线程都创建不出来时，`poll` 直接交付一次空结果，不让导航永远停在「读取中」。
    pre_failed: bool,
}

impl NeighborsTask {
    /// 立刻在后台线程枚举 `path` 所在目录。
    pub fn spawn(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let (sender, receiver) = mpsc::channel();
        let worker_path = path.clone();

        let spawned = thread::Builder::new()
            .name("ngy-neighbors".to_string())
            .spawn(move || {
                let neighbors = Neighbors::of(&worker_path);
                // 接收端可能已被丢弃（又打开了别的图），忽略发送失败。
                let _ = sender.send(neighbors);
            });

        Self {
            path,
            receiver,
            settled: false,
            pre_failed: spawned.is_err(),
        }
    }

    /// 这个任务算的是哪个文件的同目录邻居。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 非阻塞取结果。取到后任务即结束，再调用返回 `None`。
    pub fn poll(&mut self) -> Option<Neighbors> {
        if self.pre_failed {
            self.settled = true;
            return Some(Neighbors::default());
        }
        if self.settled {
            return None;
        }
        let neighbors = match self.receiver.try_recv() {
            Ok(neighbors) => neighbors,
            Err(TryRecvError::Empty) => return None,
            // 线程结束却没发结果（不该发生）：当成空结果，导航降级但不挂死。
            Err(TryRecvError::Disconnected) => Neighbors::default(),
        };
        self.settled = true;
        Some(neighbors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        // 目录名带上进程号：上次运行留下的文件会把「目录里有哪些文件」
        // 这个前提整个破坏掉，断言跟着随机失败。
        let dir = std::env::temp_dir().join(format!("ngy-neighbors-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path) {
        std::fs::write(path, b"not really an image, but the extension is what counts here").unwrap();
    }

    fn names(neighbors: &Neighbors) -> (Option<String>, Option<String>) {
        let pick = |p: &Option<PathBuf>| {
            p.as_ref()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        };
        (pick(&neighbors.previous), pick(&neighbors.next))
    }

    #[test]
    fn neighbors_follow_filename_order_and_skip_non_images() {
        let dir = temp_dir("order");
        write(&dir.join("b.png"));
        write(&dir.join("a.jpg"));
        write(&dir.join("c.WEBP"));
        // 非图片扩展名与子目录都不进列表。
        write(&dir.join("notes.txt"));
        std::fs::create_dir(dir.join("subdir")).unwrap();

        let neighbors = Neighbors::of(&dir.join("b.png"));
        assert_eq!(names(&neighbors), (Some("a.jpg".into()), Some("c.WEBP".into())));
    }

    #[test]
    fn neighbors_wrap_around_both_ends() {
        let dir = temp_dir("wrap");
        write(&dir.join("one.png"));
        write(&dir.join("two.png"));
        write(&dir.join("three.png"));
        // 字母序：one < three < two（"th" < "tw"）。

        let first = Neighbors::of(&dir.join("one.png"));
        assert_eq!(names(&first), (Some("two.png".into()), Some("three.png".into())));

        let last = Neighbors::of(&dir.join("two.png"));
        assert_eq!(names(&last), (Some("three.png".into()), Some("one.png".into())));
    }

    #[test]
    fn single_image_has_no_neighbors() {
        let dir = temp_dir("single");
        write(&dir.join("only.png"));

        let neighbors = Neighbors::of(&dir.join("only.png"));
        assert_eq!(neighbors, Neighbors::default());
    }

    #[test]
    fn ordering_is_case_insensitive() {
        let dir = temp_dir("case");
        write(&dir.join("B.PNG"));
        write(&dir.join("a.png"));

        // 排序按小写名（a 在 b 前）；两张图时环绕让前后都指向对方，
        // 正是「往返切换」的预期行为。路径保留原样，小写化只用于排序与比较。
        let neighbors = Neighbors::of(&dir.join("B.PNG"));
        assert_eq!(names(&neighbors), (Some("a.png".into()), Some("a.png".into())));
    }

    #[test]
    fn current_file_missing_from_listing_yields_nothing() {
        let dir = temp_dir("missing-current");
        write(&dir.join("a.png"));
        write(&dir.join("b.png"));
        // 当前文件已被改名 / 删除：不猜邻居，直接放弃。
        let neighbors = Neighbors::of(&dir.join("gone.png"));
        assert_eq!(neighbors, Neighbors::default());
    }

    #[test]
    fn task_delivers_result_exactly_once() {
        let dir = temp_dir("task");
        write(&dir.join("a.png"));
        write(&dir.join("b.png"));

        let mut task = NeighborsTask::spawn(dir.join("a.png"));
        assert_eq!(task.path(), dir.join("a.png").as_path());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let neighbors = loop {
            if let Some(neighbors) = task.poll() {
                break neighbors;
            }
            assert!(std::time::Instant::now() < deadline, "目录扫描不应超过 5 秒");
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        // 两张图时环绕会让前后都指向对方 —— 这正是「往返切换」的预期行为。
        assert_eq!(names(&neighbors), (Some("b.png".into()), Some("b.png".into())));
        assert!(task.poll().is_none(), "结果只应交付一次");
    }
}
