//! 画布：把图像按当前视图变换画出来。
//!
//! # 为什么必须自绘
//!
//! GPUI 的 `img()` 元素只会把图片塞进给定的包围盒，既没有「按任意倍率缩放 + 平移」
//! 的表达能力，也没有绘制变换（核实过：`Window::paint_image` 只接受轴对齐矩形）。
//! 看图器的核心交互 —— 以光标为锚点缩放、拖拽平移 —— 因此只能基于 `canvas()`
//! 自己算出目标矩形，再调用 `Window::paint_image`。
//!
//! # 绘制顺序
//!
//! ```text
//! 画布底色（近黑）
//!   → 内容区域底色（浅灰）
//!   → 棋盘格深色格子（仅当图像半透明时）
//!   → 图像本身
//! ```
//!
//! 棋盘格锚在**内容左上角**而不是画布左上角：图像被拖动时格子跟着一起动，
//! 看起来像"托着一张有底的相纸"；锚在画布上则会显得格子浮在屏幕前方，
//! 与图像的位移脱节。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{
    Bounds, Corners, Hsla, IntoElement, Pixels, Styled, Window, canvas, fill, point, px, size,
};

use crate::model::{Rect, Size, ViewTransform, ZoomMode};
use crate::render::Surface;
use crate::trace;

/// 棋盘格的格子边长（逻辑点）。
///
/// 取 32 而不是常见的 8：棋盘格是用**一个个方块**画出来的，
/// 格子越小方块数按平方增长 —— 8 像素在一张全屏的半透明图上会产生
/// 上万个方块，而 32 像素只有几百个，视觉上反而更克制。
const CHECKER_CELL: f32 = 32.0;

/// 画布实际尺寸的共享槽位。
///
/// 画布的真实尺寸只有在 `prepaint` 里才知道，而那是一个 `'static` 闭包，
/// 拿不到 `&mut self`。这个槽位就是尺寸回传视图的唯一通道：
/// 视图下一帧读到它，才能用正确的视口尺寸重算「适应窗口」。
///
/// 用 `Rc<Cell<_>>` 而不是 `Arc<Mutex<_>>`：整条路径都在 UI 线程上，
/// 加锁只会把一个简单的赋值变成可能阻塞的操作。
#[derive(Clone)]
pub struct ViewportSlot {
    size: Rc<Cell<Size>>,
}

impl Default for ViewportSlot {
    fn default() -> Self {
        Self {
            size: Rc::new(Cell::new(Size::ZERO)),
        }
    }
}

impl ViewportSlot {
    /// 最近一次绘制时画布的尺寸。首帧渲染之前是零。
    pub fn get(&self) -> Size {
        self.size.get()
    }

    fn set(&self, size: Size) {
        self.size.set(size);
    }
}

/// 画布需要的全部信息。
pub struct ViewportConfig {
    /// 可显示的纹理。加载中或解码失败时为 `None`。
    pub surface: Option<Arc<Surface>>,
    /// 图像的逻辑尺寸。没有图像时传 [`Size::ZERO`]。
    pub logical_size: Size,
    pub transform: ViewTransform,
    pub frame_index: usize,
    pub slot: ViewportSlot,
    /// 画布底色（近黑）。
    pub background: Hsla,
    /// 棋盘格的浅色与深色。
    pub checker: (Hsla, Hsla),
}

