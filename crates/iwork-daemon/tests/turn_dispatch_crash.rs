//! 回合派发的崩溃窗口：真 `SessionLine` + 真存储（`iwork-store` + proto codec），后台服务在派发途中
//! 被 `kill -9`，重启后看库里留下了什么（需求 12「重启后核对、不静默重放」；T04 方案 1.1 的 P1、P2）。
//!
//! 要守住的两条事实：
//! - P1：库里是 `Queued` 的回合一定没发过 prompt。编排先提交「开始执行」再发 prompt，所以
//!   「运行时已收到 prompt、库里还是 `Queued`」这个窗口不存在。
//! - P2：运行时会话 ID 在发 prompt 之前落盘（`TurnSessionBound`，与进 `Running` 同一次提交），
//!   重启后从存储取回原 ID 续接。
//!
//! 子进程就是本测试二进制自己：带环境变量 `IWORK_DISPATCH_CHILD=<数据目录>` 只跑 `dispatch_child`。
//! 它在数据目录里起存储、跑一个回合；假运行时到了约定的位置往 stdout 写一行 `AT …` 然后一直等，
//! 父进程读到这一行就杀掉它，再自己打开同一个库断言。

use std::future::Future;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_app::codec::ProtoEventCodec;
use iwork_core::turn::{
    Decided, PermissionDecider, RunTag, SessionLine, TurnEntry, TurnPlan, TurnPorts, VoidPlanError,
};
use iwork_domain::approval::VoidReason;
use iwork_domain::assistant::{Assistant, AssistantConfig, AssistantVersion, Avatar};
use iwork_domain::common::{ModelId, ModelSelection, Timestamp, ToolGroups};
use iwork_domain::conversation::{Conversation, ConversationKind, ConversationState};
use iwork_domain::event::{AssistantCreated, ConversationCreated, DomainEvent, TurnQueued};
use iwork_domain::id::{AssistantId, CommandId, ConversationId, RawUlid, RuntimeId, Seq, TurnId};
use iwork_domain::session::{AllowWrite, NetworkPolicy, SessionConstraints, SessionToolSet};
use iwork_domain::settings::GlobalSettings;
use iwork_domain::turn::{Turn, TurnKind, TurnState};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, PermissionHandler,
    PermissionRequest, RuntimeCapabilities, SessionSpec, TurnOutcome, UpdateStream,
};
use iwork_ports::clock::{Clock, IdGenerator, MonotonicInstant, Timer};
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_ports::read_model::{MessagePage, ReadModelPort};
use iwork_ports::store::{CommitRequest, StoragePort};
use iwork_store::{
    DataDir, ReadDefaults, SqliteProjector, SqliteReadModel, SqliteStore, StoreConfig,
};

const CHILD_ENV: &str = "IWORK_DISPATCH_CHILD";
const MODE_ENV: &str = "IWORK_DISPATCH_MODE";
/// 子进程停在哪：运行时收到 prompt 时，或运行时建好会话、还没把会话交回编排时
const STOP_AT_PROMPT: &str = "prompt";
const STOP_AT_OPEN: &str = "open";

/// 运行时为新会话分配的 ID（续接时沿用传进来的 ID）
const NEW_SESSION_ID: &str = "rt-session-1";
const CONVERSATION: u128 = 10;
const FIRST_TURN: u128 = 20;
const SECOND_TURN: u128 = 21;

type Store = SqliteStore<ProtoEventCodec>;

// ---------- 假运行时 ----------

/// 假运行时在某个位置报告并停住时，报告里带什么。
type Probe = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = String> + Send>> + Send + Sync>;

