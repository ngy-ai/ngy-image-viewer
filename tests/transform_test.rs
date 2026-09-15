//! 视图变换的集成测试。
//!
//! # 为什么这些断言值得写
//!
//! 缩放与平移的错误几乎全都是「看起来只是有点别扭」的那种：
//! 滚轮缩放时锚点慢慢漂移、窗口变窄后图像跑偏、1:1 在高分屏上变成 200%。
//! 在窗口里靠肉眼很难断定「到底对不对」，但它们全都可以被精确断言。
//!
//! 同理，方向的复合（EXIF 摆正 + 用户旋转）一旦写反，只有在
//! 「竖拍照片 + 用户旋转」这种特定组合上才会露馅。这里用**像素级实现**
//! 作为参照系，把 8×8 种组合逐一对齐 —— 代数与像素两条路径必须给出同一张图。

use ngy_image_viewer::decode::orientation;
use ngy_image_viewer::decode::Orientation;
use ngy_image_viewer::model::{Size, Vec2, ViewTransform, ZoomMode};

// ---------------------------------------------------------------------------
// 缩放锚点：整套交互手感就建立在它上面
// ---------------------------------------------------------------------------

#[test]
fn cursor_anchored_zoom_keeps_the_pixel_under_the_cursor() {
    let image = Size::new(1000.0, 500.0);
    let viewport = Size::new(800.0, 600.0);

    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);

    // 光标放在图像内部的一个非中心位置：中心点做锚点会掩盖掉一半的错误。
    let cursor = Vec2::new(500.0, 320.0);
    let before = transform.screen_to_image(cursor, image, viewport);

    transform.zoom_at(cursor, 2.0, image, viewport);

    let after = transform.screen_to_image(cursor, image, viewport);
    assert!(
        (before.x - after.x).abs() < 1e-3 && (before.y - after.y).abs() < 1e-3,
        "光标下的图像点漂移了：{before:?} → {after:?}"
    );

    // 反向缩放同样要钉住。
    transform.zoom_at(cursor, 0.5, image, viewport);
    let back = transform.screen_to_image(cursor, image, viewport);
    assert!((before.x - back.x).abs() < 1e-3 && (before.y - back.y).abs() < 1e-3);
}

#[test]
fn repeated_zooming_does_not_drift_the_anchor() {
    // 单次缩放正确不代表累积正确：每次留一点点误差的话，滚十几下之后
    // 用户正在看的细节就跑了。这里连续放大再连续缩小，验证能回到原处。
    //
    // 前提：内容明显大于视口，平移钳制不会介入。钳制与锚点的取舍
    // 由 `zooming_never_leaves_a_gap_at_the_viewport_edge` 单独说明。
    let image = Size::new(4000.0, 3000.0);
    let viewport = Size::new(900.0, 700.0);
    let cursor = Vec2::new(220.0, 480.0);

    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);
    transform.set_scale(2.0, image, viewport);
    let anchor = transform.screen_to_image(cursor, image, viewport);

    for _ in 0..12 {
        transform.zoom_at(cursor, 1.15, image, viewport);
    }
    for _ in 0..12 {
        transform.zoom_at(cursor, 1.0 / 1.15, image, viewport);
    }

    let landed = transform.screen_to_image(cursor, image, viewport);
    assert!(
        (anchor.x - landed.x).abs() < 0.5 && (anchor.y - landed.y).abs() < 0.5,
        "连续缩放后锚点漂移了：{anchor:?} → {landed:?}"
    );
}

#[test]
fn zooming_never_leaves_a_gap_at_the_viewport_edge() {
    // 与上一条测试互补：当内容比视口大时，缩放后**不允许**在视口边缘露出空白。
    //
    // 这条约束会与「锚点绝对不动」冲突 —— 于是本项目选择让锚点让路：
    // 露出空白看起来像渲染坏了，而光标下的点偏移几个像素几乎察觉不到。
    // 这里把「不许露白」这条不变量钉死，避免将来为了锚点精确而把它放开。
    let image = Size::new(1000.0, 500.0);
    let viewport = Size::new(800.0, 600.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);

    let cursor = Vec2::new(720.0, 540.0);
    for _ in 0..6 {
        transform.zoom_at(cursor, 1.2, image, viewport);

        let content = transform.content_rect(image, viewport);
        if content.size.width >= viewport.width {
            assert!(
                content.origin.x <= 0.01 && content.max().x >= viewport.width - 0.01,
                "水平方向露出了空白：{content:?}"
            );
        }
        if content.size.height >= viewport.height {
            assert!(
                content.origin.y <= 0.01 && content.max().y >= viewport.height - 0.01,
                "垂直方向露出了空白：{content:?}"
            );
        }
    }
}

