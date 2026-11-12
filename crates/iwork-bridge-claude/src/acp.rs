//! ACP agent 端（T06 方案 2.2 `acp.rs`、2.5、4.1、4.7、5.2、6.1）：一个桥接进程只受理一条会话。
//!
//! - `initialize`：声明能续接、收图片、收系统提示；`_meta.iwork` 里报等待上限（`deadlines().daemon`）与平台沙箱。
//!   底下 CLI 的路径与版本（`cli`）归 S8 的探测，这里先不报。
//! - `session/new` / `session/load`：解 `SessionSetup`，按 3.5 的次序开会话（[`startup::open`]）；成功后会话交给
//!   一个后台任务（[`run_session`]）独占：CLI 的输出、hook 通道、权限答复、prompt 请求都在它一个循环里处理，
//!   回合之外到的东西也有人收。
//! - 这条会话的沙箱（`_meta.iwork.sandbox`）报 `unavailable`：第 8 节要求只有证据时才报 `enforced`（`system/init`
//!   里的沙箱状态字段，或 `failIfUnavailable` 保证起不来就退出），这两样现在都没有（P4 未定），静态的「平台支持」
//!   不许冒充会话的实际状态。
//! - 致命错误（输出坏了、隔离失效、对账不成立、兜底）：正在跑的 prompt 回点名错误码，杀掉 CLI，会话作废。
//!   怎么停才不留下命令（5.4a）归 S6-5；取消（`session/cancel`）归 S6-4。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, ContentBlock, ContentChunk, Implementation, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, Meta, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionKind, PromptCapabilities, PromptRequest,
    PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionCapabilities, SessionId, SessionInfoUpdate, SessionNotification,
    SessionSystemPromptCapabilities, SessionUpdate, StopReason, SystemPrompt, TextContent,
    ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, Lines};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use iwork_proto::pb;
use prost::Message as _;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::cli::errors::TurnEnd;
use crate::cli::facts::{IsolationBroken, SessionExpect};
use crate::cli::input::{InputBlock, user_line};
use crate::cli::map::{Fatal, Out, Report, StreamMapper};
use crate::cli::process::ReadError;
use crate::deadlines::deadlines;
use crate::hook_server::HookEvent;
use crate::hook_server::input::{self as hook_input, HookInput};
use crate::hook_server::pending::{BackendAnswer, CallId, Delivery};
use crate::hook_server::reconcile::Reconciler;
use crate::launch::{BridgeEnv, Opening, Start};
use crate::rewrite::{CommandRewriter, NoRewrite};
use crate::setup;
use crate::startup::{self, OpenRequest, OpenSession};

/// iWork 在 ACP `_meta` 里占用的键（方案 2.5；和 `iwork-agent-acp::convert` 同一份字符串，由端到端测试对拍）。
const META_KEY: &str = "iwork";
const PERMISSION_WAIT_LIMIT_KEY: &str = "permissionWaitLimitSecs";
const SANDBOX_PLATFORM_KEY: &str = "sandboxPlatform";
const SANDBOX_KEY: &str = "sandbox";
const REPORT_KEY: &str = "report";
const CWD_KEY: &str = "cwd";
const DENY_MESSAGE_KEY: &str = "message";
const UPDATED_INPUT_KEY: &str = "updatedInput";

const ALLOW_ID: &str = "allow-once";
const REJECT_ID: &str = "reject-once";

/// 回合之外到的工具调用、解析不了的 hook 输入，回给模型的话。
pub const REASON_NO_TURN: &str = "iWork 当前没有进行中的回合，这次调用没有执行";
pub const REASON_BAD_HOOK_INPUT: &str = "iWork 无法解析这次工具调用，没有执行";
/// 后台拒绝却没给原因时（不该发生）。
const REASON_DENIED: &str = "iWork 拒绝了这次调用";

/// 桥接进程的配置：来自后台给的环境（T10 接线）与桥接自己所在的目录。
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub data_dir: PathBuf,
    pub bridge: BridgeEnv,
    pub hook_timeout: Duration,
    pub startup_limit: Duration,
}

/// 后台经环境变量给桥接的东西（`RuntimeLaunch.env`）。
pub const ENV_DATA_DIR: &str = "IWORK_DATA_DIR";
/// `claude` 可执行文件的绝对路径。后台从探测（S8）拿到后给；桥接不自己在 PATH 里找，免得两边认的不是同一个。
pub const ENV_CLAUDE_CLI: &str = "IWORK_CLAUDE_CLI";

