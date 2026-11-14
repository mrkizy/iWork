//! 集成测试的公共部件：假运行时的启动配置、内存存储、时钟与 ID、增量收集、事件快照。

#![allow(dead_code)] // 各测试文件只用到其中一部分

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use iwork_agent_acp::RuntimeLaunch;
use iwork_domain::common::{Actor, ModelId, ModelSelection, Timestamp, ToolGroup};
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{AssistantId, ConversationId, MessageId, RawUlid, RuntimeId, Seq, TurnId};
use iwork_domain::message::MessageBody;
use iwork_domain::session::{AllowWrite, NetworkPolicy, SessionConstraints, SessionToolSet};
use iwork_domain::turn::{Turn, TurnKind, TurnState};
use iwork_ports::agent::{ContentBlock, SessionSpec};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant, Timer};
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::store::{
    CommitReceipt, CommitRequest, DeliveryState, EventFilter, OutboxId, OutboxItem, RebuildReport,
    StorageError, StoragePort, StoredEvent,
};
use serde_json::Value;

pub const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_iwork-fake-agent");

/// 一份脚本及其记录文件，放在临时目录里，随它一起删掉。
pub struct Fake {
    pub dir: tempfile::TempDir,
    pub launch: RuntimeLaunch,
    pub record: PathBuf,
}

impl Fake {
    pub fn new(script: &Value) -> Self {
        let dir = tempfile::tempdir().expect("建临时目录");
        let script_path = dir.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).expect("写脚本");
        let record = dir.path().join("record.jsonl");
        let launch = RuntimeLaunch {
            program: PathBuf::from(FAKE_AGENT),
            args: vec![
                "--script".into(),
                script_path.into_os_string(),
                "--record".into(),
                record.clone().into_os_string(),
            ],
            env: Vec::new(),
        };
        Self {
            dir,
            launch,
            record,
        }
    }

    /// 记录文件里的全部条目。
    pub fn records(&self) -> Vec<Value> {
        read_records(&self.record)
    }

    /// 客户端发来的东西按到达 stdin 的先后：方法名，或「permission_response <工具调用 id> <选项>」。
    pub fn arrivals(&self) -> Vec<String> {
        self.records()
            .into_iter()
            .filter_map(|r| r["arrived"].as_str().map(str::to_owned))
            .collect()
    }

    /// 记录里某个方法收到的全部参数。
    pub fn params_of(&self, method: &str) -> Vec<Value> {
        self.records()
            .into_iter()
            .filter(|r| r["method"] == method)
            .map(|r| r["params"].clone())
            .collect()
    }
}

/// 记录文件里写完的行。假运行时可能正在写最后一行：结尾没有换行的末行还没写完，跳过，下次再读。
pub fn read_records(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let complete = match text.rfind('\n') {
        Some(end) => &text[..end],
        None => "",
    };
    complete
        .lines()
        .map(|l| serde_json::from_str(l).expect("记录是 JSON"))
        .collect()
}

pub fn spec(cwd: &Path) -> SessionSpec {
    SessionSpec {
        cwd: cwd.to_path_buf(),
        model: ModelSelection {
            model_id: ModelId("fake-model".into()),
            effort: None,
        },
        system_prompt: "你是测试助手".into(),
        mcp: Vec::new(),
        constraints: SessionConstraints {
            deny_read_write: vec![PathBuf::from("/secret")],
            read_only: Vec::new(),
            allow_write: AllowWrite::Whitelist(vec![cwd.to_path_buf()]),
            excluded_commands: Vec::new(),
            allow_unsandboxed_retry: true,
            network: NetworkPolicy::Open,
        },
        tool_set: SessionToolSet {
            enabled_groups: BTreeSet::from([ToolGroup::ReadAndSearch, ToolGroup::RunCommands]),
            include_always_on: true,
        },
        disabled_tools: Vec::new(),
        plugin_dirs: Vec::new(),
        resume: None,
        kind: TurnKind::Assistant,
    }
}

pub fn text(s: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(s.into())]
}

// ---------- 时钟与 ID ----------

/// 真实单调时钟：检查点间隔按真实时间算。
pub struct RealClock {
    start: Instant,
}

impl Default for RealClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Clock for RealClock {
    fn now(&self) -> Timestamp {
        Timestamp(1_700_000_000_000 + self.start.elapsed().as_millis() as i64)
    }
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant(self.start.elapsed().as_nanos() as u64)
    }
}