#[test]
fn zoom_is_clamped_at_both_ends() {
    let image = Size::new(1000.0, 1000.0);
    let viewport = Size::new(500.0, 500.0);
    let mut transform = ViewTransform::default();

    // 一直放大：必须停在 256 倍，而不是变成无穷大。
    for _ in 0..500 {
        transform.zoom_by(1.5, image, viewport);
    }
    assert!(transform.is_at_max_scale(), "实际倍率 {}", transform.scale());
    assert!(transform.scale() <= 256.0);

    // 一直缩小：必须停在 1/512，而不是变成 0（那会让坐标换算除零）。
    for _ in 0..500 {
        transform.zoom_by(0.5, image, viewport);
    }
    assert!(transform.is_at_min_scale(), "实际倍率 {}", transform.scale());
    assert!(transform.scale() > 0.0);
    assert!(transform.zoom_percent(1.0) > 0.0);
}

// ---------------------------------------------------------------------------
// 适应窗口 / 1:1 / 窗口变化
// ---------------------------------------------------------------------------

#[test]
fn fit_shows_the_whole_image_without_distortion() {
    let image = Size::new(1000.0, 500.0);
    let viewport = Size::new(800.0, 600.0);

    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);

    assert_eq!(transform.mode(), ZoomMode::Fit);
    assert!((transform.scale() - 0.8).abs() < 1e-6, "应取宽高两个方向中较小的倍率");

    // 内容完整落在视口内，并且居中。
    let content = transform.content_rect(image, viewport);
    assert!(content.origin.x >= -0.01 && content.origin.y >= -0.01, "{content:?}");
    assert!(content.max().x <= viewport.width + 0.01);
    assert!(content.max().y <= viewport.height + 0.01);
    assert!((content.center().x - viewport.width / 2.0).abs() < 1e-3);
    assert!((content.center().y - viewport.height / 2.0).abs() < 1e-3);
}

#[test]
fn fit_upscales_a_small_image_to_fill_the_window() {
    // 「适应窗口」的字面含义就是让图占满可用空间；小图被放大是预期行为，
    // 与各平台看图工具一致。这条断言把它钉死，避免将来被"优化"成不放大。
    let image = Size::new(16.0, 16.0);
    let viewport = Size::new(800.0, 600.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);
    assert!(transform.scale() > 1.0);
    assert!((transform.scale() - 600.0 / 16.0).abs() < 1e-3);
}

#[test]
fn actual_size_means_one_image_pixel_per_device_pixel() {
    let image = Size::new(1000.0, 500.0);
    let viewport = Size::new(800.0, 600.0);

    // 普通屏幕：1 个逻辑点 = 1 个设备像素。
    let mut transform = ViewTransform::default();
    transform.actual_size(1.0, image, viewport);
    assert_eq!(transform.mode(), ZoomMode::Actual);
    assert!((transform.zoom_percent(1.0) - 100.0).abs() < 1e-3);

    // 200% 缩放的高分屏：1 个逻辑点 = 2 个设备像素，
    // 所以 scale 必须是 0.5 —— 否则「100%」会显示成 200% 大小。
    transform.actual_size(2.0, image, viewport);
    assert!((transform.scale() - 0.5).abs() < 1e-6, "实际 {}", transform.scale());
    assert!((transform.zoom_percent(2.0) - 100.0).abs() < 1e-3);

    // 3 倍缩放屏同理。
    transform.actual_size(3.0, image, viewport);
    assert!((transform.zoom_percent(3.0) - 100.0).abs() < 1e-3);
}

