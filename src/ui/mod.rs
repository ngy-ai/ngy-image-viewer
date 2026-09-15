//! 界面层：主视图与四个区块。
//!
//! - [`view`] 是主视图，持有文档、视图变换与交互；
//! - [`panels`] 是工具栏、状态栏与信息面板，全部「数据进、元素出」；
//! - [`menu`] 是标题栏、菜单栏与下拉菜单；
//! - [`command`] 是动作清单与快捷键表，**不引用 gpui**，因此可以被单元测试钉死；
//! - [`theme`] 是颜色与尺寸常量。
//!
//! # 与渲染层的分工
//!
//! 这里**不碰像素**：解码、方向、纹理构建都在 [`crate::render`] 里完成，
//! 视图拿到的只是一个可以画的 `Arc<Surface>`。这条边界让「界面长什么样」
//! 与「像素怎么上屏」可以各自演化。

pub mod command;
pub mod format;
pub mod icons;
pub mod menu;
pub mod panels;
pub mod theme;
pub mod view;

pub use view::{ImageViewerView, ToastKind};
