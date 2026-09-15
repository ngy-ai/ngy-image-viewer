//! 具体文件操作：剪贴板、另存为、重命名 / 移动、删除到回收站。
//!
//! 本模块的每条公开函数都遵循同一个约定：**要么成功，要么返回一个
//! 能直接展示给用户的 [`FileOpError`]**，绝不 panic、绝不静默失败。
//! 这也是它与解码层共用的一条产品原则。

use std::borrow::Cow;
use std::fmt;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

use image::{DynamicImage, ImageFormat as CrateImageFormat, RgbImage, RgbaImage};

use crate::decode::ImageFormat;

/// 要写入剪贴板 / 另存为的位图：非预乘 RGBA8、逐行紧密排列。
///
/// 字段刻意是裸的：调用方（渲染层）手里已经有一块解码好的 RGBA 缓冲区，
/// 这里只借用、不拷贝。长度必须等于 `width * height * 4`，校验留给 [`Bitmap::validate`]，
/// 因为大多数调用点在此之前就已经知道数据是合法的，重复校验没有必要。
#[derive(Clone, Copy)]
pub struct Bitmap<'a> {
    pub width: u32,
    pub height: u32,
    pub rgba8: &'a [u8],
}

impl<'a> Bitmap<'a> {
    /// 校验像素数据长度与声明的尺寸是否自洽。
    ///
    /// 长度不符属于**调用方的 bug**（不是用户能修好的），所以单独一个错误变体，
    /// 而不是和「文件读写失败」混在一起 —— 两者的处理方式完全不同。
    pub fn validate(&self) -> Result<(), FileOpError> {
        let expected = expected_len(self.width, self.height);
        if self.rgba8.len() != expected {
            return Err(FileOpError::BadPixelData {
                expected,
                actual: self.rgba8.len(),
            });
        }
        Ok(())
    }

    /// 像素数据的期望字节数。
    pub fn byte_len(&self) -> usize {
        self.rgba8.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rgba8.is_empty()
    }
}

impl fmt::Debug for Bitmap<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 像素数组动辄几 MB，Debug 里只留形状，避免日志被数字淹没。
        f.debug_struct("Bitmap")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba8_len", &self.rgba8.len())
            .finish()
    }
}

/// 文件操作的统一失败类型。每个变体对应一种**不同的用户动作**。
///
/// 分类标准与解码层的 [`crate::decode::DecodeError`] 一致：只要用户看到之后
/// 该做的事不同，就必须是不同的变体。比如「格式不支持」要去导出面板换格式，
/// 「没权限」要去改文件属性，把两者压成一句字符串就会丢掉这个指引。
#[derive(Clone, Debug)]
pub enum FileOpError {
    /// 磁盘读写失败（不存在、无权限、被占用、磁盘满）。
    Io {
        path: PathBuf,
        /// 正在做的事情，例如「保存」「读取」「删除」。用于拼出「保存「x.png」失败」。
        action: &'static str,
        message: String,
    },
    /// 目标格式不支持编码（例如 AVIF / JPEG XL / RAW）。
    UnsupportedFormat { format: ImageFormat },
    /// 目标文件已存在且不允许覆盖。
    AlreadyExists { path: PathBuf },
    /// 扩展名无法识别出可写的图片格式。
    UnknownExtension { extension: String },
    /// 剪贴板不可用或被别的程序占用。
    Clipboard { message: String },
    /// 位图数据与声明的尺寸不符（调用方的 bug）。
    BadPixelData { expected: usize, actual: usize },
}

impl FileOpError {
    /// 构造一个带路径与动作的 IO 错误。
    ///
    /// 单独提供构造函数，是为了让「路径 + 动作」这两样东西**不可能被遗漏**：
    /// 底层错误原样丢给用户是没用的，必须说清「在哪个文件上、做什么时」出错。
    pub fn io(path: impl Into<PathBuf>, action: &'static str, error: impl fmt::Display) -> Self {
        Self::Io {
            path: path.into(),
            action,
            message: error.to_string(),
        }
    }

    /// 面向用户的一句话提示：说清「发生了什么」，并给出下一步能做什么。
    pub fn user_message(&self) -> String {
        match self {
            Self::Io {
                path,
                action,
                message,
            } => {
                let name = file_label(path);
                format!("{action}「{name}」失败：{message}。")
            }
            Self::UnsupportedFormat { format } => format!(
                "暂不支持保存为 {} 格式，请改用 PNG 或 JPEG 等常见格式。",
                format.display_name()
            ),
            Self::AlreadyExists { path } => {
                let name = file_label(path);
                format!("「{name}」已存在。为避免覆盖已有文件，操作已取消。")
            }
            Self::UnknownExtension { extension } => {
                if extension.is_empty() {
                    "无法从文件名推断图片格式，请给文件名加上 .png 或 .jpg 这样的扩展名。"
                        .to_string()
                } else {
                    format!(
                        "无法识别的图片扩展名「.{extension}」，请改用 PNG 或 JPEG 等常见格式。"
                    )
                }
            }
            Self::Clipboard { message } => format!(
                "复制到剪贴板失败：{message}。剪贴板可能正被其它程序占用，稍后再试一次。"
            ),
            Self::BadPixelData { expected, actual } => format!(
                "图像像素数据不完整（应有 {expected} 字节，实际 {actual} 字节），这是一个程序缺陷。"
            ),
        }
    }