#[test]
fn initial_view_stays_at_1_to_1_when_the_image_fits() {
    // 装得下就用 1:1：此时若改用「适应窗口」，小图会被放大到铺满窗口、
    // 先被插值糊一遍 —— 用户第一眼看到的已经不是自己的图。
    let image = Size::new(600.0, 400.0);
    let viewport = Size::new(900.0, 700.0);

    let initial = ViewTransform::initial(1.0, image, viewport);
    assert_eq!(initial.mode(), ZoomMode::Actual);
    assert!((initial.scale() - 1.0).abs() < 1e-6);
    assert_eq!(initial.pan(), Vec2::ZERO);
}

#[test]
fn initial_view_fits_the_window_when_the_image_overflows() {
    // 只有一个方向超出也算「装不下」：否则打开就看到被裁掉的一条边。
    let image = Size::new(1200.0, 400.0);
    let viewport = Size::new(800.0, 600.0);

    let initial = ViewTransform::initial(1.0, image, viewport);
    assert_eq!(initial.mode(), ZoomMode::Fit);

    let content = initial.content_rect(image, viewport);
    assert!(content.origin.x >= -0.01 && content.origin.y >= -0.01, "{content:?}");
    assert!(content.max().x <= viewport.width + 0.01);
    assert!(content.max().y <= viewport.height + 0.01);
}

#[test]
fn initial_view_compares_scales_not_raw_sizes() {
    // 200% 缩放的高分屏上，1:1 的倍率是 0.5：1000 逻辑像素的图只占 500 点，
    // 放进 900 点的画布绰绰有余。直接比尺寸会把它误判成「装不下」而放大。
    let image = Size::new(1000.0, 800.0);
    let viewport = Size::new(900.0, 700.0);

    let initial = ViewTransform::initial(2.0, image, viewport);
    assert_eq!(initial.mode(), ZoomMode::Actual, "200% 屏上这张图装得下");
    assert!((initial.scale() - 0.5).abs() < 1e-6);

    // 同一块画布、同样的图，1 倍屏上就是真的装不下。
    let initial = ViewTransform::initial(1.0, image, viewport);
    assert_eq!(initial.mode(), ZoomMode::Fit);
}

#[test]
fn initial_view_falls_back_to_1_to_1_when_the_canvas_is_unmeasured() {
    // 冷启动时打开结果早于首帧绘制，画布尺寸还是 0 —— 装不装得下无从判断。
    // 此时退回 1:1（最保守的起点），视图拿到真实尺寸后会再算一次。
    let image = Size::new(4000.0, 3000.0);

    let initial = ViewTransform::initial(1.0, image, Size::ZERO);
    assert_eq!(initial.mode(), ZoomMode::Actual);
    assert!((initial.scale() - 1.0).abs() < 1e-6);

    // 没有图像时同理：倍率与模式定住，不产生 NaN。
    let initial = ViewTransform::initial(2.0, Size::ZERO, Size::new(800.0, 600.0));
    assert_eq!(initial.mode(), ZoomMode::Actual);
    assert!(initial.scale().is_finite());
}

#[test]
fn zoom_percent_is_relative_to_the_device_pixel_ratio() {
    let image = Size::new(100.0, 100.0);
    let viewport = Size::new(100.0, 100.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);
    assert!((transform.scale() - 1.0).abs() < 1e-6);

    // 倍率 1.0 的含义是「1 个图像像素 = 1 个逻辑点」。
    // 在 1 倍屏上这就是 100%；在 2 倍屏上，1 个逻辑点等于 2 个设备像素，
    // 所以同一个倍率显示出来是 200%。
    //
    // 百分比必须把设备像素比算进去，否则高分屏用户按「1:1」会拿到一张两倍大的图 ——
    // 这正是各种看图工具上最常见的"100% 却不对"的投诉来源。
    assert!((transform.zoom_percent(1.0) - 100.0).abs() < 1e-3);
    assert!((transform.zoom_percent(2.0) - 200.0).abs() < 1e-3);
}

#[test]
fn toggle_switches_between_fit_and_actual() {
    let image = Size::new(1000.0, 500.0);
    let viewport = Size::new(800.0, 600.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);

    transform.toggle_fit_and_actual(1.0, image, viewport);
    assert_eq!(transform.mode(), ZoomMode::Actual);
    assert!((transform.scale() - 1.0).abs() < 1e-6);

    transform.toggle_fit_and_actual(1.0, image, viewport);
    assert_eq!(transform.mode(), ZoomMode::Fit);
    assert!((transform.scale() - 0.8).abs() < 1e-6);
}