#[derive(Default)]
struct Ctl {
    /// 每次开会话收到的续接 ID
    opened_with: Mutex<Vec<Option<String>>>,
    prompts: AtomicUsize,
    /// 设了就在这里报告并停住；没设就正常完成回合
    stop_at: Mutex<Option<(&'static str, Probe)>>,
    /// 设了就在每次收到 prompt 时记一笔探针结果（不停）
    observe: Mutex<Option<Probe>>,
    observed: Mutex<Vec<String>>,
    /// 这些会话的进程已经退出：prompt 报 `ProcessExited`
    dead: Mutex<Vec<String>>,
    /// 下一次开会话时运行时给的 ID（续接也换新 ID）；没设就续接沿用、新开用 [`NEW_SESSION_ID`]
    next_id: Mutex<Option<String>>,
}

impl Ctl {
    /// 到了 `here`：要停的话写一行 `AT <here> <探针结果>` 并永远不返回。
    async fn checkpoint(&self, here: &str) {
        let probe = match &*self.stop_at.lock().unwrap() {
            Some((at, probe)) if *at == here => probe.clone(),
            _ => return,
        };
        let seen = probe().await;
        println!("AT {here} {seen}");
        std::future::pending::<()>().await;
    }
}

struct FakeAgent(Arc<Ctl>);
struct FakeSession {
    ctl: Arc<Ctl>,
    id: String,
}
struct FakeUpdates(Option<AgentUpdate>);

impl AgentPort for FakeAgent {
    type Session = FakeSession;

    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        unimplemented!("编排不问能力")
    }

    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        spec: SessionSpec,
        _on_permission: H,
    ) -> Result<FakeSession, AgentError> {
        self.0.opened_with.lock().unwrap().push(spec.resume.clone());
        // 会话已在运行时里建好、ID 已定，还没交回编排
        self.0.checkpoint(STOP_AT_OPEN).await;
        let id = self.0.next_id.lock().unwrap().take();
        Ok(FakeSession {
            ctl: self.0.clone(),
            id: id.or(spec.resume).unwrap_or_else(|| NEW_SESSION_ID.into()),
        })
    }
}

impl AgentSession for FakeSession {
    type Updates = FakeUpdates;

    fn runtime_session_id(&self) -> Option<&str> {
        Some(&self.id)
    }

    fn permission_wait_limit(&self) -> Option<Duration> {
        None
    }

    fn sandbox(&self) -> iwork_ports::agent::SandboxStatus {
        iwork_ports::agent::SandboxStatus::Enforced
    }

    async fn terminate(&self) -> Result<(), AgentError> {
        Ok(())
    }

    async fn prompt(&self, _blocks: Vec<ContentBlock>) -> Result<FakeUpdates, AgentError> {
        if self.ctl.dead.lock().unwrap().contains(&self.id) {
            return Err(AgentError::ProcessExited {
                message: "运行时已退出".into(),
            });
        }
        self.ctl.prompts.fetch_add(1, Ordering::SeqCst);
        let observe = self.ctl.observe.lock().unwrap().clone();
        if let Some(probe) = observe {
            let seen = probe().await;
            self.ctl.observed.lock().unwrap().push(seen);
        }
        // 运行时已收到 prompt
        self.ctl.checkpoint(STOP_AT_PROMPT).await;
        Ok(FakeUpdates(Some(AgentUpdate::Finished(
            TurnOutcome::Completed,
        ))))
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }

    async fn close(self) -> Result<(), AgentError> {
        Ok(())
    }
}

impl UpdateStream for FakeUpdates {
    async fn next(&mut self) -> Option<AgentUpdate> {
        self.0.take()
    }
}

/// 这些回合不发权限请求。
struct NoPermissions;

impl PermissionDecider for NoPermissions {
    type VoidHold = ();

    async fn decide(&self, _turn_id: TurnId, _req: &PermissionRequest) -> Decided {
        unreachable!("假运行时不发权限请求")
    }

    async fn void_pending(
        &self,
        _turn_id: TurnId,
        _reason: VoidReason,
    ) -> Result<(Vec<DomainEvent>, ()), VoidPlanError> {
        Ok((Vec::new(), ()))
    }
}

// ---------- 存储与其他端口 ----------

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp(1_794_268_800_000)
    }
    fn monotonic(&self) -> MonotonicInstant {
        MonotonicInstant(0)
    }
}