/// tokio 定时器。
#[derive(Default)]
pub struct TokioTimer;

impl Timer for TokioTimer {
    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// 递增 ID：测试里只需要唯一。
#[derive(Default)]
pub struct CountingIds(AtomicU64);

impl IdGenerator for CountingIds {
    fn next(&self) -> RawUlid {
        RawUlid::from_u128(u128::from(self.0.fetch_add(1, Ordering::SeqCst)) + 1_000)
    }
}

pub fn queued_turn(ids: &CountingIds) -> Turn {
    Turn {
        id: TurnId::from_raw(ids.next()),
        conversation_id: ConversationId::from_raw(ids.next()),
        assistant_id: AssistantId::from_raw(ids.next()),
        assistant_version: 1,
        runtime_id: RuntimeId::from_raw(ids.next()),
        model: ModelSelection {
            model_id: ModelId("fake-model".into()),
            effort: None,
        },
        runtime_session_id: None,
        kind: TurnKind::Assistant,
        state: TurnState::Queued,
        started_at: Timestamp(1),
        ended_at: None,
    }
}

// ---------- 存储与推送 ----------

/// 内存存储：只记提交的事件与检查点。
#[derive(Default)]
pub struct MemStore {
    events: Mutex<Vec<DomainEvent>>,
    checkpoints: Mutex<Vec<(MessageId, u64, String)>>,
}

impl MemStore {
    pub fn events(&self) -> Vec<DomainEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn checkpoints(&self) -> Vec<(MessageId, u64, String)> {
        self.checkpoints.lock().unwrap().clone()
    }

    /// 事件的快照：每条一行，去掉 ID 与时间，只留判断对错要看的内容。
    pub fn snapshot(&self) -> Vec<String> {
        self.events().iter().map(describe).collect()
    }

    /// 最后一次回合状态变化后的状态。
    pub fn last_turn_state(&self) -> Option<TurnState> {
        self.events().iter().rev().find_map(|e| match e {
            DomainEvent::TurnStateChanged(c) => Some(c.to),
            _ => None,
        })
    }
}

pub fn describe(e: &DomainEvent) -> String {
    match e {
        DomainEvent::TurnStateChanged(c) => match &c.reason {
            Some(r) => format!("turn {:?} -> {:?} ({r})", c.from, c.to),
            None => format!("turn {:?} -> {:?}", c.from, c.to),
        },
        DomainEvent::MessageAppended(m) => format!("message appended {:?}", m.message.status),
        DomainEvent::MessageCompleted(m) => match &m.body {
            MessageBody::Inline(text) => format!("message {:?} {text:?}", m.status),
            MessageBody::Artifact(_) => format!("message {:?} <artifact>", m.status),
        },
        DomainEvent::ToolCallStarted(t) => {
            format!("tool started {} {}", t.tool_call_id.0, t.tool_name)
        }
        DomainEvent::ToolCallFinished(t) => {
            format!("tool finished {} ok={}", t.tool_call_id.0, t.ok)
        }
        DomainEvent::TurnFinished(t) => format!("turn finished {:?}", t.state),
        DomainEvent::TurnSessionBound(b) => format!("session bound {}", b.runtime_session_id),
        other => other.kind().to_string(),
    }
}

impl StoragePort for MemStore {
    async fn commit(&self, req: CommitRequest) -> Result<CommitReceipt, StorageError> {
        assert_eq!(req.actor, Actor("system".into()));
        // 新构造的事件一律带所属对话（G5，T10a 方案 2.7「怎么守住新构造的一律 Some」）：这组闭环用例经过的编排与
        // 引擎的每个构造点都在这里被查一遍
        for e in &req.events {
            assert_ne!(
                e.carried_conversation(),
                Some(None),
                "{} 没带所属对话",
                e.kind()
            );
        }
        let mut events = self.events.lock().unwrap();
        let first = events.len() as u64 + 1;
        events.extend(req.events);
        Ok(CommitReceipt {
            first_seq: Seq(first),
            last_seq: Seq(events.len() as u64),
            replayed: false,
        })
    }