    /// 给日志用的一句话摘要：短、无换行、不引导用户。
    pub fn short_reason(&self) -> String {
        match self {
            Self::Io {
                path,
                action,
                message,
            } => format!("io({action}): {}: {message}", path.display()),
            Self::UnsupportedFormat { format } => {
                format!("unsupported_format: {}", format.display_name())
            }
            Self::AlreadyExists { path } => format!("already_exists: {}", path.display()),
            Self::UnknownExtension { extension } => format!("unknown_extension: {extension}"),
            Self::Clipboard { message } => format!("clipboard: {message}"),
            Self::BadPixelData { expected, actual } => {
                format!("bad_pixel_data: expected={expected} actual={actual}")
            }
        }
    }
}

impl fmt::Display for FileOpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display 走「日志口径」：精简、单行；面向用户的文案请用 `user_message()`。
        f.write_str(&self.short_reason())
    }
}

impl std::error::Error for FileOpError {}

/// 把位图放进系统剪贴板。
///
/// # 为什么先校验再碰剪贴板
///
/// 数据长度不对是调用方的 bug，它和「剪贴板被占用」是两回事。先做长度校验，
/// 既能让这个 bug 在任何环境下都能被稳定复现（不受剪贴板可用性影响），
/// 也避免我们往系统剪贴板里塞一块长度自相矛盾的缓冲区。
pub fn copy_to_clipboard(bitmap: &Bitmap<'_>) -> Result<(), FileOpError> {
    bitmap.validate()?;

    let image = arboard::ImageData {
        width: bitmap.width as usize,
        height: bitmap.height as usize,
        bytes: Cow::Borrowed(bitmap.rgba8),
    };
    let mut clipboard = arboard::Clipboard::new().map_err(|error| FileOpError::Clipboard {
        message: error.to_string(),
    })?;
    clipboard
        .set_image(image)
        .map_err(|error| FileOpError::Clipboard {
            message: error.to_string(),
        })?;
    Ok(())
}

/// 把位图编码成文件。
///
/// - 目标格式由 `path` 的扩展名推断；
/// - 扩展名不认识时返回 [`FileOpError::UnknownExtension`]，
///   格式已知但当前构建不能编码时返回 [`FileOpError::UnsupportedFormat`]；
/// - **JPEG / BMP / PNM 不支持 alpha**：必须先把半透明像素摊平到白底，
///   否则编码器会直接失败（这是「截图另存为 .jpg」最常见的失败原因）。
///
/// # 为什么先编码到内存再落盘
///
/// 编码器是流式写入的：一旦中途失败，磁盘上就会留下一个**半截的、看起来像图片
/// 却打不开**的文件，用户会以为保存成功了。先写进内存缓冲区，编码全部成功之后
/// 才用一次 `fs::write` 落盘，就把「失败」和「写坏文件」这两件事彻底分开了。
/// 代价是多占一份编码后数据的内存，对单张图片完全可以接受。
pub fn save_bitmap(bitmap: &Bitmap<'_>, path: &Path) -> Result<(), FileOpError> {
    bitmap.validate()?;
    let format = resolve_writable_format(path)?;
    let encoder = crate_encoder_format(format)
        .ok_or(FileOpError::UnsupportedFormat { format })?;

    let image = build_image(bitmap, format)?;

    // 注意 `DynamicImage::write_to` 需要 `Write + Seek`（部分编码器要回填头部），
    // `Cursor<Vec<u8>>` 两者都满足，且让我们拿到一块完整的字节缓冲。
    let mut buffer = Cursor::new(Vec::new());
    image
        .write_to(&mut buffer, encoder)
        .map_err(|error| FileOpError::io(path, "编码", error))?;
    fs::write(path, buffer.into_inner()).map_err(|error| FileOpError::io(path, "保存", error))?;
    Ok(())
}

/// 删除到回收站（可恢复）。
///
/// `path` 是文件目录时应当拒绝（本产品只删单个文件）：
/// 一次误操作删掉整棵目录树的代价，远大于「多提供一步确认」的成本。
///
/// # 线程要求（Windows）
///
/// `Cargo.toml` 关闭了 `trash` 的默认特性，只保留 `coinit_apartmentthreaded`，
/// 也就是**假定调用线程已经初始化过 COM 单元**。因此本函数必须在 UI 主线程上调用，
/// 不要在后台线程里调。若将来确实要移出主线程，必须自行先 `CoInitializeEx`
/// 为 STA 单元，否则删除会以 HRESULT 失败。
///
/// # 为什么默认走回收站
///
/// 误删一张照片不该是不可恢复的。这里只提供「删除到回收站」这一种删除语义，
/// 不提供永久删除 —— 需要永久删除的用户可以直接去系统回收站里清空。
pub fn delete_to_trash(path: &Path) -> Result<(), FileOpError> {
    if path.is_dir() {
        return Err(FileOpError::io(
            path,
            "删除",
            "这是一个文件夹，本程序只支持删除单个文件",
        ));
    }

    trash::delete(path).map_err(|error| FileOpError::io(path, "删除", describe_trash_error(&error)))
}

