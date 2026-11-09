//! iWork 端口：编排核心与外部世界之间的接缝（方案第 5 节）。
//!
//! 只有 trait、它们的输入输出类型与错误类型，不含任何实现。实现放在各基础设施 crate，
//! daemon 组合时落实；测试用 `tests/fakes.rs` 里的假实现。
//!
//! 异步写法（方案 5.1、ADR-0002）：不依赖任何运行时，也不引宏 crate。异步方法统一写成
//! `fn f(&self, ..) -> impl Future<Output = ..> + Send`（RPITIT），保证实现放进多线程运行时能
//! `spawn`；所有 trait 都是 `Send + Sync`。代价是不能 `dyn`：core/app 对端口类型泛型，确实需要
//! 动态分发时在 daemon 包一层，不改这里。
//!
//! 每个端口一个错误枚举，变体带点名字段；`retryable()` 由错误自己回答，应用服务据此填对外错误的
//! `retryable`。

pub mod agent;
pub mod artifact;
pub mod clock;
pub mod discovery;
pub mod exec;
pub mod fs;
pub mod live;
pub mod notify;
pub mod read_model;
pub mod secret;
pub mod store;
pub mod vcs;
