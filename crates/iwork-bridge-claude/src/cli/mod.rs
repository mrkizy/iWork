//! 和 Claude Code CLI 之间的数据：输入行、输出事件、会话事实、映射、回合结局（T06 方案第 6 节）。全是纯函数；
//! 起进程、读写管道的薄壳（`process.rs`）在 S6。

pub mod errors;
pub mod events;
pub mod facts;
pub mod input;
pub mod map;
