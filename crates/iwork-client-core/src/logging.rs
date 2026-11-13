//! 桌面客户端的日志：写 `<数据目录>/logs/client.log`，格式、滚动、脱敏和后台的 `daemon.log` 是同一份（`iwork-log`，
//! ADR-0013）。审计 target（`iwork.ipc.audit`）永远不被级别过滤掉：客户端拒绝对端的 `peer_rejected`、启动时的清单
//! 审计都记在这里（T03 方案 2.12「两端都记」）。

pub use iwork_log::{LogGuard, flush_global};

const PROGRAM: &str = "iwork-client";

/// 装客户端的全局日志。要在第一次取身份、拨号之前调：tracing 不缓存也不重放，没有订阅者时发出的审计直接丢。
/// 数据目录定位不了、日志目录写不了时只写 stderr，不挡启动。返回值要留到进程结束。
pub fn init() -> LogGuard {
    let dir = iwork_log::data_dir::locate_root()
        .map(|root| iwork_log::data_dir::logs_dir(&root))
        .map_err(|iwork_log::data_dir::HomeNotFound| {
            format!(
                "找不到用户主目录，定位不了数据目录（可设环境变量 {}）",
                iwork_log::data_dir::DATA_DIR_ENV
            )
        });
    iwork_log::init(
        iwork_log::LogConfig {
            dir,
            file: iwork_log::CLIENT_LOG,
            program: PROGRAM,
            always_on: &[iwork_ipc::audit::TARGET],
        },
        None,
    )
}
