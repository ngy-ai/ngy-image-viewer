//! 解码层公共类型：领域数据、格式枚举、方向、限制与错误。
//!
//! 本模块**零 UI 依赖**，也不依赖任何具体解码库：只描述「一张图片解码后长什么样」。
//! 这样做的目的有两个：
//!
//! 1. 解码与元数据处理可以脱离窗口系统做单元测试（见 `tests/decode_test.rs`）；
//! 2. 将来若渲染层更换，这一层一行都不用改 —— 这是整个架构里最硬的一条边界。

use std::fmt;
use std::path::PathBuf;

/// 单帧像素数据。
///
/// 统一约定：**RGBA8、逐行紧密排列**（每行字节数 = `width * 4`，无 padding）。
/// 把各种原生布局（灰度、BGR、16bit、浮点）在解码阶段统一收敛成 RGBA8，
/// 是为了让渲染层只需面对一种布局；代价是少数格式多一次转换，
/// 但相比之后必然要发生的「上传 GPU 纹理」的拷贝，这个代价可以忽略。
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// 长度必须等于 `width * height * 4`。
    pub rgba8: Vec<u8>,
    /// 显示时长（毫秒）。静态图为 0。
    pub delay_ms: u32,
}

impl Frame {
    pub fn new(width: u32, height: u32, rgba8: Vec<u8>, delay_ms: u32) -> Self {
        debug_assert_eq!(
            rgba8.len(),
            width as usize * height as usize * 4,
            "帧数据长度与尺寸不符"
        );
        Self {
            width,
            height,
            rgba8,
            delay_ms,
        }
    }

    pub fn pixel_count(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    pub fn byte_len(&self) -> usize {
        self.rgba8.len()
    }

    /// 每行字节数。因为统一为紧密排列，它恒等于 `width * 4`。
    pub fn row_bytes(&self) -> usize {
        self.width as usize * 4
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// 播放时真正采用的帧间隔。
    ///
    /// 动图里 `delay = 0` 并不罕见（导出工具偷懒），若照原样播放会瞬间刷屏。
    /// 浏览器与主流查看器的既定行为是按 100ms 处理，这里保持一致。
    pub fn effective_delay_ms(&self) -> u32 {
        if self.delay_ms == 0 { 100 } else { self.delay_ms }
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 像素数组太长了，Debug 里只留形状信息，避免日志被几 MB 数字淹没。
        f.debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgba8_len", &self.rgba8.len())
            .field("delay_ms", &self.delay_ms)
            .finish()
    }
}

/// EXIF 方向的规范化表示（对应 EXIF 值 1..=8）。
///
/// 命名沿用「先旋转、再水平镜像」的语义，与 `image` crate 的 `Orientation` 一一对应，
/// 便于在两个世界之间无损互转。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Orientation {
    /// 1：无需纠正（绝大多数图片）。
    #[default]
    Normal,
    /// 2：水平镜像。
    FlipHorizontal,
    /// 3：旋转 180°。
    Rotate180,
    /// 4：垂直镜像。
    FlipVertical,
    /// 5：顺时针 90° 后水平镜像（转置）。
    Transpose,
    /// 6：顺时针 90°。竖拍手机照片最常见的方向。
    Rotate90,
    /// 7：顺时针 270° 后水平镜像（反转置）。
    Transverse,
    /// 8：顺时针 270°。
    Rotate270,
}

impl Orientation {
    /// 从 EXIF 原始值构造。越界或缺失时按「无需纠正」处理并交给调用方决定是否告警。
    pub fn from_exif(value: u8) -> Self {
        match value {
            2 => Self::FlipHorizontal,
            3 => Self::Rotate180,
            4 => Self::FlipVertical,
            5 => Self::Transpose,
            6 => Self::Rotate90,
            7 => Self::Transverse,
            8 => Self::Rotate270,
            _ => Self::Normal,
        }
    }

    pub fn from_exif_opt(value: Option<u16>) -> Self {
        value
            .and_then(|v| u8::try_from(v).ok())
            .map(Self::from_exif)
            .unwrap_or_default()
    }

    pub fn to_exif(self) -> u8 {
        match self {
            Self::Normal => 1,
            Self::FlipHorizontal => 2,
            Self::Rotate180 => 3,
            Self::FlipVertical => 4,
            Self::Transpose => 5,
            Self::Rotate90 => 6,
            Self::Transverse => 7,
            Self::Rotate270 => 8,
        }
    }

    /// 是否交换宽高（即需要旋转 90° 或 270°）。
    ///
    /// 渲染层据此决定「逻辑尺寸」是把图像宽高原样使用还是对调。
    pub fn swaps_axes(self) -> bool {
        matches!(self, Self::Transpose | Self::Rotate90 | Self::Transverse | Self::Rotate270)
    }

    pub fn is_identity(self) -> bool {
        matches!(self, Self::Normal)
    }

