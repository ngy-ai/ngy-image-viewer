//! 标题栏、菜单栏与下拉菜单的绘制。
//!
//! # 一行里塞下三件事
//!
//! 标题栏（窗口身份 + 窗口按钮）、菜单栏（文件 / 编辑 / 视图）与窗口拖拽区共用同一行。
//! 拆成两行会稳定地占掉 34px 以上的纵向空间，而这是一个**看图**的程序 —— 少一行界面
//! 就是多一行图像，这是本文件里唯一一处「为图像牺牲界面惯例」的取舍。
//!
//! # 下拉浮层靠树序，不靠层级
//!
//! GPUI 按树序绘制，后画的孩子盖住先画的：因此下拉面板**必须由根容器的最后一个孩子
//! 来画**（见 [`menu_layer`]），不能挂在菜单标签下面 —— 挂在标签下会先于画布绘制，
//! 整个面板会被画布盖住。
//!
//! 横向对齐则完全不需要像素换算：浮层在自己的行里先放一段**与标题栏左侧相同的占位**
//! （[`leading_placeholders`]），再按顺序放前面几个菜单标签的占位，自然就落在自己
//! 标签的正下方。这样一来「改了标题栏的间距、忘了同步浮层」这类错位缺陷从结构上
//! 就不可能发生。
//!
//! # 窗口按钮的行为不属于我们
//!
//! 最小化 / 最大化 / 关闭只声明自己是哪种 [`WindowControlArea`]，命中的解释权交给平台。
//! 于是「拖拽移动窗口」「双击标题栏最大化」「最大化状态下再点还原」这些行为全部由系统
//! 提供，与原生窗口逐条一致 —— 自己实现一套永远会在某个细节上和系统不一样。

use gpui_kit::*;

use crate::model::ImageDocument;
use crate::ui::command::{self, Command, Entry, MENUS};
use crate::ui::theme;
use crate::ui::view::ImageViewerView;

/// 标题栏。
///
/// 从左到右：让位（macOS）+ 应用标记 + 菜单标签 + 可拖拽的标题区 + 窗口按钮。
/// `open` 是当前展开的菜单下标，用来高亮对应标签。
pub fn title_bar(
    document: Option<&ImageDocument>,
    open: Option<usize>,
    view: &Entity<ImageViewerView>,
    window: &Window,
) -> impl IntoElement {
    let mut leading = div()
        .flex()
        .flex_row()
        .items_center()
        .h_full()
        .flex_shrink_0();
    for placeholder in leading_placeholders(true) {
        leading = leading.child(placeholder);
    }
    for (index, menu) in MENUS.iter().enumerate() {
        leading = leading.child(menu_label(menu.label, index, open, view));
    }

    // 标题区：既是「现在在看什么」的显示位，也是窗口的拖拽区。
    //
    // 这里声明成 Drag 而不是自己监听鼠标事件并调 `start_window_move()`：
    // 声明之后系统按 HTCAPTION 处理，拖拽、双击最大化、右键系统菜单一次全部到位。
    // 代价是这一块区域拿不到客户端的鼠标事件 —— 而它本来也不该有别的行为。
    let middle = div()
        .flex_1()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .h_full()
        .overflow_hidden()
        .window_control_area(WindowControlArea::Drag)
        .child(
            div()
                .text_size(px(12.0))
                .text_color(theme::text_muted())
                .child(title_text(document)),
        );

    let mut bar = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(theme::TITLE_BAR_HEIGHT))
        .flex_shrink_0()
        .pl(px(theme::TITLE_BAR_PADDING))
        .bg(theme::surface())
        .border_b_1()
        .border_color(theme::border())
        .child(leading)
        .child(middle);

    if draws_window_buttons() {
        bar = bar.child(window_buttons(window));
    } else {
        // 不减掉右侧内边距会与 macOS 的红绿灯 / 窗口管理器绘制的按钮贴在一起。
        bar = bar.pr(px(theme::TITLE_BAR_PADDING));
    }

    bar
}