/// 每个进程从不同的起点数，父子进程写进同一个库的 ID 不撞。
struct Ids(AtomicU64);

impl IdGenerator for Ids {
    fn next(&self) -> RawUlid {
        RawUlid::from_u128(u128::from(self.0.fetch_add(1, Ordering::SeqCst)))
    }
}

struct NoLive;

impl LivePublisher for NoLive {
    fn message_delta(&self, _delta: MessageDelta) {}
}

/// 假运行时不发正文，检查点定时器不会被用到。
struct NoTimer;

impl Timer for NoTimer {
    async fn sleep(&self, _duration: Duration) {
        std::future::pending::<()>().await
    }
}

fn open_store(root: &Path) -> Store {
    let codec = Arc::new(ProtoEventCodec);
    SqliteStore::open(
        &DataDir::from_root(root.to_path_buf()),
        codec.clone(),
        Arc::new(SqliteProjector::new(codec)),
        Arc::new(FixedClock),
        StoreConfig {
            fullfsync: Some(false),
            ..StoreConfig::default()
        },
    )
    .expect("打开存储")
}

fn id(n: u128) -> RawUlid {
    RawUlid::from_u128(n)
}

fn queued_turn(n: u128) -> Turn {
    Turn {
        id: TurnId::from_raw(id(n)),
        conversation_id: ConversationId::from_raw(id(CONVERSATION)),
        assistant_id: AssistantId::from_raw(id(1)),
        assistant_version: 1,
        runtime_id: RuntimeId::from_raw(id(2)),
        model: model(),
        runtime_session_id: None,
        kind: TurnKind::Assistant,
        state: TurnState::Queued,
        started_at: Timestamp(0),
        ended_at: None,
    }
}

fn model() -> ModelSelection {
    ModelSelection {
        model_id: ModelId("m".into()),
        effort: None,
    }
}

fn plan(turn: Turn) -> TurnPlan {
    TurnPlan {
        changes: None,
        run: RunTag(1),
        entry: TurnEntry::New,
        turn,
        assistant_name: "小助手".into(),
        context_project_id: None,
        spec: SessionSpec {
            cwd: "/work".into(),
            model: model(),
            system_prompt: String::new(),
            mcp: Vec::new(),
            constraints: SessionConstraints {
                deny_read_write: Vec::new(),
                read_only: Vec::new(),
                allow_write: AllowWrite::Anywhere,
                excluded_commands: Vec::new(),
                allow_unsandboxed_retry: false,
                network: NetworkPolicy::Open,
            },
            tool_set: SessionToolSet {
                enabled_groups: ToolGroups::new(),
                include_always_on: true,
            },
            disabled_tools: Vec::new(),
            plugin_dirs: Vec::new(),
            resume: None,
            kind: TurnKind::Assistant,
        },
        blocks: vec![ContentBlock::Text("hi".into())],
    }
}

/// 受理消息的一方写下的东西：对话与排队的回合。
async fn seed(store: &Store, turn: u128, command: u128) {
    let mut events: Vec<DomainEvent> = Vec::new();
    if turn == FIRST_TURN {
        // 排回合前要有它的助手（T10a A3 的归属存续）
        events.push(
            AssistantCreated {
                assistant: Assistant {
                    id: AssistantId::from_raw(id(1)),
                    builtin: false,
                    current: AssistantVersion {
                        version: 1,
                        config: AssistantConfig {
                            name: "a1".into(),
                            avatar: Avatar {
                                initial: "A".into(),
                                color: "#123456".into(),
                            },
                            prompt: "p".into(),
                            runtime_id: RuntimeId::from_raw(id(2)),
                            model: model(),
                            disabled_groups: ToolGroups::new(),
                        },
                        created_at: Timestamp(0),
                    },
                },
            }
            .into(),
        );
        events.push(
            ConversationCreated {
                conversation: Conversation {
                    id: ConversationId::from_raw(id(CONVERSATION)),
                    kind: ConversationKind::AssistantDm {
                        assistant_id: AssistantId::from_raw(id(1)),
                        context_project: None,
                    },
                    state: ConversationState::Active,
                    last_read_seq: Seq(0),
                    model_override: None,
                    approval_mode: None,
                    updated_at: Timestamp(0),
                },
            }
            .into(),
        );
    }
    events.push(
        TurnQueued {
            turn: queued_turn(turn),
        }
        .into(),
    );
    store
        .commit(CommitRequest {
            command_id: CommandId::from_raw(id(command)),
            actor: iwork_domain::common::Actor("user".into()),
            events,
            outbox: Vec::new(),
        })
        .await
        .expect("写入排队的回合");
}

