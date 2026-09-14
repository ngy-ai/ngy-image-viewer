//! 解码层集成测试。
//!
//! 测试样本全部**现场生成**，不依赖任何二进制素材文件：
//! 一来仓库里不必存二进制垃圾，二来「怎么造出这种格式」本身就写进了测试里，
//! 将来补格式时可以直接照抄。
//!
//! 覆盖的重点不是「能不能解出一张图」，而是几条产品级约定：
//! - 扩展名与内容不一致时按内容来（这是用户最常遇到的「打不开」原因）；
//! - 损坏文件、超大图、缺失解码器都要给出**可操作的提示**，而不是崩溃或静默；
//! - 多帧动图的每一帧与延迟都不能丢。

use std::path::{Path, PathBuf};

use image::{Delay, Frame as ImageFrame, ImageBuffer, Rgba};
use ngy_image_viewer::decode::{self, DecodeError, DecodeLimits, ImageFormat, sniff};

/// 每个测试用独立目录，避免并行执行时互相踩踏。
fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ngy-decode-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("创建测试目录失败");
    dir
}

/// 生成一张有梯度的 8×6 图片，避免纯色图被编码器优化掉。
fn gradient(width: u32, height: u32) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
    ImageBuffer::from_fn(width, height, |x, y| {
        Rgba([
            (x * 255 / width.max(1)) as u8,
            (y * 255 / height.max(1)) as u8,
            64,
            255,
        ])
    })
}

fn write_image(path: &Path, image: &ImageBuffer<Rgba<u8>, Vec<u8>>) {
    image
        .save(path)
        .unwrap_or_else(|error| panic!("写入 {} 失败：{error}", path.display()));
}

/// 写一个 SVG 样本。矢量图是纯文本，直接内联成字符串最直观。
fn write_svg(path: &Path, content: &str) {
    std::fs::write(path, content)
        .unwrap_or_else(|error| panic!("写入 {} 失败：{error}", path.display()));
}

#[test]
fn png_with_wrong_extension_is_still_decoded() {
    let dir = workdir("mismatch");
    // 内容是真 PNG，扩展名故意写成 .jpg —— 下载改名后的典型状态。
    //
    // 先写成 .png 再改名，是因为编码器也要按扩展名选：JPEG 编码器无法编码 RGBA8。
    let path = dir.join("actually-png.jpg");
    let source = dir.join("source.png");
    write_image(&source, &gradient(8, 6));
    std::fs::rename(&source, &path).unwrap();

    let sniffed = sniff::sniff(&std::fs::read(&path).unwrap(), Some(&path));
    assert_eq!(sniffed.format, Some(ImageFormat::Png));
    assert!(sniffed.is_mismatch(), "应识别出扩展名与内容不一致");
    assert!(sniffed.mismatch_note().unwrap().contains("PNG"));

    let data = decode::decode_path(&path, &DecodeLimits::default()).expect("按内容应能解出 PNG");
    assert_eq!(data.format, ImageFormat::Png);
    assert_eq!((data.width(), data.height()), (8, 6));
}

#[test]
fn common_formats_decode_with_correct_size() {
    let dir = workdir("formats");
    let cases: [(&str, ImageFormat); 6] = [
        ("sample.png", ImageFormat::Png),
        ("sample.bmp", ImageFormat::Bmp),
        ("sample.qoi", ImageFormat::Qoi),
        ("sample.tga", ImageFormat::Tga),
        ("sample.tiff", ImageFormat::Tiff),
        ("sample.ppm", ImageFormat::Pnm),
    ];

    for (name, expected) in cases {
        let path = dir.join(name);
        write_image(&path, &gradient(8, 6));

        let data = decode::decode_path(&path, &DecodeLimits::default())
            .unwrap_or_else(|error| panic!("{name} 解码失败：{error}"));
        assert_eq!(data.format, expected, "{name} 的格式判定不对");
        assert_eq!(
            (data.width(), data.height()),
            (8, 6),
            "{name} 的尺寸不对"
        );
        assert_eq!(data.frame_count(), 1, "{name} 不应被当成动图");
        assert_eq!(data.orientation, decode::Orientation::Normal);
        // 像素统一为 RGBA8，长度必须是 宽×高×4。
        assert_eq!(data.primary().rgba8.len(), 8 * 6 * 4);
    }
}