    async fn command_receipt(
        &self,
        _id: iwork_domain::id::CommandId,
    ) -> Result<Option<iwork_ports::store::CommitReceipt>, iwork_ports::store::StorageError> {
        unimplemented!("这个假存储的使用者不查回执")
    }
    async fn head_seq(&self) -> Result<Seq, StorageError> {
        Ok(Seq(self.events.lock().unwrap().len() as u64))
    }

    async fn read_events(
        &self,
        _filter: EventFilter,
        _from: Seq,
        _limit: usize,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        unimplemented!("回合编排不读事件")
    }

    async fn checkpoint_message(
        &self,
        message_id: MessageId,
        offset: u64,
        text: &str,
    ) -> Result<(), StorageError> {
        self.checkpoints
            .lock()
            .unwrap()
            .push((message_id, offset, text.to_owned()));
        Ok(())
    }

    async fn outbox_pending(&self, _limit: usize) -> Result<Vec<OutboxItem>, StorageError> {
        unimplemented!("回合编排不读 outbox")
    }

    async fn outbox_requeue_dispatched(
        &self,
        _redeliver_prefixes: Vec<String>,
        _fail_prefixes: Vec<String>,
    ) -> Result<iwork_ports::store::RequeueReport, StorageError> {
        unimplemented!("回合编排不读 outbox")
    }
    async fn outbox_pending_where(
        &self,
        _selector: iwork_ports::store::OutboxSelector,
        _limit: usize,
    ) -> Result<Vec<OutboxItem>, StorageError> {
        unimplemented!("回合编排不读 outbox")
    }
    async fn outbox_mark(&self, _id: OutboxId, _state: DeliveryState) -> Result<(), StorageError> {
        unimplemented!("回合编排不读 outbox")
    }

    async fn rebuild_projections(&self) -> Result<RebuildReport, StorageError> {
        unimplemented!("回合编排不重建投影")
    }
}

/// 收集推出去的正文增量。
#[derive(Default)]
pub struct Deltas(Mutex<Vec<MessageDelta>>);

impl Deltas {
    pub fn all(&self) -> Vec<MessageDelta> {
        self.0.lock().unwrap().clone()
    }
}

impl LivePublisher for Deltas {
    fn message_delta(&self, delta: MessageDelta) {
        self.0.lock().unwrap().push(delta);
    }
}

/// 进程还在跑：和清树判空同一条规则（`iwork_agent_acp::process::running`，僵尸算不在）。读不了进程状态就让测试红，
/// 不当成「不在」。
#[cfg(unix)]
pub fn process_exists(pid: u32) -> bool {
    iwork_agent_acp::process::running(pid as i32)
        .unwrap_or_else(|e| panic!("读不了进程 {pid} 的状态：{e}"))
}

#[cfg(windows)]
pub fn process_exists(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("运行 tasklist");
    // 没有匹配时 tasklist 也以 0 退出、打一行提示；非 0 是它自己出了错，不能当成「不在」
    assert!(out.status.success(), "tasklist 失败：{out:?}");
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

/// 有界地跑一条异步用例（Windows CI 上见过一次无限挂起，任务 fix-fake-agent-windows-hang）：用例本体和运行时收尾都在
/// 单独的线程里跑。卡住时让测试失败，并打印卡在哪一步（用例本体，还是运行时收尾：tokio 的运行时被丢掉时会无限等
/// 还在跑的阻塞任务）和 `diag()` 给的现场。
///
/// 总时限是 `limit`：前四分之三给用例本体和运行时收尾，最后四分之一给诊断。诊断也在单独的线程里跑，它自己卡住
/// （比如列进程的命令不返回）时照样按时失败，只说「诊断没完成」（审查 1/F-1）。`diag` 读的东西（比如假运行时的
/// 记录文件）要活到诊断结束：别把它们 move 进用例本体（审查 1/F-2）。
pub fn bounded<F, Fut>(
    limit: std::time::Duration,
    diag: impl FnOnce() -> String + Send + 'static,
    body: F,
) where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    use std::sync::atomic::AtomicU8;
    use std::sync::mpsc::RecvTimeoutError;
    const PHASES: [&str; 3] = [
        "用例本体还没跑完",
        "用例本体跑完了，运行时收尾还没完成",
        "已完成",
    ];
    let started = Instant::now();
    let diag_budget = limit / 4;
    let run_limit = limit - diag_budget;
    let phase = std::sync::Arc::new(AtomicU8::new(0));
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let p = phase.clone();
    let worker = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("建运行时");
        rt.block_on(body());
        p.store(1, Ordering::SeqCst);
        // 不直接丢掉运行时：丢掉会无限等卡住的阻塞任务。收尾只能用到总时限里剩给它的部分
        let budget = run_limit.saturating_sub(started.elapsed());
        let teardown = Instant::now();
        rt.shutdown_timeout(budget);
        let outcome = if teardown.elapsed() >= budget {
            Err(format!("运行时收尾 {budget:?} 内没完成（有阻塞任务卡住）"))
        } else {
            Ok(())
        };
        p.store(2, Ordering::SeqCst);
        let _ = tx.send(outcome);
    });
    let stuck = match rx.recv_timeout(run_limit) {
        Ok(Ok(())) => {
            let _ = worker.join();
            return;
        }
        Ok(Err(stuck)) => stuck,
        // 用例本体 panic：线程带着 panic 结束，把它原样抛出来
        Err(RecvTimeoutError::Disconnected) => {
            if let Err(panic) = worker.join() {
                std::panic::resume_unwind(panic);
            }
            panic!("用例线程没报结果就结束了");
        }
        Err(RecvTimeoutError::Timeout) => format!(
            "用例 {run_limit:?} 内没结束：{}",
            PHASES[usize::from(phase.load(Ordering::SeqCst))]
        ),
    };
    let (dtx, drx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = dtx.send(diag());
    });
    let left = limit.saturating_sub(started.elapsed());
    let scene = drx
        .recv_timeout(left)
        .unwrap_or_else(|_| format!("诊断在 {left:?} 内没完成"));
    panic!("{stuck}\n现场：\n{scene}");
}

