//! 多页 TIFF：逐页读取 IFD，按需解码。
//!
//! # 为什么必须单独一个解码器
//!
//! `image` crate（0.25）的 TIFF 解码器只读**第一个 IFD**，多页 TIFF 的其余页
//! 它根本不会碰。而扫描件、传真、传票、多页文档恰恰都是这种格式 ——
//! 只看第一页等于丢掉绝大部分内容，界面上却看不出任何异常。所以这里自己走 IFD 链。
//!
//! # 怎么拿到后面的页
//!
//! TIFF 是「随机访问」的容器：文件头里存着**第一个 IFD 的偏移**，IFD 之间用
//! next 指针串成链，而像素数据的偏移全是**绝对文件偏移**。所以「取第 k 页」不需要
//! 重新解析像素，只需要**让解码器以为第 k 页就是第一页** —— 把文件头里那个偏移
//! 换成目标页的偏移即可。
//!
//! 这件事由 [`PageView`] 完成：它包住文件、透传所有读写，只在文件头那几个字节上
//! 做替换。于是 `image` 的 TIFF 解码器可以**原样复用** —— 位深、色彩空间、CMYK、
//! 平面（planar）排列、条带与瓦片这些换算一行都不用重写，也就不会有「我们自己的
//! 实现和它不一致」这类最难查的偏差。这也正是本文件不去直接用底层 `tiff` crate 的
//! 原因：那样做，上面每一项都要自己再写一遍。
//!
//! `tiff::Decoder::new` 只读文件头指向的那一个 IFD（不顺着链走），所以**不需要**
//! 把上一页的 next 指针清零 —— 只改文件头一处即可。
//!
//! # 分页与懒解码
//!
//! `decode()` 只解**第 0 页**，这样「双击即出图」的感知速度不受页数影响；
//! 总页数由 [`Decoder::page_count`] 单独给出（只走 IFD 链、不碰像素），
//! 其余页由上层在后台按需调用 [`Decoder::decode_page`] 补齐。
//!
//! # 已知取舍
//!
//! 逐页的 `Orientation` 标签只在第 0 页被采纳：多页文档的各页一律按第一页的方向显示。
//! 真实的多页 TIFF 不会逐页翻转，为它每页多做一次全图重排不值得。

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use image::codecs::tiff::TiffDecoder as ImageTiffDecoder;
use image::metadata::Orientation as CrateOrientation;
use image::{DynamicImage, ImageDecoder};

use super::raster::{crate_limits, from_image_error, orientation_from_crate, summary_from_raw};
use super::types::{
    DecodeError, DecodeLimits, DecodeResult, ExifSummary, Frame, ImageData, ImageFormat, Orientation,
};
use super::Decoder;

pub struct TiffDecoder;

impl Decoder for TiffDecoder {
    fn id(&self) -> &'static str {
        "tiff"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Tiff]
    }

    fn probe(&self, head: &[u8]) -> bool {
        // 比 `sniff_magic` 宽松一点：它只认经典 TIFF 的 `II*\0` / `MM\0*`，
        // 而 BigTIFF（魔数 43）在这里也能正确解析，不该因为嗅探不到就没人接手。
        Layout::parse(head).is_some()
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        let (layout, offsets) = scan(src)?;
        let page = decode_page_at(src, layout, offsets[0], limits)?;
        Ok(ImageData {
            format: ImageFormat::Tiff,
            // 只有第 0 页：其余页由上层按需补（见模块文档「分页与懒解码」）。
            frames: vec![page.frame],
            orientation: page.orientation,
            exif: page.exif,
            supersample: 1.0,
        })
    }

    fn page_count(&self, src: &Path) -> DecodeResult<usize> {
        Ok(scan(src)?.1.len())
    }

    fn decode_page(&self, src: &Path, index: usize, limits: &DecodeLimits) -> DecodeResult<Frame> {
        let (layout, offsets) = scan(src)?;
        let offset = *offsets.get(index).ok_or_else(|| {
            DecodeError::corrupt(format!(
                "这张 TIFF 只有 {} 页，取不到第 {} 页",
                offsets.len(),
                index + 1
            ))
        })?;
        Ok(decode_page_at(src, layout, offset, limits)?.frame)
    }
}

