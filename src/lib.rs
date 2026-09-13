//! ngy-image-viewer：极速跨平台图片查看器。
//!
//! # 分层约定（重要）
//!
//! 为了让核心逻辑不被渲染框架绑架，代码被明确分成两层：
//!
//! - **无 UI 依赖层**：`decode`、`model`、`open_job`、`perf`。
//!   只做格式判定、解码、EXIF 处理、视图变换数学与后台任务，不引用任何 gpui 类型。
//!   这层可以脱离窗口系统独立单元测试，也保证将来若需要更换渲染层时无需重写。
//! - **UI / 渲染层**：`app` 及其后续的 `ui` / `render` / `input` 模块。
//!   允许依赖 gpui-kit，但只消费无 UI 层产出的纯数据。
//!
//! 存在 `lib.rs` 而非把所有模块挂在 `main.rs` 下，是为了让 `tests/` 下的集成测试
//! 能够直接访问这些模块。

pub mod app;
pub mod decode;
pub mod fs_ops;
pub mod input;
pub mod model;
pub mod open_job;
pub mod perf;
pub mod render;
pub mod trace;
pub mod ui;
