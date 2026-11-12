//! 任务 `signals`（可重启）：SIGTERM、SIGINT、SIGHUP → 停机「信号」一列（方案 2.8）。
//! SIGTERM 是用户绕开客户端停后台的最后办法（S9），也是系统关机、注销时收到的。

use iwork_ipc::rendezvous::StopReason;
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::stop::StopSwitch;

/// 登记好的三个信号。登记要早于写 `Ready`：之后到的信号一定走停机流程，不会把进程直接结束。
pub struct Signals {
    term: Signal,
    int: Signal,
    hup: Signal,
}

impl Signals {
    pub fn register() -> std::io::Result<Self> {
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    async fn next(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.int.recv() => {}
            _ = self.hup.recv() => {}
        }
    }
}

/// 任务体。`pre` 是启动表第 11 步已经登记好的（第一次运行用它）；被监督器重启时重新登记。
pub async fn run(pre: Option<Signals>, stop: StopSwitch) {
    let mut signals = match pre {
        Some(s) => s,
        None => Signals::register().expect("重新登记信号处理"),
    };
    loop {
        signals.next().await;
        tracing::info!("收到停机信号");
        stop.request(StopReason::Signal);
    }
}
