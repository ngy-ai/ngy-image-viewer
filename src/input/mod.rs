//! 输入处理：把鼠标与键盘事件翻译成领域层的操作。
//!
//! # 为什么要有这一层
//!
//! 事件回调本身很短，但里面藏着若干**容易写错、又只能靠手感验证**的换算：
//! 滚轮增量到缩放倍率、拖拽起点到位移增量、双击的判定。
//! 把它们抽成纯函数/纯状态机，就能在没有窗口的情况下把边界情况测清楚。

pub mod handlers;

pub use handlers::{PanGesture, zoom_factor_from_lines, zoom_factor_from_pixels};