#[test]
fn animated_gif_keeps_every_frame_and_delay() {
    let dir = workdir("gif");
    let path = dir.join("anim.gif");

    let delays_ms = [100u32, 50, 30];
    let colors = [
        [255u8, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
    ];

    {
        let file = std::fs::File::create(&path).expect("创建 GIF 失败");
        let mut encoder = image::codecs::gif::GifEncoder::new(std::io::BufWriter::new(file));
        let frames = delays_ms.iter().zip(colors).map(|(delay, color)| {
            let buffer: ImageBuffer<Rgba<u8>, Vec<u8>> =
                ImageBuffer::from_pixel(4, 4, Rgba(color));
            ImageFrame::from_parts(
                buffer,
                0,
                0,
                Delay::from_numer_denom_ms(*delay, 1),
            )
        });
        encoder.encode_frames(frames).expect("写入 GIF 帧失败");
    }

    let data = decode::decode_path(&path, &DecodeLimits::default()).expect("GIF 应能解码");
    assert_eq!(data.format, ImageFormat::Gif);
    assert!(data.is_animated());
    assert_eq!(data.frame_count(), delays_ms.len());

    for (index, expected) in delays_ms.iter().enumerate() {
        // GIF 的延迟精度只有 10ms，这里用的都是 10 的倍数，可以精确比对。
        assert_eq!(
            data.frames[index].delay_ms, *expected,
            "第 {index} 帧的延迟不对"
        );
    }
    assert_eq!(data.loop_duration_ms(), 180);
    assert_eq!(data.frame_index_at(0), 0);
    assert_eq!(data.frame_index_at(150), 2);
    assert_eq!(data.frame_index_at(180), 0);
}

#[test]
fn corrupt_file_reports_failure_instead_of_panicking() {
    let dir = workdir("corrupt");

    // 情形一：只有 PNG 文件头，后面全是垃圾 —— 典型的下载中断文件。
    let truncated = dir.join("truncated.png");
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&[0u8; 32]);
    std::fs::write(&truncated, &bytes).unwrap();
    let error = decode::decode_path(&truncated, &DecodeLimits::default())
        .expect_err("截断的 PNG 不应解码成功");
    assert!(!error.user_message().is_empty(), "必须给出可读提示");
    assert!(matches!(
        error,
        DecodeError::Corrupt { .. } | DecodeError::Unsupported { .. }
    ));

    // 情形二：内容完全不是图片。
    let garbage = dir.join("garbage.bin");
    std::fs::write(&garbage, b"this is definitely not an image, just text\n").unwrap();
    let error = decode::decode_path(&garbage, &DecodeLimits::default())
        .expect_err("非图片内容不应解码成功");
    assert!(matches!(error, DecodeError::Unsupported { .. }));

    // 情形三：空文件。必须明确说「文件为空」，而不是把底层解码库的
    // 「failed to fill whole buffer」原样抛给用户。
    let empty = dir.join("empty.png");
    std::fs::write(&empty, b"").unwrap();
    let error =
        decode::decode_path(&empty, &DecodeLimits::default()).expect_err("空文件不应解码成功");
    assert!(matches!(error, DecodeError::Corrupt { .. }), "实际得到 {error:?}");
    assert!(error.user_message().contains("为空"));
}

#[test]
fn oversized_image_is_rejected_before_allocating() {
    let dir = workdir("limits");
    let path = dir.join("big.png");
    write_image(&path, &gradient(64, 64));

    // 把上限压到 1000 像素：64×64=4096 必然越界。
    let limits = DecodeLimits {
        max_pixels: 1_000,
        ..DecodeLimits::default()
    };
    let error =
        decode::decode_path(&path, &limits).expect_err("超过像素上限时必须拒绝，而不是硬解");
    match error {
        DecodeError::TooLarge {
            width,
            height,
            pixels,
            limit,
        } => {
            assert_eq!((width, height, pixels, limit), (64, 64, 4096, 1_000));
        }
        other => panic!("期望 TooLarge，实际为 {other:?}"),
    }
    // 提示要让用户知道发生了什么、以及尺寸是多少。
    let message = error.user_message();
    assert!(message.contains("64×64"), "提示应带上实际尺寸：{message}");
}

#[test]
fn platform_formats_fail_without_panicking_and_name_the_format() {
    let dir = workdir("platform-formats");

    // HEIC 与 AVIF 都走系统原生解码器，它们在结构上都是 ISOBMFF 容器 +
    // 一种需要额外系统组件的视频编码。这里用「结构正确、内容垃圾」的文件
    // 验证平台路径本身不崩：COM 初始化、解码器工厂创建、错误分类都必须走通，
    // 并且最终落到一条可读、且点名了格式的提示上。
    //
    // 具体落到哪一类取决于这台机器装没装对应的系统扩展：
    // 装了 → 内容损坏（Corrupt）；没装 → 缺组件（MissingPlatformSupport）。
    // 两种都是「明确、可操作」的失败，这个测试都接受，
    // 但**不接受** panic、空消息，或者一句没头没脑的「无法识别的格式」。
    let samples: [(&str, &[u8], ImageFormat, &str); 2] = [
        (
            "photo.heic",
            b"\x00\x00\x00\x20ftypheic",
            ImageFormat::Heic,
            "HEIC",
        ),
        (
            "photo.avif",
            b"\x00\x00\x00\x20ftypavif",
            ImageFormat::Avif,
            "AVIF",
        ),
    ];

    for (name, magic, format, label) in samples {
        let path = dir.join(name);
        let mut bytes = magic.to_vec();
        bytes.extend_from_slice(&[0u8; 64]);
        std::fs::write(&path, &bytes).unwrap();

        let sniffed = sniff::sniff(&bytes, Some(&path));
        assert_eq!(sniffed.format, Some(format), "{name} 的格式判定不对");

        let error = decode::decode_path(&path, &DecodeLimits::default())
            .expect_err("垃圾内容不应解码成功");

        assert!(
            matches!(
                error,
                DecodeError::Corrupt { .. }
                    | DecodeError::MissingPlatformSupport { .. }
                    | DecodeError::Unsupported { .. }
            ),
            "{name} 实际得到 {error:?}"
        );

        let message = error.user_message();
        assert!(!message.is_empty(), "{name} 必须给出可读提示");
        assert!(
            message.contains(label),
            "{name} 的提示应点名格式 {label}：{message}"
        );
    }
}

