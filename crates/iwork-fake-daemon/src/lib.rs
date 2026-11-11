//! 假后台（T09 方案 2.6）：内存里一条带 seq 的事件日志，经 `iwork-ipc` 的真 `Server` 对外说话。
//!
//! 客户端永远用真 `Client` 连它；网页层的测试与界面审查吃 client-core 对着它跑出来的 `ViewPatch` 录像。
//! 数据形状按 T09 方案第 3 节补完字段后的协议造（见 [`tables`]）。只在开发与测试里用，发布的客户端不依赖本 crate。
//! 信任策略见 `daemon` 模块文档：默认真校验只认同一可执行文件，`dev` feature 下 DevTrust。
//!
//! 结构：[`tables`] 折叠与查询（纯数据）；[`routes`] 事件 → 路由键；[`commands`] 命令受理；[`scenario`] 场景登记表；
//! [`fixtures`] 造事件的小函数。本文件只有把它们接到 ipc 服务端上的外壳。

pub mod commands;
pub mod fixtures;
pub mod routes;
pub mod scenario;
pub mod tables;

#[cfg(unix)]
mod daemon;

#[cfg(unix)]
pub use daemon::{FakeDaemon, self_trusting_identity};
pub use scenario::UnknownScenario;
pub use tables::Tables;

use std::sync::Arc;

use iwork_ipc::server::RoutedEvent;
use iwork_proto::pb;

/// 日志、表、幂等记录：一把锁下的全部可变状态。追加、折叠、发布在同一把锁里做，所以发布顺序就是 seq 顺序。
#[derive(Default)]
pub struct State {
    pub log: Vec<Arc<RoutedEvent>>,
    pub tables: Tables,
    /// command_id → 第一次受理的结果（重发回 replayed）。
    pub accepted: std::collections::HashMap<String, pb::CommandAccepted>,
    /// 生成 id 的计数。
    pub next_id: u64,
}

impl State {
    pub fn head(&self) -> u64 {
        self.log.len() as u64
    }

    /// 生成一个本进程内唯一、可读的 id。
    pub fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-gen-{:06}", self.next_id)
    }
}

/// 信封时间：固定起点加 seq 秒，录像可复现。
pub fn envelope_time(seq: u64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: fixtures::BASE_TIME + seq as i64,
        nanos: 0,
    }
}
