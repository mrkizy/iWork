//! `iwork-ipc`：客户端与后台服务之间唯一的本机专线（设计第 3 节；T03 方案）。
//!
//! 只搬运，不解释：命令、查询的含义归 `iwork-app`，事件属于谁由上层随事件给出路由键。这里负责
//! - 传输（[`transport`]）：Unix domain socket / 命名管道，不开任何 TCP 端口；
//! - 对端校验（[`peer`]）：两端在发出任何帧之前都判定对端同用户、同签名者或同安装映像；
//! - 帧（[`frame`]）：长度上限、握手前上限、方向；
//! - 服务端（[`server`]）：握手、命令串行、查询分道、每连接一条按 seq 排好的事件流、背压；
//! - 客户端（[`client`]）：建连、握手、唯一的有序出口 `Incoming`；
//! - 会合点（[`rendezvous`]）：socket 目录里给人看的锁文件与启动状态，以及「再拉有没有用」的唯一判据。

pub mod audit;
pub mod client;
pub mod endpoint;
pub mod frame;
pub mod peer;
pub mod rendezvous;
pub mod server;
pub mod transport;

pub use endpoint::{EndpointError, LocalEndpoint};

#[cfg(unix)]
pub use transport::unix::{LocalConnector, LocalListener};
#[cfg(windows)]
pub use transport::windows::{LocalConnector, LocalListener};
