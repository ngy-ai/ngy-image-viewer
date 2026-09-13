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