/// 重命名 / 移动。同目录改名与跨目录移动都要支持。
///
/// 会依次校验：源存在、目标目录存在、目标不与自己相同、目标已存在时不覆盖。
/// 任一条不满足都返回明确错误，**不会覆盖已有文件**。
///
/// # 跨盘移动
///
/// [`std::fs::rename`] 只在同一个卷内可用；把照片挪到另一个盘符时系统会返回
/// 「不是同一个设备」。这种情况下退化为「复制 + 删除源文件」，并且**只有复制成功
/// 才删除源文件**，避免失败时两头空。代价是跨盘移动不具备原子性，这是可接受的：
/// 同盘移动仍然是原子的。
pub fn rename(from: &Path, to: &Path) -> Result<(), FileOpError> {
    // 目标与源完全相同时，等价于「文件已存在于目标位置」，直接按不可覆盖处理。
    // 只比较字面路径，不比较 canonical，这样「只改大小写」的改名（Windows 上常见）
    // 仍能正常走后面的流程 —— 它的 `to` 会命中「同一个文件」而放行。
    if from == to {
        return Err(FileOpError::AlreadyExists {
            path: to.to_path_buf(),
        });
    }
    if !from.exists() {
        return Err(FileOpError::io(from, "重命名", "源文件不存在"));
    }

    let target_dir = parent_dir(to);
    if !target_dir.is_dir() {
        return Err(FileOpError::io(&target_dir, "重命名", "目标文件夹不存在"));
    }

    // `to.exists()` 对「只改大小写」的改名在 Windows 上会为真（文件系统不区分大小写），
    // 此时 canonical 路径与源相同，属于合法改名，必须放行而不是误报「已存在」。
    if to.exists() && !same_file(from, to) {
        return Err(FileOpError::AlreadyExists {
            path: to.to_path_buf(),
        });
    }

    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if is_cross_device(&error) && from.is_file() => move_across_devices(from, to),
        Err(error) => Err(FileOpError::io(to, "重命名", error)),
    }
}

/// 弹出「另存为」对话框。返回 `None` 表示用户取消。
///
/// # 为什么不阻塞 UI 事件循环
///
/// `rfd` 的对话框是**阻塞式**的：它内部自己跑一个模态消息循环，直到用户做出选择才返回。
/// 如果把这段代码直接放在 GPUI 的主线程上，主线程就被这个模态循环占住，
/// 窗口无法重绘、无法响应。
///
/// 因此这里把对话框放到一个独立的命名线程上执行，主线程通过 channel 等待结果。
/// 由于对话框是模态的、用户**必须**做出选择，调用方等待是预期行为；
/// 关键区别是「等待发生在哪个线程、被谁占用」。
///
/// # 线程边界的未来改动点
///
/// 本函数是同步签名，主线程最终仍会阻塞在 `receiver.recv()` 上。将来若要真正异步，
/// 改动点就在这里：把 `receiver` 交给调用方、由 UI 的事件循环去轮询 / 唤醒，
/// 而不是在这里 `recv()`。目前的实现把「弹窗」与「等待」隔在同一处的相邻两行，
/// 就是为了让那次改动只需动这一小段。
///
/// # 平台注意
///
/// Windows 上 `rfd` 会自行初始化 COM 单元，放到独立线程可正常工作。
/// macOS 上的原生面板要求在主线程弹出；移植到 macOS 时这里需要改走平台的主线程调度，
/// 不能照搬「换一个线程」的做法。
/// 弹出「另存为」对话框，返回结果通道（非阻塞）。
///
/// # 为什么主线程不能在这里阻塞
///
/// `rfd` 的对话框是**阻塞式**的：它内部自己跑一个模态消息循环，直到用户做出选择才返回。
/// 如果把这段代码直接放在 GPUI 的主线程上，主线程就被这个模态循环占住，窗口无法重绘、
/// 无法响应 —— 用户把焦点切回主窗口时，系统会把它标记为「未响应」。
///
/// 因此这里把对话框放到一个独立的命名线程上执行，主线程**立即拿回一个接收端**，
/// 由 UI 在自己的事件循环里轮询（`view.rs` 的 `pump_dialog`）。调用线程的「等待」
/// 发生在一个不相关的工作线程上，主线程始终在泵 GPUI 的消息循环。
///
/// # 平台注意
///
/// Windows 上 `rfd` 会自行初始化 COM 单元，放到独立线程可正常工作。
/// macOS 上的原生面板要求在主线程弹出；移植到 macOS 时这里需要改走平台的主线程调度，
/// 不能照搬「换一个线程」的做法。
pub fn pick_save_path(suggested_name: &str, directory: Option<&Path>) -> mpsc::Receiver<Option<PathBuf>> {
    let suggested_name = suggested_name.to_string();
    let directory = directory.map(Path::to_path_buf);
    let (sender, receiver) = mpsc::channel();

    // 把 sender 交给对话框线程；失败就让它随错误一起被丢弃，
    // 接收端随即「断开」，调用方轮询时会收到 `Disconnected`、按取消处理。
    if thread::Builder::new()
        .name("ngy-save-dialog".to_string())
        .spawn(move || {
            let mut dialog = rfd::FileDialog::new().set_file_name(&suggested_name);
            if let Some(directory) = directory.as_deref() {
                dialog = dialog.set_directory(directory);
            }
            // 发送失败说明调用方已经不等了（例如窗口在对话框弹出前被关掉），忽略即可。
            let _ = sender.send(dialog.save_file());
        })
        .is_err()
    {
        // sender 已随 Err 被丢弃，receiver 断开。
    }

    receiver
}

