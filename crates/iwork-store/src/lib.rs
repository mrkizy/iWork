//! 基础设施层：SQLite 事件日志与投影、制品存储，实现 `StoragePort`、`ReadModelPort` 与 `ArtifactStore`
//! （技术设计第 5 节，方案 `docs/plans/T02.md`）。
//!
//! 一个 SQLite 库（`iwork.db`，WAL）里有三组表，归属登记在 [`tables`]：
//! - **日志与幂等**（`meta`、`events`、`commands`、`outbox`）：只追加，永不清；
//! - **非投影的持久表**（`message_checkpoints`、`artifact_objects`）：不从事件推导，重建不清；
//! - **投影表**：从事件推导，[`SqliteStore::rebuild_projections`] 清掉重算。
//!
//! 写：单写者线程 + 组提交（[`writer`]）；一次命令 = 追加事件 + 投影 + outbox + 记 `command_id`，
//! 同一事务。读：只读连接池。制品：按内容寻址的文件 + 写线程上登记（[`FsArtifactStore`]）。
//!
//! 再加一种要动几处：
//! - 加一个事件：`projector/mod.rs` 的穷举 `match` + `projector/artifact_refs.rs` 的穷举 `match`
//!   （漏了都编译不过）；带新实体时 `snapshot.rs` 登记一行。
//! - 加一张表：一个新迁移文件 + `migrate.rs` 的 `MIGRATIONS` 一行 + `tables.rs` 登记一行。
//! - 加一个落列的枚举：`sql.rs` 的 `column_text!` 一张表。

mod artifacts;
pub mod data_dir;
mod migrate;
mod projector;
mod read_model;
mod reader;
mod reply;
mod snapshot;
mod sql;
mod store;
pub mod tables;
mod writer;

pub use artifacts::{FsArtifactStore, RecoverReport};
pub use data_dir::{DataDir, DataDirError};
#[doc(hidden)]
pub use migrate::{Migration, OpenHooks};
pub use projector::SqliteProjector;
#[doc(hidden)]
pub use projector::artifact_refs_in;
pub use read_model::{ReadDefaults, SqliteReadModel};
#[doc(hidden)]
pub use store::OpenOptions;
pub use store::{SqliteStore, StoreConfig};
pub use writer::{Envelope, WriteTx};

// 单测与集成测试共用一份夹具；夹具里按外部 crate 的路径 `iwork_store::…` 引用本 crate
#[cfg(test)]
extern crate self as iwork_store;
#[cfg(test)]
#[path = "../tests/support/fixtures.rs"]
mod fixtures;
#[cfg(test)]
mod projection_tests;
#[cfg(test)]
mod tests;