/// 构建画布元素。
pub fn viewport(config: ViewportConfig) -> impl IntoElement {
    let ViewportConfig {
        surface,
        logical_size,
        transform,
        frame_index,
        slot,
        background,
        checker,
    } = config;

    let prepaint_slot = slot.clone();

    canvas(
        move |bounds, _window, _cx| {
            // `prepaint` 是唯一能拿到画布真实尺寸的地方，顺手回传给视图。
            let measured = Size::new(to_f32(bounds.size.width), to_f32(bounds.size.height));
            trace_frame("paint.prepaint", || {
                format!(
                    "画布实测尺寸={:.2}×{:.2}（原点 {:.2},{:.2}）{}",
                    measured.width,
                    measured.height,
                    to_f32(bounds.origin.x),
                    to_f32(bounds.origin.y),
                    if measured.is_empty() {
                        " ← 尺寸为空，画布不会画出任何东西"
                    } else {
                        ""
                    },
                )
            });
            prepaint_slot.set(measured);
        },
        move |bounds, _state, window, _cx| {
            let viewport = Size::new(to_f32(bounds.size.width), to_f32(bounds.size.height));

            // 两种模式在这里就地求值：绘制闭包拿不到 `&mut self`，但把纯函数算一遍，
            // 就能让**第一帧**也落在正确位置，不必等视图下一帧把尺寸回传上来。
            //
            // - `Fit`：适应窗口跟着画布尺寸走，每帧按当前画布算一次；
            // - `Pending`：刚打开、初始视图还没定（见 `ZoomMode::Pending`）。画布尺寸
            //   与屏幕缩放只有在这里才同时可得 —— 就地算出与视图层**同一个**结果，
            //   第一帧就是最终画面，不会「先按甲模式画一帧、下一帧跳成乙模式」。
            let effective = match transform.mode() {
                ZoomMode::Fit => ViewTransform::fitted(logical_size, viewport),
                ZoomMode::Pending => ViewTransform::initial(
                    // 与 `ui/view.rs` 的 `pixel_ratio_of` 同义（窗口的 DPR）。
                    f32::from(window.scale_factor()),
                    logical_size,
                    viewport,
                ),
                ZoomMode::Actual | ZoomMode::Free => transform,
            };

            window.paint_quad(fill(bounds, background));

            // ---- 以下每一幕都必须能对上，否则结果就是「一片黑」----
            //
            // 这一段是排查黑屏的主战场：GPUI 的 `paint_image` 在「可见区域为空」时
            // 会直接返回 `Ok(())`，也就是「调用成功，但什么都没画」。这种失败
            // 没有任何报错渠道，只能由我们在这里自己判定并打出来。
            //
            // 判据要区分「正常」与「异常」：启动中 / 正在打开时没有纹理是**正常**的
            // （占位层会显示进度），此时逻辑尺寸也为零。真正的异常是
            // 「已经有了图像尺寸，却没有纹理可画」—— 那才会安静地黑屏。
            if logical_size.is_empty() && surface.is_none() {
                trace::step_once(
                    "paint.loading",
                    "paint",
                    "画布尚无图像（正在打开），本轮只绘制底色，占位层负责提示",
                );
                return;
            }
            let Some(surface) = surface.as_ref() else {
                trace::fail(
                    "paint",
                    format!(
                        "有图像尺寸（{:.2}×{:.2}）却没有纹理：界面只会显示画布底色（近黑），这就是黑屏",
                        logical_size.width, logical_size.height
                    ),
                );
                return;
            };
            if logical_size.is_empty() {
                trace::fail(
                    "paint",
                    "纹理存在但逻辑尺寸为空：视图认为「没有图像」，不会绘制内容",
                );
                return;
            }
            if viewport.is_empty() {
                trace::fail(
                    "paint",
                    format!(
                        "画布尺寸为空（{:.2}×{:.2}）：内容无处安放，只会画出底色",
                        viewport.width, viewport.height
                    ),
                );
                return;
            }

            let content = effective.content_rect(logical_size, viewport);

            trace_frame("paint.decide", || {
                format!(
                    "绘制决策：模式={:?} 倍率={:.4} 平移=({:.2},{:.2}) 逻辑尺寸={:.2}×{:.2} \
                     画布={:.2}×{:.2} 内容矩形=({:.2},{:.2}) {:.2}×{:.2} 帧号={frame_index} 帧数={} 半透明={}",
                    effective.mode(),
                    effective.scale(),
                    effective.pan().x,
                    effective.pan().y,
                    logical_size.width,
                    logical_size.height,
                    viewport.width,
                    viewport.height,
                    content.origin.x,
                    content.origin.y,
                    content.size.width,
                    content.size.height,
                    surface.frame_count(),
                    surface.has_transparency(),
                )
            });

            if surface.has_transparency() {
                paint_checkerboard(bounds, content, window, checker);
            }

            paint_image(bounds, content, surface, frame_index, window);
        },
    )
    .absolute()
    .inset_0()
}

