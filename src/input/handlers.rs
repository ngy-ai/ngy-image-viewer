//! 输入的纯逻辑部分：拖拽状态机与滚轮换算。
//!
//! 这里**不引用任何 GPUI 类型**，原因与领域层相同：这些换算是画面的手感来源，
//! 而手感的好坏只能靠精确断言来守住 —— 比如「一档滚轮对应 1.15 倍」，
//! 涨到 1.5 倍就会显得一跳一跳，掉到 1.05 倍又要点很多次。

use crate::model::Vec2;

/// 拖拽平移的状态机。
///
/// 存在的意义是解决「位移增量从哪来」：鼠标移动事件给的是**当前位置**，
/// 而平移需要的是**相对上一次的差量**。如果每帧都拿当前位置去平移，
/// 图像会以鼠标到原点的距离为幅度疯狂飞出屏幕。
///
/// 另外它记录了「上一次位置」而不是「按下时的位置」，这样按下之后
/// 即使鼠标先移开很远、再移回来，图像也只会跟着走相同距离 —— 与直接拖拽一致。
#[derive(Clone, Copy, Debug, Default)]
pub struct PanGesture {
    last_position: Option<Vec2>,
}

impl PanGesture {
    /// 按下时调用，记录起点。
    pub fn begin(&mut self, position: Vec2) {
        self.last_position = if position.is_finite() {
            Some(position)
        } else {
            None
        };
    }

    /// 移动时调用，返回本次应当平移的位移。
    ///
    /// 未处于拖拽状态、或坐标非法时返回 `None`。
    pub fn update(&mut self, position: Vec2) -> Option<Vec2> {
        if !position.is_finite() {
            return None;
        }
        let previous = self.last_position?;
        self.last_position = Some(position);
        Some(position - previous)
    }

    /// 松开时调用。
    pub fn end(&mut self) {
        self.last_position = None;
    }

    pub fn is_active(&self) -> bool {
        self.last_position.is_some()
    }
}

/// 滚轮「行数」增量 → 缩放倍率。
///
/// # 方向
///
/// **正值（滚轮向下）表示缩小，负值（滚轮向上）表示放大。**
///
/// 这个方向不是随意的：GPUI 的滚动偏移量沿 y 轴**向下增长**，
/// 所以正的增量代表「往下滚」。往下滚 = 把内容推远 = 缩小 ——
/// 所有看图工具都是这个手感。要改的时候请只改这一处，
/// 并同步改下面的单元测试，否则「滚轮方向反了」这种问题会很难定位。
///
/// # 力度
///
/// 鼠标滚轮一格通常给出 1.0 行（部分系统 3.0）。这里把它折成
/// 「一格约 1.15 倍」：连续滚动时能明显感到变化，又不会一档跨太远。
pub fn zoom_factor_from_lines(lines: f32) -> f32 {
    if !lines.is_finite() || lines == 0.0 {
        return 1.0;
    }
    const PER_LINE: f32 = 0.15;
    // 负号即上面说的方向；夹到 ±1 是为了挡住某些驱动的异常大增量，
    // 免得一次滚动直接跨到缩放极限（那看起来像"卡住了"）。
    let exponent = (-lines * PER_LINE).clamp(-1.0, 1.0);
    exponent.exp()
}