/// 弹出「打开」对话框，返回结果通道（非阻塞）。
///
/// 与 [`pick_save_path`] 走同一套「对话框独立线程 + channel 回传」的方案，
/// 理由见那一处的说明（`rfd` 的对话框是阻塞的模态循环，但跑在独立线程上，
/// 主线程只拿回接收端、由 UI 在事件循环里轮询，因此界面不会被冻住）。
/// 这里只多了一个扩展名过滤器。
///
/// # 过滤器的作用与边界
///
/// 过滤器只负责「让用户少翻几屏无关文件」，**不承担格式判定**：格式最终以内容为准
/// （见 `decode/sniff.rs`），所以列表即使不全也不会打不开文件 —— 用户切到「所有文件」
/// 即可，选中之后照样能正常解码。也正因如此，这里刻意不从解码器注册表推导列表：
/// 注册表认识它的每一个格式，而这里要列的是「用户机器上常见的那些」。
pub fn pick_open_path() -> mpsc::Receiver<Option<PathBuf>> {
    let (sender, receiver) = mpsc::channel();

    // 把 sender 交给对话框线程；失败就让它随错误一起被丢弃，
    // 接收端随即「断开」，调用方轮询时会收到 `Disconnected`、按取消处理。
    if thread::Builder::new()
        .name("ngy-open-dialog".to_string())
        .spawn(move || {
            let dialog = rfd::FileDialog::new().add_filter("图片", IMAGE_EXTENSIONS);
            // 发送失败说明调用方已经不等了（例如窗口先被关掉），忽略即可。
            let _ = sender.send(dialog.pick_file());
        })
        .is_err()
    {
        // sender 已随 Err 被丢弃，receiver 断开。
    }

    receiver
}

/// 「打开」对话框里预置的图片扩展名。
///
/// `pub(crate)`：目录导航（`neighbors.rs`）用它筛选同目录的候选文件，
/// 两处必须始终是同一份列表 —— 导航跳过去却打不开的文件，比不跳更糟。
pub(crate) const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "gif", "webp", "bmp", "tif", "tiff", "ico", "svg", "tga", "dds",
    "qoi", "pnm", "ppm", "pgm", "pbm", "hdr", "exr", "jxl", "heic", "heif", "avif", "nef", "nrw",
    "arw", "srf", "sr2", "cr2", "cr3", "dng", "orf", "rw2", "raf", "pef", "srw", "jxr", "wdp",
    "hdp", "cur", "icns", "psd", "psb", "jp2", "j2k", "jpx", "jpf", "jpc", "mj2", "flif", "pict",
    "pct", "pic", "xbm", "xpm", "mng", "jng",
];

/// 弹出「重命名」对话框（本质是选一个新路径），返回结果通道（非阻塞）。
///
/// 系统没有「重命名」这个独立对话框，重命名用户视角上就是「在同一个文件夹里
/// 另存为一个新名字」，因此复用 [`pick_save_path`]，并把默认目录设为当前文件所在目录。
pub fn pick_rename_path(current: &Path) -> mpsc::Receiver<Option<PathBuf>> {
    let suggested_name = current
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "rename".to_string());
    pick_save_path(&suggested_name, Some(&parent_dir(current)))
}

/// 给出「另存为」时的默认文件名与默认扩展名。
///
/// 例如 `photo.heic` 建议另存为 `photo.png` —— 因为 HEIC 不可编码，
/// 直接沿用原扩展名会让用户在点下保存之后才失败。
///
/// 返回值是 `(文件名, 扩展名)`：扩展名单独返回，是因为文件对话框需要
/// 一个不带前导点的「默认类型」来预选格式。
pub fn suggest_save_name(current: &Path, format: ImageFormat) -> (String, String) {
    let stem = current
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "image".to_string());
    let extension = format.canonical_extension().to_string();
    (format!("{stem}.{extension}"), extension)
}

