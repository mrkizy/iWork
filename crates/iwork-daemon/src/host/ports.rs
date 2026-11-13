//! 回合宿主用到的端口（T10a 方案 2.5）。宿主与会话线任务只经这里碰存储、引擎、运行时与文件系统；
//! 真装配在 `assembly`，测试换假运行时、假版本控制。

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use iwork_core::approval::engine::{
    ApprovalEngine, Committed, EngineError, Scope, SessionDecider, SessionKeys,
};
use iwork_core::approval::env::Platform;
use iwork_core::turn::{PermissionDecider, ResumeReason};
use iwork_domain::action::ActionTarget;
use iwork_domain::id::{Seq, TurnId};
use iwork_ports::agent::AgentPort;
use iwork_ports::clock::{Clock, IdGenerator, Timer};
use iwork_ports::fs::PathResolver;
use iwork_ports::live::LivePublisher;
use iwork_ports::read_model::ReadModelPort;
use iwork_ports::store::{CommitRequest, StoragePort};

use super::changes::ChangeReader;

/// 续接时告诉模型「批准的是哪一次调用」：工具名与动作摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedCall {
    pub tool: String,
    pub summary: String,
}

impl GrantedCall {
    pub fn reason(&self) -> ResumeReason<'_> {
        ResumeReason::Approved {
            tool: &self.tool,
            summary: &self.summary,
        }
    }
}

/// 宿主要引擎做的几件事。
pub trait HostEngine: Send + Sync + 'static {
    type Decider: PermissionDecider;
    /// 一条会话的判定方，交给 `SessionLine::new`（同一份 `SessionKeys` 给运行时与审批，D8）。
    fn decider_for(&self, keys: SessionKeys) -> Self::Decider;
    /// 追到存储此刻的 head（D7 ① 读 `turns_to_resume` 之前）。
    fn catch_up(&self) -> impl Future<Output = Result<(), EngineError>> + Send;
    /// 中断时批准已落盘、还没交付的回合，连同批准它的那条事件的 seq（宿主只认这次运行里落盘的，D7 ①）。
    fn turns_to_resume(&self) -> BTreeMap<TurnId, Seq>;
    /// 这个回合名下一次性放行的那次调用（续接说明用）；没有为 `None`。
    fn granted_call(&self, turn: TurnId) -> Option<GrantedCall>;
    fn commit_with(
        &self,
        scope: Scope,
        req: CommitRequest,
    ) -> impl Future<Output = Result<Committed, EngineError>> + Send;
    /// 追平并清孤儿（`engine-sync` 任务调，T07 6.10.8）。
    fn sync(&self) -> impl Future<Output = Result<(), EngineError>> + Send;
}