/// 下拉菜单浮层。由根容器的**最后一个孩子**绘制（原因见模块文档）。
///
/// 没有展开的菜单时返回一个空元素：浮层若常驻，它会持续占住命中区，
/// 把画布与标题栏的鼠标事件全部吃掉。
pub fn menu_layer(
    open: Option<usize>,
    has_image: bool,
    view: &Entity<ImageViewerView>,
) -> AnyElement {
    let Some(index) = open else {
        return div().into_any_element();
    };
    let Some(menu) = MENUS.get(index) else {
        return div().into_any_element();
    };

    // 与标题栏等宽的占位：让位（macOS）+ 应用标记（这里只要宽度）+ 前面的菜单标签。
    let mut leading = div().flex().flex_row().items_center().flex_shrink_0();
    for placeholder in leading_placeholders(false) {
        leading = leading.child(placeholder);
    }
    for _ in 0..index {
        leading = leading.child(
            div()
                .w(px(theme::MENU_LABEL_WIDTH))
                .flex_shrink_0(),
        );
    }

    let panel = div()
        .flex()
        .flex_col()
        .w(px(theme::MENU_PANEL_WIDTH))
        .py(px(4.0))
        .rounded_md()
        .bg(theme::surface_active())
        .border_1()
        .border_color(theme::border())
        // 面板压在画布上，而画布可能是任何颜色。加一层投影把它与图像分开，
        // 否则在浅色照片上这条边界会糊掉（近黑的描边在深色照片上又几乎看不见）。
        .shadow_lg();
    let mut panel = panel;
    for entry in menu.entries {
        panel = panel.child(match entry {
            Entry::Separator => div()
                .h(px(1.0))
                .my(px(4.0))
                .mx_2()
                .bg(theme::border())
                .into_any_element(),
            Entry::Item(command) => menu_item(*command, has_image, view),
        });
    }

    div()
        .absolute()
        .top(px(theme::TITLE_BAR_HEIGHT))
        .left_0()
        .right_0()
        .bottom_0()
        .flex()
        .flex_row()
        .items_start()
        .pl(px(theme::TITLE_BAR_PADDING))
        // 点空白处关闭。这一层**只从标题栏下沿开始**：若连标题栏一起盖住，
        // 「菜单展开后横向划过另一个菜单即切换」这条习惯动作就会被它吃掉。
        .on_mouse_down(MouseButton::Left, {
            let view = view.clone();
            move |_event, _window, cx| {
                view.update(cx, |this, cx| this.close_menu(cx));
            }
        })
        .child(leading)
        .child(panel)
        .into_any_element()
}

/// 一个菜单项。
fn menu_item(command: Command, has_image: bool, view: &Entity<ImageViewerView>) -> AnyElement {
    let enabled = !command.needs_image() || has_image;

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .h(px(theme::MENU_ITEM_HEIGHT))
        .px_3()
        .text_size(px(12.0))
        .text_color(if enabled {
            theme::text()
        } else {
            theme::text_faint()
        })
        .child(div().flex_shrink_0().child(command.label()))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(11.0))
                .text_color(theme::text_faint())
                .child(command.shortcut_label()),
        );

    if enabled {
        // 禁用项不挂任何监听：既不会有悬停反馈，也点不动，与它的灰色外观一致。
        row = row
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_mouse_down(MouseButton::Left, {
                let view = view.clone();
                move |_event, window, cx| {
                    // `run_command` 自己会关掉菜单并请求重绘，这里不再重复。
                    view.update(cx, |this, cx| this.run_command(command, window, cx));
                }
            });
    }

    row.into_any_element()
}

/// 标题栏上的一个菜单标签。
///
/// 高度写死成标题栏的高度而不是 `h_full()`：这一层外面套了一层同样没有显式高度的
/// 横向分组，百分比高度在"父级高度由内容决定"的链条上会退化成内容高度，
/// 表现是「当前菜单的背景色只包住两个字」，而不是铺满整条标题栏。
fn menu_label(
    label: &'static str,
    index: usize,
    open: Option<usize>,
    view: &Entity<ImageViewerView>,
) -> AnyElement {
    let active = open == Some(index);

    let mut element = div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(theme::MENU_LABEL_WIDTH))
        .h(px(theme::TITLE_BAR_HEIGHT))
        .flex_shrink_0()
        .text_size(px(12.0))
        .text_color(if active {
            theme::text()
        } else {
            theme::text_muted()
        })
        .child(label)
        .on_mouse_down(MouseButton::Left, {
            let view = view.clone();
            move |_event, _window, cx| {
                view.update(cx, |this, cx| this.toggle_menu(index, cx));
            }
        });

    if active {
        element = element.bg(theme::surface_active());
    } else {
        element = element.hover(|style| style.bg(theme::surface_hover()));
    }

    // 只有已经有菜单展开时才监听悬停：没有展开时划过标签不该有任何副作用，
    // 而每个鼠标移动事件都进一次实体更新是白白的开销。
    if open.is_some() {
        element = element.on_mouse_move({
            let view = view.clone();
            move |_event, _window, cx| {
                view.update(cx, |this, cx| this.hover_menu(index, cx));
            }
        });
    }

    element.into_any_element()
}

/// 标题栏右侧的三个窗口按钮。
fn window_buttons(window: &Window) -> impl IntoElement {
    let maximized = window.is_maximized();

    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(theme::TITLE_BAR_HEIGHT))
        .flex_shrink_0()
        .child(window_button(
            WindowControlArea::Min,
            false,
            div()
                .w(px(9.0))
                .h(px(1.0))
                .bg(theme::text_muted())
                .into_any_element(),
        ))
        .child(window_button(
            WindowControlArea::Max,
            false,
            maximize_glyph(maximized),
        ))
        .child(window_button(
            WindowControlArea::Close,
            true,
            div()
                .text_size(px(13.0))
                .text_color(theme::text_muted())
                .child("×")
                .into_any_element(),
        ))
}