#[test]
fn jxl_garbage_reports_corruption_while_still_naming_the_format() {
    let dir = workdir("jxl-corrupt");

    // 有正确的裸 codestream 签名，后面却是垃圾。
    // 关键点：现在 JXL **有**解码器，所以失败原因必须是「内容损坏」，
    // 而不是退回「暂不支持 JPEG XL」—— 后者会把用户引向完全错误的方向。
    let jxl = dir.join("photo.jxl");
    std::fs::write(&jxl, [0xFF, 0x0A, 0x00, 0x01, 0x02, 0x03]).unwrap();

    let error = decode::decode_path(&jxl, &DecodeLimits::default()).expect_err("垃圾内容不应解码成功");
    assert!(
        matches!(error, DecodeError::Corrupt { .. }),
        "实际得到 {error:?}"
    );
    let message = error.user_message();
    assert!(message.contains("JPEG XL"), "提示应点名格式：{message}");
    assert!(message.contains("photo.jxl"), "提示应带上文件名：{message}");
}

#[test]
fn svg_is_rasterized_above_its_declared_size() {
    let dir = workdir("svg-size");
    let path = dir.join("icon.svg");
    write_svg(
        &path,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24">
             <rect width="24" height="24" fill="#4C8DFF"/>
           </svg>"##,
    );

    let data = decode::decode_path(&path, &DecodeLimits::default()).expect("SVG 应能解码");
    assert_eq!(data.format, ImageFormat::Svg);
    // 逻辑尺寸 = 文件里声明的尺寸：状态栏、1:1、适应窗口都按它算。
    assert_eq!((data.width(), data.height()), (24, 24));
    // 像素尺寸被放大到倍率上限，用于保证放大后依然清晰。
    assert_eq!((data.primary().width, data.primary().height), (192, 192));
    assert_eq!(data.primary().rgba8.len(), 192 * 192 * 4);
    assert!(
        (data.supersample() - 8.0).abs() < 1e-6,
        "超采样倍率不对：{}",
        data.supersample()
    );

    // 画布中心应当是矢量里填的蓝色、完全不透明。
    let frame = data.primary();
    let center = ((frame.height as usize / 2) * frame.width as usize + frame.width as usize / 2) * 4;
    let pixel = &frame.rgba8[center..center + 4];
    let expected = [0x4Cu8, 0x8D, 0xFF, 0xFF];
    for (channel, (actual, want)) in pixel.iter().zip(expected).enumerate() {
        assert!(
            actual.abs_diff(want) <= 2,
            "第 {channel} 个通道应为 {want}，实际 {actual}（整像素 {pixel:?}）"
        );
    }
}