/// 卡住时的现场：假运行时进程还在不在（按程序名精确匹配），和它的记录文件。
pub fn fake_diag(record: &Path) -> String {
    #[cfg(windows)]
    let procs = std::process::Command::new("tasklist")
        .args([
            "/FI",
            "IMAGENAME eq iwork-fake-agent.exe",
            "/FO",
            "CSV",
            "/NH",
        ])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
    #[cfg(unix)]
    let procs = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,stat=,comm="])
        .output()
        .map(|out| {
            // comm 是程序名（macOS 上是程序的完整路径）：按文件名精确比，不按命令行子串
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| is_fake_agent_row(l, cfg!(target_os = "linux"), exe_name))
                .collect::<Vec<_>>()
                .join("\n")
        });
    let procs = procs.unwrap_or_else(|e| format!("列不出进程：{e}"));
    let record = std::fs::read_to_string(record).unwrap_or_else(|e| format!("读不了记录：{e}"));
    format!("假运行时进程：\n{procs}\n记录：\n{record}")
}

/// 假运行时的程序名。
const FAKE_AGENT_NAME: &str = "iwork-fake-agent";

/// `ps -o pid=,ppid=,stat=,comm=` 的一行是不是假运行时。前三列是进程号、父进程号、状态，之后整段是程序名（路径里
/// 可能有空格）。`linux` 与 `exe_name`（按进程号读真实可执行文件名）可以换成替身，测试不同平台的写法。
pub fn is_fake_agent_row(
    row: &str,
    linux: bool,
    exe_name: impl FnOnce(u32) -> Option<String>,
) -> bool {
    let mut cols = row.split_whitespace();
    let pid = cols.next().and_then(|p| p.parse::<u32>().ok());
    let comm = cols.skip(2).collect::<Vec<_>>().join(" ");
    let Some(name) = Path::new(&comm).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name == FAKE_AGENT_NAME {
        return true;
    }
    // Linux 的 comm 只留前 15 字节（TASK_COMM_LEN 16 含结尾的 0）：截断名不能直接认（名字前 15 字节相同的别的程序也是
    // 这个样子），按进程号读真实可执行文件名确认
    linux
        && name == &FAKE_AGENT_NAME[..15]
        && pid
            .and_then(exe_name)
            .is_some_and(|exe| exe == FAKE_AGENT_NAME)
}

/// 按进程号读真实可执行文件的文件名（Linux：`/proc/<pid>/exe`）；读不到为 `None`。
#[cfg(unix)]
fn exe_name(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()?
        .file_name()?
        .to_str()
        .map(str::to_owned)
}
