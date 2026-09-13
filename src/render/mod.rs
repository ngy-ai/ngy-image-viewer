//! 渲染适配层：把领域层的纯数据翻译成 GPUI 能画的东西。
//!
//! # 为什么单独一层
//!
//! 这是整个项目里**唯一**同时知道「图像数据长什么样」和「GPUI 怎么画」的地方。
//! 把它和领域层分开的好处很直接：像素格式、通道顺序、纹理上限、坐标系换算
//! 这些容易出错的细节都被关在一个目录里，改渲染框架时只需要重写这一层。
//!
//! # 与领域层的分工
//!
//! - [`crate::model`] 回答「图像多大、转到几度、缩放到几倍」；
//! - [`surface`] 回答「怎么把这些像素变成一张纹理」；
//! - 视图层只负责把两者接起来，并调用 GPUI 的绘制 API。

pub mod surface;
pub mod viewport;

pub use surface::{Surface, SurfaceError};
pub use viewport::{ViewportConfig, ViewportSlot, viewport};