/// 把图像画到 `content` 描述的目标矩形上。
fn paint_image(
    canvas_bounds: Bounds<Pixels>,
    content: Rect,
    surface: &Surface,
    frame_index: usize,
    window: &mut Window,
) {
    // `paint_image` 内部会断言帧号有效（它会直接索引帧数组），
    // 所以动画计时器算出的帧号必须先夹一次 —— 否则一张刚被换掉的动图
    // 会让整个界面 panic，而这只是"晚了一帧"而已。
    let frame_count = surface.frame_count();
    let frame_index = frame_index.min(frame_count.saturating_sub(1));

    let destination = Bounds::new(
        point(
            canvas_bounds.origin.x + px(content.origin.x),
            canvas_bounds.origin.y + px(content.origin.y),
        ),
        size(
            px(content.size.width.max(1.0)),
            px(content.size.height.max(1.0)),
        ),
    );

    // 先把「到底有没有画出东西」自己算一遍，再决定要不要调用 API。
    //
    // 这里不能用 `paint_image` 的返回值来判定：它在可见区域为空时返回的是
    // `Ok(())` —— 也就是说，纹理画不出来和画得很成功，返回值完全一样。
    // 黑屏最难查的地方正在这里：**调用成功，画面上什么都没有**。
    let visible = destination.intersect(&canvas_bounds);
    if to_f32(visible.size.width) <= 0.0 || to_f32(visible.size.height) <= 0.0 {
        let message = format!(
            "图像完全落在画布之外，没有任何像素会被绘制。\
             目标矩形 origin=({:.2},{:.2}) size={:.2}×{:.2}；画布 origin=({:.2},{:.2}) size={:.2}×{:.2}",
            to_f32(destination.origin.x),
            to_f32(destination.origin.y),
            to_f32(destination.size.width),
            to_f32(destination.size.height),
            to_f32(canvas_bounds.origin.x),
            to_f32(canvas_bounds.origin.y),
            to_f32(canvas_bounds.size.width),
            to_f32(canvas_bounds.size.height),
        );
        trace::fail("paint", message);
        return;
    }

    trace_frame("paint.call", || {
        format!(
            "paint_image：目标矩形 origin=({:.2},{:.2}) size={:.2}×{:.2} 可见区域={:.2}×{:.2} \
             帧号={frame_index}/{frame_count} 纹理尺寸={}×{} 纹理字节={}",
            to_f32(destination.origin.x),
            to_f32(destination.origin.y),
            to_f32(destination.size.width),
            to_f32(destination.size.height),
            to_f32(visible.size.width),
            to_f32(visible.size.height),
            surface.texture_size().0,
            surface.texture_size().1,
            surface.image().as_bytes(0).map(|bytes| bytes.len()).unwrap_or(0),
        )
    });

    // `bounds` 与 `image_bounds` 传同一个矩形：整张纹理一次画完。
    // 超出画布的部分由 GPUI 的内容遮罩裁掉，不需要我们再算一次交集 ——
    // 传一个"已经裁剪过"的矩形反而会让纹理坐标映射出错。
    let result = window.paint_image(
        destination,
        destination,
        Corners::default(),
        surface.image().clone(),
        frame_index,
        false,
    );

    match result {
        Ok(()) => trace_frame("paint.ok", || {
            format!(
                "paint_image 返回成功（纹理 #{}，{}×{} 的 {} 帧）",
                surface.image().id.0,
                surface.texture_size().0,
                surface.texture_size().1,
                frame_count,
            )
        }),
        Err(error) => trace::fail(
            "paint",
            format!(
                "paint_image 返回错误：{error:#}。\
                 常见原因是纹理超出 GPU 图集容量（当前纹理 {}×{}）。\
                 这条以前只在 NGY_PERF=1 时才输出，因此表现为「黑屏但控制台寂静」。",
                surface.texture_size().0,
                surface.texture_size().1,
            ),
        ),
    }
}