/// 动作给人看的一行：命令是原文，路径是路径。续接说明里放它（原始工具输入不放，T06 方案 5.3）。
fn target_summary(t: &ActionTarget) -> String {
    match t {
        // 动作里的 argv 不一定带程序名（如 `git push` 记成程序 git、argv [push]）
        ActionTarget::Command { argv, program, .. } => {
            if argv.first().is_some_and(|a| a == program) {
                argv.join(" ")
            } else {
                std::iter::once(program.as_str())
                    .chain(argv.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
        ActionTarget::Path(p) => p.0.display().to_string(),
        ActionTarget::Paths(ps) => ps
            .iter()
            .map(|p| p.0.display().to_string())
            .collect::<Vec<_>>()
            .join("、"),
        ActionTarget::ToolName(n) | ActionTarget::Remote(n) => n.clone(),
        ActionTarget::None => String::new(),
    }
}

impl<C, G, S, A, T, R> HostEngine for Arc<ApprovalEngine<C, G, S, A, T, R>>
where
    C: Clock + 'static,
    G: IdGenerator + 'static,
    S: StoragePort + 'static,
    A: AgentPort + 'static,
    T: Timer + 'static,
    R: PathResolver + 'static,
{
    type Decider = SessionDecider<C, G, S, A, T, R>;

    fn decider_for(&self, keys: SessionKeys) -> Self::Decider {
        ApprovalEngine::decider_for(self, keys)
    }

    fn catch_up(&self) -> impl Future<Output = Result<(), EngineError>> + Send {
        ApprovalEngine::catch_up(self)
    }

    fn turns_to_resume(&self) -> BTreeMap<TurnId, Seq> {
        ApprovalEngine::turns_to_resume(self).into_iter().collect()
    }

    fn granted_call(&self, turn: TurnId) -> Option<GrantedCall> {
        let folded = self.folded();
        folded.conversations().find_map(|(_, c)| {
            c.grants.values().find(|g| g.turn == turn).map(|g| {
                let rec = &g.record;
                GrantedCall {
                    tool: rec.tool.0.clone(),
                    summary: rec
                        .actions
                        .get(rec.representative as usize)
                        .map(|a| target_summary(&a.target))
                        .unwrap_or_default(),
                }
            })
        })
    }

    fn commit_with(
        &self,
        scope: Scope,
        req: CommitRequest,
    ) -> impl Future<Output = Result<Committed, EngineError>> + Send {
        ApprovalEngine::commit_with(self, scope, req)
    }

    fn sync(&self) -> impl Future<Output = Result<(), EngineError>> + Send {
        ApprovalEngine::sync(self)
    }
}

/// 算会话路径事实（`EnvPaths`）要的、和对话无关的那几样。
#[derive(Debug, Clone)]
pub struct HostEnv {
    pub platform: Platform,
    pub home: PathBuf,
    /// iWork 数据目录
    pub data_dir: PathBuf,
    /// 系统临时目录
    pub tmp_dirs: Vec<PathBuf>,
}

impl HostEnv {
    pub fn platform() -> Platform {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }
}

/// 宿主的全部端口。存储是带封口闸的那一个（E4）。
pub trait HostPorts: Send + Sync + 'static {
    type Store: StoragePort + 'static;
    type Read: ReadModelPort + 'static;
    type Agent: AgentPort + 'static;
    type Engine: HostEngine;
    type Live: LivePublisher + 'static;
    type Clock: Clock + 'static;
    type Ids: IdGenerator + 'static;
    type Timer: Timer + 'static;
    type Fs: PathResolver + 'static;
    type Changes: ChangeReader;

    fn store(&self) -> &Self::Store;
    fn read(&self) -> &Self::Read;
    fn agent(&self) -> &Self::Agent;
    fn engine(&self) -> &Self::Engine;
    fn live(&self) -> &Self::Live;
    fn clock(&self) -> &Self::Clock;
    fn ids(&self) -> &Self::Ids;
    fn timer(&self) -> &Self::Timer;
    fn fs(&self) -> &Self::Fs;
    fn env(&self) -> &HostEnv;
    /// 改动探针（D10）。7/F-2 修好之前组装处给的是关着的：`capture` 一次版本控制调用都不发
    fn changes(&self) -> &Self::Changes;
    /// 封存储的闸（E4）：`abort_all` 在宿主的锁里置了封口之后调。
    fn seal_store(&self);
}

/// 一组具体的端口（组装处与测试都用它，只是填的东西不同）。
pub struct Wired<St, Rd, Ag, En, Lv, C, I, T, Fs, Ch> {
    pub store: St,
    pub read: Rd,
    pub agent: Ag,
    pub engine: En,
    pub live: Lv,
    pub clock: C,
    pub ids: I,
    pub timer: T,
    pub fs: Fs,
    pub env: HostEnv,
    pub changes: Ch,
    /// 存储的封口闸（和 `store` 是同一个闸）
    pub gate: Arc<crate::gate::SealGate>,
}

impl<St, Rd, Ag, En, Lv, C, I, T, Fs, Ch> HostPorts for Wired<St, Rd, Ag, En, Lv, C, I, T, Fs, Ch>
where
    St: StoragePort + 'static,
    Rd: ReadModelPort + 'static,
    Ag: AgentPort + 'static,
    En: HostEngine,
    Lv: LivePublisher + 'static,
    C: Clock + 'static,
    I: IdGenerator + 'static,
    T: Timer + 'static,
    Fs: PathResolver + 'static,
    Ch: ChangeReader,
{
    type Store = St;
    type Read = Rd;
    type Agent = Ag;
    type Engine = En;
    type Live = Lv;
    type Clock = C;
    type Ids = I;
    type Timer = T;
    type Fs = Fs;
    type Changes = Ch;

    fn store(&self) -> &St {
        &self.store
    }
    fn read(&self) -> &Rd {
        &self.read
    }
    fn agent(&self) -> &Ag {
        &self.agent
    }
    fn engine(&self) -> &En {
        &self.engine
    }
    fn live(&self) -> &Lv {
        &self.live
    }
    fn clock(&self) -> &C {
        &self.clock
    }
    fn ids(&self) -> &I {
        &self.ids
    }
    fn timer(&self) -> &T {
        &self.timer
    }
    fn fs(&self) -> &Fs {
        &self.fs
    }
    fn env(&self) -> &HostEnv {
        &self.env
    }
    fn changes(&self) -> &Ch {
        &self.changes
    }
    fn seal_store(&self) {
        self.gate.seal();
    }
}