/// 一个窗口按钮。
///
/// 只声明控件区域、不处理点击：命中之后由系统按「最小化 / 最大化 / 关闭」的既定语义
/// 处理（含最大化状态下的还原切换），我们连「当前是不是最大化」都不用自己维护。
/// 无障碍语义同样来自系统 —— 这三个区域在平台上就是真正的窗口控件，
/// 再补一个自绘标签只会和系统提供的那份重复。
fn window_button(area: WindowControlArea, danger: bool, glyph: AnyElement) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(theme::WINDOW_BUTTON_WIDTH))
        .h(px(theme::TITLE_BAR_HEIGHT))
        .flex_shrink_0()
        .window_control_area(area)
        // 关闭按钮悬停时变红是各家系统的既定做法：一次误点会丢掉当前视图状态，
        // 颜色是最省事的警告方式。
        .hover(move |style| {
            style.bg(if danger {
                theme::danger()
            } else {
                theme::surface_hover()
            })
        })
        .child(glyph)
        .into_any_element()
}

/// 最大化 / 还原图标。
///
/// 用两个方框画出来，而不是取 `❐` 这类字形：那些码位并不保证在系统字体里存在，
/// 缺字时用户看到的是一个豆腐块，比图标不精确难看得多。
fn maximize_glyph(maximized: bool) -> AnyElement {
    let stroke = theme::text_muted();

    if maximized {
        div()
            .relative()
            .w(px(11.0))
            .h(px(11.0))
            .child(
                div()
                    .absolute()
                    .left_0()
                    .bottom_0()
                    .w(px(9.0))
                    .h(px(9.0))
                    .border_1()
                    .border_color(stroke),
            )
            .child(
                div()
                    .absolute()
                    .right_0()
                    .top_0()
                    .w(px(9.0))
                    .h(px(9.0))
                    .border_1()
                    .border_color(stroke)
                    // 用标题栏底色盖住下面那个方框被压住的一角，
                    // 于是看上去就是「两个错开的窗口」而不是一个田字。
                    .bg(theme::surface()),
            )
            .into_any_element()
    } else {
        div()
            .w(px(9.0))
            .h(px(9.0))
            .border_1()
            .border_color(stroke)
            .into_any_element()
    }
}

/// 标题栏左侧、菜单标签之前的占位。
///
/// 标题栏用可见版本（`with_app_mark = true`），下拉浮层用等宽的隐形版本。
/// 两处复用同一段构造，是浮层不需要任何像素换算就能对齐自己标签的原因。
fn leading_placeholders(with_app_mark: bool) -> Vec<AnyElement> {
    let mut items = Vec::new();

    if cfg!(target_os = "macos") {
        // 系统的红绿灯按钮画在左上角，藏不掉也移不走，只能让开。
        items.push(
            div()
                .w(px(theme::TRAFFIC_LIGHTS_WIDTH))
                .flex_shrink_0()
                .into_any_element(),
        );
    }

    if with_app_mark {
        items.push(app_mark());
    } else {
        items.push(
            div()
                .w(px(theme::APP_MARK_WIDTH))
                .flex_shrink_0()
                .into_any_element(),
        );
    }

    items
}

/// 应用标记。
///
/// 自绘标题栏之后，系统不再提供「这是哪个程序」的视觉线索（原来那个图标和名字在
/// 系统标题栏上），这里用一个色块把它补回来 —— 窗口被拖到一堆窗口里时，
/// 它是唯一能一眼认出本程序的东西。
fn app_mark() -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(theme::APP_MARK_WIDTH))
        .h(px(theme::TITLE_BAR_HEIGHT))
        .flex_shrink_0()
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(18.0))
                .h(px(18.0))
                .rounded_sm()
                .bg(theme::primary())
                .text_color(theme::text())
                .text_size(px(11.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child("N"),
        )
        .into_any_element()
}

/// 标题栏中间显示的文字。
///
/// 只放文件名，不放完整路径：路径通常很长，居中显示会把中间挤满、
/// 又把两端的菜单与窗口按钮顶开；文件名才是"我在看哪张图"的答案。
/// 完整路径在 EXIF 面板里可以看到。
///
/// 同一段文字也用作窗口标题（见 `ImageViewerView::sync_window_title`）。
pub fn title_text(document: Option<&ImageDocument>) -> String {
    match document {
        Some(document) => format!("{} — {}", document.file_name(), command::APP_NAME),
        None => command::APP_NAME.to_string(),
    }
}

/// 是否需要我们自绘最小化 / 最大化 / 关闭。
///
/// - **Windows**：启动时把系统标题栏设为透明（`TitlebarOptions::appears_transparent`），
///   系统就不再画这三个按钮，必须自己画，并把它们声明成对应的控件区域让系统接管行为；
/// - **macOS**：红绿灯按钮由系统绘制且无法隐藏，再画一份就是重复；
/// - **Linux**：装饰默认由窗口管理器绘制（是否走客户端装饰取决于具体 WM），同样不画。
///
/// 于是非 Windows 平台上，这一行退化成一条纯菜单栏 —— 仍然有用，
/// 而且不会出现两套最小化按钮这种一眼可见的错。
fn draws_window_buttons() -> bool {
    cfg!(windows)
}