    /// 把方向拆成「是否水平镜像 + 顺时针 90° 的个数」的规范形式。
    ///
    /// 任何方向都能唯一写成 `R^k ∘ Fh^m`（先水平镜像，再顺时针旋转 k 个 90°）。
    /// 这是二面体群 D4 的标准表示，有了它，方向的复合只需要整数运算，
    /// 不必为 8×8 种组合手写一张表。
    fn form(self) -> (bool, u8) {
        match self {
            Self::Normal => (false, 0),
            Self::FlipHorizontal => (true, 0),
            Self::Rotate90 => (false, 1),
            Self::Transverse => (true, 1),
            Self::Rotate180 => (false, 2),
            Self::FlipVertical => (true, 2),
            Self::Rotate270 => (false, 3),
            Self::Transpose => (true, 3),
        }
    }

    /// [`Self::form`] 的逆运算。
    fn from_form(mirrored: bool, quarter_turns: u8) -> Self {
        match (mirrored, quarter_turns % 4) {
            (false, 0) => Self::Normal,
            (true, 0) => Self::FlipHorizontal,
            (false, 1) => Self::Rotate90,
            (true, 1) => Self::Transverse,
            (false, 2) => Self::Rotate180,
            (true, 2) => Self::FlipVertical,
            (false, 3) => Self::Rotate270,
            (true, 3) => Self::Transpose,
            // 取模之后不可能到这里；保留一个确定的分支以免引入 panic。
            _ => Self::Normal,
        }
    }

    /// 复合方向：**先应用 `self`，再应用 `next`**。
    ///
    /// 这是「按 EXIF 摆正之后再按用户操作旋转」这类叠加场景的基础。
    /// 参数顺序很容易记反，所以函数名刻意写成 `then` 而不是 `compose` ——
    /// 读作「先 A 然后 B」，与调用处的直觉一致。
    ///
    /// 推导：`E2 ∘ E1 = R^k2 ∘ Fh^m2 ∘ R^k1 ∘ Fh^m1`，
    /// 用 `Fh ∘ R^k = R^(-k) ∘ Fh` 把 `next` 的镜像项推到最左边，
    /// 就得到唯一的 `R^k ∘ Fh^m` 形式。
    pub fn then(self, next: Self) -> Self {
        let (first_mirrored, first_turns) = self.form();
        let (next_mirrored, next_turns) = next.form();

        let mirrored = first_mirrored ^ next_mirrored;
        let quarter_turns = if next_mirrored {
            (next_turns + 4 - first_turns % 4) % 4
        } else {
            (next_turns + first_turns) % 4
        };

        Self::from_form(mirrored, quarter_turns)
    }

    /// 顺时针旋转 `turns` 个 90° 之后的方向。供「旋转按钮」直接使用。
    pub fn rotated_clockwise(self, turns: u8) -> Self {
        self.then(match turns % 4 {
            1 => Self::Rotate90,
            2 => Self::Rotate180,
            3 => Self::Rotate270,
            _ => Self::Normal,
        })
    }

    /// 提供给信息面板的中文说明。
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "正常（无需纠正）",
            Self::FlipHorizontal => "水平镜像",
            Self::Rotate180 => "旋转 180°",
            Self::FlipVertical => "垂直镜像",
            Self::Transpose => "顺时针 90° 后水平镜像",
            Self::Rotate90 => "顺时针旋转 90°",
            Self::Transverse => "顺时针 270° 后水平镜像",
            Self::Rotate270 => "顺时针旋转 270°",
        }
    }
}

/// 图片格式。
///
/// 这里刻意把「容器格式」而不是「解码器」作为分类单位：RAW 各家的容器差异极大，
/// 但对用户而言都是「相机原片」，提示语与降级策略完全一致，没必要在类型上再细分。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    WebP,
    Tiff,
    Bmp,
    Ico,
    Pnm,
    Tga,
    Dds,
    Hdr,
    OpenExr,
    Qoi,
    Farbfeld,
    Avif,
    /// JPEG XL（含 codestream 与 ISOBMFF 两种封装）。
    Jxl,
    Svg,
    /// HEIC / HEIF（同一容器族的两种叫法，解码路径完全一致）。
    Heic,
    /// 相机原片（CR2/NEF/ARW/ORF/RAF/DNG...）。
    Raw,
    /// JPEG XR（又称 HD Photo，扩展名 `.jxr` `.wdp` `.hdp`）。
    /// Windows 上由 WIC 原生解码，其它平台暂无后端。
    Jxr,
    /// Windows 光标文件（`.cur`），容器与 ICO 完全相同，仅类型字段不同。
    Cur,
    /// macOS 图标文件（`.icns`），容器内以 PNG（现代）或 JPEG 2000（旧）内嵌多尺寸图像。
    Icns,
    /// Photoshop 文档（`.psd` `.psb`）。走 `psd` crate 合成扁平化图层；
    /// CMYK / 多通道等 crate 不支持的颜色模式会被显式拒绝而非静默降级。
    Psd,
    /// JPEG 2000 系列（`.jp2` `.j2k` `.jpx` `.jpf` `.jpc` `.mj2`）。
    /// 走 jpeg2k 的 openjpeg-sys C 后端，跨平台（构建环境需具备 C 编译器与 Windows SDK）。
    Jp2,
    /// X BitMap（`.xbm`）：纯文本 C 数组，1 位单色，自写解析器展开为 RGBA8。
    Xbm,
    /// X PixMap（`.xpm`）：纯文本 C 数组，自写解析器，支持 1/2 字符/像素与透明度键。
    Xpm,
    /// FLIF（Free Lossless Image Format，`.flif`）：纯 Rust 解码器（仅 8 位、非动画、非隔行）。
    Flif,
    /// PICT（Apple QuickDraw picture，`.pict` `.pct` `.pic`）：纯 Rust 软栅格化解码器。
    Pict,
    /// MNG（多图像/动画容器，`.mng`）：Rust 生态无解码库，仅识别并给出可操作拒绝。
    Mng,
    /// JNG（JPEG 变体容器，`.jng`）：同 MNG，仅识别并给出可操作拒绝。
    Jng,
}