/// 读模型里的回合（对话视图带着它）。
async fn read_turn(store: &Store, n: u128) -> Turn {
    let view = read_model(store)
        .conversation(
            ConversationId::from_raw(id(CONVERSATION)),
            MessagePage {
                before: None,
                limit: 50,
            },
        )
        .await
        .expect("读对话");
    view.turns
        .into_iter()
        .find(|t| t.id == TurnId::from_raw(id(n)))
        .expect("回合在读模型里")
}

fn read_model(store: &Store) -> SqliteReadModel<ProtoEventCodec> {
    store.read_model(ReadDefaults {
        global_settings: GlobalSettings {
            playful_ui: true,
            autostart: false,
            projects_root: PathBuf::from("/projects"),
        },
    })
}

/// 跑一个回合。`resume` 是会话线起步时已知的运行时会话 ID（后台服务重启后从回合上取回的）。
async fn run(
    store: &Store,
    agent: &FakeAgent,
    resume: Option<String>,
    turn: Turn,
    ids_from: u64,
) -> TurnState {
    let mut line = SessionLine::new(Arc::new(NoPermissions), resume);
    run_on(&mut line, store, agent, turn, ids_from).await
}

async fn run_on(
    line: &mut SessionLine<FakeAgent, NoPermissions>,
    store: &Store,
    agent: &FakeAgent,
    turn: Turn,
    ids_from: u64,
) -> TurnState {
    let ids = Ids(AtomicU64::new(ids_from));
    let history = read_model(store);
    let ports = TurnPorts {
        store,
        clock: &FixedClock,
        ids: &ids,
        live: &NoLive,
        timer: &NoTimer,
        history: &history,
    };
    line.run_turn(agent, ports, plan(turn))
        .await
        .expect("编排没出错")
}

fn tokio() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

// ---------- 子进程 ----------

/// 子进程入口。没有环境变量时什么都不做（正常跑测试时它是个空测试）。
#[test]
fn dispatch_child() {
    let Some(root) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let mode: &'static str = match std::env::var(MODE_ENV).unwrap().as_str() {
        STOP_AT_PROMPT => STOP_AT_PROMPT,
        STOP_AT_OPEN => STOP_AT_OPEN,
        other => panic!("不认识的模式 {other}"),
    };
    tokio().block_on(async {
        let store = Arc::new(open_store(Path::new(&root)));
        seed(&store, FIRST_TURN, 1).await;
        let ctl = Arc::new(Ctl::default());
        // 停住时报告：库里（读模型）这个回合当时的状态与会话 ID
        let probe_store = store.clone();
        let probe: Probe = Arc::new(move || {
            let store = probe_store.clone();
            Box::pin(async move {
                let t = read_turn(&store, FIRST_TURN).await;
                format!(
                    "{:?} {}",
                    t.state,
                    t.runtime_session_id.as_deref().unwrap_or("-")
                )
            })
        });
        *ctl.stop_at.lock().unwrap() = Some((mode, probe));
        run(
            &store,
            &FakeAgent(ctl),
            None,
            queued_turn(FIRST_TURN),
            1_000,
        )
        .await;
        unreachable!("假运行时停住了，回合不会结束");
    });
}

