//! 「设置关联格式」浮层：挑出双击时要由本程序打开的图片格式。
//!
//! # 为什么是自绘浮层，而不是系统对话框
//!
//! 系统没有「一次挑一批扩展名」这种对话框 —— 它的「打开方式」是逐扩展名处理的。
//! 走系统那条路，用户要为 58 个格式开 58 次界面，而且中途完全看不出
//! 「现在到底关联了哪些」。一个网格能让全部现状在一屏里说完。
//!
//! # 三档视觉，对应三种真实处境
//!
//! 每个格式是一个小方块，样式由 [`AssocState`] 决定 —— 这是这个界面存在的意义：
//!
//! | 状态 | 样式 | 用户该知道的事 |
//! | --- | --- | --- |
//! | 未关联 | 灰字灰框 | 这个格式与本程序没关系 |
//! | 已登记 | **主色字**、灰框 | 已经写进注册表，但系统当前用别的程序打开它（多半是那个程序占着「默认应用」），要去系统设置里切一下 |
//! | 已关联 | 主色字、**主色框**、深一档的底 | 双击立即生效 |
//!
//! 「已登记」与「已关联」必须长得不一样。把它们画成一样，用户勾完 png 去双击、
//! 结果打开了 WPS，只会认为这个功能是坏的 —— 而真相是他的系统上有另一个程序
//! 持有这个扩展名，程序在法律上就抢不过来（见 [`crate::fs_ops::associations`] 的模块文档）。
//!
//! # 不用实心主色做选中态
//!
//! 两套皮肤的主色（`#4C8DFF` / `#3B7BEA`）都是中深蓝，而浅色皮肤的正文色
//! （`#1B1D22`）压在上面几乎读不清。所以强调一律走「主色边框 + 主色文字」，
//! 与菜单里选中项的写法保持一致 —— 那套语言已经在两套皮肤上验证过可读性。

use gpui_kit::*;

use crate::fs_ops::associations::{AssocState, Association};
use crate::ui::theme::Skin;
use crate::ui::view::ImageViewerView;

/// 一行放几个格式。
///
/// 手工分批而不是让容器自动换行：gpui-pre 这一版没有 `flex_wrap`，
/// 而 58 个格式排成一行会直接冲出窗口。
const COLUMNS: usize = 6;

const CHIP_WIDTH: f32 = 74.0;
const CHIP_HEIGHT: f32 = 28.0;
/// 卡片宽度。六列 × 74 + 五道 4px 间距 + 左右各 20px 内边距 = 504，留一点余量。
const PANEL_WIDTH: f32 = 520.0;

/// 浮层整体。由根容器的**最后一个孩子**绘制（与下拉菜单同一个理由：
/// GPUI 按树序绘制，后画的才盖得住画布）。
pub fn panel(skin: &Skin, entries: &[Association], view: &Entity<ImageViewerView>) -> AnyElement {
    let mut grid = div().flex().flex_col().gap_1();
    for (row_index, chunk) in entries.chunks(COLUMNS).enumerate() {
        let mut row = div().flex().flex_row().gap_1();
        for (offset, association) in chunk.iter().enumerate() {
            // 全局下标：点击时靠它定位要切换哪一项（分批之后行内偏移不够用）。
            row = row.child(chip(
                skin,
                association,
                row_index * COLUMNS + offset,
                view,
            ));
        }
        grid = grid.child(row);
    }

    let card = div()
        .flex()
        .flex_col()
        .gap_3()
        .w(px(PANEL_WIDTH))
        .p_5()
        .rounded_lg()
        .bg(skin.surface)
        .border_1()
        .border_color(skin.border)
        // 卡片压在画布上，而画布可以是任何一张图。不投影的话，
        // 白底照片上的这张卡片会与背景糊在一起。
        .shadow_lg()
        .child(heading(skin))
        .child(grid)
        .child(summary(skin, entries))
        .child(footer(skin, view))
        // 点卡片内部不关面板。不拦住的话，点任何一个格式都会冒泡到遮罩上、
        // 把面板关掉 —— 用户看到的是「点一下就没了」。
        .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
            cx.stop_propagation();
        });

    let scrim = div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .flex()
        .items_center()
        .justify_center()
        // 半透明遮罩：把注意力收到卡片上，同时告诉用户「这层下面是图像，但现在先处理这件事」。
        // 用 `skin.scrim` 而不是画布色 —— 后者与背景同色，混出来等于没有遮罩。
        .bg(skin.scrim)
        .on_mouse_down(MouseButton::Left, {
            let view = view.clone();
            move |_event, window, cx| {
                view.update(cx, |this, cx| this.close_assoc_panel(window, cx));
            }
        })
        .child(card);

    scrim.into_any_element()
}

