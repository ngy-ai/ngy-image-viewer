//! 界面三个区块：顶部工具栏、底部状态栏、右侧 EXIF 面板，以及结果浮层。
//!
//! # 设计约定
//!
//! 这里的每个函数都遵循同一个模式：**数据进、元素出**，并且所有需要改状态的动作
//! 都通过一个 `Entity<ImageViewerView>` 回调出去。好处有两个：
//!
//! 1. 区块本身没有状态，「显示什么」完全由调用方决定，不会出现两处状态打架；
//! 2. 每个区块都能单独替换样式，不影响交互逻辑。
//!
//! # 视觉基调
//!
//! 界面要**退让**：工具栏与状态栏用低对比度表面色，文字用中等灰，
//! 只有「当前生效的模式」与「主要动作」才用主色。这样用户的注意力
//! 始终在图像上，而不是在界面控件上。

use gpui_kit::*;

use crate::model::ImageDocument;
use crate::ui::format::{bytes_text, duration_text, size_text, zoom_text};
use crate::ui::icons;
use crate::ui::theme::{self, Skin};
use crate::ui::view::ImageViewerView;

/// 面板与工具栏共用的区块分隔线。
fn hairline(skin: &Skin) -> Hsla {
    skin.border
}

/// 顶部工具栏。
///
/// 布局：左侧是文件名与格式角标，中部是缩放读数与适应/1:1 切换，
/// 右侧是旋转、翻转、复制、另存为、重命名、删除与信息面板开关。
pub fn toolbar(
    skin: &Skin,
    document: Option<&ImageDocument>,
    zoom_percent: f32,
    fits: bool,
    info_open: bool,
    view: &Entity<ImageViewerView>,
) -> impl IntoElement {
    let file_name = document
        .map(|document| document.file_name())
        .unwrap_or_else(|| "未打开图片".to_string());
    let format = document.map(|document| document.format_label().to_string());
    let editable = document.is_some();

    let mut left = div().flex().flex_row().items_center().gap_2().child(
        div()
            .max_w(px(320.0))
            .overflow_hidden()
            .text_color(skin.text)
            .text_size(px(13.0))
            .font_weight(FontWeight::MEDIUM)
            .child(file_name),
    );

    if let Some(format) = format {
        left = left.child(
            div()
                .px_2()
                .py(px(1.0))
                .rounded_sm()
                .bg(skin.surface_active)
                .text_color(skin.text_muted)
                .text_size(px(10.0))
                .child(format),
        );
    }

    // 中部：缩放读数 + 两个模式按钮。读数用等宽数字感的固定宽度，
    // 免得百分比在 9%↔100% 之间跳动时整条工具栏跟着抖。
    let middle = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(
            div()
                .w(px(64.0))
                .text_center()
                .text_color(skin.text_muted)
                .text_size(px(12.0))
                .child(zoom_text(zoom_percent)),
        )
        .child(icon_button(
            skin,
            icons::FIT,
            "适应窗口",
            fits,
            editable,
            {
                let view = view.clone();
                move |_, _, cx| update(&view, cx, |this, cx| this.set_fit_mode(cx))
            },
        ))
        .child(icon_button(
            skin,
            icons::ACTUAL,
            "1:1",
            !fits,
            editable,
            {
                let view = view.clone();
                move |_, _, cx| update(&view, cx, |this, cx| this.set_actual_mode(cx))
            },
        ));

    let mut right = div().flex().flex_row().items_center().gap_1();

    right = right
        .child(icon_button(skin, icons::ROTATE_CCW, "←90°", false, editable, {
            let view = view.clone();
            move |_, _, cx| update(&view, cx, |this, cx| this.rotate_counter_clockwise(cx))
        }))
        .child(icon_button(skin, icons::ROTATE_CW, "90°→", false, editable, {
            let view = view.clone();
            move |_, _, cx| update(&view, cx, |this, cx| this.rotate_clockwise(cx))
        }))
        .child(icon_button(skin, icons::FLIP_H, "水平翻转", false, editable, {
            let view = view.clone();
            move |_, _, cx| update(&view, cx, |this, cx| this.flip_horizontal(cx))
        }))
        .child(icon_button(skin, icons::FLIP_V, "垂直翻转", false, editable, {
            let view = view.clone();
            move |_, _, cx| update(&view, cx, |this, cx| this.flip_vertical(cx))
        }))
        .child(icon_button(skin, icons::COPY, "复制", false, editable, {
            let view = view.clone();
            move |_, _, cx| update(&view, cx, |this, cx| this.copy_to_clipboard(cx))
        }))
        .child(icon_button(skin, icons::SAVE_AS, "另存为", false, editable, {
            let view = view.clone();
            move |_, _, cx| update(&view, cx, |this, cx| this.save_as(cx))
        }))
        .child(icon_button(skin, icons::RENAME, "重命名", false, editable, {
            let view = view.clone();
            move |_, _, cx| update(&view, cx, |this, cx| this.rename(cx))
        }))
        .child(icon_button(skin, icons::TRASH, "删除", false, editable, {
            let view = view.clone();
            move |_, _, cx| update(&view, cx, |this, cx| this.delete_to_trash(cx))
        }))
        .child(icon_button(skin, icons::INFO, "EXIF", info_open, editable, {
            let view = view.clone();
            move |_, _, cx| update(&view, cx, |this, cx| this.toggle_info_panel(cx))
        }));

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .w_full()
        .h(px(theme::TOOLBAR_HEIGHT))
        .px_3()
        .bg(skin.surface)
        .border_b_1()
        .border_color(hairline(skin))
        .child(left)
        .child(middle)
        .child(right)
}