impl BridgeConfig {
    /// 从本进程的环境与可执行文件位置取。hook 程序在桥接同一目录下（方案 3.2）。
    pub fn from_process() -> Result<Self, String> {
        let abs = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .ok_or_else(|| format!("环境变量 {name} 缺失或不是绝对路径"))
        };
        let exe = std::env::current_exe().map_err(|e| format!("取不到桥接自己的路径：{e}"))?;
        let hook = exe
            .parent()
            .ok_or("桥接的路径没有上一级目录")?
            .join("iwork-claude-hook");
        let hook_program = hook
            .to_str()
            .ok_or_else(|| format!("hook 程序的路径不是 UTF-8：{}", hook.display()))?
            .to_owned();
        Ok(Self {
            data_dir: abs(ENV_DATA_DIR)?,
            bridge: BridgeEnv {
                cli: abs(ENV_CLAUDE_CLI)?,
                hook_program,
                inherited: crate::launch::env::LaunchEnv::from_vars(std::env::vars_os().collect()),
            },
            hook_timeout: Duration::from_secs(crate::launch::settings::HOOK_TIMEOUT_SECS),
            startup_limit: startup::STARTUP_LIMIT,
        })
    }
}

type Conn = ConnectionTo<Client>;

/// 带点名错误码的 JSON-RPC 错误：`data` 为 `{"iwork": {"code", "reason"?}}`（方案 2.6 第 7 条）。
fn named_error(code: &str, reason: Option<&str>) -> agent_client_protocol::Error {
    let mut inner = json!({ "code": code });
    if let Some(reason) = reason {
        inner["reason"] = json!(reason);
    }
    agent_client_protocol::Error::internal_error().data(json!({ META_KEY: inner }))
}

fn iwork_meta(inner: Value) -> Meta {
    let mut meta = Map::new();
    meta.insert(META_KEY.into(), inner);
    meta
}

fn iwork_value<'a>(meta: Option<&'a Meta>, key: &str) -> Option<&'a Value> {
    meta.and_then(|m| m.get(META_KEY)).and_then(|v| v.get(key))
}

/// 给正在跑的会话任务的请求。
enum Command {
    Prompt {
        blocks: Vec<InputBlock>,
        done: oneshot::Sender<Result<StopReason, agent_client_protocol::Error>>,
    },
}

/// 桥接进程里那一条会话。
#[derive(Default)]
struct Slot {
    /// 开过会话之后有值；会话任务结束（致命错误）后发送端关闭，之后的 prompt 回 `cli_exited`
    commands: Option<mpsc::UnboundedSender<Command>>,
    opened: bool,
}