/// 一页的完整产出。
struct DecodedPage {
    frame: Frame,
    orientation: Orientation,
    exif: Option<ExifSummary>,
}

// ---- 文件头与 IFD 链 ----

/// IFD 链的长度上限。
///
/// 损坏或恶意构造的链可以成环（第 k 页的 next 指回第 j 页），没有这个上限就是死循环。
/// 取 4096：比任何真实文档都大，又不至于让一次扫描耗时可见。
const MAX_PAGES: usize = 4096;

/// TIFF 文件头的关键布局。
///
/// 两种形态的差别只在「偏移有多宽」，而它决定了文件头里那个待替换字段的位置与长度，
/// 所以必须带着走，不能解析完就丢掉。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    little_endian: bool,
    bigtiff: bool,
}

impl Layout {
    const CLASSIC_HEADER_LEN: usize = 8;
    const HEADER_LEN: usize = 16;

    /// 识别文件头。不是 TIFF 时返回 `None`。
    fn parse(head: &[u8]) -> Option<Self> {
        if head.len() < Self::CLASSIC_HEADER_LEN {
            return None;
        }
        let little_endian = match &head[0..2] {
            b"II" => true,
            b"MM" => false,
            _ => return None,
        };
        match read_uint(&head[2..4], 2, little_endian)? {
            42 => Some(Self {
                little_endian,
                bigtiff: false,
            }),
            43 => {
                if head.len() < Self::HEADER_LEN {
                    return None;
                }
                // BigTIFF 紧跟魔数的两个字段是固定的：偏移宽度 8、保留字段 0。
                // 不满足就不是 BigTIFF，别硬当成它去解析 —— 那会让后面每一步都错位。
                if read_uint(&head[4..6], 2, little_endian)? != 8
                    || read_uint(&head[6..8], 2, little_endian)? != 0
                {
                    return None;
                }
                Some(Self {
                    little_endian,
                    bigtiff: true,
                })
            }
            _ => None,
        }
    }

    /// 「第一个 IFD 偏移」字段在文件里的位置与长度。
    fn first_ifd_field(self) -> (u64, usize) {
        if self.bigtiff {
            (8, 8)
        } else {
            (4, 4)
        }
    }

    fn first_ifd_offset(self, head: &[u8]) -> Option<u64> {
        let (offset, width) = self.first_ifd_field();
        let start = offset as usize;
        read_uint(head.get(start..start + width)?, width, self.little_endian)
    }

    /// 一个 IFD 里「条目数」字段的宽度。
    fn count_width(self) -> usize {
        if self.bigtiff {
            8
        } else {
            2
        }
    }

    /// 一个目录项的宽度。
    fn entry_width(self) -> usize {
        if self.bigtiff {
            20
        } else {
            12
        }
    }

    /// 把「第一个 IFD 偏移」字段改写成 `offset` 所需的字节，按**文件自己的字节序**编码。
    ///
    /// 返回 `(缓冲区, 有效长度)`：经典 TIFF 用前 4 字节，BigTIFF 用满 8 字节。
    ///
    /// 注意这里要的是**文件里的字节顺序**（值从低地址开始），而不是 u64 的顺序，
    /// 所以大端必须从 `to_be_bytes()` 的**尾部**取那几字节 —— 那个方法是右对齐的，
    /// 直接取前 4 字节只会得到一串 0。
    fn patched_field(self, offset: u64) -> ([u8; 8], usize) {
        let width = self.first_ifd_field().1;
        let raw = if self.little_endian {
            offset.to_le_bytes()
        } else {
            offset.to_be_bytes()
        };
        let mut bytes = [0u8; 8];
        if self.little_endian {
            bytes[..width].copy_from_slice(&raw[..width]);
        } else {
            bytes[..width].copy_from_slice(&raw[8 - width..]);
        }
        (bytes, width)
    }
}

