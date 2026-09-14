//! 文件能力层：把「用户点了一个按钮」翻译成「系统上真实发生了什么事」。
//!
//! # 这一层解决什么问题
//!
//! 复制到剪贴板、另存为、重命名、删除到回收站，这四件事的共同点是：
//! **它们都要碰操作系统，因而都可能失败，而且失败原因对用户是有区别的**
//! （磁盘满、没权限、格式不支持、剪贴板被占用……）。产品要求「绝不静默失败」，
//! 所以这里把每一种失败收敛成一个独立的错误变体，每个变体对应一句能指导
//! 下一步动作的中文提示（见 [`file_ops::FileOpError`]）。
//!
//! # 为什么与 UI 解耦
//!
//! 本模块不引用任何 gpui 类型，只做「路径 / 像素进，结果出」的纯系统调用。
//! 这样它既能脱离窗口系统做单元测试（扩展名推断、alpha 摊平、路径校验都能单测），
//! 也保证了将来更换渲染层时这一层不需要重写。它对应 `lib.rs` 里「无 UI 依赖层」的分类。
//!
//! # 线程边界（重要）
//!
//! 有两处必须留意调用线程：
//!
//! 1. [`file_ops::delete_to_trash`] 在 Windows 上依赖 `trash` crate 的 COM 单元，
//!    必须在**已经初始化 COM 的线程**（UI 主线程）上调用；
//! 2. [`file_ops::pick_save_path`] / [`file_ops::pick_rename_path`] 内部会把阻塞式
//!    文件对话框放到独立线程上执行，并用 channel 把选择结果交回调用方。
//!
//! 具体取舍写在各自的函数文档里。
//!
//! # 编码能力的唯一来源
//!
//! 「哪种扩展名可以另存为」由 [`crate::decode::ImageFormat::can_encode`] 决定，
//! 本层不另立一套判断。这样做的好处是：解码层新增 / 收回某个编码器时，
//! 另存为的可用格式会自动跟着变，不会出现「UI 说能存、编码时才报错」的错配。

pub mod file_ops;
pub mod neighbors;

pub use neighbors::{Neighbors, NeighborsTask};
pub use file_ops::{
    Bitmap, FileOpError, copy_to_clipboard, delete_to_trash, pick_rename_path, pick_save_path,
    rename, save_bitmap, suggest_save_name, writable_format_for,
};