/// 在给定的传输上说 ACP，直到对端断开。
pub async fn serve<T>(config: BridgeConfig, transport: T) -> Result<(), String>
where
    T: agent_client_protocol::ConnectTo<agent_client_protocol::Agent> + 'static,
{
    let config = Arc::new(config);
    let slot = Arc::new(Mutex::new(Slot::default()));
    let (c1, c2, c3) = (config.clone(), config.clone(), config.clone());
    let (s1, s2, s3) = (slot.clone(), slot.clone(), slot.clone());
    Agent
        .v1()
        .name("iwork-bridge-claude")
        .on_receive_request(
            async move |_req: InitializeRequest, responder, _cx| {
                responder.respond(initialize_response(&c1))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, cx: Conn| {
                let (config, slot) = (c2.clone(), s1.clone());
                cx.clone().spawn(async move {
                    let session_id = new_session_id();
                    let opened = open(
                        &config,
                        &slot,
                        &cx,
                        req.meta.as_ref(),
                        req.cwd,
                        system_prompt_text(req.system_prompt.as_ref()),
                        Start::New {
                            session_id: session_id.clone(),
                        },
                    )
                    .await;
                    responder.respond_with_result(opened.map(|()| {
                        NewSessionResponse::new(SessionId::new(session_id))
                            .meta(iwork_meta(json!({ SANDBOX_KEY: "unavailable" })))
                    }))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, cx: Conn| {
                let (config, slot) = (c3.clone(), s2.clone());
                cx.clone().spawn(async move {
                    let opened = open(
                        &config,
                        &slot,
                        &cx,
                        req.meta.as_ref(),
                        req.cwd,
                        system_prompt_text(req.system_prompt.as_ref()),
                        Start::Resume {
                            session_id: req.session_id.0.to_string(),
                        },
                    )
                    .await;
                    responder.respond_with_result(opened.map(|()| {
                        LoadSessionResponse::new()
                            .meta(iwork_meta(json!({ SANDBOX_KEY: "unavailable" })))
                    }))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx: Conn| {
                let slot = s3.clone();
                cx.spawn(async move {
                    let result = prompt(&slot, &req.prompt).await.map(PromptResponse::new);
                    responder.respond_with_result(result)
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(transport)
        .await
        .map_err(|e| format!("ACP 连接出错：{e}"))
}

/// 在 stdin / stdout 上说 ACP（`acp` 子命令）。
pub async fn serve_stdio(config: BridgeConfig) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let incoming = futures::stream::unfold(
        BufReader::new(tokio::io::stdin()).lines(),
        |mut lines| async move {
            match lines.next_line().await {
                Ok(Some(line)) => Some((Ok(line), lines)),
                Ok(None) => None,
                Err(e) => Some((Err(e), lines)),
            }
        },
    );
    let outgoing = futures::sink::unfold(tokio::io::stdout(), |mut w, line: String| async move {
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok::<_, std::io::Error>(w)
    });
    serve(config, Lines::new(Box::pin(outgoing), Box::pin(incoming))).await
}

fn initialize_response(config: &BridgeConfig) -> InitializeResponse {
    let caps = AgentCapabilities::new()
        .load_session(true)
        .prompt_capabilities(PromptCapabilities::new().image(true))
        .session_capabilities(
            SessionCapabilities::new().system_prompt(SessionSystemPromptCapabilities::new()),
        );
    InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(caps)
        .agent_info(Implementation::new(
            "iwork-bridge-claude",
            env!("CARGO_PKG_VERSION"),
        ))
        .meta(iwork_meta(json!({
            PERMISSION_WAIT_LIMIT_KEY: deadlines(config.hook_timeout).daemon.as_secs(),
            // 只说平台能力；每条会话实际的沙箱在开会话的响应里报（第 8 节）。M1 只在 unix 上跑
            SANDBOX_PLATFORM_KEY: "supported",
        })))
}

/// 系统提示的文字（只收文字块，3.1 用 `--append-system-prompt` 交给 CLI）。
fn system_prompt_text(prompt: Option<&SystemPrompt>) -> String {
    prompt
        .map(|p| {
            p.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// 新会话的编号：随机 UUID v4（经 `--session-id` 交给 CLI，5.2）。
fn new_session_id() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("取随机数");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 开会话，成功后起会话任务。一个桥接进程只开一次。
async fn open(
    config: &BridgeConfig,
    slot: &Mutex<Slot>,
    cx: &Conn,
    meta: Option<&Meta>,
    cwd: PathBuf,
    system_prompt: String,
    start: Start,
) -> Result<(), agent_client_protocol::Error> {
    {
        let mut slot = slot.lock().expect("锁没有中毒");
        if slot.opened {
            return Err(named_error("setup_invalid", Some("session")));
        }
        slot.opened = true;
    }
    let meta = meta.map(|m| Value::Object(m.clone()));
    let setup =
        setup::from_meta(meta.as_ref()).map_err(|e| named_error("setup_invalid", Some(e.field)))?;
    let opening = Opening {
        cwd,
        system_prompt,
        start,
    };
    let session = startup::open(OpenRequest {
        data_dir: &config.data_dir,
        setup: &setup,
        opening: &opening,
        bridge: &config.bridge,
        hook_timeout: config.hook_timeout,
        startup_limit: config.startup_limit,
    })
    .await
    .map_err(|e| {
        tracing::warn!(code = e.code(), error = %e, "开会话失败");
        named_error(e.code(), None)
    })?;
    let expect = SessionExpect {
        plugin_dirs: setup.plugin_dirs.clone(),
        memory_dir: session.dir.files().memory,
    };
    let mapper = StreamMapper::new(setup.model.effort, expect);
    let (tx, rx) = mpsc::unbounded_channel();
    slot.lock().expect("锁没有中毒").commands = Some(tx);
    let worker = Worker {
        cx: cx.clone(),
        session_id: SessionId::new(match &opening.start {
            Start::New { session_id } | Start::Resume { session_id } => session_id.clone(),
        }),
        workspace: opening.cwd.clone(),
        rewriter: Box::new(NoRewrite),
        mapper,
        reconciler: Reconciler::default(),
        turn: None,
    };
    cx.spawn(async move {
        run_session(worker, session, rx).await;
        Ok(())
    })?;
    Ok(())
}

/// 把 prompt 交给会话任务，等这一轮结束。
async fn prompt(
    slot: &Mutex<Slot>,
    blocks: &[ContentBlock],
) -> Result<StopReason, agent_client_protocol::Error> {
    let blocks = blocks
        .iter()
        .map(input_block)
        .collect::<Result<Vec<_>, _>>()?;
    let commands = slot
        .lock()
        .expect("锁没有中毒")
        .commands
        .clone()
        .ok_or_else(|| named_error("setup_invalid", Some("session")))?;
    let (done, result) = oneshot::channel();
    commands
        .send(Command::Prompt { blocks, done })
        .map_err(|_| named_error("cli_exited", None))?;
    result
        .await
        .unwrap_or_else(|_| Err(named_error("cli_exited", None)))
}

/// ACP 内容块 → 桥接收的几种（5.2 最后一条）。
fn input_block(b: &ContentBlock) -> Result<InputBlock, agent_client_protocol::Error> {
    Ok(match b {
        ContentBlock::Text(t) => InputBlock::Text(t.text.clone()),
        ContentBlock::Image(i) => InputBlock::Image {
            mime: i.mime_type.clone(),
            data_base64: i.data.clone(),
        },
        ContentBlock::ResourceLink(l) => InputBlock::ResourceLink { uri: l.uri.clone() },
        _ => return Err(named_error("setup_invalid", Some("prompt"))),
    })
}

/// 会话任务的状态。
struct Worker {
    cx: Conn,
    session_id: SessionId,
    workspace: PathBuf,
    rewriter: Box<dyn CommandRewriter>,
    mapper: StreamMapper,
    reconciler: Reconciler,
    /// 正在跑的那一轮：结束时往这里回
    turn: Option<oneshot::Sender<Result<StopReason, agent_client_protocol::Error>>>,
}

/// 一次权限请求的结论，回到会话任务里交给 hook。
struct Answered {
    call: CallId,
    tool_use_id: String,
    answer: BackendAnswer,
}

/// 致命：这条会话不能再用了。
struct SessionFatal {
    code: &'static str,
    reason: Option<&'static str>,
}

impl From<Fatal> for SessionFatal {
    fn from(f: Fatal) -> Self {
        match f {
            Fatal::BadLine(_) => Self {
                code: "cli_exited",
                reason: None,
            },
            Fatal::Isolation(i) => Self {
                code: "isolation_broken",
                reason: Some(isolation_kind(&i)),
            },
        }
    }
}

/// `isolation_broken` 的种类（只是枚举，不带路径或名字：`reason` 进后台的错误信息）。
fn isolation_kind(i: &IsolationBroken) -> &'static str {
    match i {
        IsolationBroken::PermissionMode { .. } => "permission_mode",
        IsolationBroken::UnexpectedPlugin { .. } => "plugin",
        IsolationBroken::PluginsUnreadable => "plugins_unreadable",
        IsolationBroken::MemoryPath => "memory_path",
    }
}

const BACKSTOP: SessionFatal = SessionFatal {
    code: "approval_backstop",
    reason: None,
};

/// 会话任务：独占 CLI、hook 通道与映射，直到致命错误。
async fn run_session(
    mut w: Worker,
    mut s: OpenSession,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    let (answered_tx, mut answered) = mpsc::unbounded_channel::<Answered>();
    // 开会话期间收下的行（`init` 可能已经在里面）先过映射：更新事实，回合之外的正文只记诊断
    let early = std::mem::take(&mut s.early_lines);
    let mut fatal = None;
    for line in early {
        match w.mapper.on_line(&line) {
            Ok(outs) => w.emit(outs),
            Err(f) => {
                fatal = Some(SessionFatal::from(f));
                break;
            }
        }
    }
    while fatal.is_none() {
        let result = tokio::select! {
            cmd = commands.recv() => match cmd {
                Some(Command::Prompt { blocks, done }) => w.begin(&mut s, blocks, done).await,
                // 连接没了：没有人再发 prompt，会话到此为止
                None => break,
            },
            line = s.cli.next_line() => w.on_line(line),
            ev = s.hook_events.recv() => match ev {
                Some(ev) => w.on_hook(&s, ev, &answered_tx),
                None => Err(SessionFatal { code: "channel_failed", reason: None }),
            },
            Some(a) = answered.recv() => w.on_answered(&s, a),
        };
        if let Err(f) = result {
            fatal = Some(f);
        }
    }
    if let Some(f) = fatal {
        tracing::warn!(code = f.code, reason = ?f.reason, "会话出现致命错误，结束 CLI");
        if let Some(done) = w.turn.take() {
            let _ = done.send(Err(named_error(f.code, f.reason)));
        }
    }
    // 停在这里的还在等的 hook 一律拒绝；怎么停才不留下命令（5.4a）归 S6-5
    s.pending.deny_all(REASON_NO_TURN);
    let _ = s.cli.kill().await;
    drop(s.serving);
    if let Err(e) = s.dir.remove() {
        tracing::warn!(error = %e, "删会话目录失败");
    }
}

impl Worker {
    fn send(&self, update: SessionUpdate) {
        let _ = self
            .cx
            .send_notification(SessionNotification::new(self.session_id.clone(), update));
    }

    fn report(&self, kind: pb::runtime_report::Kind) {
        let encoded = BASE64.encode(pb::RuntimeReport { kind: Some(kind) }.encode_to_vec());
        let n = SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new()),
        )
        .meta(iwork_meta(json!({ REPORT_KEY: encoded })));
        let _ = self.cx.send_notification(n);
    }

    /// 映射出来的东西发出去；结局回给正在跑的 prompt。
    fn emit(&mut self, outs: Vec<Out>) {
        for out in outs {
            match out {
                Out::Text(t) => self.send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(t)),
                ))),
                Out::Thought(t) => self.send(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(t)),
                ))),
                Out::ToolCall {
                    id,
                    name,
                    raw_input,
                } => self.send(SessionUpdate::ToolCall(
                    ToolCall::new(id, name.clone())
                        .name(name)
                        .status(ToolCallStatus::Pending)
                        .raw_input(raw_input),
                )),
                Out::ToolCallUpdate {
                    id,
                    failed,
                    content,
                } => {
                    let status = if failed {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    };
                    self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        id,
                        ToolCallUpdateFields::new()
                            .status(status)
                            .content(vec![ContentBlock::Text(TextContent::new(content)).into()]),
                    )));
                }
                Out::Report(Report::Model { id, effort }) => {
                    self.report(pb::runtime_report::Kind::Model(pb::ModelSelection {
                        model_id: id,
                        effort: Some(effort.to_owned()),
                    }))
                }
                Out::Report(Report::Usage(u)) => {
                    self.report(pb::runtime_report::Kind::Usage(pb::Usage {
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        cache_read_tokens: u.cache_read_tokens,
                        cache_write_tokens: u.cache_write_tokens,
                        cost_usd: u.cost_usd,
                    }))
                }
                Out::Report(Report::Quota(windows)) => {
                    self.report(pb::runtime_report::Kind::Quota(pb::QuotaReport {
                        windows: windows
                            .into_iter()
                            .map(|w| pb::QuotaWindowReport {
                                window: w.window,
                                used_pct: u32::from(w.used_pct),
                                resets_at: Some(prost_types::Timestamp {
                                    seconds: w.resets_at_secs,
                                    nanos: 0,
                                }),
                            })
                            .collect(),
                    }))
                }
                // 桥接在会话里不发别的控制请求（取消归 S6-4）
                Out::Control(_) => {}
                Out::End(end) => {
                    if let Some(done) = self.turn.take() {
                        let _ = done.send(match end {
                            TurnEnd::Completed => Ok(StopReason::EndTurn),
                            TurnEnd::Cancelled => Ok(StopReason::Cancelled),
                            TurnEnd::Error { code, reason } => Err(named_error(code, reason)),
                        });
                    }
                }
                Out::Diagnostic(d) => tracing::debug!(diagnostic = ?d, "映射诊断"),
            }
        }
    }

    /// prompt 到了：先报缓存的模型与额度，再把用户消息写给 CLI。
    async fn begin(
        &mut self,
        s: &mut OpenSession,
        blocks: Vec<InputBlock>,
        done: oneshot::Sender<Result<StopReason, agent_client_protocol::Error>>,
    ) -> Result<(), SessionFatal> {
        if self.turn.is_some() {
            let _ = done.send(Err(named_error("setup_invalid", Some("prompt"))));
            return Ok(());
        }
        let line = match user_line(&blocks) {
            Ok(l) => l,
            Err(e) => {
                let _ = done.send(Err(named_error("setup_invalid", Some(e.field))));
                return Ok(());
            }
        };
        self.turn = Some(done);
        let outs = self.mapper.begin_turn();
        self.emit(outs);
        s.cli.write_line(&line).await.map_err(|_| SessionFatal {
            code: "cli_exited",
            reason: None,
        })
    }

    fn on_line(&mut self, line: Result<Option<String>, ReadError>) -> Result<(), SessionFatal> {
        let line = match line {
            Ok(Some(l)) => l,
            Ok(None) | Err(ReadError::Io(_)) => {
                let outs = self.mapper.on_exit();
                self.emit(outs);
                return Err(SessionFatal {
                    code: "cli_exited",
                    reason: None,
                });
            }
            Err(ReadError::Line(e)) => return Err(Fatal::BadLine(e).into()),
        };
        let outs = self.mapper.on_line(&line).map_err(SessionFatal::from)?;
        // 对账（4.7）：没标错误的结果，它的调用一定得到过允许
        for out in &outs {
            if let Out::ToolCallUpdate { id, failed, .. } = out
                && self.reconciler.on_result(id, *failed).is_err()
            {
                tracing::warn!(tool_use_id = %id, "工具结果没对上一次允许：hook 没生效");
                return Err(SessionFatal {
                    code: "hook_inactive",
                    reason: None,
                });
            }
        }
        self.emit(outs);
        Ok(())
    }

    fn on_hook(
        &mut self,
        s: &OpenSession,
        event: HookEvent,
        answered: &mpsc::UnboundedSender<Answered>,
    ) -> Result<(), SessionFatal> {
        match event {
            HookEvent::Call { id, input } => {
                self.on_call(s, id, &input, answered);
                Ok(())
            }
            // 续接后再来的报到不用管
            HookEvent::SessionStart => Ok(()),
            HookEvent::Backstop { .. }
            | HookEvent::AllowUndelivered { .. }
            | HookEvent::HookLost { .. } => Err(BACKSTOP),
        }
    }

    /// 一次工具调用来问：先保证 `tool_call` 发过（4.1），再发权限请求；结论回到会话任务里交给 hook。
    fn on_call(
        &mut self,
        s: &OpenSession,
        id: CallId,
        input: &[u8],
        answered: &mpsc::UnboundedSender<Answered>,
    ) {
        let reject = |message: &str| BackendAnswer::Reject {
            message: message.to_owned(),
        };
        let call = match hook_input::parse(input) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(missing = e.0, "hook 输入解析不了，答拒绝");
                s.pending.answer(id, reject(REASON_BAD_HOOK_INPUT));
                return;
            }
        };
        if !self.mapper.in_turn() {
            s.pending.answer(id, reject(REASON_NO_TURN));
            return;
        }
        if let Some(out) =
            self.mapper
                .note_tool_call(&call.tool_use_id, &call.tool_name, &call.tool_input)
        {
            self.emit(vec![out]);
        }
        let request = permission_request(&self.session_id, &call);
        let cx = self.cx.clone();
        let answered = answered.clone();
        let tool_use_id = call.tool_use_id.clone();
        let rewritten = self.rewritten(&call);
        let _ = self.cx.spawn(async move {
            let answer = match cx.send_request(request).block_task().await {
                Ok(response) => backend_answer(&response, rewritten),
                Err(e) => {
                    tracing::warn!(error = %e, "权限请求出错，按认不出的答复处理");
                    BackendAnswer::Unrecognized
                }
            };
            let _ = answered.send(Answered {
                call: id,
                tool_use_id,
                answer,
            });
            Ok(())
        });
    }

    /// 命令改写（第 11 节）：只看 Bash 的 `command`，M1 恒不改写。改写了就作为改写后的输入（JSON 文本）。
    fn rewritten(&self, call: &HookInput) -> Option<String> {
        if call.tool_name != "Bash" {
            return None;
        }
        let command = call.tool_input.get("command")?.as_str()?;
        let new = self.rewriter.rewrite(command, &self.workspace)?;
        let mut input = call.tool_input.clone();
        input["command"] = Value::String(new);
        Some(input.to_string())
    }

    fn on_answered(&mut self, s: &OpenSession, a: Answered) -> Result<(), SessionFatal> {
        let allow = matches!(a.answer, BackendAnswer::Allow { .. });
        match s.pending.answer(a.call, a.answer) {
            Delivery::Delivered => {
                if allow {
                    self.reconciler.on_allowed(&a.tool_use_id);
                }
                Ok(())
            }
            // 后台记了交付、动作却没执行：按兜底处理（4.4）
            Delivery::Late { was_allow: true } => Err(BACKSTOP),
            Delivery::Late { was_allow: false } => Ok(()),
        }
    }
}