/// 滚轮「像素」增量 → 缩放倍率。
///
/// 触摸板给出的是精确的像素增量，一次滑动就是几百像素。
/// 如果按行数那套换算，轻轻一滑就会缩放到极限 —— 所以这里用
/// 「每 120 像素约 1.15 倍」的力度，与一格滚轮大致相当。
pub fn zoom_factor_from_pixels(pixels: f32) -> f32 {
    if !pixels.is_finite() || pixels == 0.0 {
        return 1.0;
    }
    const PIXELS_PER_LINE: f32 = 120.0;
    zoom_factor_from_lines(pixels / PIXELS_PER_LINE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pan_returns_the_delta_not_the_position() {
        let mut gesture = PanGesture::default();
        gesture.begin(Vec2::new(100.0, 100.0));
        // 从 (100,100) 移到 (140,90)：位移应当是 (+40, -10)，而不是 (140, 90)。
        assert_eq!(gesture.update(Vec2::new(140.0, 90.0)), Some(Vec2::new(40.0, -10.0)));
        assert_eq!(gesture.update(Vec2::new(140.0, 90.0)), Some(Vec2::ZERO));
    }

    #[test]
    fn pan_ignores_moves_before_the_button_is_pressed() {
        let mut gesture = PanGesture::default();
        assert_eq!(gesture.update(Vec2::new(10.0, 10.0)), None);
        assert!(!gesture.is_active());
    }

    #[test]
    fn pan_stops_after_release() {
        let mut gesture = PanGesture::default();
        gesture.begin(Vec2::new(0.0, 0.0));
        assert!(gesture.is_active());
        gesture.end();
        assert!(!gesture.is_active());
        assert_eq!(gesture.update(Vec2::new(50.0, 50.0)), None);
    }

    #[test]
    fn non_finite_positions_are_rejected() {
        // 一个 NaN 的位置只要被写进平移量，之后所有坐标都会变成 NaN，
        // 表现为「图像凭空消失」，且极难定位。入口处直接拒掉。
        let mut gesture = PanGesture::default();
        gesture.begin(Vec2::new(0.0, 0.0));
        assert_eq!(gesture.update(Vec2::new(f32::NAN, 0.0)), None);
        assert_eq!(gesture.update(Vec2::new(0.0, f32::INFINITY)), None);
        // 被拒之后状态没有被污染，后续正常移动仍然工作。
        assert_eq!(gesture.update(Vec2::new(10.0, 10.0)), Some(Vec2::new(10.0, 10.0)));
    }

    #[test]
    fn zoom_direction_matches_the_convention() {
        // 向上滚（负增量）= 放大。
        assert!(zoom_factor_from_lines(-1.0) > 1.0);
        // 向下滚（正增量）= 缩小。
        assert!(zoom_factor_from_lines(1.0) < 1.0);
        assert_eq!(zoom_factor_from_lines(0.0), 1.0);
    }

    #[test]
    fn one_wheel_line_is_a_modest_step() {
        let factor = zoom_factor_from_lines(-1.0);
        assert!(
            (1.10..=1.20).contains(&factor),
            "一档滚轮的倍率应在 1.15 附近，实际 {factor}"
        );
    }

    #[test]
    fn one_trackpad_swipe_is_not_a_jump_to_the_extreme() {
        // 触摸板一次向上滑动可能给出 300 像素。按「每 120 像素一格」换算，
        // 应当是 2.5 格 ≈ 1.45 倍，而不是一步到极限。
        let factor = zoom_factor_from_pixels(-300.0);
        assert!(factor > 1.0 && factor < 1.6, "实际 {factor}");

        // 反向滑动是对称的。
        let inverse = zoom_factor_from_pixels(300.0);
        assert!((factor * inverse - 1.0).abs() < 1e-3, "两个方向应当互逆");
    }

    #[test]
    fn absurd_deltas_cannot_blow_up_the_scale() {
        // 某些驱动会给出极大的增量（甚至有 bug 给出天文数字）。
        // 单次换算必须被夹住，否则会一步跨到缩放上限，看起来像「卡住了」。
        for delta in [1_000.0f32, 1_000_000.0, f32::INFINITY] {
            let factor = zoom_factor_from_lines(delta);
            assert!(factor.is_finite(), "增量 {delta} 产生了非有限倍率");
            assert!(factor <= std::f32::consts::E + 1e-3, "增量 {delta} 的倍率过大：{factor}");
        }
        for delta in [-1_000.0f32, -1_000_000.0, f32::NEG_INFINITY] {
            let factor = zoom_factor_from_lines(delta);
            assert!(factor.is_finite() && factor > 0.0, "增量 {delta} 产生了 {factor}");
        }
    }

    #[test]
    fn zero_and_nan_deltas_are_no_ops() {
        assert_eq!(zoom_factor_from_pixels(0.0), 1.0);
        assert_eq!(zoom_factor_from_pixels(f32::NAN), 1.0);
        assert_eq!(zoom_factor_from_lines(f32::NAN), 1.0);
    }
}