/// 一个 32px 的方形按钮。
///
/// `active` 表示「当前生效的模式」，用主色底提示；`enabled` 为假时降低不透明度
/// 并且不响应点击 —— 没有打开的图片时，这些按钮点了也没有意义。
fn icon_button<F>(
    skin: &Skin,
    icon: &'static [u8],
    label: &'static str,
    active: bool,
    enabled: bool,
    handler: F,
) -> impl IntoElement
where
    F: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
{
    let background = if !enabled {
        skin.surface
    } else if active {
        skin.primary
    } else {
        skin.surface
    };
    let foreground = if !enabled {
        skin.text_faint
    } else if active {
        skin.text
    } else {
        skin.text_muted
    };

    // 图标以 currentColor 描边，由 text_color 着色，自动跟随三态；
    // 固定 16px 居中，与文字并排。按钮宽度改为自适应（min_w），
    // 因为加上图标后「适应窗口」这类长标签需要更多横向空间。
    let mut button = div()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(4.0))
        .min_w(px(theme::BUTTON_SIZE))
        .h(px(theme::BUTTON_SIZE))
        .px(px(6.0))
        .rounded_md()
        .bg(background)
        .text_color(foreground)
        .text_size(px(11.0))
        .child(svg().data(icon).size(px(16.0)).text_color(foreground))
        .child(label);

    if enabled {
        // 悬停高亮：只改底色，不动布局，避免鼠标划过时按钮"跳"一下。
        button = button
            .cursor_pointer()
            .hover(move |style| style.bg(if active { skin.primary_hover } else { skin.surface_hover }))
            .on_mouse_down(MouseButton::Left, handler);
    }

    button
}

/// 底部状态栏。
///
/// 单行小字：左侧是尺寸、格式、文件大小与当前缩放，右侧是操作提示与加载耗时。
/// 这一行的作用是「随时能确认自己在看什么」，所以信息要全，但绝不能抢眼。
///
/// 提示语只留三条最常用的：状态栏是**唯一**常驻的说明位，而快捷键一多就没人看了
/// （完整清单在「视图」菜单的快捷键提示里，那里按需展开）。
/// 底部状态栏。
///
/// `page` 是页码 `(当前, 总数)`；不分页的格式传 `None`，那一段整段不画。
/// 它由调用方给出，而不是这里从文档里取：多页文档里「请求的页」与「已画出的页」
/// 会在补页的几十毫秒里不一致，而状态栏要显示的是**请求的**那一页 ——
/// 否则用户按下翻页键，状态栏毫无反应，看起来像没生效。
pub fn status_bar(
    skin: &Skin,
    document: Option<&ImageDocument>,
    page: Option<(usize, usize)>,
    zoom_percent: f32,
) -> impl IntoElement {
    let (size_text, format_text, bytes_text) = match document {
        Some(document) => {
            let size = document.logical_size();
            (
                size_text(size.width as u32, size.height as u32),
                document.format_label().to_string(),
                bytes_text(document.byte_len()),
            )
        }
        None => ("—".to_string(), "—".to_string(), "—".to_string()),
    };

    let timing = document
        .map(|document| duration_text(document.decode_ms()))
        .unwrap_or_default();

    // 页码紧跟格式名：它回答的是「这份文件里我现在在哪」，
    // 与格式、尺寸、大小同属一组「这张图是什么样」的信息。
    let mut leading = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .child(size_text)
        .child(format_text);
    if let Some((current, total)) = page {
        leading = leading.child(format!("第 {current} / {total} 页"));
    }
    let leading = leading.child(bytes_text).child(zoom_text(zoom_percent));

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(theme::STATUS_BAR_HEIGHT))
        .px_3()
        .bg(skin.surface)
        .border_t_1()
        .border_color(hairline(skin))
        .text_size(px(11.0))
        .text_color(skin.text_faint)
        .child(leading)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(HINT)
                .child(timing),
        )
}

