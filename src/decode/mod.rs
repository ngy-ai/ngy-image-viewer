//! 解码层：把「磁盘上任意一个图片文件」变成「统一的内存位图 + 元数据」。
//!
//! # 边界
//!
//! 这一层以及它下面的 [`types`] **不依赖任何 UI / 渲染框架**，
//! 也不知道窗口、纹理、帧率的存在。它只认 `Path` 进、[`ImageData`] 出。
//! 这条边界是有意为之：图片查看器最核心的价值是「解码正确 + 足够快」，
//! 而这部分能力不应该和渲染框架的命运绑定在一起。
//!
//! # 派发流程
//!
//! ```text
//! decode_path(path)
//!   ├─ read_head()          只读文件头（16 KiB），不读整个文件
//!   ├─ sniff()              以 magic bytes 定格式，扩展名兜底并消歧
//!   ├─ decoder_for(format)  按格式挑解码器
//!   └─ 失败时依次尝试其它 probe 命中的解码器（互为兜底）
//! ```
//!
//! 「失败时依次尝试」看着多余，但它是「绝不静默失败」之外的另一个要求 ——
//! **绝不因为判错格式而打不开**。典型场景：NEF 与 TIFF 共用文件头，
//! 万一 RAW 解码器处理不了这张图，还可以退回当普通 TIFF 读。
//!
//! # 扩展新格式
//!
//! 实现 [`Decoder`] 并在 [`DecoderRegistry::new`] 里注册即可，
//! 调用方（`open_job`、UI）一行都不用改。

pub mod avif;
pub mod cur;
pub mod demosaic;
pub mod dds;
pub mod exif;
pub mod heic;
pub mod icns;
pub mod jp2;
pub mod jxl;
pub mod flif;
pub mod mng;
pub mod pict;
pub mod xbm;
pub mod xpm;
pub mod jxr;
pub mod orientation;
pub mod psd;
pub mod raster;
pub mod raw;
pub mod sniff;
pub mod svg;
pub mod types;
/// Windows 专属的 WIC 解码后端，供 HEIC 与 AVIF 共用。
#[cfg(windows)]
pub mod wic;

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::sync::OnceLock;

use crate::trace;

pub use types::{
    DecodeError, DecodeLimits, DecodeResult, ExifSummary, Frame, ImageData, ImageFormat, Orientation,
};

/// 所有解码器的统一契约。
pub trait Decoder: Send + Sync {
    /// 解码器标识，只用于日志。
    fn id(&self) -> &'static str;

    /// 本解码器能够处理的格式集合。
    ///
    /// 注意这里是复数：解码器与格式不是一对一关系
    /// （一个 `image` crate 就覆盖了十几个格式，将来还有平台分支）。
    fn formats(&self) -> &'static [ImageFormat];

    /// 仅依据文件头快速判断「我可能能处理这个文件」。
    ///
    /// 允许误报（后续 `decode` 会失败并被换掉），但不允许漏报：
    /// 它是「按格式派发」之外唯一的补救机会。
    fn probe(&self, head: &[u8]) -> bool;

    /// 真正解码。
    ///
    /// 实现必须遵守两条约定：
    /// - 纯计算，不做任何与 UI / 线程调度相关的事；
    /// - 失败返回语义明确的 [`DecodeError`]，**绝不 panic**（这是一条硬性产品要求）。
    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData>;
}

/// 解码器注册表：负责「格式 → 解码器」的派发与兜底。
pub struct DecoderRegistry {
    decoders: Vec<Box<dyn Decoder>>,
}

impl Default for DecoderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DecoderRegistry {
    pub fn new() -> Self {
        Self {
            decoders: vec![
                // 顺序 = 兜底尝试顺序，越靠前越优先。
                // 这些解码器的 `formats()` 互不重叠，实际派发由 magic bytes 决定，
                // 顺序只影响「内容判不出格式时谁先自荐」这种边缘情况。
                Box::new(raster::RasterDecoder),
                Box::new(jxl::JxlDecoder),
                Box::new(svg::SvgDecoder),
                // 容器与 ICO 完全相同的 Windows 光标，以及内嵌 PNG 的 macOS 图标，
                // 都走 `image` crate，与上面的栅格解码器同属一类。
                Box::new(cur::CurDecoder),
                Box::new(icns::IcnsDecoder),
                // Photoshop 文档（含图层合并图的纯 Rust 解码），与上面同属纯计算路径。
                // 注意 `self::psd` 指向本仓库的解码器模块——外部依赖 crate 也叫 `psd`，
                // 裸写 `psd::` 会被解析成那个依赖 crate，这里必须用 `self::` 消歧。
                Box::new(crate::decode::psd::PsdDecoder),
                // 平台后端排在最后：它们的文件头分别与 ISOBMFF 容器、TIFF 共用，
                // 而且都要依赖系统组件，只有前面都没认出来才轮到它们去尝试。
                Box::new(avif::AvifDecoder),
                Box::new(heic::HeicDecoder),
                Box::new(jxr::JxrDecoder),
                // JPEG 2000：走 jpeg2k 的 openjpeg-sys C 后端（跨平台），放在平台后端区末尾。
                Box::new(jp2::Jp2Decoder),
                // 纯计算、零 C 依赖的自写 / 纯 Rust 解码器：与上面同属纯计算路径。
                Box::new(xbm::XbmDecoder),
                Box::new(xpm::XpmDecoder),
                Box::new(flif::FlifDecoder),
                Box::new(pict::PictDecoder),
                // MNG / JNG：Rust 生态无解码库，仅识别 + 给出可操作拒绝（见 mng.rs）。
                Box::new(mng::MngDecoder),
                Box::new(raw::RawDecoder),
                // DDS 单独成解码器：Windows 走 WIC（覆盖 BC1–BC7），其余平台走 image crate。
                Box::new(dds::DdsDecoder),
            ],
        }
    }