/// 一次工具调用的权限请求：`toolCall.name` = hook 输入的 `tool_name`，`rawInput` = `tool_input` 原样（4.1）。
fn permission_request(session: &SessionId, call: &HookInput) -> RequestPermissionRequest {
    let mut request = RequestPermissionRequest::new(
        session.clone(),
        ToolCallUpdate::new(
            call.tool_use_id.clone(),
            ToolCallUpdateFields::new()
                .title(call.tool_name.clone())
                .name(call.tool_name.clone())
                .raw_input(call.tool_input.clone()),
        ),
        vec![
            PermissionOption::new(ALLOW_ID, "允许", PermissionOptionKind::AllowOnce),
            PermissionOption::new(REJECT_ID, "拒绝", PermissionOptionKind::RejectOnce),
        ],
    );
    if let Some(cwd) = &call.cwd {
        request = request.meta(iwork_meta(json!({ CWD_KEY: cwd })));
    }
    request
}

/// 后台的权限答复 → 交给 hook 的结论（翻译在 `pending::translate`，这里只取出字段）。
/// 选了允许：`_meta.iwork.updatedInput` 有就用它，没有再看命令改写；选了拒绝：带 `message`；别的一律认不出。
fn backend_answer(
    response: &RequestPermissionResponse,
    rewritten: Option<String>,
) -> BackendAnswer {
    let text = |key: &str| {
        iwork_value(response.meta.as_ref(), key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match &response.outcome {
        RequestPermissionOutcome::Selected(s) if s.option_id.0.as_ref() == ALLOW_ID => {
            BackendAnswer::Allow {
                updated_input: text(UPDATED_INPUT_KEY).or(rewritten),
            }
        }
        RequestPermissionOutcome::Selected(s) if s.option_id.0.as_ref() == REJECT_ID => {
            BackendAnswer::Reject {
                message: text(DENY_MESSAGE_KEY).unwrap_or_else(|| REASON_DENIED.to_owned()),
            }
        }
        _ => BackendAnswer::Unrecognized,
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::v1::schema::SelectedPermissionOutcome;

    use super::*;

    #[test]
    fn session_ids_are_uuid_v4() {
        let id = new_session_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(id, new_session_id());
    }

    fn selected(option: &str, meta: Option<Value>) -> RequestPermissionResponse {
        let mut r = RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(option.to_owned()),
        ));
        if let Some(m) = meta {
            r = r.meta(iwork_meta(m));
        }
        r
    }

    #[test]
    fn backend_answers() {
        assert_eq!(
            backend_answer(&selected(ALLOW_ID, None), None),
            BackendAnswer::Allow {
                updated_input: None
            }
        );
        assert_eq!(
            backend_answer(
                &selected(
                    ALLOW_ID,
                    Some(json!({ UPDATED_INPUT_KEY: "{\"command\":\"x\"}" }))
                ),
                Some("{\"command\":\"rewritten\"}".into())
            ),
            BackendAnswer::Allow {
                updated_input: Some("{\"command\":\"x\"}".into())
            },
            "后台给的改写优先"
        );
        assert_eq!(
            backend_answer(
                &selected(REJECT_ID, Some(json!({ DENY_MESSAGE_KEY: "不行" }))),
                None
            ),
            BackendAnswer::Reject {
                message: "不行".into()
            }
        );
        assert_eq!(
            backend_answer(&selected("whatever", None), None),
            BackendAnswer::Unrecognized
        );
        assert_eq!(
            backend_answer(
                &RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
                None
            ),
            BackendAnswer::Unrecognized
        );
    }
}
