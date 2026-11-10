//! 客户端 Rust 侧（设计 3.6；T09 方案）：连接、界面状态、帧合并；不依赖 Tauri，便于单测。
//!
//! - [`store`]：状态库，唯一写入口 `Store::apply`；
//! - [`derive`]：从状态算视图模型（纯函数）：计数、文案、可否点击都在这里。

pub mod derive;
pub mod store;