/// 在内容区域内铺一层低对比棋盘格。
fn paint_checkerboard(
    canvas_bounds: Bounds<Pixels>,
    content: Rect,
    window: &mut Window,
    (light, dark): (Hsla, Hsla),
) {
    // 只在「内容 ∩ 画布」范围内画。图像之外的空白处出现棋盘格会让人
    // 误以为那张透明图比实际更大。
    let left = content.origin.x.max(0.0);
    let top = content.origin.y.max(0.0);
    let right = content.max().x.min(to_f32(canvas_bounds.size.width));
    let bottom = content.max().y.min(to_f32(canvas_bounds.size.height));
    if right <= left || bottom <= top {
        return;
    }

    // 先铺满浅色底子，再按棋盘格补深色方块 ——
    // 一半的方块数，同样的观感。
    window.paint_quad(fill(
        Bounds::new(
            point(
                canvas_bounds.origin.x + px(left),
                canvas_bounds.origin.y + px(top),
            ),
            size(px(right - left), px(bottom - top)),
        ),
        light,
    ));

    // 格子原点锚在内容左上角：拖动图像时格子随之移动。
    let first_column = ((left - content.origin.x) / CHECKER_CELL).floor() as i64;
    let last_column = ((right - content.origin.x) / CHECKER_CELL).ceil() as i64;
    let first_row = ((top - content.origin.y) / CHECKER_CELL).floor() as i64;
    let last_row = ((bottom - content.origin.y) / CHECKER_CELL).ceil() as i64;

    for row in first_row..last_row {
        for column in first_column..last_column {
            if (row + column) % 2 == 0 {
                continue;
            }

            let cell_x = content.origin.x + column as f32 * CHECKER_CELL;
            let cell_y = content.origin.y + row as f32 * CHECKER_CELL;

            // 与可见区域求交，避免为画布外的格子生成绘制指令。
            let x0 = cell_x.max(left);
            let y0 = cell_y.max(top);
            let x1 = (cell_x + CHECKER_CELL).min(right);
            let y1 = (cell_y + CHECKER_CELL).min(bottom);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }

            window.paint_quad(fill(
                Bounds::new(
                    point(
                        canvas_bounds.origin.x + px(x0),
                        canvas_bounds.origin.y + px(y0),
                    ),
                    size(px(x1 - x0), px(y1 - y0)),
                ),
                dark,
            ));
        }
    }
}

/// `Pixels` → `f32`。
///
/// GPUI 刻意不公开 `Pixels` 的字段，强制走 `From` 转换 —— 这样单位换算
/// 在类型层面是显式的，不会出现「把设备像素当逻辑点用」这种静默错误。
#[inline]
fn to_f32(pixels: Pixels) -> f32 {
    f32::from(pixels)
}

/// 逐帧路径上的阶段日志。
///
/// 传的是**闭包**而不是拼好的字符串：`format!` 会在实参位置被无条件求值，
/// 于是日志关闭时每个绘制帧仍要白白拼几段字符串并丢掉 —— 绘制路径是逐帧跑的，
/// 这点开销会直接吃掉「跟手」这条产品要求。包成闭包后，关闭时连字符串都不存在。
fn trace_frame(key: &str, message: impl FnOnce() -> String) {
    if !trace::enabled() {
        return;
    }
    trace::step_once(key, "paint", message());
}