#[test]
fn svg_size_falls_back_to_viewbox_then_to_a_default() {
    let dir = workdir("svg-default-size");

    // 情形一：只有 viewBox、没有 width/height。这是「可缩放图标」最常见的写法，
    // 必须能打开，并按 viewBox 的坐标系确定尺寸。
    let viewbox_only = dir.join("viewbox-only.svg");
    write_svg(
        &viewbox_only,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
             <circle cx="5" cy="5" r="5" fill="#34D399"/>
           </svg>"##,
    );
    let data = decode::decode_path(&viewbox_only, &DecodeLimits::default()).expect("SVG 应能解码");
    assert_eq!((data.width(), data.height()), (10, 10));
    assert!(data.supersample() > 1.0, "小尺寸矢量应被超采样");
    // 圆形之外的角落应当是透明的 —— 透明通道被保留，没有被填成黑色。
    let frame = data.primary();
    assert_eq!(&frame.rgba8[0..4][3], &0, "角落应当是透明的");

    // 情形二：width/height 与 viewBox 都没有。此时只能退到约定的默认尺寸，
    // 但不能失败 —— 一张「什么都不声明」的 SVG 依然是一张图。
    let bare = dir.join("bare.svg");
    write_svg(&bare, r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#);
    let data = decode::decode_path(&bare, &DecodeLimits::default()).expect("空 SVG 也应能打开");
    assert_eq!((data.width(), data.height()), (100, 100));
}

#[test]
fn svg_translucent_fill_keeps_straight_alpha() {
    let dir = workdir("svg-alpha");
    let path = dir.join("translucent.svg");
    write_svg(
        &path,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4">
             <rect width="4" height="4" fill="#ff0000" fill-opacity="0.5"/>
           </svg>"##,
    );

    let data = decode::decode_path(&path, &DecodeLimits::default()).expect("SVG 应能解码");
    let frame = data.primary();
    let center = ((frame.height as usize / 2) * frame.width as usize + frame.width as usize / 2) * 4;
    let pixel = &frame.rgba8[center..center + 4];

    // 这是「反预乘」的验收点。tiny-skia 内部存的是预乘像素，
    // 若忘了还原，这里会看到红色分量 ≈128 而不是 255 ——
    // 表现就是「半透明图形整体发暗」。
    assert!(
        pixel[0] >= 250,
        "红色分量被预乘压暗了，说明没有还原成非预乘：{pixel:?}"
    );
    assert!(
        pixel[1] <= 4 && pixel[2] <= 4,
        "绿色与蓝色分量应当保持为 0：{pixel:?}"
    );
    assert!(
        (120..=135).contains(&pixel[3]),
        "alpha 应当约为 128：{pixel:?}"
    );
}

#[test]
fn corrupt_svg_reports_failure_instead_of_panicking() {
    let dir = workdir("svg-corrupt");
    let path = dir.join("broken.svg");
    // 根元素都没闭合：能通过文件头探测，但解析必然失败。
    write_svg(
        &path,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect"#,
    );

    let error =
        decode::decode_path(&path, &DecodeLimits::default()).expect_err("残缺的 SVG 不应解码成功");
    assert!(
        matches!(
            error,
            DecodeError::Corrupt { .. } | DecodeError::Unsupported { .. }
        ),
        "实际得到 {error:?}"
    );
    assert!(!error.user_message().is_empty());
}

#[test]
fn raw_extension_is_routed_to_raw_family_with_tiff_fallback() {
    let dir = workdir("raw");

    // NEF/ARW/DNG 与普通 TIFF 共用文件头，只能靠扩展名消歧：判定上必须归到 RAW 族。
    let header_only = dir.join("DSC_0001.NEF");
    std::fs::write(&header_only, b"II\x2a\x00\x08\x00\x00\x00\x00\x00").unwrap();
    let sniffed = sniff::sniff(&std::fs::read(&header_only).unwrap(), Some(&header_only));
    assert_eq!(sniffed.format, Some(ImageFormat::Raw));
    assert_eq!(sniffed.source, decode::sniff::SniffSource::Extension);

    // 但「判定为 RAW」不等于「只准用 RAW 解码器」。
    // 真 NEF 里往往带着完整的 TIFF 结构，这时按 TIFF 解反而能让用户先看到图；
    // 硬报「不支持 RAW」是把可用信息丢掉。用一张真的 TIFF 改名成 .nef 验证这条兜底。
    let path = dir.join("DSC_0002.NEF");
    let source = dir.join("source.tiff");
    write_image(&source, &gradient(8, 6));
    std::fs::rename(&source, &path).unwrap();

    let data = decode::decode_path(&path, &DecodeLimits::default())
        .expect("TIFF 结构的 .nef 应能通过兜底路径打开");
    assert_eq!(data.format, ImageFormat::Tiff);
    assert_eq!((data.width(), data.height()), (8, 6));

    // 而真正无法作为 TIFF 解析的 RAW：必须给出可读的失败提示，而不是崩溃。
    let error = decode::decode_path(&header_only, &DecodeLimits::default())
        .expect_err("只有文件头的 RAW 不应解码成功");
    assert!(!error.user_message().is_empty());
}

#[test]
fn exif_orientation_is_parsed_from_jpeg() {
    let dir = workdir("exif");
    let path = dir.join("oriented.jpg");

    // 用 image 写不出带 EXIF 的 JPEG（它只写像素），所以这里直接拼一个
    // 最小可用的 EXIF APP1 段 + 一张真实 JPEG，验证「方向会被读出来」。
    let mut jpeg = Vec::new();
    {
        let buffer = gradient(4, 4);
        let mut cursor = std::io::Cursor::new(&mut jpeg);
        image::DynamicImage::ImageRgba8(buffer)
            .write_to(&mut cursor, image::ImageFormat::Jpeg)
            .expect("写入 JPEG 失败");
    }

    // APP1 段：FFE1 + 长度 + "Exif\0\0" + TIFF 头 + 1 个 IFD（Orientation=6）
    let mut app1_payload = b"Exif\x00\x00".to_vec();
    app1_payload.extend_from_slice(&build_tiff_orientation(6));
    let segment_len = (app1_payload.len() + 2) as u16;
    let mut with_exif = Vec::new();
    with_exif.extend_from_slice(&jpeg[..2]); // SOI
    with_exif.push(0xFF);
    with_exif.push(0xE1);
    with_exif.extend_from_slice(&segment_len.to_be_bytes());
    with_exif.extend_from_slice(&app1_payload);
    with_exif.extend_from_slice(&jpeg[2..]);
    std::fs::write(&path, &with_exif).unwrap();

    let data = decode::decode_path(&path, &DecodeLimits::default()).expect("带 EXIF 的 JPEG 应能解码");
    assert_eq!(data.format, ImageFormat::Jpeg);
    assert_eq!(data.orientation, decode::Orientation::Rotate90);
    // 方向会交换逻辑宽高：原图 4×4 看不出差别，这里只验证映射本身。
    assert_eq!(data.orientation.to_exif(), 6);
    assert_eq!(
        data.exif.as_ref().and_then(|exif| exif.orientation_raw),
        Some(6)
    );
}

/// 构造一个只含 Orientation 字段的 TIFF/EXIF 头（小端）。
fn build_tiff_orientation(orientation: u16) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"II\x2a\x00");
    data.extend_from_slice(&8u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes()); // 1 个条目
    data.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    data.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    data.extend_from_slice(&1u32.to_le_bytes()); // count
    let mut value = [0u8; 4];
    value[..2].copy_from_slice(&orientation.to_le_bytes());
    data.extend_from_slice(&value);
    data.extend_from_slice(&0u32.to_le_bytes()); // 没有下一个 IFD
    data
}

