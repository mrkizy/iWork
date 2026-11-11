//! 开发构建的组装件：真进程测试经环境变量把它们装进后台（只在 `dev` feature 下编译；发布构建里不存在）。
//!
//! - `IWORK_DEV_WORK=turns=N[,cards=M][,uncleared]`：[`DevWork`]，用 T05 的 `SupervisedProcess` 起 N 个真进程组
//!   （每组一个 `sh` 带一个后台 `sleep`），停机时按 `RunningWork` 的约定封口、清空并核对。`uncleared` 让第 3 步
//!   多报一个没清空的会话。每起一个组往 stderr 写一行 `IWORK-DEV-PGID <pgid>`。
//! - `IWORK_DEV_OUTBOX_RECEIVER=<文件>`：登记两个假去向 `dev-redeliver:`（`Redeliver`）与 `dev-fail:`（`Fail`），
//!   投递就是往这个文件追加一行 `<outbox id>`（接收方按 id 去重，由测试做）。
//! - `IWORK_DEV_STOP_ON=<文件>:<原因>`：就绪后等这个文件出现，删掉它并按原因请求停机
//!   （`user-quit` / `signal` / `upgrade` / `fatal`）。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use iwork_agent_acp::RuntimeLaunch;
use iwork_agent_acp::process::SupervisedProcess;
use iwork_ipc::rendezvous::StopReason;
use iwork_ports::store::OutboxItem;

use crate::stop::StopSwitch;
use crate::tasks::outbox::{DispatchFuture, OnUncertain, OutboxDispatch, OutboxRoute};
use crate::work::{AbortReport, RunningCount, RunningWork};

pub const WORK_ENV: &str = "IWORK_DEV_WORK";
pub const RECEIVER_ENV: &str = "IWORK_DEV_OUTBOX_RECEIVER";
pub const STOP_ON_ENV: &str = "IWORK_DEV_STOP_ON";

/// 假的在跑的东西。
pub struct DevWork {
    procs: Mutex<Vec<(SupervisedProcess, iwork_agent_acp::process::Pipes)>>,
    /// 进程收到 stdin 结束后再过 2s 才退出（模拟桥接收尾，`old_tree_may_outlive_restart` 用）
    eof_delay: bool,
    turns: AtomicU32,
    cards: AtomicU32,
    uncleared: bool,
    sealed: AtomicBool,
}

impl DevWork {
    /// 按环境变量起；没设返回 `None`。必须在 tokio 运行时里调。
    pub fn from_env() -> Option<Self> {
        let spec = std::env::var(WORK_ENV).ok()?;
        let mut turns = 0;
        let mut cards = 0;
        let mut uncleared = false;
        let mut eof_delay = false;
        for part in spec.split(',') {
            match part.split_once('=') {
                Some(("turns", n)) => turns = n.parse().expect("IWORK_DEV_WORK turns"),
                Some(("cards", n)) => cards = n.parse().expect("IWORK_DEV_WORK cards"),
                None if part == "uncleared" => uncleared = true,
                None if part == "eof-delay" => eof_delay = true,
                _ => panic!("IWORK_DEV_WORK 不认识：{part}"),
            }
        }
        let work = Self {
            procs: Mutex::new(Vec::new()),
            eof_delay,
            turns: AtomicU32::new(turns),
            cards: AtomicU32::new(cards),
            uncleared,
            sealed: AtomicBool::new(false),
        };
        for _ in 0..turns {
            work.open_session().expect("起假会话");
        }
        Some(work)
    }

    /// 开一个会话（起一个进程组）。封口之后立即失败、不起进程。
    pub fn open_session(&self) -> std::io::Result<u32> {
        if self.sealed.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("已封口：停机中不再开会话"));
        }
        let script = if self.eof_delay {
            "sleep 600 & read _x; sleep 2"
        } else {
            "sleep 600 & exec sleep 600"
        };
        let (p, pipes) = SupervisedProcess::spawn(&RuntimeLaunch {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            env: vec![],
        })?;
        let pgid = p.pid();
        let mut err = std::io::stderr();
        let _ = writeln!(err, "IWORK-DEV-PGID {pgid}");
        let _ = err.flush();
        self.procs.lock().unwrap().push((p, pipes));
        Ok(pgid)
    }
}

impl RunningWork for DevWork {
    fn running(&self) -> RunningCount {
        RunningCount {
            turns: self.turns.load(Ordering::SeqCst),
            cards: self.cards.load(Ordering::SeqCst),
        }
    }

    async fn cancel_all(&self) {
        tracing::info!("dev work：cancel_all");
        let procs: Vec<_> = std::mem::take(&mut *self.procs.lock().unwrap());
        for (p, _) in &procs {
            p.shutdown().await;
        }
        self.turns.store(0, Ordering::SeqCst);
        self.cards.store(0, Ordering::SeqCst);
    }

    async fn abort_all(&self) -> AbortReport {
        // 先封口，再关会话
        self.sealed.store(true, Ordering::SeqCst);
        let procs: Vec<_> = std::mem::take(&mut *self.procs.lock().unwrap());
        let mut not_cleared = 0;
        for (p, _) in &procs {
            let cleared = p.kill_tree_and_wait().await && matches!(p.tree_is_empty(), Ok(true));
            if !cleared {
                not_cleared += 1;
            }
        }
        if self.uncleared {
            not_cleared += 1;
        }
        tracing::info!(sessions = procs.len(), not_cleared, "dev work：abort_all");
        AbortReport {
            sessions: procs.len() as u32,
            not_cleared,
        }
    }
}

struct AppendToFile(PathBuf);