/// 按扩展名推断可写的图片格式。无法编码的格式返回 `None`。
///
/// 这是 UI 侧「文件对话框里应该预选哪种格式」的唯一查询入口：
/// 它同时排除了「扩展名不认识」和「认识但编不了」两种情况，
/// 所以返回值是 `Option` 而不是错误 —— 预选是尽力而为，不该打断用户。
pub fn writable_format_for(path: &Path) -> Option<ImageFormat> {
    resolve_writable_format(path).ok()
}

// ---------------------------------------------------------------------------
// 内部实现
// ---------------------------------------------------------------------------

/// 位图应当占用的字节数。集中在一处，保证各处校验用的是同一个公式。
fn expected_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 4
}

/// 解析扩展名并确认当前构建确实能编码该格式。
///
/// 返回值区分了两种「存不了」：格式已知但编不了（`UnsupportedFormat`），
/// 与格式根本不认识（`UnknownExtension`）。两者的用户动作不同，不能合并。
fn resolve_writable_format(path: &Path) -> Result<ImageFormat, FileOpError> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();

    if extension.is_empty() {
        return Err(FileOpError::UnknownExtension {
            extension: String::new(),
        });
    }

    match ImageFormat::from_extension(extension) {
        Some(format) if format.can_encode() => Ok(format),
        Some(format) => Err(FileOpError::UnsupportedFormat { format }),
        None => Err(FileOpError::UnknownExtension {
            extension: extension.to_ascii_lowercase(),
        }),
    }
}

/// 把解码层的 [`ImageFormat`] 映射到 `image` crate 的编码器格式。
///
/// 这是一层薄的适配：解码层既不想依赖 `image` crate（它背着整个解码树），
/// 编码却必须落到具体的编码器上。映射表只覆盖 `can_encode()` 为真的那些格式，
/// 因此正常情况下不会返回 `None`；返回 `None` 时上层按 `UnsupportedFormat` 处理。
fn crate_encoder_format(format: ImageFormat) -> Option<CrateImageFormat> {
    Some(match format {
        ImageFormat::Png => CrateImageFormat::Png,
        ImageFormat::Jpeg => CrateImageFormat::Jpeg,
        ImageFormat::Gif => CrateImageFormat::Gif,
        ImageFormat::WebP => CrateImageFormat::WebP,
        ImageFormat::Tiff => CrateImageFormat::Tiff,
        ImageFormat::Bmp => CrateImageFormat::Bmp,
        ImageFormat::Ico => CrateImageFormat::Ico,
        ImageFormat::Pnm => CrateImageFormat::Pnm,
        ImageFormat::Tga => CrateImageFormat::Tga,
        ImageFormat::Qoi => CrateImageFormat::Qoi,
        _ => return None,
    })
}

/// 把位图转成待编码的 `DynamicImage`，必要时摊平 alpha。
///
/// 关键取舍：**不支持 alpha 的格式必须在这之前摊平**，而且摊平之后要降级成
/// RGB8（而不是留着 alpha 通道设为 255）。JPEG 编码器只接受灰度 / RGB 输入，
/// 给它 RGBA 会直接报错 —— 这正是「截图另存为 .jpg 失败」的根因。
fn build_image(bitmap: &Bitmap<'_>, format: ImageFormat) -> Result<DynamicImage, FileOpError> {
    let expected = expected_len(bitmap.width, bitmap.height);
    if bitmap.rgba8.len() != expected {
        return Err(FileOpError::BadPixelData {
            expected,
            actual: bitmap.rgba8.len(),
        });
    }

    if format.supports_alpha() {
        let image = RgbaImage::from_raw(bitmap.width, bitmap.height, bitmap.rgba8.to_vec())
            .ok_or(FileOpError::BadPixelData {
                expected,
                actual: bitmap.rgba8.len(),
            })?;
        Ok(DynamicImage::ImageRgba8(image))
    } else {
        let pixels = flatten_to_rgb(bitmap.width, bitmap.height, bitmap.rgba8)?;
        let image = RgbImage::from_raw(bitmap.width, bitmap.height, pixels).ok_or(
            FileOpError::BadPixelData {
                expected,
                actual: bitmap.rgba8.len(),
            },
        )?;
        Ok(DynamicImage::ImageRgb8(image))
    }
}

