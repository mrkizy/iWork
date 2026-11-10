//! 客户端 Rust 侧（设计 3.6；T09 方案）：连接、界面状态、帧合并；不依赖 Tauri，便于单测。
//!
//! - [`App`]：给 Tauri 壳的全部接口（attach / next_frame / intent，外加效果通道）；
//! - [`store`]：状态库，唯一写入口 `Store::apply`；
//! - [`derive`]：从状态算视图模型（纯函数）：计数、文案、可否点击都在这里；
//! - [`frame`]：取帧、代次、分块；
//! - [`session`]：会话主循环、重连、在飞查询与命令。

pub mod attach;
pub mod derive;
pub mod frame;
pub mod intent;
pub mod session;
pub mod store;

use iwork_proto::pb;
use tokio::sync::{mpsc, oneshot, watch};

pub use intent::Effect;
pub use session::dial::{DialError, Dialer, DirectDialer};
pub use session::{FrameError, Request, Stats};

/// 会话的句柄。克隆便宜；全部句柄丢掉后会话结束。
#[derive(Clone)]
pub struct App {
    tx: mpsc::UnboundedSender<Request>,
    stats: watch::Receiver<Stats>,
}

/// 启动时的可调项（测试里调小分块上限，逼着每种 patch 都被切）。
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub chunk_max: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            chunk_max: frame::FRAME_CHUNK_MAX,
        }
    }
}

impl App {
    /// 在当前 tokio 运行时上起会话任务。返回句柄与效果通道（通知、打开外链）。
    pub fn start<D: Dialer>(dialer: D, opts: Options) -> (App, mpsc::UnboundedReceiver<Effect>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (effects_tx, effects_rx) = mpsc::unbounded_channel();
        let (stats_tx, stats_rx) = watch::channel(Stats::default());
        let session =
            session::Session::new(dialer, opts.chunk_max, effects_tx).with_stats(stats_tx);
        tokio::spawn(session.run(rx));
        (
            App {
                tx,
                stats: stats_rx,
            },
            effects_rx,
        )
    }

    /// 网页（重新）挂上：换代次，返回新代次。
    pub async fn attach(&self) -> Result<u32, FrameError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Request::Attach(tx))
            .map_err(|_| FrameError::Closed)?;
        rx.await.map_err(|_| FrameError::Closed)
    }

    /// 取一块帧（长等待）。
    pub async fn next_frame(&self, req: pb::NextFrame) -> Result<pb::FrameChunk, FrameError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Request::NextFrame(req, tx))
            .map_err(|_| FrameError::Closed)?;
        rx.await.map_err(|_| FrameError::Closed)?
    }

    /// 网页操作。
    pub async fn intent(&self, intent: pb::UiIntent) -> pb::IntentAck {
        let (tx, rx) = oneshot::channel();
        let closed = || pb::IntentAck {
            error: Some(pb::IntentError {
                code: "closed".into(),
                message: "客户端会话已结束".into(),
                retryable: false,
            }),
        };
        if self.tx.send(Request::Intent(intent, tx)).is_err() {
            return closed();
        }
        rx.await.unwrap_or_else(|_| closed())
    }

    /// 观测值（测试与基准用）。
    pub fn stats(&self) -> watch::Receiver<Stats> {
        self.stats.clone()
    }
}