/// 标题与一句说明。
fn heading(skin: &Skin) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_size(px(13.0))
                .text_color(skin.text)
                .child("设置关联格式"),
        )
        .child(
            div()
                .text_size(px(11.0))
                .text_color(skin.text_muted)
                .child(
                    "勾选后，双击这些格式的图片会用本程序打开；取消勾选即撤销关联。\
                     被别的程序占着的格式，用「设为默认…」到系统设置里一次切换。",
                ),
        )
        .into_any_element()
}

/// 一个格式。
fn chip(
    skin: &Skin,
    association: &Association,
    index: usize,
    view: &Entity<ImageViewerView>,
) -> AnyElement {
    // 「已关联」与「已登记」的区别只在边框与底色上：前者是主色描边 + 深一档的底，
    // 后者是灰描边。文字同为主色，因为两者都是「用户已经勾上」的状态。
    let (border, text) = match association.state {
        AssocState::Default => (skin.primary, skin.primary),
        AssocState::Registered => (skin.border, skin.primary),
        AssocState::None => (skin.border, skin.text_faint),
    };
    let background = match association.state {
        AssocState::Default => skin.surface_active,
        _ => skin.surface_hover,
    };

    let view = view.clone();
    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(CHIP_WIDTH))
        .h(px(CHIP_HEIGHT))
        // 网格是在固定宽度里手工分行的，任何一个块被压扁都会让整行错位，
        // 因此明确禁止收缩。
        .flex_shrink_0()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .bg(background)
        .text_size(px(11.0))
        .text_color(text)
        .cursor_pointer()
        .hover(|style| style.border_color(skin.primary))
        .child(association.dotted())
        .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
            view.update(cx, |this, cx| this.toggle_association(index, cx));
        })
        .into_any_element()
}

/// 底部的一行统计。它承担的是「为什么有些勾了却还是别的程序打开」这个解释。
fn summary(skin: &Skin, entries: &[Association]) -> AnyElement {
    let total = entries.len();
    let linked = entries
        .iter()
        .filter(|entry| entry.state == AssocState::Default)
        .count();
    let registered = entries
        .iter()
        .filter(|entry| entry.state == AssocState::Registered)
        .count();

    let mut text = format!("已关联 {linked} / {total}");
    if registered > 0 {
        text.push_str(&format!(
            "；另有 {registered} 个已登记，但系统当前用别的程序打开 —— 点「设为默认…」到系统设置里一次性切过来"
        ));
    }

    div()
        .text_size(px(11.0))
        .text_color(skin.text_muted)
        .child(text)
        .into_any_element()
}

/// 底部按钮行。
fn footer(skin: &Skin, view: &Entity<ImageViewerView>) -> AnyElement {
    // 「设为默认…」放最左边并强调：它是这个面板的终点 —— 勾选只把本程序登记进候选
    // 列表，真正让它成为双击打开方式的那一步在系统 UI 里，而这个按钮把用户直接
    // 送到那一页（见 `fs_ops::associations::open_defaults_settings`）。
    // 中间那个弹性的空 div 只负责把其余按钮推到右边。
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(button(skin, "设为默认…", true, view, |this, window, cx| {
            this.make_default(window, cx)
        }))
        .child(div().flex_1())
        .child(button(
            skin,
            "全部关联",
            false,
            view,
            |this, _window, cx| this.associate_all(cx),
        ))
        .child(button(
            skin,
            "全部取消",
            false,
            view,
            |this, _window, cx| this.disassociate_all(cx),
        ))
        .child(button(skin, "完成", true, view, |this, window, cx| {
            this.close_assoc_panel(window, cx)
        }))
        .into_any_element()
}

/// 一个按钮。
///
/// `action` 收的是函数指针而不是闭包：三个按钮的动作都是 [`ImageViewerView`]
/// 上的方法，写成函数指针就不必为每个按钮再包一层捕获闭包。
fn button(
    skin: &Skin,
    label: &'static str,
    emphasized: bool,
    view: &Entity<ImageViewerView>,
    action: fn(&mut ImageViewerView, &mut Window, &mut Context<ImageViewerView>),
) -> AnyElement {
    let (border, text) = if emphasized {
        (skin.primary, skin.primary)
    } else {
        (skin.border, skin.text_muted)
    };

    let view = view.clone();
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(26.0))
        .px_3()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .bg(skin.surface_hover)
        .text_size(px(11.0))
        .text_color(text)
        .cursor_pointer()
        .hover(|style| style.border_color(skin.primary))
        .child(label)
        .on_mouse_down(MouseButton::Left, move |_event, window, cx| {
            view.update(cx, |this, cx| action(this, window, cx));
        })
        .into_any_element()
}