    pub fn decoders(&self) -> impl Iterator<Item = &dyn Decoder> {
        self.decoders.iter().map(|decoder| decoder.as_ref())
    }

    /// 找出声明支持该格式的解码器。
    pub fn decoder_for(&self, format: ImageFormat) -> Option<&dyn Decoder> {
        self.decoders
            .iter()
            .find(|decoder| decoder.formats().contains(&format))
            .map(|decoder| decoder.as_ref())
    }

    fn index_for(&self, format: ImageFormat) -> Option<usize> {
        self.decoders
            .iter()
            .position(|decoder| decoder.formats().contains(&format))
    }

    /// 解码一个文件。
    pub fn decode_path(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        let head = read_head(src, sniff::HEAD_LEN)?;
        trace::step(
            "decode",
            format!(
                "读到文件头 {} 字节：{}",
                head.len(),
                trace::hex_preview(&head, 16),
            ),
        );
        // 空文件必须单独说清楚。让它落到解码器里会得到「failed to fill whole buffer」
        // 这种对用户毫无意义的报错，还可能被误判成「文件被其它程序占用」。
        if head.is_empty() {
            return Err(DecodeError::corrupt("文件为空（0 字节）"));
        }
        let sniffed = sniff::sniff(&head, Some(src));
        trace::step(
            "decode",
            format!(
                "格式判定：format={:?} 依据={:?} magic={:?} ext={:?} 内容与扩展名矛盾={}",
                sniffed.format,
                sniffed.source,
                sniffed.magic_format,
                sniffed.extension_format,
                sniffed.is_mismatch(),
            ),
        );

        // 建立候选顺序：先按内容判定的格式精确命中，再让其它解码器自荐兜底。
        let mut order: Vec<usize> = Vec::new();
        if let Some(format) = sniffed.format {
            if let Some(index) = self.index_for(format) {
                order.push(index);
            }
        }
        for (index, decoder) in self.decoders.iter().enumerate() {
            if !order.contains(&index) && decoder.probe(&head) {
                order.push(index);
            }
        }
        trace::step(
            "decode",
            format!(
                "候选解码器（按尝试顺序）：{}",
                order
                    .iter()
                    .map(|index| self.decoders[*index].id())
                    .collect::<Vec<_>>()
                    .join(" → "),
            ),
        );

        let mut last_error = None;
        for index in order {
            let decoder = self.decoders[index].id();
            let started = std::time::Instant::now();
            match self.decoders[index].decode(src, limits) {
                Ok(data) => {
                    trace::step(
                        "decode",
                        format!(
                            "解码器 {decoder} 成功：{} {}×{} 帧数={} 方向={:?} 超采样={} 耗时 {:.2} ms",
                            data.format.display_name(),
                            data.primary().width,
                            data.primary().height,
                            data.frames.len(),
                            data.orientation,
                            data.supersample(),
                            started.elapsed().as_secs_f64() * 1000.0,
                        ),
                    );
                    return Ok(data);
                }
                Err(error) => {
                    // 单个解码器失败**不算**整体失败（后面还有兜底），所以走阶段日志；
                    // 只有当所有候选都失败时，才由下面的 `fail` 定性为一次真正的失败。
                    trace::step(
                        "decode",
                        format!(
                            "解码器 {decoder} 失败（{:.2} ms）：{}",
                            started.elapsed().as_secs_f64() * 1000.0,
                            error.short_reason(),
                        ),
                    );
                    last_error = Some(error);
                }
            }
        }

        let error = last_error.unwrap_or_else(|| {
            DecodeError::unsupported(sniffed.format, unsupported_hint(sniffed.format))
        });
        trace::fail(
            "decode",
            format!(
                "全部候选解码器都没能解出 {}：{}",
                src.display(),
                error.short_reason(),
            ),
        );
        Err(error)
    }
}