/// 子进程走到停点的时限：编译好的二进制起库、跑到停点只要几百毫秒，留足机器繁忙时的余量
const CHILD_DEADLINE: Duration = Duration::from_secs(60);

/// 起子进程跑到 `mode` 停住，返回它报告的那一行（去掉 `AT <mode> `），然后 `kill -9`。
/// 超时或子进程提前退出都先杀掉它，再点名卡在哪个停点。
fn crash_at(root: &Path, mode: &str) -> String {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "dispatch_child",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, root)
        .env(MODE_ENV, mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("起子进程");
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let prefix = format!("AT {mode} ");
    let (tx, rx) = std::sync::mpsc::channel();
    // 读输出放在线程里，主线程才能限时等
    std::thread::spawn(move || {
        for line in stdout.lines() {
            let Ok(line) = line else { break };
            // libtest 先在同一行打出「test dispatch_child ... 」，标记不在行首
            if let Some(at) = line.find(&prefix) {
                let _ = tx.send(line[at + prefix.len()..].to_owned());
                return;
            }
        }
        // 输出结束还没见到标记：发送端随线程丢弃，主线程收到 Disconnected
    });
    let seen = rx.recv_timeout(CHILD_DEADLINE);
    // Unix 上是 SIGKILL，Windows 上是 TerminateProcess：不给进程任何收尾的机会
    child.kill().expect("杀子进程");
    child.wait().expect("等子进程退出");
    match seen {
        Ok(seen) => seen,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("子进程 {CHILD_DEADLINE:?} 内没走到停点 {mode}，已杀掉")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("子进程没走到停点 {mode} 就退出了")
        }
    }
}

fn data_root() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::Builder::new()
        .prefix("iwork-dispatch-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("data");
    (tmp, root)
}

// ---------- 测试 ----------

/// ① 运行时收到 prompt 的那一刻，库里已经是 `Running` 且记着会话 ID（提交先于 prompt，
/// 「已发 prompt、库里仍是 Queued」的窗口不存在）；② 此时崩溃，重启后库里取得到原会话 ID，
/// 用它续接；③ 重建投影后读到的回合不变。
#[test]
fn crash_after_prompt_leaves_running_turn_with_session_id() {
    let (_tmp, root) = data_root();
    let seen_at_prompt = crash_at(&root, STOP_AT_PROMPT);
    assert_eq!(
        seen_at_prompt,
        format!("Running {NEW_SESSION_ID}"),
        "运行时收到 prompt 时，回合已提交为 Running 并记下会话 ID"
    );

    tokio().block_on(async {
        let store = open_store(&root);
        let crashed = read_turn(&store, FIRST_TURN).await;
        assert_eq!(crashed.state, TurnState::Running);
        assert_eq!(crashed.runtime_session_id.as_deref(), Some(NEW_SESSION_ID));

        store.rebuild_projections().await.expect("重建投影");
        assert_eq!(read_turn(&store, FIRST_TURN).await, crashed, "重建后一致");

        // 重启后的会话线从存储取回原 ID：下一回合续接原会话，不新建
        seed(&store, SECOND_TURN, 2).await;
        let ctl = Arc::new(Ctl::default());
        let resume = crashed.runtime_session_id.clone();
        let end = run(
            &store,
            &FakeAgent(ctl.clone()),
            resume,
            queued_turn(SECOND_TURN),
            2_000,
        )
        .await;
        assert_eq!(end, TurnState::Completed);
        assert_eq!(
            *ctl.opened_with.lock().unwrap(),
            vec![Some(NEW_SESSION_ID.to_owned())],
            "续接用的是崩溃前落盘的会话 ID"
        );
        let resumed = read_turn(&store, SECOND_TURN).await;
        assert_eq!(resumed.runtime_session_id.as_deref(), Some(NEW_SESSION_ID));
        store.close().unwrap();
    });
}