impl ImageFormat {
    /// 状态栏与提示语里展示的名字。
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::WebP => "WebP",
            Self::Tiff => "TIFF",
            Self::Bmp => "BMP",
            Self::Ico => "ICO",
            Self::Pnm => "PNM",
            Self::Tga => "TGA",
            Self::Dds => "DDS",
            Self::Hdr => "HDR",
            Self::OpenExr => "EXR",
            Self::Qoi => "QOI",
            Self::Farbfeld => "Farbfeld",
            Self::Avif => "AVIF",
            Self::Jxl => "JPEG XL",
            Self::Svg => "SVG",
            Self::Heic => "HEIC/HEIF",
            Self::Raw => "相机 RAW",
            Self::Jxr => "JPEG XR",
            Self::Cur => "CUR",
            Self::Icns => "ICNS",
            Self::Psd => "PSD",
            Self::Jp2 => "JPEG 2000",
            Self::Xbm => "XBM",
            Self::Xpm => "XPM",
            Self::Flif => "FLIF",
            Self::Pict => "PICT",
            Self::Mng => "MNG",
            Self::Jng => "JNG",
        }
    }

    /// 另存为时的默认扩展名。
    pub fn canonical_extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::WebP => "webp",
            Self::Tiff => "tiff",
            Self::Bmp => "bmp",
            Self::Ico => "ico",
            Self::Pnm => "pnm",
            Self::Tga => "tga",
            Self::Dds => "dds",
            Self::Hdr => "hdr",
            Self::OpenExr => "exr",
            Self::Qoi => "qoi",
            Self::Farbfeld => "ff",
            Self::Avif => "avif",
            Self::Jxl => "jxl",
            Self::Svg => "svg",
            Self::Heic => "heic",
            Self::Raw => "dng",
            Self::Jxr => "jxr",
            Self::Cur => "cur",
            Self::Icns => "icns",
            Self::Psd => "psd",
            Self::Jp2 => "jp2",
            Self::Xbm => "xbm",
            Self::Xpm => "xpm",
            Self::Flif => "flif",
            Self::Pict => "pict",
            Self::Mng => "mng",
            Self::Jng => "jng",
        }
    }

    /// 这个格式是否可能包含**按时间播放的**多帧内容。
    ///
    /// 用途有两个：决定要不要走动画解码路径，以及把「动图」与「多页文档」分开
    /// （见 [`ImageData::is_animated`] 与 `ImageDocument::is_paged`）。
    ///
    /// 它同时是「哪些格式的多个 frame 表示时间轴」这个问题的**唯一答案** ——
    /// `decode` 层只有 GIF / APNG / 动画 WebP 三条路径会产出多帧
    /// （见 `raster.rs` 的格式分派），这里与那里必须一致。
    pub fn may_be_animated(self) -> bool {
        matches!(self, Self::Png | Self::Gif | Self::WebP)
    }

    /// 是否可以编码回写（另存为用）。
    ///
    /// RAW / SVG / HEIC / JPEG XL 这类「只读」格式返回 `false`：
    /// 它们的编码器要么不存在、要么代价过高，与其在另存为时才失败，
    /// 不如在格式选择阶段就把它们排除掉。
    ///
    /// 注意 AVIF 也在只读之列：我们的 `image` 依赖刻意关掉了 `avif` 特性
    /// （它会拖进 rav1e 这一大坨编码器），所以这里**不能**声称能编码 ——
    /// 声称了就会在用户点下「保存」之后才报错。
    pub fn can_encode(self) -> bool {
        matches!(
            self,
            Self::Png
                | Self::Jpeg
                | Self::Gif
                | Self::WebP
                | Self::Tiff
                | Self::Bmp
                | Self::Ico
                | Self::Pnm
                | Self::Tga
                | Self::Qoi
        )
    }

    /// 该格式的编码器是否支持透明通道。
    ///
    /// 另存为 JPEG 时必须先把 alpha 摊平到白底，否则编码会直接失败 ——
    /// 这是「截图另存为.jpg」最常见的失败原因。
    pub fn supports_alpha(self) -> bool {
        !matches!(self, Self::Jpeg | Self::Bmp | Self::Pnm)
    }

    /// 由扩展名推断格式（**兜底路径**，优先级低于 magic bytes）。
    pub fn from_extension(ext: &str) -> Option<Self> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        Some(match ext.as_str() {
            "png" | "apng" => Self::Png,
            "jpg" | "jpeg" | "jpe" | "jfif" | "jif" => Self::Jpeg,
            "gif" => Self::Gif,
            "webp" => Self::WebP,
            "tif" | "tiff" => Self::Tiff,
            "bmp" | "dib" => Self::Bmp,
            "ico" => Self::Ico,
            "pnm" | "pbm" | "pgm" | "ppm" | "pam" => Self::Pnm,
            "tga" | "targa" | "icb" | "vda" | "vst" => Self::Tga,
            "dds" => Self::Dds,
            "hdr" | "rgbe" => Self::Hdr,
            "exr" => Self::OpenExr,
            "qoi" => Self::Qoi,
            "ff" | "farbfeld" => Self::Farbfeld,
            "avif" | "avifs" => Self::Avif,
            "jxl" => Self::Jxl,
            "svg" | "svgz" => Self::Svg,
            "heic" | "heif" | "hif" | "avci" | "avcs" => Self::Heic,
            "jxr" | "wdp" | "hdp" => Self::Jxr,
            "cur" => Self::Cur,
            "icns" => Self::Icns,
            "psd" | "psb" => Self::Psd,
            "jp2" | "j2k" | "jpx" | "jpf" | "jpc" | "mj2" => Self::Jp2,
            "xbm" => Self::Xbm,
            "xpm" => Self::Xpm,
            "flif" => Self::Flif,
            "pict" | "pct" | "pic" => Self::Pict,
            "mng" => Self::Mng,
            "jng" => Self::Jng,
            "cr2" | "cr3" | "crw" | "nef" | "nrw" | "arw" | "sr2" | "srf" | "orf" | "raf"
            | "rw2" | "raw" | "dng" | "pef" | "ptx" | "rwl" | "rwz" | "x3f" | "3fr" | "fff"
            | "iiq" | "mos" | "mrw" | "srw" | "erf" | "mef" | "kap" | "dcr" | "k25" | "kdc"
            | "mdc" | "bay" | "rdc" | "cxi" | "eip" | "qtk" | "pxn" => Self::Raw,
            _ => return None,
        })
    }

    /// 由扩展名推断，输入接受 `Path::extension()` 的结果。
    pub fn from_extension_os(ext: &std::ffi::OsStr) -> Option<Self> {
        ext.to_str().and_then(Self::from_extension)
    }
}