#[test]
fn io_errors_carry_the_file_name_for_the_user() {
    let dir = workdir("io");
    let path = dir.join("missing.png");
    let _ = std::fs::remove_file(&path);

    let error = decode::decode_path(&path, &DecodeLimits::default()).expect_err("文件不存在");
    assert!(matches!(error, DecodeError::Io { .. }));
    assert!(error.user_message().contains("missing.png"));
}

/// 把一张图像存成 PNG 并返回其原始字节，供容器格式内嵌复用。
fn png_bytes(path: &Path, image: &ImageBuffer<Rgba<u8>, Vec<u8>>) -> Vec<u8> {
    let png_path = path.with_extension("png");
    write_image(&png_path, image);
    std::fs::read(&png_path).expect("读取生成的 PNG 失败")
}

#[test]
fn cur_file_decodes_with_correct_size() {
    // CUR 与 ICO 是同一容器，区别只是目录头类型字段（2 = 光标）。
    // 这里现场造一个「PNG 内嵌」的 8×6 CUR，验证解码器能解出正确的尺寸。
    let dir = workdir("cur");
    let png = png_bytes(&dir.join("inner.cur"), &gradient(8, 6));

    let mut cur = Vec::new();
    cur.extend_from_slice(&0u16.to_le_bytes()); // reserved
    cur.extend_from_slice(&2u16.to_le_bytes()); // type = 2（光标）
    cur.extend_from_slice(&1u16.to_le_bytes()); // 1 个条目
    cur.push(8); // 宽度
    cur.push(6); // 高度
    cur.push(0); // color count
    cur.push(0); // reserved
    cur.extend_from_slice(&0u16.to_le_bytes()); // hotspot x
    cur.extend_from_slice(&0u16.to_le_bytes()); // hotspot y
    cur.extend_from_slice(&(png.len() as u32).to_le_bytes()); // 资源字节数
    cur.extend_from_slice(&22u32.to_le_bytes()); // 资源偏移 = 目录头 6 + 条目 16
    cur.extend_from_slice(&png);

    let path = dir.join("cursor.cur");
    std::fs::write(&path, &cur).unwrap();

    let sniffed = sniff::sniff(&cur, Some(&path));
    assert_eq!(sniffed.format, Some(ImageFormat::Cur));

    let data = decode::decode_path(&path, &DecodeLimits::default()).expect("CUR 应能解出");
    assert_eq!(data.format, ImageFormat::Cur);
    assert_eq!((data.width(), data.height()), (8, 6));
}