/// 把 RGBA8 摊平到白底，输出 RGB8。
///
/// # 为什么是白底
///
/// 透明像素本身没有颜色，必须挑一个背景。白底有两个好处：多数文档 / 网页背景是白的，
/// 摊平后的观感与 PNG 预览最接近；而且「透明 → 白」是各类截图工具的既定行为，用户不会意外。
///
/// 合成采用标准「over」公式：`结果 = 前景 × α + 白底 × (1 − α)`。
/// 加 127 做四舍五入，避免 α 恰好为 128 时整除向偏暗一侧取整。
fn flatten_to_rgb(width: u32, height: u32, rgba8: &[u8]) -> Result<Vec<u8>, FileOpError> {
    let expected = expected_len(width, height);
    if rgba8.len() != expected {
        return Err(FileOpError::BadPixelData {
            expected,
            actual: rgba8.len(),
        });
    }

    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for pixel in rgba8.chunks_exact(4) {
        let alpha = pixel[3] as u32;
        for channel in 0..3 {
            let blended = (pixel[channel] as u32 * alpha + 255 * (255 - alpha) + 127) / 255;
            rgb.push(blended as u8);
        }
    }
    Ok(rgb)
}

/// 取目标路径的父目录；不带目录的相对文件名按「当前目录」处理。
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// 两个路径是否指向同一个文件（用于放行「只改大小写」的改名）。
///
/// 拿不到 canonical 路径时保守返回 `false`：宁可多报一次「已存在」，
/// 也不要在不确定的情况下覆盖掉用户的文件。
fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 判断底层 IO 错误是否为「跨设备 / 跨盘」。
///
/// 这里直接比对操作系统错误码（Windows `ERROR_NOT_SAME_DEVICE` = 17，
/// Unix `EXDEV` = 18），而不依赖 `std::io::ErrorKind::CrossesDevices`，
/// 是为了让判断在两端都确定、不受标准库版本差异影响。
fn is_cross_device(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    const NOT_SAME_DEVICE: i32 = 17;
    #[cfg(unix)]
    const NOT_SAME_DEVICE: i32 = 18;
    #[cfg(not(any(windows, unix)))]
    const NOT_SAME_DEVICE: i32 = -1;

    error.raw_os_error() == Some(NOT_SAME_DEVICE)
}

/// 跨盘移动的降级实现：复制成功之后才删除源文件。
///
/// 删除失败时不会回滚复制 —— 此时数据已安全到达目标位置，回滚反而会丢数据。
/// 这种情况下向用户报告「已复制但源文件未能删除」，比谎报整体失败有用。
fn move_across_devices(from: &Path, to: &Path) -> Result<(), FileOpError> {
    fs::copy(from, to).map_err(|error| FileOpError::io(to, "移动（跨盘复制）", error))?;
    fs::remove_file(from)
        .map_err(|error| FileOpError::io(from, "移动（删除源文件）", error))?;
    Ok(())
}

/// 把 `trash` 的底层错误翻译成用户能看懂、能行动的一句话。
///
/// 直接把 HRESULT 或 `Os { code }` 抛给用户是没用的：用户既不知道 `0x80004005` 是什么，
/// 也不知道该做什么。这里按「用户下一步能做什么」重新分类。
fn describe_trash_error(error: &trash::Error) -> String {
    match error {
        trash::Error::CouldNotAccess { .. } => {
            "文件不存在，或当前程序没有访问它的权限".to_string()
        }
        trash::Error::TargetedRoot => "出于安全考虑，不能删除磁盘根目录".to_string(),
        trash::Error::CanonicalizePath { .. } => "无法解析该文件的真实路径".to_string(),
        trash::Error::ConvertOsString { .. } => "文件路径中包含无法处理的字符".to_string(),
        trash::Error::Os { code, .. } => format!(
            "系统拒绝删除（错误码 {code}），回收站可能已被关闭，或文件正被其它程序占用"
        ),
        // 其余变体（含平台专属与「恢复」相关）归为同一类提示：
        // 对删除动作而言，用户能做的都是「稍后重试或先手动处理」。
        _ => "回收站当前不可用（可能已被禁用，或磁盘空间不足）".to_string(),
    }
}