/// 扫一遍 IFD 链，返回布局与**每一页的 IFD 偏移**。
///
/// 只读目录、不碰像素：一份 200 页的 TIFF 走完这条链也就是几十次小读取。
fn scan(src: &Path) -> DecodeResult<(Layout, Vec<u64>)> {
    let mut file = File::open(src).map_err(|error| DecodeError::io(src, error))?;
    let file_len = file
        .metadata()
        .map_err(|error| DecodeError::io(src, error))?
        .len();

    let mut header = [0u8; Layout::HEADER_LEN];
    let filled = read_at(src, &mut file, 0, &mut header)?;
    let layout = Layout::parse(&header[..filled])
        .ok_or_else(|| DecodeError::corrupt("TIFF 文件头不完整或签名不对"))?;
    let mut next = layout
        .first_ifd_offset(&header[..filled])
        .ok_or_else(|| DecodeError::corrupt("TIFF 文件头不完整"))?;

    let mut offsets = Vec::new();
    while next != 0 {
        if offsets.len() >= MAX_PAGES {
            return Err(DecodeError::corrupt(format!(
                "TIFF 的目录链超过 {MAX_PAGES} 页，可能已损坏"
            )));
        }
        if next >= file_len {
            return Err(DecodeError::corrupt(format!(
                "TIFF 目录偏移 {next} 超出文件末尾（{file_len} 字节），文件可能已损坏"
            )));
        }
        offsets.push(next);
        next = next_ifd_offset(src, &mut file, layout, next, file_len)?;
    }

    if offsets.is_empty() {
        return Err(DecodeError::corrupt("TIFF 里没有任何图像目录（IFD）"));
    }
    Ok((layout, offsets))
}

/// 读一个 IFD 的 next 指针。
fn next_ifd_offset(
    src: &Path,
    file: &mut File,
    layout: Layout,
    ifd: u64,
    file_len: u64,
) -> DecodeResult<u64> {
    let count_width = layout.count_width();
    let count = read_uint_at(src, file, ifd, count_width, layout.little_endian)?;
    // 条目数先卡一道：损坏的 count 会让下面的乘法跑到文件之外，在那里报错不如在这里拦住。
    if count > u64::from(u32::MAX) {
        return Err(DecodeError::corrupt(format!(
            "TIFF 目录声明了 {count} 个条目，明显不合法"
        )));
    }

    let field_width = layout.first_ifd_field().1;
    let next_at = ifd + count_width as u64 + count * layout.entry_width() as u64;
    if next_at + field_width as u64 > file_len {
        return Err(DecodeError::corrupt(
            "TIFF 目录的尾部超出文件末尾，文件可能已损坏",
        ));
    }
    read_uint_at(src, file, next_at, field_width, layout.little_endian)
}

/// 从文件里读一个 `width` 字节的无符号整数（按给定字节序解释）。
fn read_uint_at(
    src: &Path,
    file: &mut File,
    offset: u64,
    width: usize,
    little_endian: bool,
) -> DecodeResult<u64> {
    let mut buffer = [0u8; 8];
    // 大端的值在高位：把读到的字节放到缓冲区末尾，才能共用同一个 `from_be_bytes`。
    let start = if little_endian { 0 } else { 8 - width };
    let filled = read_at(src, file, offset, &mut buffer[start..start + width])?;
    if filled < width {
        return Err(DecodeError::corrupt(format!(
            "TIFF 目录在偏移 {offset} 处提前结束，文件可能已损坏"
        )));
    }
    Ok(if little_endian {
        u64::from_le_bytes(buffer)
    } else {
        u64::from_be_bytes(buffer)
    })
}