/// 状态栏右侧的常驻提示。
///
/// 只写滚轮与双击 —— 这两个是「不看说明根本猜不到」的操作，
/// 其余操作在工具栏上都有可见的按钮。
const HINT: &str = "滚轮缩放 · 拖动平移 · 双击切换适应/1:1";

/// 右侧可折叠的 EXIF 信息面板。
///
/// 分四组展示。分组而不是平铺一串键值对，是因为用户找的是
/// 「这张照片在哪拍的、用什么拍的」，而不是某个具体的 EXIF 标签号。
pub fn info_panel(skin: &Skin, document: Option<&ImageDocument>) -> impl IntoElement {
    let mut content = div().flex().flex_col().gap_4().p_4();

    match document {
        None => {
            content = content.child(section_title(skin, "没有可显示的信息"));
        }
        Some(document) => {
            let size = document.logical_size();
            content = content.child(section(
                skin,
                "文件",
                vec![
                    ("文件名", document.file_name()),
                    ("格式", document.format_label().to_string()),
                    (
                        "尺寸",
                        format!("{} 像素", size_text(size.width as u32, size.height as u32)),
                    ),
                    ("大小", bytes_text(document.byte_len())),
                ],
            ));

            let exif = document.exif();
            content = content.child(section(
                skin,
                "相机与镜头",
                vec![
                    (
                        "相机",
                        exif.and_then(|exif| exif.camera()).unwrap_or_else(no_data),
                    ),
                    (
                        "镜头",
                        exif.and_then(|exif| exif.lens.clone()).unwrap_or_else(no_data),
                    ),
                    (
                        "软件",
                        exif.and_then(|exif| exif.software.clone())
                            .unwrap_or_else(no_data),
                    ),
                ],
            ));

            content = content.child(section(
                skin,
                "曝光参数",
                vec![
                    (
                        "快门",
                        exif.and_then(|exif| exif.exposure_time.clone())
                            .unwrap_or_else(no_data),
                    ),
                    (
                        "光圈",
                        exif.and_then(|exif| exif.f_number.clone())
                            .unwrap_or_else(no_data),
                    ),
                    (
                        "ISO",
                        exif.and_then(|exif| exif.iso.clone()).unwrap_or_else(no_data),
                    ),
                    (
                        "焦距",
                        exif.and_then(|exif| exif.focal_length.clone())
                            .unwrap_or_else(no_data),
                    ),
                ],
            ));

            content = content.child(section(
                skin,
                "时间与方向",
                vec![
                    (
                        "拍摄时间",
                        exif.and_then(|exif| exif.date_taken.clone())
                            .unwrap_or_else(no_data),
                    ),
                    (
                        "EXIF 方向",
                        exif.and_then(|exif| exif.orientation_raw)
                            .map(|raw| format!("{raw}（{}）", document.exif_orientation().label()))
                            .unwrap_or_else(no_data),
                    ),
                    (
                        "已应用的旋转",
                        if document.has_user_orientation() {
                            document.user_orientation().label().to_string()
                        } else {
                            "无".to_string()
                        },
                    ),
                ],
            ));
        }
    }

    div()
        .flex()
        .flex_col()
        .w(px(theme::INFO_PANEL_WIDTH))
        .h_full()
        .bg(skin.surface)
        .border_l_1()
        .border_color(hairline(skin))
        // 面板比画布更亮一点点，用半透明感把它与画布区分开，
        // 同时不产生一条生硬的边。
        .child(content)
}

fn section(
    skin: &Skin,
    title: &'static str,
    rows: Vec<(&'static str, String)>,
) -> impl IntoElement {
    let mut list = div().flex().flex_col().gap_2();
    for (key, value) in rows {
        list = list.child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap_2()
                .child(
                    div()
                        .w(px(72.0))
                        .flex_shrink_0()
                        .text_color(skin.text_faint)
                        .text_size(px(11.0))
                        .child(key),
                )
                .child(
                    div()
                        .flex_1()
                        .text_color(skin.text_muted)
                        .text_size(px(11.0))
                        .child(value),
                ),
        );
    }

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(section_title(skin, title))
        .child(list)
}

fn section_title(skin: &Skin, title: &'static str) -> impl IntoElement {
    div()
        .text_color(skin.text)
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .child(title)
}

fn no_data() -> String {
    "—".to_string()
}

/// 通过实体句柄修改视图状态。
///
/// 每个交互都要写一遍「升级句柄 → update → notify」，抽出来既少噪音，
/// 也保证了 notify 不会被漏掉 —— 漏掉的表现是「点了按钮没反应」，
/// 而这类问题在代码评审里几乎看不出来。
fn update<F>(view: &Entity<ImageViewerView>, cx: &mut App, action: F)
where
    F: FnOnce(&mut ImageViewerView, &mut Context<ImageViewerView>),
{
    view.update(cx, |this, cx| {
        action(this, cx);
        cx.notify();
    });
}