/// EXIF 中与「看图和了解这张照片」相关的字段。
///
/// 只保留极少数真正会被用户看的项：相机、镜头、曝光三要素、时间。
/// 不做通用的「EXIF 全表」展示，因为那对看图没有帮助，只会把面板淹掉。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct ExifSummary {
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens: Option<String>,
    pub software: Option<String>,
    pub exposure_time: Option<String>,
    pub f_number: Option<String>,
    pub iso: Option<String>,
    pub focal_length: Option<String>,
    pub date_taken: Option<String>,
    /// 原始 EXIF 方向值（1..=8）。保留它是因为方向异常时用户与开发者都需要看到原始值。
    pub orientation_raw: Option<u16>,
}

impl ExifSummary {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// 组装「厂商 + 型号」，并去掉常见的前缀重复。
    ///
    /// 厂商字段的写法非常不统一：`Canon` / `NIKON CORPORATION` / `OLYMPUS IMAGING CORP.`。
    /// 直接拼接会得到 `NIKON CORPORATION NIKON Z 7` 这种明显重复的标题。
    /// 这里取厂商名的**首个单词**做前缀比较，能覆盖绝大多数机型的实际写法。
    pub fn camera(&self) -> Option<String> {
        match (&self.camera_make, &self.camera_model) {
            (Some(make), Some(model)) => {
                let key = make_key(make);
                if !key.is_empty() && model.to_ascii_lowercase().starts_with(&key) {
                    Some(model.clone())
                } else {
                    Some(format!("{make} {model}"))
                }
            }
            (None, Some(model)) => Some(model.clone()),
            (Some(make), None) => Some(make.clone()),
            (None, None) => None,
        }
    }
}