/// 定位读满 `buffer`，返回实际读到的字节数（文件短于请求时不足额）。
///
/// 与 `std::io::Read::read_exact` 的差别只在「不足额是返回值而不是错误」——
/// 调用方要自己判断这是「文件短」还是「结构性损坏」，两者给的提示不一样。
fn read_at(src: &Path, file: &mut File, offset: u64, buffer: &mut [u8]) -> DecodeResult<usize> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| DecodeError::io(src, error))?;
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            // 信号中断是可重试的瞬时错误，不该让它变成「这张图打不开」。
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(DecodeError::io(src, error)),
        }
    }
    Ok(filled)
}

// ---- 单页解码 ----

fn decode_page_at(
    src: &Path,
    layout: Layout,
    ifd: u64,
    limits: &DecodeLimits,
) -> DecodeResult<DecodedPage> {
    let file = File::open(src).map_err(|error| DecodeError::io(src, error))?;
    let (patched, patched_width) = layout.patched_field(ifd);
    let (field, _) = layout.first_ifd_field();
    let view = PageView::new(file, field, patched, patched_width);

    let mut decoder = ImageTiffDecoder::new(BufReader::new(view))
        .map_err(|error| DecodeError::corrupt(format!("TIFF 目录无法解析：{error}")))?;

    let (width, height) = decoder.dimensions();
    limits.check_dimensions(width, height)?;
    let orientation =
        orientation_from_crate(decoder.orientation().unwrap_or(CrateOrientation::NoTransforms));
    // 先取元数据再消费 decoder：理由与 `raster::decode_single` 相同 ——
    // 错过这个时机就得为一张几十 MB 的 TIFF 再读一次文件。
    let exif_raw = decoder.exif_metadata().ok().flatten();
    // 真正让 `max_alloc` 生效的是这一步（`image` 的 `ImageReader` 也是这么做的）。
    decoder
        .set_limits(crate_limits(limits))
        .map_err(|error| from_image_error(error, src))?;

    let image = DynamicImage::from_decoder(decoder).map_err(|error| from_image_error(error, src))?;
    let buffer = image.into_rgba8();
    let (buffer_width, buffer_height) = buffer.dimensions();
    limits.check_dimensions(buffer_width, buffer_height)?;

    Ok(DecodedPage {
        frame: Frame::new(buffer_width, buffer_height, buffer.into_raw(), 0),
        orientation,
        exif: summary_from_raw(exif_raw),
    })
}

/// 「把文件头里第一个 IFD 偏移换成目标页」的只读视图。
///
/// 只替换那几个字节，其余读操作原样透传 —— 多页 TIFF 动辄上百 MB，为每页复制一份
/// 是没必要的。除文件头外，替换窗口不会与任何数据重合（它就在偏移 4 或 8 处）。
struct PageView {
    file: File,
    /// 被替换字段在文件里的起始偏移。
    field: u64,
    value: [u8; 8],
    /// `value` 的有效长度（经典 TIFF 4 字节，BigTIFF 8 字节）。
    width: usize,
}

impl PageView {
    fn new(file: File, field: u64, value: [u8; 8], width: usize) -> Self {
        Self {
            file,
            field,
            value,
            width,
        }
    }
}

impl Read for PageView {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = self.file.stream_position()?;
        let read = self.file.read(buf)?;

        // 只有落在那几个字节上的部分才需要替换。先算交集，避免逐字节做范围判断。
        let field_end = self.field + self.width as u64;
        let from = self.field.saturating_sub(start).min(read as u64) as usize;
        let to = field_end.saturating_sub(start).min(read as u64) as usize;
        for index in from..to {
            buf[index] = self.value[(start + index as u64 - self.field) as usize];
        }
        Ok(read)
    }
}

