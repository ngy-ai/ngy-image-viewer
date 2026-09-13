//! DDS（DirectDraw Surface）解码：游戏 / 3D 资源里最常见的纹理容器。
//!
//! # 为什么要多条路径
//!
//! DDS 不是一种压缩算法，而是一堆压缩算法的容器：未压缩 RGBA、DXT1/3/5（BC1/2/3）、
//! 以及较新的 BC4/BC5/BC6H/BC7。覆盖面差异很大，没有哪一条后端能通吃：
//!
//! | 后端 | 覆盖的压缩格式 |
//! | --- | --- |
//! | 本模块的纯 Rust 路径 | 未压缩（按位掩码提取通道）+ DXT1/3/5、DX10 的 BC1–BC3 |
//! | `image` crate | DXT1/3/5、DX10 的 BC1–BC3（但不接受未压缩 DDS） |
//! | Windows WIC（系统解码器） | 未压缩 + BC1–BC7（Windows 10+ 自带） |
//!
//! 因此解码顺序定为：**先纯 Rust 路径**（未压缩自己解、压缩交给 `image` crate）→
//! **失败时 Windows 再试 WIC**（补上 BC4–BC7）。非 Windows 没有 WIC，就只能解到
//! 未压缩与 DXT1/3/5 为止——这已经覆盖了绝大多数真实 DDS。
//!
//! # 复用而非重写
//!
//! WIC 路径直接复用 [`super::wic`]（与 HEIC/AVIF 同一套 COM 初始化与错误分类）；
//! 压缩路径直接复用 [`super::raster::RasterDecoder`]（与 PNG/JPEG 同一套
//! 尺寸限制与 RGBA 统一产出）。本模块只负责「未压缩 DDS 的位掩码解析」与「后端选路」，
//! 不重复任何 DXT 像素逻辑。

use std::path::Path;

use super::Decoder;
use super::types::{DecodeLimits, DecodeResult, ImageData, ImageFormat};

pub struct DdsDecoder;

impl Decoder for DdsDecoder {
    fn id(&self) -> &'static str {
        "dds"
    }

    fn formats(&self) -> &'static [ImageFormat] {
        &[ImageFormat::Dds]
    }

    fn probe(&self, head: &[u8]) -> bool {
        super::sniff::sniff_magic(head) == Some(ImageFormat::Dds)
    }

    fn decode(&self, src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        decode_dds(src, limits)
    }
}

/// 选路：先纯 Rust（未压缩 + DXT），失败再让 Windows WIC 补 BC4–BC7。
fn decode_dds(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
    match dds_native::decode(src, limits) {
        Ok(data) => Ok(data),
        Err(native_error) => {
            #[cfg(windows)]
            {
                // WIC 覆盖 BC4–BC7 等本模块解不了的格式；成功就返回，否则保留
                // 原生路径的错误（通常信息量更大）。
                dds_windows::decode(src, limits).or(Err(native_error))
            }
            #[cfg(not(windows))]
            {
                Err(native_error)
            }
        }
    }
}

/// 跨平台纯 Rust 路径：未压缩 DDS 自己解，压缩的交给 `image` crate。
mod dds_native {
    use std::path::Path;

    use super::super::Decoder;
    use super::super::types::{DecodeError, DecodeLimits, DecodeResult, Frame, ImageData, ImageFormat, Orientation};

    pub fn decode(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        let buf = std::fs::read(src).map_err(|error| DecodeError::io(src, error))?;

        // 先判定压缩 / 未压缩：压缩的（FourCC 标记）交给 image crate 的 DDS 解码器，
        // 它支持 DXT1/3/5 与 DX10 的 BC1–BC3；未压缩的才由本模块按掩码解析。
        if is_uncompressed(&buf) {
            decode_uncompressed(&buf, limits)
        } else {
            // 直接复用 raster 解码器：它走 image crate，已经开启 dds 特性。
            super::super::raster::RasterDecoder.decode(src, limits)
        }
    }