/// 进程级共享的注册表。
///
/// 用 `OnceLock` 而不是 `lazy_static`：注册表里全是不可变数据，
/// 首次使用时初始化即可，之后是一次指针解引用，无锁、无原子操作开销。
pub fn registry() -> &'static DecoderRegistry {
    static REGISTRY: OnceLock<DecoderRegistry> = OnceLock::new();
    REGISTRY.get_or_init(DecoderRegistry::new)
}

/// 便利入口：用默认注册表解码。
pub fn decode_path(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    registry().decode_path(src, limits)
}

/// 读取文件头。
///
/// 单独抽出来是因为它同时服务于两个目的：格式探测，以及「文件能不能读」的早期判定。
/// 读不到文件时立刻失败，不必等到解码器启动之后。
pub fn read_head(src: &Path, max_len: usize) -> DecodeResult<Vec<u8>> {
    let mut file = File::open(src).map_err(|error| DecodeError::io(src, error))?;
    let mut buffer = vec![0u8; max_len];
    let mut filled = 0;

    while filled < max_len {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            // 信号中断属于可重试的瞬时错误，不该让它变成「打不开这张图」。
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(DecodeError::io(src, error)),
        }
    }

    buffer.truncate(filled);
    Ok(buffer)
}

/// 当前没有解码器可用的格式，给出一句能指导下一步的说明。
///
/// 这里的文案会随解码器逐步补齐而缩短；保留它是因为「暂不支持」本身
/// 也必须是一条明确、不甩锅的提示。
fn unsupported_hint(format: Option<ImageFormat>) -> String {
    match format {
        Some(ImageFormat::Avif) => "AVIF 解码组件尚未就绪。".to_string(),
        Some(ImageFormat::Jxl) => "JPEG XL 解码器尚未就绪。".to_string(),
        Some(ImageFormat::Svg) => "SVG 解码器尚未就绪。".to_string(),
        Some(ImageFormat::Heic) => "HEIC/HEIF 需要系统提供解码组件。".to_string(),
        Some(ImageFormat::Raw) => "相机 RAW 解码器尚未就绪。".to_string(),
        Some(other) => format!("当前构建未包含 {} 的解码器。", other.display_name()),
        None => "文件头不匹配任何已知的图片格式，扩展名也不是已知格式。".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_implemented_decoders_only() {
        let registry = registry();
        for format in [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Tiff,
            ImageFormat::Gif,
            ImageFormat::WebP,
            ImageFormat::Jxl,
            ImageFormat::Svg,
            ImageFormat::Raw,
            ImageFormat::Heic,
            ImageFormat::Avif,
            ImageFormat::Jxr,
            ImageFormat::Cur,
            ImageFormat::Icns,
            ImageFormat::Psd,
            ImageFormat::Jp2,
            ImageFormat::Xbm,
            ImageFormat::Xpm,
            ImageFormat::Flif,
            ImageFormat::Pict,
            ImageFormat::Mng,
            ImageFormat::Jng,
        ] {
            assert!(
                registry.decoder_for(format).is_some(),
                "{} 应当已有解码器",
                format.display_name()
            );
        }

        // 注册表里出现的每一个解码器，都必须至少声明一种格式；
        // 空声明会让 `decoder_for` 永远找不到它，等于注册了却没生效。
        for decoder in registry.decoders() {
            assert!(
                !decoder.formats().is_empty(),
                "解码器 {} 没有声明任何格式",
                decoder.id()
            );
        }
    }

    #[test]
    fn missing_file_reports_io_error() {
        let error = decode_path(Path::new("no/such/image.png"), &DecodeLimits::default())
            .expect_err("不存在的文件必须报错而不是 panic");
        assert!(matches!(error, DecodeError::Io { .. }));
    }

    #[test]
    fn empty_file_reports_corrupt_not_unsupported() {
        // 空文件必须被单独识别出来：告诉用户「文件为空」远比「无法识别格式」有用。
        let dir = std::env::temp_dir().join("ngy-decode-tests-empty");
        std::fs::create_dir_all(&dir).unwrap();

        for name in ["empty.bin", "empty.png"] {
            let path = dir.join(name);
            std::fs::write(&path, b"").unwrap();

            let error =
                decode_path(&path, &DecodeLimits::default()).expect_err("空文件不应解码成功");
            assert!(
                matches!(error, DecodeError::Corrupt { .. }),
                "{name} 实际得到 {error:?}"
            );
            assert!(error.user_message().contains("为空"));
        }
    }

    #[test]
    fn read_head_truncates_to_available_bytes() {
        let dir = std::env::temp_dir().join("ngy-decode-tests-head");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.bin");
        std::fs::write(&path, b"abcdef").unwrap();

        let head = read_head(&path, 1024).unwrap();
        assert_eq!(head, b"abcdef");
    }
}
