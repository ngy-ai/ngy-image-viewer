//! 领域层：把「解码出来的像素」变成「界面可以直接使用的状态」。
//!
//! # 边界
//!
//! 与解码层一样，这一层**不依赖任何 UI / 渲染框架**：
//!
//! - [`transform`] 是纯粹的缩放平移数学，可以在没有窗口的情况下精确断言；
//! - [`document`] 只描述一张图片的事实（尺寸、方向、元数据），不描述它怎么被画出来。
//!
//! 这条边界让「滚轮缩放能否把光标下的像素钉住」「EXIF 方向与用户旋转如何叠加」
//! 这类最容易出错、又最难用肉眼验证的逻辑，可以在单元测试里被钉死。

pub mod document;
pub mod transform;

pub use document::ImageDocument;
pub use transform::{Rect, Size, Vec2, ViewTransform, ZoomMode};