/// 取厂商名的首个有效单词（小写），用于与型号做前缀比较。
fn make_key(make: &str) -> String {
    make.to_ascii_lowercase()
        .split(|c: char| c.is_whitespace() || c == ',' || c == '.')
        .find(|token| !token.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// 解码资源上限。
///
/// 存在的意义是「宁可给一条明确的提示，也不要 OOM 或长时间无响应」：
/// 超大图（全景扫描、科研影像、恶意的解压炸弹）在这一层就被挡住。
#[derive(Clone, Copy, Debug)]
pub struct DecodeLimits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_pixels: u64,
    pub max_frames: usize,
    /// 交给底层解码器的总分配上限（字节）。
    pub max_alloc_bytes: u64,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            // 32768 覆盖所有现实中的显示器与相机输出，也远大于 GPU 常见纹理上限。
            max_width: 32_768,
            max_height: 32_768,
            // 1.2 亿像素：约 14000×8600 的单反全景，再往上就按降级路径处理。
            max_pixels: 120_000_000,
            max_frames: 512,
            max_alloc_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

impl DecodeLimits {
    /// 不设任何限制。仅供测试与「用户明确要求强行打开」的路径使用。
    pub fn unlimited() -> Self {
        Self {
            max_width: u32::MAX,
            max_height: u32::MAX,
            max_pixels: u64::MAX,
            max_frames: usize::MAX,
            max_alloc_bytes: u64::MAX,
        }
    }

    /// 尺寸校验。宽高与总像素数任一越界都拒绝。
    pub fn check_dimensions(&self, width: u32, height: u32) -> Result<(), DecodeError> {
        if width == 0 || height == 0 {
            return Err(DecodeError::Corrupt {
                message: "图像尺寸为 0，文件可能不完整".to_string(),
            });
        }
        if width > self.max_width || height > self.max_height {
            return Err(DecodeError::TooLarge {
                width,
                height,
                pixels: width as u64 * height as u64,
                limit: self.max_pixels,
            });
        }
        let pixels = width as u64 * height as u64;
        if pixels > self.max_pixels {
            return Err(DecodeError::TooLarge {
                width,
                height,
                pixels,
                limit: self.max_pixels,
            });
        }
        Ok(())
    }

    pub fn check_frame_count(&self, frames: usize) -> Result<(), DecodeError> {
        if frames > self.max_frames {
            return Err(DecodeError::TooManyFrames {
                frames,
                limit: self.max_frames,
            });
        }
        Ok(())
    }
}

/// 解码失败的原因。
///
/// 每个变体都对应一种**不同的用户动作**：
/// 装扩展、换文件、降低尺寸、上报格式……所以不能简单压扁成一个字符串。
#[derive(Clone, Debug)]
pub enum DecodeError {
    /// 磁盘层面读不到文件：不存在、无权限、被其它程序独占。
    Io { path: PathBuf, message: String },
    /// 认不出格式，或这个格式在当前平台没有可用解码器。
    Unsupported { format: Option<ImageFormat>, hint: String },
    /// 尺寸或像素数越界。
    TooLarge {
        width: u32,
        height: u32,
        pixels: u64,
        limit: u64,
    },
    /// 帧数越界（异常动图或损坏文件）。
    TooManyFrames { frames: usize, limit: usize },
    /// 累计内存占用越界（多见于多帧动图：单帧都不大，加起来撑爆内存）。
    TooMuchMemory { used: u64, limit: u64 },
    /// 解码器报错、文件被截断或内容损坏。
    Corrupt { message: String },
    /// 依赖平台能力但当前系统缺失（例如 Windows 没装 HEIF 图像扩展）。
    MissingPlatformSupport { what: String, how_to_fix: String },
}

impl DecodeError {
    pub fn io(path: impl Into<PathBuf>, error: impl fmt::Display) -> Self {
        Self::Io {
            path: path.into(),
            message: error.to_string(),
        }
    }

    pub fn unsupported(format: Option<ImageFormat>, hint: impl Into<String>) -> Self {
        Self::Unsupported {
            format,
            hint: hint.into(),
        }
    }

    pub fn corrupt(message: impl Into<String>) -> Self {
        Self::Corrupt {
            message: message.into(),
        }
    }

    /// 面向用户的一句话提示：说清「发生了什么」，并给出下一步能做什么。
    ///
    /// 产品要求是「绝不静默失败」，所以这里必须是可读、可操作的中文，
    /// 而不是把底层错误原样抛给用户。
    pub fn user_message(&self) -> String {
        match self {
            Self::Io { path, message } => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                if name.is_empty() {
                    // 底层解码库报的 IO 错误不带路径，此时不硬凑一个空文件名出来。
                    format!("读取文件失败：{message}")
                } else {
                    format!(
                        "无法读取「{name}」：{message}。文件可能已被移动、删除，或被其它程序占用。"
                    )
                }
            }
            Self::Unsupported { format, hint } => match format {
                Some(format) => format!(
                    "暂不支持解码 {} 格式：{hint}",
                    format.display_name()
                ),
                None => format!("无法识别的图片格式：{hint}"),
            },
            Self::TooLarge {
                width,
                height,
                pixels,
                limit,
            } => format!(
                "图像过大（{width}×{height}，约 {} 万像素），超过上限（约 {} 万像素）。已停止解码以避免内存耗尽。",
                pixels / 10_000,
                limit / 10_000
            ),
            Self::TooManyFrames { frames, limit } => format!(
                "动图帧数过多（{frames} 帧，上限 {limit} 帧），已停止解码以避免内存耗尽。"
            ),
            Self::TooMuchMemory { used, limit } => format!(
                "解码所需内存过大（约 {} MB，上限 {} MB）。若确实需要打开，可考虑先缩小图片。",
                used / (1024 * 1024),
                limit / (1024 * 1024)
            ),
            Self::Corrupt { message } => {
                format!("文件内容损坏或不完整：{message}。若文件来自网络，可尝试重新下载。")
            }
            Self::MissingPlatformSupport { what, how_to_fix } => {
                format!("当前系统缺少解码 {what} 所需的组件。{how_to_fix}")
            }
        }
    }

    /// 给日志用的一句话摘要：短、无换行、不引导用户。
    pub fn short_reason(&self) -> String {
        match self {
            Self::Io { message, .. } => format!("io: {message}"),
            Self::Unsupported { format, hint } => match format {
                Some(format) => format!("unsupported({}): {hint}", format.display_name()),
                None => format!("unsupported: {hint}"),
            },
            Self::TooLarge {
                width,
                height,
                pixels,
                limit,
            } => format!("too_large: {width}x{height} pixels={pixels} limit={limit}"),
            Self::TooManyFrames { frames, limit } => {
                format!("too_many_frames: frames={frames} limit={limit}")
            }
            Self::TooMuchMemory { used, limit } => {
                format!("too_much_memory: used={used} limit={limit}")
            }
            Self::Corrupt { message } => format!("corrupt: {message}"),
            Self::MissingPlatformSupport { what, .. } => format!("missing_platform_support: {what}"),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Display 走「日志口径」：精简、单行；面向用户的文案请用 `user_message()`。
        f.write_str(&self.short_reason())
    }
}

impl std::error::Error for DecodeError {}

/// 解码层统一的返回类型。
pub type DecodeResult<T> = Result<T, DecodeError>;

/// 一张（可能多帧的）图片解码完成后的全部数据。
#[derive(Clone)]
pub struct ImageData {
    pub format: ImageFormat,
    /// 至少一帧。静态图就是长度 1。
    pub frames: Vec<Frame>,
    /// 已从 EXIF 解析出的方向。**像素本身尚未应用该方向**，
    /// 因为旋转/翻转交由渲染层用变换完成，避免多一次全图拷贝。
    pub orientation: Orientation,
    pub exif: Option<ExifSummary>,
    /// 像素尺寸相对「逻辑尺寸」的倍率。
    ///
    /// 对绝大多数格式恒为 `1.0`：像素多大，图就是多大。
    /// 只有 SVG 会大于 1 —— 矢量图没有原生分辨率，为了在放大时仍然清晰，
    /// 我们会把它光栅化到比声明尺寸更高的分辨率。此时：
    ///
    /// - `Frame::width/height` 是**纹理**尺寸（真实像素）；
    /// - [`ImageData::width`] / [`ImageData::height`] 是**逻辑**尺寸（SVG 自己声明的尺寸），
    ///   也是状态栏显示、1:1 缩放、适应窗口计算所依据的尺寸。
    pub supersample: f32,
}

impl ImageData {
    pub fn single(format: ImageFormat, frame: Frame) -> Self {
        Self {
            format,
            frames: vec![frame],
            orientation: Orientation::Normal,
            exif: None,
            supersample: 1.0,
        }
    }

    /// 取「像素尺寸 → 逻辑尺寸」的换算倍率。
    ///
    /// 对非法值（0、负数、NaN、无穷）一律退回 `1.0`：
    /// 这个字段的唯一影响是显示尺寸，任何异常都不该让图片消失或尺寸变成 0。
    pub fn supersample(&self) -> f64 {
        if self.supersample.is_finite() && self.supersample > 0.0 {
            self.supersample as f64
        } else {
            1.0
        }
    }

    /// 把像素数换算成逻辑尺寸（四舍五入到至少 1 像素）。
    fn to_logical(&self, pixels: u32) -> u32 {
        let scale = self.supersample();
        if scale == 1.0 {
            return pixels;
        }
        let logical = (pixels as f64 / scale).round();
        if logical.is_finite() && logical >= 1.0 {
            logical as u32
        } else {
            1
        }
    }

    /// 用于展示与渲染的主帧（动图的第一帧）。
    pub fn primary(&self) -> &Frame {
        &self.frames[0]
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// 是否是「按时间自动播放的动画」。
    ///
    /// 判据必须带上**格式**这一维，不能只数帧数：多页 TIFF 补页之后
    /// `frames.len()` 也会大于 1，但那些是**页**而不是帧 ——
    /// 只数帧数的话，打开一份 30 页的 TIFF 就会像动图一样自己翻起来，
    /// 而且用户按「下一页」时永远追不上它。
    pub fn is_animated(&self) -> bool {
        self.format.may_be_animated() && self.frames.len() > 1
    }

    /// 逻辑宽度：已经考虑 EXIF 方向对宽高的交换，以及 SVG 的超采样倍率。
    pub fn width(&self) -> u32 {
        if self.orientation.swaps_axes() {
            self.to_logical(self.primary().height)
        } else {
            self.to_logical(self.primary().width)
        }
    }

    /// 逻辑高度：已经考虑 EXIF 方向对宽高的交换，以及 SVG 的超采样倍率。
    pub fn height(&self) -> u32 {
        if self.orientation.swaps_axes() {
            self.to_logical(self.primary().width)
        } else {
            self.to_logical(self.primary().height)
        }
    }

    pub fn pixel_count(&self) -> u64 {
        self.primary().pixel_count()
    }

    pub fn total_pixel_count(&self) -> u64 {
        self.frames.iter().map(Frame::pixel_count).sum()
    }

    /// 一轮播放的总时长（毫秒）。
    pub fn loop_duration_ms(&self) -> u64 {
        self.frames
            .iter()
            .map(|f| f.effective_delay_ms() as u64)
            .sum()
    }

    /// 按已播放时间取当前应显示的帧序号。静态图恒为 0。
    pub fn frame_index_at(&self, elapsed_ms: u64) -> usize {
        if self.frames.len() <= 1 {
            return 0;
        }
        let total = self.loop_duration_ms();
        if total == 0 {
            return 0;
        }
        let mut remaining = elapsed_ms % total;
        for (index, frame) in self.frames.iter().enumerate() {
            let delay = frame.effective_delay_ms() as u64;
            if remaining < delay {
                return index;
            }
            remaining -= delay;
        }
        self.frames.len() - 1
    }
}

impl fmt::Debug for ImageData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageData")
            .field("format", &self.format)
            .field("frames", &self.frames.len())
            .field("size", &format_args!("{}x{}", self.width(), self.height()))
            .field("orientation", &self.orientation)
            .field("exif", &self.exif.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, delay_ms: u32) -> Frame {
        Frame::new(width, height, vec![128; width as usize * height as usize * 4], delay_ms)
    }

    #[test]
    fn orientation_roundtrip() {
        for value in 1..=8u8 {
            let orientation = Orientation::from_exif(value);
            assert_eq!(orientation.to_exif(), value);
        }
        // 越界值退化为「无需纠正」，而不是 panic。
        assert_eq!(Orientation::from_exif(0), Orientation::Normal);
        assert_eq!(Orientation::from_exif(9), Orientation::Normal);
    }

    #[test]
    fn then_with_identity_is_a_no_op_on_both_sides() {
        for exif in 1..=8u8 {
            let orientation = Orientation::from_exif(exif);
            assert_eq!(orientation.then(Orientation::Normal), orientation);
            assert_eq!(Orientation::Normal.then(orientation), orientation);
        }
    }

    #[test]
    fn flipping_twice_in_a_row_returns_to_the_original() {
        for flip in [Orientation::FlipHorizontal, Orientation::FlipVertical] {
            assert_eq!(flip.then(flip), Orientation::Normal);
        }
        for rotation in [Orientation::Rotate90, Orientation::Rotate180, Orientation::Rotate270]
        {
            assert_eq!(rotation.then(rotation).then(rotation).then(rotation), Orientation::Normal);
        }
    }

    #[test]
    fn rotation_order_is_not_commutative_but_is_well_defined() {
        // 镜像与旋转不可交换 —— 这正是「按 EXIF 摆正之后再用户旋转」必须
        // 明确顺序的原因。这里把两种顺序的结果都钉死，防止将来改错。
        assert_eq!(
            Orientation::FlipHorizontal.then(Orientation::Rotate90),
            Orientation::Transverse
        );
        assert_eq!(
            Orientation::Rotate90.then(Orientation::FlipHorizontal),
            Orientation::Transpose
        );
    }

    #[test]
    fn rotated_clockwise_accumulates() {
        let mut orientation = Orientation::Normal;
        for _ in 0..4 {
            orientation = orientation.rotated_clockwise(1);
        }
        assert_eq!(orientation, Orientation::Normal);
        assert_eq!(
            Orientation::Normal.rotated_clockwise(1),
            Orientation::Rotate90
        );
        assert_eq!(
            Orientation::Normal.rotated_clockwise(3),
            Orientation::Rotate270
        );
        assert_eq!(Orientation::Normal.rotated_clockwise(4), Orientation::Normal);
    }

    #[test]
    fn swaps_axes_of_a_composition_is_the_xor_of_its_parts() {
        // 宽高是否交换只取决于「两个方向各自是否交换」的异或。
        // 这条不变量能挡住绝大多数方向复合的写错。
        for a in 1..=8u8 {
            for b in 1..=8u8 {
                let first = Orientation::from_exif(a);
                let second = Orientation::from_exif(b);
                assert_eq!(
                    first.then(second).swaps_axes(),
                    first.swaps_axes() ^ second.swaps_axes(),
                    "EXIF {a} then {b}"
                );
            }
        }
    }

    #[test]
    fn orientation_swaps_axes() {
        assert!(Orientation::Rotate90.swaps_axes());
        assert!(Orientation::Rotate270.swaps_axes());
        assert!(!Orientation::Rotate180.swaps_axes());
        assert!(!Orientation::FlipHorizontal.swaps_axes());
    }

    #[test]
    fn logical_size_follows_orientation() {
        let mut data = ImageData::single(ImageFormat::Jpeg, solid(40, 10, 0));
        assert_eq!((data.width(), data.height()), (40, 10));
        data.orientation = Orientation::Rotate90;
        assert_eq!((data.width(), data.height()), (10, 40));
    }

    #[test]
    fn supersample_shrinks_logical_size_back_to_declared_size() {
        // 一个 24×24 的 SVG 被光栅化到 192×192：像素是 8 倍，逻辑仍是 24×24。
        let mut data = ImageData::single(ImageFormat::Svg, solid(192, 192, 0));
        data.supersample = 8.0;
        assert_eq!((data.width(), data.height()), (24, 24));
        // 像素尺寸（纹理）保持原样，渲染层据此上传纹理。
        assert_eq!((data.primary().width, data.primary().height), (192, 192));

        // 非法的超采样倍率必须退化为 1，而不是让尺寸变成 0 或 NaN。
        for bad in [0.0f32, -2.0, f32::NAN, f32::INFINITY] {
            data.supersample = bad;
            assert_eq!(data.supersample(), 1.0, "倍率 {bad} 应被忽略");
            assert_eq!((data.width(), data.height()), (192, 192));
        }
    }

    #[test]
    fn frame_index_at_wraps_around() {
        let data = ImageData {
            format: ImageFormat::Gif,
            frames: vec![solid(1, 1, 100), solid(1, 1, 50)],
            orientation: Orientation::Normal,
            exif: None,
            supersample: 1.0,
        };
        assert_eq!(data.loop_duration_ms(), 150);
        assert_eq!(data.frame_index_at(0), 0);
        assert_eq!(data.frame_index_at(99), 0);
        assert_eq!(data.frame_index_at(100), 1);
        assert_eq!(data.frame_index_at(149), 1);
        // 第二轮回到第一帧。
        assert_eq!(data.frame_index_at(150), 0);
    }

    #[test]
    fn zero_delay_falls_back_to_100ms() {
        let frame = solid(1, 1, 0);
        assert_eq!(frame.effective_delay_ms(), 100);
    }

    #[test]
    fn extension_detection_covers_raw_and_aliases() {
        assert_eq!(ImageFormat::from_extension("JPG"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::from_extension(".jpeg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::from_extension("cr2"), Some(ImageFormat::Raw));
        assert_eq!(ImageFormat::from_extension("dng"), Some(ImageFormat::Raw));
        assert_eq!(ImageFormat::from_extension("hif"), Some(ImageFormat::Heic));
        assert_eq!(ImageFormat::from_extension("nope"), None);
    }

    #[test]
    fn limits_reject_oversized_and_degenerate_images() {
        let limits = DecodeLimits::default();
        assert!(limits.check_dimensions(4000, 3000).is_ok());
        assert!(matches!(
            limits.check_dimensions(0, 100),
            Err(DecodeError::Corrupt { .. })
        ));
        assert!(matches!(
            limits.check_dimensions(20_000, 20_000),
            Err(DecodeError::TooLarge { .. })
        ));

        let tiny = DecodeLimits {
            max_pixels: 100,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            tiny.check_dimensions(11, 10),
            Err(DecodeError::TooLarge { .. })
        ));
    }

    #[test]
    fn camera_name_avoids_duplicated_make() {
        let summary = ExifSummary {
            camera_make: Some("Canon".to_string()),
            camera_model: Some("Canon EOS R5".to_string()),
            ..ExifSummary::default()
        };
        assert_eq!(summary.camera().as_deref(), Some("Canon EOS R5"));

        let summary = ExifSummary {
            camera_make: Some("NIKON CORPORATION".to_string()),
            camera_model: Some("NIKON Z 7".to_string()),
            ..ExifSummary::default()
        };
        assert_eq!(summary.camera().as_deref(), Some("NIKON Z 7"));
    }

    #[test]
    fn every_error_has_actionable_message() {
        let samples = [
            DecodeError::io(PathBuf::from("a.png"), "拒绝访问"),
            DecodeError::unsupported(Some(ImageFormat::Heic), "缺少系统扩展"),
            DecodeError::corrupt("截断的 IDAT"),
            DecodeError::TooMuchMemory {
                used: 4 * 1024 * 1024 * 1024,
                limit: 2 * 1024 * 1024 * 1024,
            },
            DecodeError::MissingPlatformSupport {
                what: "HEIC".to_string(),
                how_to_fix: "请在 Microsoft Store 安装「HEIF 图像扩展」。".to_string(),
            },
        ];
        for error in samples {
            let message = error.user_message();
            assert!(!message.is_empty());
            assert!(!error.short_reason().is_empty());
        }
    }
}