#[test]
fn icns_file_with_embedded_png_decodes() {
    // 现代 `.icns` 把各尺寸图像以 PNG 内嵌（OSType 如 `ic08`）。
    // 现场造一个含单张 8×6 PNG 的 icns，验证容器解析与内嵌 PNG 解码都正确。
    let dir = workdir("icns");
    let png = png_bytes(&dir.join("inner.icns"), &gradient(8, 6));

    let mut resource = Vec::new();
    resource.extend_from_slice(b"ic08"); // OSType：256×256 PNG
    resource.extend_from_slice(&((8 + png.len()) as u32).to_be_bytes()); // 含 8 字节头的资源长度
    resource.extend_from_slice(&png);

    let mut icns = Vec::new();
    icns.extend_from_slice(b"icns");
    icns.extend_from_slice(&((8 + resource.len()) as u32).to_be_bytes()); // 容器总长度（大端，含 8 字节头）
    icns.extend_from_slice(&resource);

    let path = dir.join("icon.icns");
    std::fs::write(&path, &icns).unwrap();

    let sniffed = sniff::sniff(&icns, Some(&path));
    // icns 没有魔数型特征码之外的额外判定，扩展名兜底到 Icns 即可。
    assert_eq!(sniffed.extension_format, Some(ImageFormat::Icns));

    let data = decode::decode_path(&path, &DecodeLimits::default()).expect("ICNS 应能解出");
    assert_eq!(data.format, ImageFormat::Icns);
    assert_eq!((data.width(), data.height()), (8, 6));
}

// ---------------------------------------------------------------------------
// Photoshop（PSD）解码。
//
// 重点不在「解出一张图」，而在「绝不对不支持的形态静默吐错图」：
// `psd` crate 对 CMYK / 高位深不会报错，却会把 C/M/Y/K 等通道当 R/G/B/A 输出，
// 得到一张颜色全错、却「看起来解码成功」的图。所以必须在转 RGBA 之前显式拦截。
// ---------------------------------------------------------------------------

/// 现场造一个最小合法 PSD 字节（1×1，RawData 压缩，平面排列的通道）。
///
/// `color_mode` / `depth` 直接对应文件头字段；`channels` 是按 R/G/B(/A) 顺序排好的
/// 每通道字节（长度需与通道数一致）。三个 major section 长度均为 0，
/// 这与 `psd` crate 的切分方式一致（每个段以 4 字节大端长度开头）。
fn minimal_psd(color_mode: u8, depth: u8, channels: &[u8]) -> Vec<u8> {
    let channel_count: u16 = if color_mode == 0x04 { 4 } else { 3 };
    let mut b = Vec::with_capacity(48 + channels.len());
    b.extend_from_slice(b"8BPS");
    b.extend_from_slice(&[0x00, 0x01]); // version 1
    b.extend_from_slice(&[0x00; 6]); // reserved
    b.extend_from_slice(&channel_count.to_be_bytes());
    b.extend_from_slice(&1u32.to_be_bytes()); // height
    b.extend_from_slice(&1u32.to_be_bytes()); // width
    b.extend_from_slice(&[0x00, depth]); // depth
    b.extend_from_slice(&[0x00, color_mode]); // color mode
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // color mode data section (len 0)
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // image resources section (len 0)
    b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // layer & mask section (len 0)
    b.extend_from_slice(&[0x00, 0x00]); // compression = 0 (RawData)
    b.extend_from_slice(channels);
    b
}

#[test]
fn psd_8bit_rgb_decodes_merged_image() {
    // 8 位 RGB 是 PSD 最常见、也最该「开箱即解」的形态。
    let bytes = minimal_psd(0x03, 0x08, &[0xFF, 0x00, 0x00]); // 纯红
    let dir = workdir("psd");
    let path = dir.join("red.psd");
    std::fs::write(&path, &bytes).unwrap();

    let data = decode::decode_path(&path, &DecodeLimits::default()).expect("PSD 应解码成功");
    assert_eq!(data.format, ImageFormat::Psd);
    let frame = data.primary();
    assert_eq!((frame.width, frame.height), (1, 1));
    // 非预乘 RGBA8、逐行：纯红点应为 (255, 0, 0, 255)。
    assert_eq!(&frame.rgba8[..], &[0xFF, 0x00, 0x00, 0xFF]);
}

#[test]
fn psb_is_rejected_with_actionable_message() {
    // `psd` crate 完全不支持 PSB（版本号 2），及早给可操作提示而不是假装能解。
    let mut bytes = minimal_psd(0x03, 0x08, &[0xFF, 0x00, 0x00]);
    bytes[4] = 0x00;
    bytes[5] = 0x02; // 版本号改为 2 → PSB
    let dir = workdir("psb");
    let path = dir.join("big.psb");
    std::fs::write(&path, &bytes).unwrap();

    let error =
        decode::decode_path(&path, &DecodeLimits::default()).expect_err("PSB 应被明确拒绝");
    assert!(
        matches!(error, DecodeError::Unsupported { .. }),
        "实际得到 {error:?}"
    );
    assert!(
        error.user_message().contains("PSB"),
        "提示里必须点名 PSB：{}",
        error.user_message()
    );
}