    /// 解析 DDS 头，判断是否为「未压缩」格式（像素格式不是 FourCC）。
    fn is_uncompressed(buf: &[u8]) -> bool {
        // 最小可用头：4 字节 magic + 124 字节头。
        if buf.len() < 128 || &buf[0..4] != b"DDS " {
            return false;
        }
        if u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) != 124 {
            return false;
        }
        // 像素格式位于固定偏移：dwSize @76, dwFlags @80。DDPF_FOURCC = 0x4。
        let pf_flags = u32::from_le_bytes([buf[80], buf[81], buf[82], buf[83]]);
        pf_flags & 0x4 == 0
    }

    /// 按位掩码把未压缩 DDS 解成 RGBA8。
    ///
    /// DDS 的未压缩像素用「每个通道一个位掩码」描述（例如 A8R8G8B8 的 R 掩码
    /// `0x00FF0000`），同一套逻辑能处理任意字节序 / 任意通道排布与 16/24/32 位位深。
    /// DDS 文件按**自底向上**存储像素，这里翻成标准自顶向下。
    fn decode_uncompressed(buf: &[u8], limits: &DecodeLimits) -> DecodeResult<ImageData> {
        let header = parse_header(buf)
            .ok_or_else(|| DecodeError::corrupt("DDS 文件头不完整或不是合法的未压缩 DDS"))?;
        let width = header.width;
        let height = header.height;
        limits.check_dimensions(width, height)?;

        let bpp = header.rgb_bit_count / 8;
        if !(2..=4).contains(&bpp) {
            return Err(DecodeError::unsupported(
                Some(ImageFormat::Dds),
                "不支持的未压缩 DDS 位深（仅支持 16/24/32 位）。",
            ));
        }

        // 行跨度：未压缩时优先用头里的 pitch（可能含行对齐填充），否则按宽×字节数。
        let pitch = if header.pitch > 0 {
            header.pitch as usize
        } else {
            width as usize * bpp as usize
        };
        let pixel_data = &buf[128..];
        if pixel_data.len() < pitch * height as usize {
            return Err(DecodeError::corrupt(
                "DDS 像素数据在读取过程中提前结束，可能文件被截断。",
            ));
        }

        let mut out = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height as usize {
            // DDS 自底向上：源第 `height-1-y` 行对应输出第 `y` 行。
            let src_row = (height as usize - 1 - y) * pitch;
            let dst_row = y * width as usize * 4;
            for x in 0..width as usize {
                let src_off = src_row + x * bpp as usize;
                let value = read_pixel_value(pixel_data, src_off, bpp);
                let r = extract_channel(value, header.r_mask);
                let g = extract_channel(value, header.g_mask);
                let b = extract_channel(value, header.b_mask);
                let a = if header.a_mask != 0 {
                    extract_channel(value, header.a_mask)
                } else {
                    255
                };
                let dst = dst_row + x * 4;
                out[dst..dst + 4].copy_from_slice(&[r, g, b, a]);
            }
        }

        Ok(ImageData {
            format: ImageFormat::Dds,
            frames: vec![Frame::new(width, height, out, 0)],
            orientation: Orientation::Normal,
            exif: None,
            supersample: 1.0,
        })
    }

    /// 从 `buf[off..]` 按 `bpp` 读出一个像素的整数值（小端）。
    fn read_pixel_value(buf: &[u8], off: usize, bpp: u32) -> u32 {
        match bpp {
            2 => u16::from_le_bytes([buf[off], buf[off + 1]]) as u32,
            3 => u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], 0]),
            _ => u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]),
        }
    }

    /// 用位掩码从像素值里取出某个通道，并缩放到 0–255。
    fn extract_channel(value: u32, mask: u32) -> u8 {
        if mask == 0 {
            return 0;
        }
        let shift = mask.trailing_zeros();
        let bits = mask.count_ones();
        let raw = (value & mask) >> shift;
        if bits >= 8 {
            (raw >> (bits - 8)) as u8
        } else {
            let max = (1u32 << bits) - 1;
            ((raw * 255) / max) as u8
        }
    }

    /// 解析 DDS 头为关键字段（其余字段这里用不到，跳过）。
    fn parse_header(buf: &[u8]) -> Option<Header> {
        if buf.len() < 128 || &buf[0..4] != b"DDS " {
            return None;
        }
        if u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) != 124 {
            return None;
        }
        let width = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let height = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let pitch = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);

        // 像素格式块从偏移 76 开始，共 32 字节。
        let pf_size = u32::from_le_bytes([buf[76], buf[77], buf[78], buf[79]]);
        if pf_size != 32 {
            return None;
        }
        let pf_flags = u32::from_le_bytes([buf[80], buf[81], buf[82], buf[83]]);
        let rgb_bit_count = u32::from_le_bytes([buf[88], buf[89], buf[90], buf[91]]);
        let r_mask = u32::from_le_bytes([buf[92], buf[93], buf[94], buf[95]]);
        let g_mask = u32::from_le_bytes([buf[96], buf[97], buf[98], buf[99]]);
        let b_mask = u32::from_le_bytes([buf[100], buf[101], buf[102], buf[103]]);
        let a_mask = u32::from_le_bytes([buf[104], buf[105], buf[106], buf[107]]);

        Some(Header {
            width,
            height,
            pitch,
            rgb_bit_count,
            r_mask,
            g_mask,
            b_mask,
            a_mask,
            _pf_flags: pf_flags,
        })
    }

    struct Header {
        width: u32,
        height: u32,
        pitch: u32,
        rgb_bit_count: u32,
        r_mask: u32,
        g_mask: u32,
        b_mask: u32,
        a_mask: u32,
        _pf_flags: u32,
    }
}