impl Seek for PageView {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

/// 把一段字节当成一个 `width` 字节的无符号整数读出来。
fn read_uint(bytes: &[u8], width: usize, little_endian: bool) -> Option<u64> {
    let slice = bytes.get(..width)?;
    // 先按字节序把这几字节摆到 8 字节缓冲区的正确一端，再统一解释。
    // 直接 `try_into::<[u8; 8]>()` 是不行的 —— 那个转换要求长度**恰好**是 8。
    let mut raw = [0u8; 8];
    if little_endian {
        raw[..width].copy_from_slice(slice);
        Some(u64::from_le_bytes(raw))
    } else {
        // 大端的值在高位。
        raw[8 - width..].copy_from_slice(slice);
        Some(u64::from_be_bytes(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 现场造一个多页灰度 TIFF（未压缩、8 位、每页一条带）。
    ///
    /// 刻意不用 `image` 的编码器：它只会写单页，而这里要的正是「多页」。
    /// 每个 IFD 里的标签按号升序排列 —— 这是 TIFF 规范的要求，`tiff` crate 也照此读。
    /// `samples` 是各页像素首尾相接的扁平数组，每页 `width * height` 个字节。
    fn multi_page_tiff(samples: &[u8], width: u32, height: u32, little_endian: bool) -> Vec<u8> {
        let page_len = (width * height) as usize;
        assert!(page_len > 0 && samples.len() % page_len == 0, "样本长度必须是整页");
        let count = samples.len() / page_len;

        let ifd_len = 2 + 9 * 12 + 4; // 条目数 + 9 个条目 + next 指针
        let header_len = 8;
        let data_start = header_len + count * ifd_len;
        let mut out = vec![0u8; data_start + count * page_len];

        // 所有写入都按文件的字节序编码 —— 大端的文件里，条目里的值也必须是大端。
        let u16_at = |value: u16| {
            if little_endian {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            }
        };
        let u32_at = |value: u32| {
            if little_endian {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            }
        };

        out[0..2].copy_from_slice(if little_endian { b"II" } else { b"MM" });
        out[2..4].copy_from_slice(&u16_at(42));
        out[4..8].copy_from_slice(&u32_at(header_len as u32));

        for index in 0..count {
            let ifd = header_len + index * ifd_len;
            let data = data_start + index * page_len;
            let next = if index + 1 < count {
                (header_len + (index + 1) * ifd_len) as u32
            } else {
                0
            };

            out[ifd..ifd + 2].copy_from_slice(&u16_at(9));
            // (标签, 类型, 值)。类型 3 = SHORT，4 = LONG。
            let entries: [(u16, u16, u32); 9] = [
                (256, 4, width),           // ImageWidth
                (257, 4, height),          // ImageLength
                (258, 3, 8),               // BitsPerSample
                (259, 3, 1),               // Compression = 无压缩
                (262, 3, 1),               // Photometric = BlackIsZero
                (273, 4, data as u32),     // StripOffsets
                (277, 3, 1),               // SamplesPerPixel
                (278, 4, height),          // RowsPerStrip
                (279, 4, page_len as u32), // StripByteCounts
            ];
            for (slot, (tag, kind, value)) in entries.iter().enumerate() {
                let at = ifd + 2 + slot * 12;
                out[at..at + 2].copy_from_slice(&u16_at(*tag));
                out[at + 2..at + 4].copy_from_slice(&u16_at(*kind));
                out[at + 4..at + 8].copy_from_slice(&u32_at(1)); // 数量
                // SHORT 只占前 2 字节、LONG 占满 4 字节，两种都靠这一句写进去。
                out[at + 8..at + 12].copy_from_slice(&u32_at(*value));
            }
            let next_at = ifd + 2 + 9 * 12;
            out[next_at..next_at + 4].copy_from_slice(&u32_at(next));

            out[data..data + page_len]
                .copy_from_slice(&samples[index * page_len..(index + 1) * page_len]);
        }
        out
    }

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("ngy-tiff-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn layout_recognizes_classic_tiff_in_both_byte_orders() {
        let little = Layout::parse(b"II\x2a\x00\x08\x00\x00\x00").unwrap();
        assert!(little.little_endian && !little.bigtiff);
        assert_eq!(
            little.first_ifd_offset(b"II\x2a\x00\x08\x00\x00\x00"),
            Some(8)
        );

        let big = Layout::parse(b"MM\x00\x2a\x00\x00\x00\x08").unwrap();
        assert!(!big.little_endian && !big.bigtiff);
        assert_eq!(big.first_ifd_offset(b"MM\x00\x2a\x00\x00\x00\x08"), Some(8));
    }

    #[test]
    fn layout_recognizes_bigtiff_only_with_its_two_fixed_fields() {
        let head = b"II\x2b\x00\x08\x00\x00\x00\x20\x00\x00\x00\x00\x00\x00\x00";
        let layout = Layout::parse(head).unwrap();
        assert!(layout.bigtiff && layout.little_endian);
        assert_eq!(layout.first_ifd_offset(head), Some(0x20));
        // 偏移宽度或保留字段不对的，不能当 BigTIFF 读 —— 那会让后面每一步都错位。
        assert_eq!(
            Layout::parse(b"II\x2b\x00\x10\x00\x00\x00\x20\x00\x00\x00\x00\x00\x00\x00"),
            None
        );
    }

    #[test]
    fn layout_rejects_anything_that_is_not_tiff() {
        assert_eq!(Layout::parse(b""), None);
        assert_eq!(Layout::parse(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(Layout::parse(b"II\x2a\x00"), None, "只有 4 字节，偏移字段还没读到");
        assert_eq!(Layout::parse(b"XX\x2a\x00\x08\x00\x00\x00"), None);
        // 经典 TIFF 的魔数必须是 42；43 只在完整的 BigTIFF 头下才认。
        assert_eq!(Layout::parse(b"II\x2c\x00\x08\x00\x00\x00"), None);
    }

    #[test]
    fn patched_field_encodes_in_the_files_own_byte_order() {
        let little = Layout::parse(b"II\x2a\x00\x08\x00\x00\x00").unwrap();
        let (bytes, width) = little.patched_field(0x1234);
        assert_eq!(width, 4);
        assert_eq!(&bytes[..4], &[0x34, 0x12, 0x00, 0x00]);

        let big = Layout::parse(b"MM\x00\x2a\x00\x00\x00\x08").unwrap();
        let (bytes, width) = big.patched_field(0x1234);
        assert_eq!(width, 4);
        assert_eq!(&bytes[..4], &[0x00, 0x00, 0x12, 0x34]);

        let bigtiff =
            Layout::parse(b"II\x2b\x00\x08\x00\x00\x00\x20\x00\x00\x00\x00\x00\x00\x00").unwrap();
        let (bytes, width) = bigtiff.patched_field(0x_0000_0001_2345_6789);
        assert_eq!(width, 8, "BigTIFF 的偏移字段是 8 字节");
        assert_eq!(
            &bytes[..8],
            &[0x89, 0x67, 0x45, 0x23, 0x01, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn scan_walks_the_whole_ifd_chain() {
        // 三页，每页 2×1。
        let path = write_temp(
            "three-pages.tif",
            &multi_page_tiff(&[0, 0, 128, 128, 255, 255], 2, 1, true),
        );
        let (layout, offsets) = scan(&path).unwrap();
        assert!(layout.little_endian && !layout.bigtiff);
        assert_eq!(offsets.len(), 3, "三页应当有三个 IFD");
        assert_eq!(TiffDecoder.page_count(&path).unwrap(), 3);
    }

    #[test]
    fn scan_stops_at_a_cycle_instead_of_looping_forever() {
        // 手工把「第一个 IFD 的 next 指针」指回它自己（IFD 在偏移 8，
        // 条目数 9 → next 指针在 8 + 2 + 9*12 = 118）。
        let mut bytes = multi_page_tiff(&[0, 0], 2, 1, true);
        let next_at = 8 + 2 + 9 * 12;
        bytes[next_at..next_at + 4].copy_from_slice(&8u32.to_le_bytes());

        let path = write_temp("cyclic.tif", &bytes);
        let error = scan(&path).expect_err("成环的目录链必须被拦下，而不是一直转");
        assert!(
            matches!(error, DecodeError::Corrupt { .. }),
            "实际得到 {error:?}"
        );
        assert!(
            error.short_reason().contains("损坏"),
            "{}",
            error.short_reason()
        );
    }

    #[test]
    fn scan_rejects_a_pointer_past_the_end() {
        let mut bytes = multi_page_tiff(&[0, 0], 2, 1, true);
        bytes[4..8].copy_from_slice(&9_999_999u32.to_le_bytes());

        let path = write_temp("out-of-range.tif", &bytes);
        let error = scan(&path).expect_err("越界偏移必须报错");
        assert!(matches!(error, DecodeError::Corrupt { .. }));
    }

    #[test]
    fn both_byte_orders_produce_the_same_page_map() {
        // 大端文件里的偏移是多字节大端写法，读错了页数就对不上。
        for little_endian in [true, false] {
            let name = if little_endian {
                "order-le.tif"
            } else {
                "order-be.tif"
            };
            let path = write_temp(
                name,
                &multi_page_tiff(&[0, 0, 0, 0, 255, 255, 255, 255], 2, 2, little_endian),
            );
            let (layout, offsets) = scan(&path).unwrap();
            assert_eq!(layout.little_endian, little_endian);
            assert_eq!(offsets.len(), 2);
        }
    }

    #[test]
    fn each_page_decodes_to_its_own_pixels() {
        // 第 0 页全黑、第 1 页全白：页号只要取错，像素立刻对不上。
        let path = write_temp(
            "two-pages.tif",
            &multi_page_tiff(&[0, 0, 0, 0, 255, 255, 255, 255], 2, 2, true),
        );
        let limits = DecodeLimits::default();
        let decoder = TiffDecoder;

        let first = decoder.decode(&path, &limits).unwrap();
        assert_eq!(first.frame_count(), 1, "打开时只解第 0 页");
        assert_eq!(first.primary().rgba8[0..4], [0, 0, 0, 255]);

        let second = decoder.decode_page(&path, 1, &limits).unwrap();
        assert_eq!(second.rgba8[0..4], [255, 255, 255, 255]);

        // 第 0 页用 `decode_page` 取也应当与 `decode` 完全一致。
        let again = decoder.decode_page(&path, 0, &limits).unwrap();
        assert_eq!(again.rgba8, first.primary().rgba8);
    }

    #[test]
    fn asking_for_a_page_that_does_not_exist_is_an_error_not_a_fallback() {
        let path = write_temp("one-page.tif", &multi_page_tiff(&[0, 0, 0, 0], 2, 2, true));
        let decoder = TiffDecoder;
        assert_eq!(decoder.page_count(&path).unwrap(), 1);

        let error = decoder
            .decode_page(&path, 5, &DecodeLimits::default())
            .expect_err("越界的页号必须报错，不能悄悄退回第一页");
        assert!(
            error.short_reason().contains("只有 1 页"),
            "{}",
            error.short_reason()
        );
    }

    #[test]
    fn page_view_only_replaces_the_pointer_field() {
        // 直接验 `PageView` 本身：替换窗口之外的字节必须原样透传。
        let path = write_temp("passthrough.tif", &multi_page_tiff(&[7, 9, 0, 0], 2, 2, true));
        let layout = Layout::parse(b"II\x2a\x00\x00\x00\x00\x00").unwrap();
        let (patched, patched_width) = layout.patched_field(0xDEAD_BEEF);
        let file = File::open(&path).unwrap();
        let mut view = PageView::new(file, 4, patched, patched_width);

        let mut header = [0u8; 16];
        view.read_exact(&mut header).unwrap();
        assert_eq!(&header[0..2], b"II");
        assert_eq!(
            &header[4..8],
            &[0xEF, 0xBE, 0xAD, 0xDE],
            "文件头里的第一个 IFD 偏移已被替换"
        );

        // 对照：直接从文件读同样的位置，确认除了那 4 个字节以外完全一致。
        let mut plain = [0u8; 16];
        File::open(&path)
            .unwrap()
            .read_exact(&mut plain)
            .unwrap();
        assert_eq!(&header[8..16], &plain[8..16]);
    }
}