#[test]
fn psd_cmyk_is_rejected_not_silently_wrong() {
    // 头号风险：crate 对 CMYK 不会报错，却会把 C/M/Y/K 当 R/G/B/A 吐出颜色全错的图。
    // 必须在解码前显式拒绝，绝不能交付一张「看起来成功」的错图。
    let bytes = minimal_psd(0x04, 0x08, &[0x00, 0x00, 0x00, 0x00]);
    let dir = workdir("psd-cmyk");
    let path = dir.join("cmyk.psd");
    std::fs::write(&path, &bytes).unwrap();

    let error = decode::decode_path(&path, &DecodeLimits::default())
        .expect_err("CMYK PSD 应被明确拒绝");
    assert!(
        matches!(error, DecodeError::Unsupported { .. }),
        "实际得到 {error:?}"
    );
    assert!(
        error.user_message().contains("CMYK"),
        "提示里必须点名 CMYK：{}",
        error.user_message()
    );
}

#[test]
fn psd_16bit_is_rejected() {
    // 16 位 / 通道的 PSD：crate 只做粗糙的 16→8 截断且不支持透明度，
    // 直接拒绝比交付可能偏色的图更稳妥。
    let bytes = minimal_psd(0x03, 0x10, &[0xFF, 0x00, 0x00, 0x00, 0x00, 0x00]);
    let dir = workdir("psd-16bit");
    let path = dir.join("deep.psd");
    std::fs::write(&path, &bytes).unwrap();

    let error = decode::decode_path(&path, &DecodeLimits::default())
        .expect_err("16 位 PSD 应被明确拒绝");
    assert!(
        matches!(error, DecodeError::Unsupported { .. }),
        "实际得到 {error:?}"
    );
}

// ---------------------------------------------------------------------------
// JPEG 2000（JP2）解码：走 jpeg2k 的 C 后端 openjpeg-sys（工业级 OpenJPEG 绑定）。
//
// 真解码端到端需要真实 JP2 样本，而仓库不入库二进制素材、测试环境也没有
// JP2 编码器（Pillow 未安装、jpeg2k 未暴露编码入口）。因此这里只验证
// 「坏文件绝不崩溃、也绝不静默」这一产品级约定；像素收敛逻辑（8 位直通 /
// 灰度扩展 / 16 位缩放）由 jp2.rs 内的单元测试覆盖。
// ---------------------------------------------------------------------------

#[test]
fn jp2_corrupt_does_not_panic_and_names_the_format() {
    // 只有 JP2 容器的 12 字节签名，后面没有 codestream：OpenJPEG 必然解码失败。
    // 关键不是「被拒」，而是「以一条可读、点名格式、且不崩溃的方式失败」。
    let bytes = [
        0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
    ];
    let dir = workdir("jp2-corrupt");
    let path = dir.join("broken.jp2");
    std::fs::write(&path, &bytes).unwrap();

    let error = decode::decode_path(&path, &DecodeLimits::default())
        .expect_err("损坏的 JP2 不应解码成功");
    // 解码失败不能是静默，也不能 panic；落到 Corrupt 或 Unsupported 都算「明确失败」。
    assert!(
        matches!(
            error,
            DecodeError::Corrupt { .. } | DecodeError::Unsupported { .. }
        ),
        "实际得到 {error:?}"
    );
    let message = error.user_message();
    assert!(!message.is_empty(), "必须给出可读提示");
    assert!(
        message.contains("JPEG 2000") || message.contains("JP2"),
        "提示应点名格式：{message}"
    );
}

// ---------------------------------------------------------------------------
// XBM / XPM / FLIF / PICT / MNG / JNG 解码。
//
// XBM / XPM 是自写纯 Rust 解析器：用「现场生成样本」的方式验证端到端正确。
// FLIF 用纯 Rust 的 `flif` crate（无编码器，故只验证「坏文件不崩溃、点名格式」+
// 转换逻辑已由 flif.rs 内单测覆盖）。PICT 用 `oxideav-pict` 自带编码器回环验证。
// MNG / JNG 在 Rust 生态无解码库，只验证「识别 + 给出可操作拒绝」。
// ---------------------------------------------------------------------------

#[test]
fn xbm_decodes_black_and_white() {
    let dir = workdir("xbm");
    let path = dir.join("icons.xbm");
    // 宽 4：位 0/2 设 1（黑），位 1/3 为 0（白）→ 字节 0b0000_0101 = 0x05。
    let xbm = "\
#define pic_width 4
#define pic_height 1
static unsigned char pic_bits[] = {
0x05
};
";
    std::fs::write(&path, xbm).unwrap();

    let data = decode::decode_path(&path, &DecodeLimits::default()).expect("XBM 应解出");
    assert_eq!(data.format, ImageFormat::Xbm);
    assert_eq!((data.width(), data.height()), (4, 1));
    let px = &data.primary().rgba8;
    assert_eq!(&px[0..4], &[0, 0, 0, 255], "像素 0 应黑");
    assert_eq!(&px[4..8], &[255, 255, 255, 255], "像素 1 应白");
    assert_eq!(&px[8..12], &[0, 0, 0, 255], "像素 2 应黑");
    assert_eq!(&px[12..16], &[255, 255, 255, 255], "像素 3 应白");
}