impl OutboxDispatch for AppendToFile {
    fn dispatch<'a>(&'a self, item: &'a OutboxItem) -> DispatchFuture<'a> {
        Box::pin(async move {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.0)
                .map_err(|e| crate::tasks::outbox::DispatchError {
                    retryable: true,
                    message: e.to_string(),
                })?;
            // 一行一次写（O_APPEND）：两条道并发追加时不交错
            f.write_all(format!("{}\n", item.id.0).as_bytes())
                .map_err(|e| crate::tasks::outbox::DispatchError {
                    retryable: true,
                    message: e.to_string(),
                })
        })
    }
}

/// 假去向（没设环境变量时为空）。
pub fn outbox_routes() -> Vec<OutboxRoute> {
    let Ok(file) = std::env::var(RECEIVER_ENV) else {
        return Vec::new();
    };
    let file = PathBuf::from(file);
    vec![
        OutboxRoute::new(
            "dev-redeliver:",
            OnUncertain::Redeliver,
            Box::new(AppendToFile(file.clone())),
        ),
        OutboxRoute::new("dev-fail:", OnUncertain::Fail, Box::new(AppendToFile(file))),
    ]
}

/// 停机触发器的配置。
pub fn stop_trigger() -> Option<(PathBuf, StopReason)> {
    let v = std::env::var(STOP_ON_ENV).ok()?;
    let (file, reason) = v.rsplit_once(':').expect("IWORK_DEV_STOP_ON=<文件>:<原因>");
    let reason = match reason {
        "user-quit" => StopReason::UserQuit,
        "signal" => StopReason::Signal,
        "upgrade" => StopReason::Upgrade,
        "fatal" => StopReason::Fatal,
        other => panic!("IWORK_DEV_STOP_ON 不认识的原因 {other}"),
    };
    Some((PathBuf::from(file), reason))
}

/// 任务体：等文件出现，删掉，请求停机。
pub async fn stop_on(file: PathBuf, reason: StopReason, stop: StopSwitch) {
    loop {
        if std::fs::remove_file(&file).is_ok() {
            tracing::info!(?reason, "开发触发器：请求停机");
            stop.request(reason);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::Gate;
    use crate::status::{StatusBoard, StatusIdentity};
    use crate::stop::{StopHooks, stop};
    use iwork_ipc::rendezvous::{ExeStamp, Phase};
    use iwork_ports::store::StorageError;
    use std::sync::Arc;

    fn group_alive(pgid: u32) -> bool {
        // SAFETY：信号 0 只探测
        unsafe { libc::kill(-(pgid as i32), 0) == 0 }
    }

    fn work(turns: u32) -> DevWork {
        let w = DevWork {
            procs: Mutex::new(Vec::new()),
            eof_delay: false,
            turns: AtomicU32::new(turns),
            cards: AtomicU32::new(0),
            uncleared: false,
            sealed: AtomicBool::new(false),
        };
        for _ in 0..turns {
            w.open_session().unwrap();
        }
        w
    }

    struct NoHooks;
    impl StopHooks for NoHooks {
        async fn stop_tasks(&mut self) {}
        async fn stop_ipc(&mut self) {}
        async fn close_store(&mut self) -> Result<(), StorageError> {
            Ok(())
        }
    }

    fn board(dir: &std::path::Path) -> StatusBoard {
        StatusBoard::create(
            dir,
            StatusIdentity {
                version: "t".into(),
                protocol_major: 1,
                exe_stamp: ExeStamp {
                    len: 0,
                    modified_unix_ns: 0,
                },
                log_dir: dir.into(),
            },
            Phase::Ready,
        )
    }

    /// S20：封口之后开会话立即失败、不起进程；第 3 步之后进程组是空的。
    #[tokio::test(flavor = "multi_thread")]
    async fn abort_all_seals_host() {
        let w = work(2);
        let pgids: Vec<u32> = w
            .procs
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.pid())
            .collect();
        assert!(pgids.iter().all(|g| group_alive(*g)), "对照：进程组起来了");
        let report = w.abort_all().await;
        assert_eq!(
            report,
            AbortReport {
                sessions: 2,
                not_cleared: 0
            }
        );
        assert!(pgids.iter().all(|g| !group_alive(*g)), "进程组都空了");
        let err = w.open_session().unwrap_err();
        assert!(err.to_string().contains("封口"), "{err}");
        assert!(w.procs.lock().unwrap().is_empty(), "没起新进程");
    }

    /// S20：一条停机第 1 步之前进门、拖到第 3 步之后才去开会话的命令 → 被拒（封口兜住了关门等不完的在途命令）。
    #[tokio::test(flavor = "multi_thread")]
    async fn late_command_cannot_start_session_during_stop() {
        let dir = tempfile::tempdir().unwrap();
        let b = board(dir.path());
        let w = Arc::new(work(1));
        let gate = Arc::new(Gate::default());
        let late = {
            let (w, gate) = (w.clone(), gate.clone());
            tokio::spawn(async move {
                let _ticket = gate.enter().expect("停机前进门");
                // 比第 1 步等在途命令的 1s 更久
                tokio::time::sleep(Duration::from_millis(1500)).await;
                w.open_session()
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let phase = stop(
            StopReason::Signal,
            false,
            &b,
            &gate,
            w.as_ref(),
            &mut NoHooks,
        )
        .await;
        assert!(matches!(phase, Phase::Exited { .. }));
        let opened = late.await.unwrap();
        assert!(opened.is_err(), "封口之后开会话应被拒：{opened:?}");
        assert!(w.procs.lock().unwrap().is_empty());
    }
}