#[cfg(windows)]
mod dds_windows {
    use std::path::Path;

    use super::super::types::{DecodeLimits, DecodeResult, ImageData};
    use super::super::wic;

    pub fn decode(src: &Path, limits: &DecodeLimits) -> DecodeResult<ImageData> {
        wic::decode(
            src,
            limits,
            wic::WicRequest {
                format: super::super::types::ImageFormat::Dds,
                component: "DDS",
                install_hint: "Windows 已内置 DDS 解码器。若提示缺少组件，请在「设置 → 应用 → 可选功能」中确认「Windows 图像处理组件」未被精简。",
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_recognizes_dds_magic() {
        // DDS 文件头固定以 "DDS "（含尾部空格）开头。
        assert!(DdsDecoder.probe(b"DDS \x7c\x00\x00\x00"));
        assert!(!DdsDecoder.probe(b"BM\x36\x00\x00\x00"));
    }

    #[test]
    fn decodes_dxt1_dds() {
        // 合成一张 4×4 单色 DXT1 的 DDS，验证压缩路径（image crate）能出图。
        let dir = std::env::temp_dir().join("ngy-decode-tests-dds");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.dxt1.dds");
        std::fs::write(&path, make_dxt1_dds(4, 4)).unwrap();

        let data = DdsDecoder
            .decode(&path, &DecodeLimits::default())
            .expect("DXT1 DDS 应当能解码");
        assert_eq!((data.primary().width, data.primary().height), (4, 4));
        let frame = &data.frames[0];
        assert_eq!(frame.rgba8.len(), 4 * 4 * 4);
        for chunk in frame.rgba8.chunks(4) {
            assert!(chunk[0] > 150 && chunk[1] < 80 && chunk[2] < 80, "像素不是红：{chunk:?}");
        }
    }

    #[test]
    fn decodes_uncompressed_rgba_dds() {
        // 合成一张 4×4 未压缩 BGRA（A8R8G8B8）的 DDS，验证纯 Rust 路径能按掩码解出正确颜色。
        let dir = std::env::temp_dir().join("ngy-decode-tests-dds");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.uncompressed.dds");
        std::fs::write(&path, make_uncompressed_rgba_dds(4, 4)).unwrap();

        let data = DdsDecoder
            .decode(&path, &DecodeLimits::default())
            .expect("未压缩 DDS 应当能解码");
        assert_eq!((data.primary().width, data.primary().height), (4, 4));
        let frame = &data.frames[0];
        assert_eq!(frame.rgba8.len(), 4 * 4 * 4);
        // 整张图填红色 RGB(220,40,40)，验证 BGRA 字节序被正确翻成 RGBA。
        for chunk in frame.rgba8.chunks(4) {
            assert_eq!(chunk, &[220, 40, 40, 255], "像素不是红：{chunk:?}");
        }
    }

    /// 生成合法的未压缩 DDS：w×h，像素格式 A8R8G8B8，字节序 BGRA，整张填红。
    fn make_uncompressed_rgba_dds(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"DDS ");
        out.extend_from_slice(&124u32.to_le_bytes()); // dwSize
        out.extend_from_slice(&0x100Fu32.to_le_bytes()); // flags: CAPS|HEIGHT|WIDTH|PIXELFORMAT|PITCH
        out.extend_from_slice(&h.to_le_bytes()); // dwHeight
        out.extend_from_slice(&w.to_le_bytes()); // dwWidth
        out.extend_from_slice(&(w * 4).to_le_bytes()); // dwPitchOrLinearSize
        out.extend_from_slice(&0u32.to_le_bytes()); // dwDepth
        out.extend_from_slice(&0u32.to_le_bytes()); // dwMipMapCount
        out.extend_from_slice(&[0u8; 44]); // dwReserved[11]

        // DDS_PIXELFORMAT（32 字节）：DDPF_RGB | DDPF_ALPHAPIXELS
        out.extend_from_slice(&32u32.to_le_bytes()); // dwSize
        out.extend_from_slice(&0x41u32.to_le_bytes()); // dwFlags
        out.extend_from_slice(&0u32.to_le_bytes()); // dwFourCC
        out.extend_from_slice(&32u32.to_le_bytes()); // dwRGBBitCount
        out.extend_from_slice(&0x00FF_0000u32.to_le_bytes()); // R mask
        out.extend_from_slice(&0x0000_FF00u32.to_le_bytes()); // G mask
        out.extend_from_slice(&0x0000_00FFu32.to_le_bytes()); // B mask
        out.extend_from_slice(&0xFF00_0000u32.to_le_bytes()); // A mask

        out.extend_from_slice(&0x1000u32.to_le_bytes()); // dwCaps: TEXTURE
        out.extend_from_slice(&[0u8; 16]); // dwCaps2/3/4 + Reserved2

        // 像素：整张红 (R=220,G=40,B=40,A=255)，字节序 BGRA，自底向上存储。
        for _ in 0..(w as usize * h as usize) {
            out.extend_from_slice(&[40, 40, 220, 255]); // B, G, R, A
        }
        out
    }

    /// 生成合法的 DXT1 DDS：4×4 全红。DXT1 以 4×4 块压缩，整张图就是一个块。
    fn make_dxt1_dds(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"DDS ");
        out.extend_from_slice(&124u32.to_le_bytes()); // dwSize
        // 标志：CAPS | HEIGHT | WIDTH | PIXELFORMAT | LINEARSIZE
        out.extend_from_slice(&0x81007u32.to_le_bytes());
        out.extend_from_slice(&h.to_le_bytes());
        out.extend_from_slice(&w.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes()); // dwPitchOrLinearSize：一个 DXT1 块 8 字节
        out.extend_from_slice(&0u32.to_le_bytes()); // dwDepth
        out.extend_from_slice(&0u32.to_le_bytes()); // dwMipMapCount
        out.extend_from_slice(&[0u8; 44]); // dwReserved[11]

        // DDS_PIXELFORMAT：FourCC = DXT1
        out.extend_from_slice(&32u32.to_le_bytes()); // dwSize
        out.extend_from_slice(&0x4u32.to_le_bytes()); // dwFlags: DDPF_FOURCC
        out.extend_from_slice(b"DXT1"); // dwFourCC
        out.extend_from_slice(&0u32.to_le_bytes()); // dwRGBBitCount
        out.extend_from_slice(&[0u8; 16]); // R/G/B/A 四个掩码占位（FourCC 时不用）

        out.extend_from_slice(&0x1000u32.to_le_bytes()); // dwCaps: TEXTURE
        out.extend_from_slice(&[0u8; 16]); // dwCaps2/3/4 + Reserved2

        // 一个 DXT1 块：color0 = 红（R5G6B5 = 0xF800，小端字节序为 [0x00, 0xF8]），
        // color1 = 0，16 个索引全 0 → 取 color0。color0 > color1 走四色模式，索引 0 即纯红。
        out.extend_from_slice(&[0x00, 0xF8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        out
    }
}