/// 取用于展示的文件名；拿不到时退化为完整路径，而不是显示空白。
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::DecodeLimits;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ngy-fs-ops-{tag}"));
        // 每次从干净目录开始：用例会创建 / 改名 / 移动文件，上一次运行残留的文件
        // 会让「目标已存在」这类断言出现假失败，让测试变得不可重复。
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bitmap<'a>(width: u32, height: u32, rgba8: &'a [u8]) -> Bitmap<'a> {
        Bitmap {
            width,
            height,
            rgba8,
        }
    }

    #[test]
    fn writable_formats_follow_extension_and_encodability() {
        for name in ["a.png", "a.JPG", "a.jpeg", "a.bmp", "a.webp"] {
            assert!(
                writable_format_for(Path::new(name)).is_some(),
                "{name} 应当可写"
            );
        }
        // 已知但本构建不可编码的格式。
        for name in ["a.avif", "a.jxl", "a.heic", "a.nef"] {
            assert!(
                writable_format_for(Path::new(name)).is_none(),
                "{name} 不应被判为可写"
            );
        }
        // 无扩展名 / 完全不认识的扩展名。
        for name in ["a", "a.txt", "a.unknown"] {
            assert!(
                writable_format_for(Path::new(name)).is_none(),
                "{name} 不应被判为可写"
            );
        }
    }

    #[test]
    fn suggest_save_name_replaces_unencodable_extension() {
        // HEIC 源建议另存为 PNG：不能沿用不可编码的扩展名。
        let (name, extension) = suggest_save_name(Path::new("C:/photos/photo.heic"), ImageFormat::Png);
        assert_eq!(name, "photo.png");
        assert_eq!(extension, "png");

        // PNG 源保留原名原扩展名。
        let (name, extension) = suggest_save_name(Path::new("photo.png"), ImageFormat::Png);
        assert_eq!(name, "photo.png");
        assert_eq!(extension, "png");

        // 换格式时只替换扩展名。
        let (name, extension) = suggest_save_name(Path::new("shot.png"), ImageFormat::Jpeg);
        assert_eq!(name, "shot.jpg");
        assert_eq!(extension, "jpg");

        // 拿不到文件名时也不能返回空串（否则对话框会没有默认名）。
        let (name, _) = suggest_save_name(Path::new("C:/"), ImageFormat::Png);
        assert!(!name.is_empty(), "默认文件名不应为空");
    }

    #[test]
    fn alpha_is_flattened_over_white() {
        // 纯透明像素：没有自己的颜色，摊平后应当就是白底。
        let rgb = flatten_to_rgb(1, 1, &[255, 0, 0, 0]).unwrap();
        assert_eq!(rgb, vec![255, 255, 255]);

        // 半透明红叠到白底：红通道保持，绿蓝约为 (1 - 128/255) * 255 ≈ 127。
        let rgb = flatten_to_rgb(1, 1, &[255, 0, 0, 128]).unwrap();
        assert_eq!(rgb[0], 255);
        assert!(
            (rgb[1] as i32 - 127).abs() <= 1,
            "绿通道应接近 127，实际 {}",
            rgb[1]
        );
        assert!(
            (rgb[2] as i32 - 127).abs() <= 1,
            "蓝通道应接近 127，实际 {}",
            rgb[2]
        );

        // 完全不透明的像素必须原样保留，不能被白底污染。
        let rgb = flatten_to_rgb(1, 1, &[10, 20, 30, 255]).unwrap();
        assert_eq!(rgb, vec![10, 20, 30]);
    }

    #[test]
    fn mismatched_bitmap_size_is_reported_as_bad_pixel_data() {
        let bad = bitmap(4, 4, &[0u8; 10]);

        // 先于剪贴板访问就应当报错，从而不依赖任何系统状态。
        let error = copy_to_clipboard(&bad).expect_err("长度不符应报错");
        assert!(
            matches!(
                error,
                FileOpError::BadPixelData {
                    expected: 64,
                    actual: 10
                }
            ),
            "实际为 {error:?}"
        );

        let error = save_bitmap(&bad, Path::new("whatever.png")).expect_err("长度不符应报错");
        assert!(matches!(error, FileOpError::BadPixelData { .. }));

        // 长度正确时校验必须通过。
        assert!(bitmap(1, 1, &[0, 0, 0, 255]).validate().is_ok());
    }

    #[test]
    fn save_bitmap_separates_unsupported_format_from_unknown_extension() {
        let pixels = [1u8, 2, 3, 255];
        let bitmap = bitmap(1, 1, &pixels);
        let dir = temp_dir("format-errors");

        // 已知格式但本构建不编码 → UnsupportedFormat（用户应换格式）。
        let error = save_bitmap(&bitmap, &dir.join("a.avif")).expect_err("AVIF 应当被拒绝");
        assert!(
            matches!(
                error,
                FileOpError::UnsupportedFormat {
                    format: ImageFormat::Avif
                }
            ),
            "实际为 {error:?}"
        );

        // 不认识的扩展名 → UnknownExtension（用户应改扩展名）。
        let error = save_bitmap(&bitmap, &dir.join("a.txt")).expect_err("未知扩展名应被拒绝");
        assert!(matches!(error, FileOpError::UnknownExtension { .. }));

        // 完全没有扩展名。
        let error = save_bitmap(&bitmap, &dir.join("noext")).expect_err("无扩展名应被拒绝");
        assert!(matches!(error, FileOpError::UnknownExtension { .. }));

        // 上面几条都应在写文件之前失败，不能留下垃圾文件。
        assert!(!dir.join("a.avif").exists());
        assert!(!dir.join("a.txt").exists());
        assert!(!dir.join("noext").exists());
    }

    #[test]
    fn saving_png_keeps_alpha_and_saving_jpeg_flattens_it() {
        let dir = temp_dir("save-roundtrip");
        let pixels = [255u8, 0, 0, 128];
        let bitmap = bitmap(1, 1, &pixels);

        // PNG 支持 alpha：往返之后像素应当逐字节一致。
        let png = dir.join("out.png");
        save_bitmap(&bitmap, &png).expect("保存 PNG 失败");
        let data = crate::decode::decode_path(&png, &DecodeLimits::default()).expect("读回 PNG 失败");
        assert_eq!(data.primary().rgba8, vec![255, 0, 0, 128]);

        // JPEG 不支持 alpha：能编码成功本身就证明摊平发生了（否则编码器会报错），
        // 解回来再确认像素确实落在白底合成的结果附近。
        let jpg = dir.join("out.jpg");
        save_bitmap(&bitmap, &jpg).expect("保存 JPEG 失败");
        let data = crate::decode::decode_path(&jpg, &DecodeLimits::default()).expect("读回 JPEG 失败");
        let rgba = &data.primary().rgba8;
        assert_eq!(rgba[3], 255, "JPEG 不应带 alpha");
        assert!(
            rgba[0] > 240 && (110..=145).contains(&rgba[1]) && (110..=145).contains(&rgba[2]),
            "JPEG 摊平后的像素偏离白底合成结果：{rgba:?}"
        );
    }

    #[test]
    fn rename_validates_source_target_directory_and_overwrite() {
        let dir = temp_dir("rename");
        let from = dir.join("from.png");
        std::fs::write(&from, b"x").unwrap();

        // 目标目录不存在。
        let error = rename(&from, &dir.join("no-such-dir/out.png")).expect_err("目标目录缺失应报错");
        assert!(matches!(error, FileOpError::Io { .. }));

        // 目标已存在：不得覆盖，且源文件必须完好。
        let existing = dir.join("existing.png");
        std::fs::write(&existing, b"y").unwrap();
        let error = rename(&from, &existing).expect_err("目标已存在应报错");
        assert!(matches!(error, FileOpError::AlreadyExists { .. }));
        assert_eq!(std::fs::read(&from).unwrap(), b"x", "源文件不应被改动");
        assert_eq!(std::fs::read(&existing).unwrap(), b"y", "目标文件不应被覆盖");

        // 目标与源相同。
        let error = rename(&from, &from).expect_err("目标与源相同应报错");
        assert!(matches!(error, FileOpError::AlreadyExists { .. }));

        // 源不存在。
        let error = rename(&dir.join("missing.png"), &dir.join("out.png")).expect_err("源缺失应报错");
        assert!(matches!(error, FileOpError::Io { .. }));

        // 同目录改名。
        let renamed = dir.join("renamed.png");
        rename(&from, &renamed).expect("同目录改名失败");
        assert!(!from.exists() && renamed.exists());

        // 跨目录移动。
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let moved = sub.join("moved.png");
        rename(&renamed, &moved).expect("跨目录移动失败");
        assert!(!renamed.exists() && moved.exists());
        assert_eq!(std::fs::read(&moved).unwrap(), b"x", "移动后内容应保持不变");
    }

    #[test]
    fn delete_to_trash_refuses_directories_without_touching_the_trash() {
        let dir = temp_dir("trash-dir-reject");

        // 目录必须在调用任何回收站 API 之前就被拒绝，测试因此不会碰真实回收站。
        let error = delete_to_trash(&dir).expect_err("目录应被拒绝");
        assert!(matches!(error, FileOpError::Io { .. }));

        let message = error.user_message();
        assert!(
            message.contains("文件夹") || message.contains("目录"),
            "提示应说明「不支持删除文件夹」：{message}"
        );
        assert!(dir.exists(), "拒绝时不应删除任何东西");
    }

    #[test]
    fn every_error_has_a_non_empty_message_and_a_file_name_when_it_has_a_path() {
        let samples = [
            FileOpError::io(PathBuf::from("C:/photos/a.png"), "保存", "磁盘空间不足"),
            FileOpError::UnsupportedFormat {
                format: ImageFormat::Avif,
            },
            FileOpError::AlreadyExists {
                path: PathBuf::from("C:/photos/b.png"),
            },
            FileOpError::UnknownExtension {
                extension: "txt".to_string(),
            },
            FileOpError::UnknownExtension {
                extension: String::new(),
            },
            FileOpError::Clipboard {
                message: "剪贴板被占用".to_string(),
            },
            FileOpError::BadPixelData {
                expected: 64,
                actual: 10,
            },
        ];

        for error in &samples {
            let user = error.user_message();
            assert!(!user.is_empty(), "user_message 不应为空：{error:?}");

            let reason = error.short_reason();
            assert!(!reason.is_empty(), "short_reason 不应为空：{error:?}");
            assert!(!reason.contains('\n'), "short_reason 必须是单行：{reason}");
            // Display 走日志口径，必须与 short_reason 一致。
            assert_eq!(error.to_string(), reason);

            // 带路径的变体，用户提示里必须出现文件名。
            match error {
                FileOpError::Io { path, .. } | FileOpError::AlreadyExists { path } => {
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    assert!(
                        user.contains(&name),
                        "user_message 应包含文件名 {name}：{user}"
                    );
                }
                _ => {}
            }
        }
    }
}