#[test]
fn xpm_decodes_colors_and_transparency() {
    let dir = workdir("xpm");
    let path = dir.join("icon.xpm");
    // 2×2，cpp=1：a=红 b=绿 c=透明。
    let xpm = "/* XPM */
static char *pic[] = {
\"2 2 3 1\",
\"a c #FF0000\",
\"b c #00FF00\",
\"c c none\",
\"ab\",
\"ca\"
};
";
    std::fs::write(&path, xpm).unwrap();

    let data = decode::decode_path(&path, &DecodeLimits::default()).expect("XPM 应解出");
    assert_eq!(data.format, ImageFormat::Xpm);
    assert_eq!((data.width(), data.height()), (2, 2));
    let px = &data.primary().rgba8;
    assert_eq!(&px[0..4], &[255, 0, 0, 255], "像素(0,0) 应红");
    assert_eq!(&px[4..8], &[0, 255, 0, 255], "像素(1,0) 应绿");
    assert_eq!(&px[8..12], &[0, 0, 0, 0], "像素(0,1) 应透明");
    assert_eq!(&px[12..16], &[255, 0, 0, 255], "像素(1,1) 应红");
}

#[test]
fn flif_corrupt_does_not_panic_and_names_the_format() {
    let dir = workdir("flif-corrupt");
    let path = dir.join("broken.flif");
    // FLIF 魔数 + 垃圾内容。
    let mut bytes = b"FLIF".to_vec();
    bytes.extend_from_slice(&[0u8; 32]);
    std::fs::write(&path, &bytes).unwrap();

    let sniffed = sniff::sniff(&bytes, Some(&path));
    assert_eq!(sniffed.format, Some(ImageFormat::Flif));

    let error = decode::decode_path(&path, &DecodeLimits::default())
        .expect_err("垃圾 FLIF 不应解码成功");
    assert!(
        matches!(error, DecodeError::Corrupt { .. } | DecodeError::Unsupported { .. }),
        "实际得到 {error:?}"
    );
    assert!(
        error.user_message().contains("FLIF"),
        "提示应点名格式：{}",
        error.user_message()
    );
}

#[test]
fn pict_roundtrip_via_encoder() {
    let dir = workdir("pict");
    let path = dir.join("sample.pict");
    let (w, h) = (8u32, 6u32);
    let mut src = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h {
        for x in 0..w {
            let o = ((y * w + x) as usize) * 4;
            src[o] = (x * 255 / w.max(1)) as u8;
            src[o + 1] = (y * 255 / h.max(1)) as u8;
            src[o + 2] = 64;
            src[o + 3] = 255;
        }
    }
    // 用 oxideav-pict 自带的编码器造一个真实 PICT 样本（packType=1）。
    let encoded = oxideav_pict::encode_pict(w, h, &src).expect("PICT 编码失败");
    std::fs::write(&path, &encoded).unwrap();

    let data = decode::decode_path(&path, &DecodeLimits::default()).expect("PICT 应解出");
    assert_eq!(data.format, ImageFormat::Pict);
    assert_eq!((data.width(), data.height()), (w, h));
    let px = &data.primary().rgba8;
    assert_eq!(px.len(), w as usize * h as usize * 4);
    // packType=1 是 0xFF R G B：R/G/B 原样保留，alpha 恒为 255。
    for p in 0..(w as usize * h as usize) {
        assert_eq!(px[p * 4 + 3], 255, "alpha 应不透明");
        assert_eq!(
            &px[p * 4..p * 4 + 3],
            &src[p * 4..p * 4 + 3],
            "第 {p} 像素 RGB 应一致"
        );
    }
}

#[test]
fn mng_and_jng_are_recognized_and_rejected_with_guidance() {
    let dir = workdir("mng-jng");
    let cases: [(&str, &[u8], ImageFormat, &str); 2] = [
        (
            "anim.mng",
            &[0x8A, b'M', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            ImageFormat::Mng,
            "MNG",
        ),
        (
            "photo.jng",
            &[0x8B, b'J', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            ImageFormat::Jng,
            "JNG",
        ),
    ];
    for (name, magic, format, label) in cases {
        let path = dir.join(name);
        let mut bytes = magic.to_vec();
        bytes.extend_from_slice(&[0u8; 64]);
        std::fs::write(&path, &bytes).unwrap();

        let sniffed = sniff::sniff(&bytes, Some(&path));
        assert_eq!(sniffed.format, Some(format), "{name} 应被识别");

        let error = decode::decode_path(&path, &DecodeLimits::default())
            .expect_err("{name} 应被拒绝而非崩溃");
        assert!(
            matches!(error, DecodeError::Unsupported { .. }),
            "{name} 实际得到 {error:?}"
        );
        assert!(
            error.user_message().contains(label),
            "{name} 的提示应点名格式：{}",
            error.user_message()
        );
    }
}