/// 运行时建好会话、编排还没提交时崩溃：库里是 `Queued`、没有会话 ID，也没有发过 prompt；
/// 重启后照常派发是安全的（只发一次 prompt），新会话 ID 随开始执行一起落盘。
#[test]
fn crash_after_open_before_commit_leaves_queued_turn_safe_to_dispatch() {
    let (_tmp, root) = data_root();
    let seen_at_open = crash_at(&root, STOP_AT_OPEN);
    assert_eq!(seen_at_open, "Queued -", "对照：建会话时回合还在排队");

    tokio().block_on(async {
        let store = open_store(&root);
        let crashed = read_turn(&store, FIRST_TURN).await;
        assert_eq!(crashed.state, TurnState::Queued);
        assert_eq!(crashed.runtime_session_id, None);

        // 重新派发：没有会话 ID，新建会话，prompt 只发这一次
        let ctl = Arc::new(Ctl::default());
        let end = run(
            &store,
            &FakeAgent(ctl.clone()),
            crashed.runtime_session_id.clone(),
            crashed,
            2_000,
        )
        .await;
        assert_eq!(end, TurnState::Completed);
        assert_eq!(*ctl.opened_with.lock().unwrap(), vec![None]);
        assert_eq!(ctl.prompts.load(Ordering::SeqCst), 1);
        let done = read_turn(&store, FIRST_TURN).await;
        assert_eq!(done.runtime_session_id.as_deref(), Some(NEW_SESSION_ID));
        store.close().unwrap();
    });
}

/// 沿用的会话进程已经退出：续接一次，运行时给了新会话 ID。新 ID 先落盘再发 prompt；
/// 每次 prompt 送达时，库里这个回合都已是 Running 且记着当时的会话 ID。
#[test]
fn reopened_session_is_bound_before_prompt() {
    let (_tmp, root) = data_root();
    tokio().block_on(async {
        let store = Arc::new(open_store(&root));
        seed(&store, FIRST_TURN, 1).await;
        seed(&store, SECOND_TURN, 2).await;
        let ctl = Arc::new(Ctl::default());
        let agent = FakeAgent(ctl.clone());
        let mut line = SessionLine::new(Arc::new(NoPermissions), None);

        let observing = |turn: u128| -> Probe {
            let store = store.clone();
            Arc::new(move || {
                let store = store.clone();
                Box::pin(async move {
                    let t = read_turn(&store, turn).await;
                    format!(
                        "{:?} {}",
                        t.state,
                        t.runtime_session_id.as_deref().unwrap_or("-")
                    )
                })
            })
        };
        *ctl.observe.lock().unwrap() = Some(observing(FIRST_TURN));
        let end = run_on(&mut line, &store, &agent, queued_turn(FIRST_TURN), 1_000).await;
        assert_eq!(end, TurnState::Completed);

        ctl.dead.lock().unwrap().push(NEW_SESSION_ID.into());
        *ctl.next_id.lock().unwrap() = Some("rt-session-2".into());
        *ctl.observe.lock().unwrap() = Some(observing(SECOND_TURN));
        let end = run_on(&mut line, &store, &agent, queued_turn(SECOND_TURN), 2_000).await;
        assert_eq!(end, TurnState::Completed);

        assert_eq!(
            *ctl.opened_with.lock().unwrap(),
            vec![None, Some(NEW_SESSION_ID.to_owned())],
            "第二回合按记下的 ID 续接"
        );
        assert_eq!(
            *ctl.observed.lock().unwrap(),
            vec![
                format!("Running {NEW_SESSION_ID}"),
                "Running rt-session-2".to_owned(),
            ],
            "每次 prompt 送达前，回合已是 Running 且绑定了当时的会话"
        );
        assert_eq!(
            ctl.prompts.load(Ordering::SeqCst),
            2,
            "死掉的会话上没送到的不算"
        );
        let second = read_turn(&store, SECOND_TURN).await;
        assert_eq!(second.runtime_session_id.as_deref(), Some("rt-session-2"));
    });
}