#[test]
fn toggle_from_a_free_zoom_first_returns_to_fit() {
    // 从任意手动倍率双击，直觉是「回到适应窗口」这个熟悉的基准点；
    // 再双击一次才落到 1:1。这条断言把这个两步行为固定下来 ——
    // 若改成"一次直达 1:1"，用户会觉得双击是个随机跳转。
    let image = Size::new(1000.0, 500.0);
    let viewport = Size::new(800.0, 600.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);
    transform.zoom_by(3.0, image, viewport);
    assert_eq!(transform.mode(), ZoomMode::Free);

    transform.toggle_fit_and_actual(1.0, image, viewport);
    assert_eq!(transform.mode(), ZoomMode::Fit);

    transform.toggle_fit_and_actual(1.0, image, viewport);
    assert_eq!(transform.mode(), ZoomMode::Actual);
}

#[test]
fn resize_refits_only_in_fit_mode() {
    let image = Size::new(1000.0, 500.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, Size::new(800.0, 600.0));
    let fitted = transform.scale();

    // 适应窗口模式：窗口变大 → 重新计算，图像继续铺满。
    transform.apply_viewport_change(image, Size::new(1600.0, 1200.0));
    assert!(transform.scale() > fitted, "适应窗口模式应当在窗口变大后重新适应");

    // 1:1 模式：倍率是用户的明确选择，窗口怎么变都不动它。
    transform.actual_size(1.0, image, Size::new(800.0, 600.0));
    transform.apply_viewport_change(image, Size::new(400.0, 300.0));
    assert!((transform.scale() - 1.0).abs() < 1e-6, "1:1 模式不应随窗口改变倍率");
    assert_eq!(transform.mode(), ZoomMode::Actual);

    // 自由缩放模式同理。
    transform.zoom_by(2.0, image, Size::new(400.0, 300.0));
    let free = transform.scale();
    transform.apply_viewport_change(image, Size::new(2000.0, 1500.0));
    assert!((transform.scale() - free).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// 平移与边界
// ---------------------------------------------------------------------------

#[test]
fn panning_cannot_push_the_image_out_of_view() {
    let image = Size::new(1000.0, 1000.0);
    let viewport = Size::new(400.0, 400.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);
    // fit 之后内容正好是 400×400，与视口等大，此时不该能拖动。
    assert_eq!(transform.scale(), 0.4);

    transform.pan_by(Vec2::new(10_000.0, 10_000.0), image, viewport);
    assert!(transform.pan().x.abs() < 1e-3 && transform.pan().y.abs() < 1e-3);

    // 放大到内容远大于视口之后才能拖动，且拖到边缘就停。
    transform.zoom_by(4.0, image, viewport);
    let content = transform.content_size(image);
    transform.pan_by(Vec2::new(10_000.0, 10_000.0), image, viewport);
    let limit = (content.width - viewport.width) / 2.0;
    assert!((transform.pan().x - limit).abs() < 1e-3, "{:?}", transform.pan());
    assert!((transform.pan().y - limit).abs() < 1e-3);

    // 反方向同理。
    transform.pan_by(Vec2::new(-99_999.0, -99_999.0), image, viewport);
    assert!((transform.pan().x + limit).abs() < 1e-3);
}

#[test]
fn a_small_image_stays_inside_the_viewport() {
    // 比视口小的图可以在视口内自由挪动，但不能整张挪出视野 ——
    // 否则用户拖动一下图就"消失"了，还得靠双击找回来。
    let image = Size::new(100.0, 100.0);
    let viewport = Size::new(1000.0, 800.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);

    transform.pan_by(Vec2::new(100_000.0, 100_000.0), image, viewport);
    let content = transform.content_rect(image, viewport);
    assert!(content.max().x <= viewport.width + 0.01, "{content:?}");
    assert!(content.max().y <= viewport.height + 0.01, "{content:?}");

    transform.pan_by(Vec2::new(-100_000.0, -100_000.0), image, viewport);
    let content = transform.content_rect(image, viewport);
    assert!(content.origin.x >= -0.01, "{content:?}");
    assert!(content.origin.y >= -0.01, "{content:?}");
}

#[test]
fn center_only_clears_the_pan() {
    let image = Size::new(1000.0, 1000.0);
    let viewport = Size::new(400.0, 400.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);
    transform.zoom_by(4.0, image, viewport);
    transform.pan_by(Vec2::new(50.0, -30.0), image, viewport);
    assert_ne!(transform.pan(), Vec2::ZERO);

    let scale = transform.scale();
    transform.center();
    assert_eq!(transform.pan(), Vec2::ZERO);
    assert_eq!(transform.scale(), scale, "居中不应改变倍率");
}

// ---------------------------------------------------------------------------
// 坐标换算
// ---------------------------------------------------------------------------

#[test]
fn screen_and_image_coordinates_round_trip() {
    let image = Size::new(1234.0, 567.0);
    let viewport = Size::new(800.0, 600.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);
    transform.zoom_by(1.7, image, viewport);
    transform.pan_by(Vec2::new(-13.0, 27.0), image, viewport);

    for probe in [
        Vec2::new(0.0, 0.0),
        Vec2::new(400.0, 300.0),
        Vec2::new(799.0, 599.0),
        Vec2::new(123.5, 456.25),
    ] {
        let round_trip = transform.image_to_screen(
            transform.screen_to_image(probe, image, viewport),
            image,
            viewport,
        );
        assert!(
            (round_trip.x - probe.x).abs() < 1e-2 && (round_trip.y - probe.y).abs() < 1e-2,
            "{probe:?} 往返后变成 {round_trip:?}"
        );
    }
}

#[test]
fn the_image_center_maps_to_the_viewport_center_when_not_panned() {
    let image = Size::new(1000.0, 400.0);
    let viewport = Size::new(640.0, 480.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, viewport);

    let center = transform.image_to_screen(Vec2::new(500.0, 200.0), image, viewport);
    assert!((center.x - 320.0).abs() < 1e-3);
    assert!((center.y - 240.0).abs() < 1e-3);
}

// ---------------------------------------------------------------------------
// 退化输入：一个 NaN 就足以让整张图消失
// ---------------------------------------------------------------------------

#[test]
fn degenerate_sizes_and_inputs_never_produce_nan() {
    let mut transform = ViewTransform::default();

    // 空视口 / 空图像：不能除零，也不能让状态变成 NaN。
    transform.fit(Size::new(100.0, 100.0), Size::ZERO);
    assert!(transform.scale().is_finite());
    transform.fit(Size::ZERO, Size::new(100.0, 100.0));
    assert!(transform.scale().is_finite());

    let image = Size::new(100.0, 100.0);
    let viewport = Size::new(400.0, 400.0);
    transform.fit(image, viewport);

    // 非有限的输入（损坏的滚轮增量、异常的鼠标坐标）必须被忽略。
    let before = transform;
    transform.zoom_at(Vec2::new(f32::NAN, 0.0), 1.5, image, viewport);
    transform.zoom_at(Vec2::ZERO, f32::NAN, image, viewport);
    transform.zoom_at(Vec2::ZERO, 0.0, image, viewport);
    transform.zoom_at(Vec2::ZERO, -2.0, image, viewport);
    transform.pan_by(Vec2::new(f32::INFINITY, 0.0), image, viewport);
    assert_eq!(transform, before, "非法输入不应改变视图状态");

    // 换算结果也必须是有限数。
    let point = transform.image_to_screen(Vec2::new(50.0, 50.0), image, viewport);
    assert!(point.is_finite());
}

#[test]
fn resizing_to_a_zero_viewport_keeps_the_state_sane() {
    let image = Size::new(1000.0, 500.0);
    let mut transform = ViewTransform::default();
    transform.fit(image, Size::new(800.0, 600.0));

    // 窗口最小化时视口可能是零尺寸；随后恢复时必须还能正常显示。
    transform.apply_viewport_change(image, Size::ZERO);
    assert!(transform.scale().is_finite() && transform.scale() > 0.0);
    assert!(transform.pan().is_finite());

    transform.apply_viewport_change(image, Size::new(1024.0, 768.0));
    let content = transform.content_rect(image, Size::new(1024.0, 768.0));
    assert!(content.size.width > 0.0 && content.size.height > 0.0);
}

// ---------------------------------------------------------------------------
// 方向复合：代数路径必须与像素路径给出同一张图
// ---------------------------------------------------------------------------

/// 把 `orientation::apply` 的「恒等返回 None」统一成一个便于比较的形状。
fn apply_orientation(
    width: u32,
    height: u32,
    data: Vec<u8>,
    value: Orientation,
) -> (u32, u32, Vec<u8>) {
    match orientation::apply(width, height, &data, value) {
        Some(result) => result,
        None => (width, height, data),
    }
}

/// 3×2 的样本图，6 个像素各不相同。
///
/// 刻意用**非正方形**且**不对称**的图案：正方形或对称图会掩盖掉
/// 大部分方向写错的情况（转 180° 和镜像看起来一样）。
fn asymmetric_sample() -> (u32, u32, Vec<u8>) {
    let mut data = Vec::with_capacity(3 * 2 * 4);
    for index in 0..6u8 {
        data.extend_from_slice(&[index * 10, index * 20, index * 30, 255]);
    }
    (3, 2, data)
}

#[test]
fn every_orientation_pair_matches_the_pixel_level_result() {
    let (width, height, original) = asymmetric_sample();

    for first_exif in 1..=8u8 {
        for second_exif in 1..=8u8 {
            let first = Orientation::from_exif(first_exif);
            let second = Orientation::from_exif(second_exif);

            // 路径一：先做像素变换，再做一次像素变换。
            let (w1, h1, once) = apply_orientation(width, height, original.clone(), first);
            let (w2, h2, twice) = apply_orientation(w1, h1, once, second);

            // 路径二：先把两个方向复合成一个，再一次性做像素变换。
            let (w3, h3, direct) =
                apply_orientation(width, height, original.clone(), first.then(second));

            assert_eq!(
                (w2, h2),
                (w3, h3),
                "EXIF {first_exif} then {second_exif} 的尺寸对不上"
            );
            assert_eq!(
                twice, direct,
                "EXIF {first_exif} then {second_exif} 的像素对不上：\
                 `then` 的复合顺序与像素实现不一致"
            );
        }
    }
}

#[test]
fn rotated_clockwise_matches_the_rotate90_orientation() {
    // `rotated_clockwise(1)` 与 `then(Rotate90)` 必须是同一件事 ——
    // 这两条路径分别被「旋转按钮」和「EXIF 摆正」使用，一旦分叉就会
    // 出现「有的图正、有的图歪」这种最难排查的现象。
    for exif in 1..=8u8 {
        let base = Orientation::from_exif(exif);
        assert_eq!(base.rotated_clockwise(1), base.then(Orientation::Rotate90));
        assert_eq!(base.rotated_clockwise(2), base.then(Orientation::Rotate180));
        assert_eq!(base.rotated_clockwise(3), base.then(Orientation::Rotate270));
    }
}

#[test]
fn exif_orientation_then_user_rotation_is_not_the_other_way_round() {
    // 「先 EXIF 摆正，再用户旋转」与「先用户旋转，再 EXIF 摆正」在
    // 竖拍照片上会给出不同的结果。这条断言把正确的顺序固定下来。
    let (width, height, original) = asymmetric_sample();
    let exif = Orientation::Rotate90;
    let user = Orientation::FlipHorizontal;

    let correct = apply_orientation(width, height, original.clone(), exif.then(user));
    let reversed = apply_orientation(width, height, original, user.then(exif));

    assert_ne!(
        correct.2, reversed.2,
        "这个样本上两种顺序应当产生不同结果，否则这条测试失去了意义"
    );

    // 正确顺序 = 先做 EXIF 的像素变换，再做用户操作的像素变换。
    let (w1, h1, once) = apply_orientation(width, height, correct_preimage(), exif);
    let (_, _, twice) = apply_orientation(w1, h1, once, user);
    assert_eq!(twice, correct.2);
}

/// 与 [`asymmetric_sample`] 相同的图案，单独取一份避免被移动语义干扰。
fn correct_preimage() -> Vec<u8> {
    asymmetric_sample().2
}
