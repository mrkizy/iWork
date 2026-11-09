//! `AgentPort` 的 ACP 实现：每条会话一个运行时进程，后台服务经它的 stdin / stdout 说 ACP（客户端端）。
//!
//! 一条会话的数据流：
//! - 运行时的 `session/update` 在连接的派发循环里**同步**转成 [`AgentUpdate`] 放进当前回合的队列；
//! - `session/request_permission` 在派发循环外的任务里交给 [`PermissionHandler`]，不占住派发循环，
//!   所以等审批期间其他消息照常处理，而且转给处理方之前，它之前到达的更新都已进了队列（顺序保证，
//!   编排核心据此把「工具调用开始」排在「等待审批」之前）；
//! - prompt 响应到达时推最后一条（结束或失败），然后关闭队列。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CancelNotification, ClientCapabilities, ContentBlock as AcpContentBlock, Implementation,
    InitializeRequest, InitializeResponse, LoadSessionRequest, NewSessionRequest, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SessionId,
    SessionNotification, SystemPrompt, SystemPromptMode, TextContent,
};
use agent_client_protocol::{Agent, Client, Lines};
use iwork_ports::agent::{
    AgentError, AgentPort, AgentSession, AgentUpdate, ContentBlock, PermissionHandler,
    RuntimeCapabilities, SessionSpec, TurnOutcome, UpdateStream,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

use crate::convert::{self, CLI_KEY, META_KEY, UpdateMapper};
use crate::diagnostics::RawLog;
use crate::process::{GRACE_PERIOD, ResourceSample, RuntimeLaunch, SupervisedProcess};
use crate::registry::{CliInfo, IncompatibleCombo, KNOWN_INCOMPATIBLE, RuntimeRegistration};

/// 出错后等进程退出的时限：用来区分「进程死了」和「协议出错」。
const EXIT_PROBE: Duration = Duration::from_secs(1);

/// 一种 ACP 运行时：怎么启动它，以及哪些版本组合不许运行。
#[derive(Debug, Clone)]
pub struct AcpRuntime {
    launch: RuntimeLaunch,
    incompatible: &'static [IncompatibleCombo],
}

impl AcpRuntime {
    /// 用已知不兼容表 [`KNOWN_INCOMPATIBLE`]。
    pub fn new(launch: RuntimeLaunch) -> Self {
        Self::with_incompatible(launch, KNOWN_INCOMPATIBLE)
    }

    pub fn with_incompatible(
        launch: RuntimeLaunch,
        incompatible: &'static [IncompatibleCombo],
    ) -> Self {
        Self {
            launch,
            incompatible,
        }
    }

    /// 起进程、握手、做兼容检查。成功时进程与连接都可用。
    async fn connect<H: PermissionHandler + 'static>(
        &self,
        handler: Option<Arc<H>>,
    ) -> Result<Connection, AgentError> {
        let (process, stdin, stdout) =
            SupervisedProcess::spawn(&self.launch).map_err(|e| AgentError::Protocol {
                message: e.to_string(),
            })?;
        let process = Arc::new(process);
        let shared = Arc::new(Shared::default());
        let cx = start_connection(stdin, stdout, shared.clone(), handler).await?;
        let init = InitializeRequest::new(ProtocolVersion::V1)
            .client_capabilities(ClientCapabilities::new())
            .client_info(Implementation::new(
                "iwork",
                env!("CARGO_PKG_VERSION").to_string(),
            ));
        let response = match cx.connection.send_request(init).block_task().await {
            Ok(r) => r,
            Err(e) => return Err(failure(&process, "initialize", e).await),
        };
        let registration = registration_of(&response).map_err(|message| {
            process.kill_tree();
            AgentError::Protocol { message }
        })?;
        if let Err(reason) = crate::registry::check_compatibility(&registration, self.incompatible)
        {
            process.kill_tree();
            return Err(AgentError::Incompatible { version: reason });
        }
        Ok(Connection {
            process,
            shared,
            cx,
            registration,
            init: response,
        })
    }
}

impl AgentPort for AcpRuntime {
    type Session = AcpSession;

    /// 起一个探测进程握手后立即结束它。能力随运行时版本变，不缓存。
    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        let conn = self.connect::<NoPermissions>(None).await?;
        let caps = capabilities_of(&conn.init);
        conn.cx.stop();
        conn.process.shutdown().await;
        Ok(caps)
    }

    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        spec: SessionSpec,
        on_permission: H,
    ) -> Result<AcpSession, AgentError> {
        let conn = self.connect(Some(Arc::new(on_permission))).await?;
        match open(&conn, &spec).await {
            Ok(session_id) => Ok(AcpSession {
                session_id,
                conn,
                next_turn: Mutex::new(0),
            }),
            Err(e) => {
                conn.cx.stop();
                conn.process.kill_tree();
                Err(e)
            }
        }
    }
}

/// 建会话：有旧会话 id 就续接（`session/load`），否则新建。会话配置放进 `_meta`。
async fn open(conn: &Connection, spec: &SessionSpec) -> Result<String, AgentError> {
    if !spec.mcp.is_empty() {
        // MCP 服务的启动配置（命令、密钥）随本机导入功能接入，这里还没有可下发的内容
        return Err(AgentError::Protocol {
            message: "暂不支持给会话挂 MCP 服务".into(),
        });
    }
    let caps = &conn.init.agent_capabilities;
    let system_prompt = if spec.system_prompt.is_empty() {
        None
    } else if caps.session_capabilities.system_prompt.is_some() {
        Some(
            SystemPrompt::new(vec![AcpContentBlock::Text(TextContent::new(
                spec.system_prompt.clone(),
            ))])
            .mode(SystemPromptMode::Append),
        )
    } else {
        return Err(AgentError::Protocol {
            message: format!(
                "运行时 {} 不支持会话系统提示",
                conn.registration.adapter_name
            ),
        });
    };
    let meta = convert::session_setup_meta(&spec.model, &spec.constraints, &spec.tool_set)?;
    let cx = &conn.cx.connection;
    match &spec.resume {
        Some(old) => {
            if !caps.load_session {
                return Err(AgentError::Protocol {
                    message: format!("运行时 {} 不支持续接会话", conn.registration.adapter_name),
                });
            }
            let req = LoadSessionRequest::new(SessionId::new(old.clone()), spec.cwd.clone())
                .system_prompt(system_prompt)
                .meta(meta);
            match cx.send_request(req).block_task().await {
                Ok(_) => Ok(old.clone()),
                Err(e) => Err(failure(&conn.process, "session/load", e).await),
            }
        }
        None => {
            let req = NewSessionRequest::new(spec.cwd.clone())
                .system_prompt(system_prompt)
                .meta(meta);
            match cx.send_request(req).block_task().await {
                Ok(r) => Ok(r.session_id.0.to_string()),
                Err(e) => Err(failure(&conn.process, "session/new", e).await),
            }
        }
    }
}

/// 一次请求失败时判断原因：进程已经（或马上）退出就是进程退出，否则是协议错误。
async fn failure(
    process: &SupervisedProcess,
    method: &str,
    error: agent_client_protocol::Error,
) -> AgentError {
    match process.wait_exit(EXIT_PROBE).await {
        Some(exit) => AgentError::ProcessExited {
            message: format!("{method} 期间运行时退出（{}）", exit.description),
        },
        None => AgentError::Protocol {
            message: format!("{method} 失败：{error}"),
        },
    }
}

fn registration_of(init: &InitializeResponse) -> Result<RuntimeRegistration, String> {
    let info = init
        .agent_info
        .as_ref()
        .ok_or("运行时的 initialize 响应缺少 agentInfo，无法登记版本")?;
    let cli = match init
        .meta
        .as_ref()
        .and_then(|m| m.get(META_KEY))
        .and_then(|v| v.get(CLI_KEY))
    {
        None => None,
        Some(v) => {
            let path = v.get("path").and_then(Value::as_str);
            let version = v.get("version").and_then(Value::as_str);
            match (path, version) {
                (Some(path), Some(version)) => Some(CliInfo {
                    path: PathBuf::from(path),
                    version: version.into(),
                }),
                _ => {
                    return Err(format!(
                        "_meta.{META_KEY}.{CLI_KEY} 须同时有字符串 path 与 version：{v}"
                    ));
                }
            }
        }
    };
    Ok(RuntimeRegistration {
        adapter_name: info.name.clone(),
        adapter_version: info.version.clone(),
        protocol_version: init.protocol_version.as_u16(),
        cli,
    })
}

fn capabilities_of(init: &InitializeResponse) -> RuntimeCapabilities {
    let caps = &init.agent_capabilities;
    RuntimeCapabilities {
        supports_resume: caps.load_session,
        supports_image_input: caps.prompt_capabilities.image,
        // 思考强度的可选值随模型接法（技术设计 9.6）由各适配器报告，ACP 稳定版没有这个字段
        effort_levels: Vec::new(),
    }
}

// ---------- 连接 ----------

/// 一个运行时进程及其 ACP 连接。
#[derive(Debug)]
struct Connection {
    process: Arc<SupervisedProcess>,
    shared: Arc<Shared>,
    cx: ConnectionHandle,
    registration: RuntimeRegistration,
    init: InitializeResponse,
}

/// 连接任务的句柄：`stop` 让连接任务收尾退出。
#[derive(Debug)]
struct ConnectionHandle {
    connection: ConnectionTo<Agent>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
}

impl ConnectionHandle {
    fn stop(&self) {
        if let Some(tx) = self.stop.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = tx.send(());
        }
    }
}

/// 派发循环与会话方法共用的状态。
#[derive(Debug, Default)]
struct Shared {
    /// 正在进行的回合；只有它的队列收更新
    turn: Mutex<Option<ActiveTurn>>,
    /// 每次取消加一；挂起的权限请求看到它变化就以「已取消」答复运行时
    cancel_epoch: watch::Sender<u64>,
    raw: RawLog,
}

#[derive(Debug)]
struct ActiveTurn {
    id: u64,
    tx: mpsc::UnboundedSender<AgentUpdate>,
    mapper: UpdateMapper,
    cancel_requested: bool,
}

impl Shared {
    fn lock_turn(&self) -> std::sync::MutexGuard<'_, Option<ActiveTurn>> {
        self.turn.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn on_update(&self, notification: SessionNotification) {
        let mut turn = self.lock_turn();
        let Some(active) = turn.as_mut() else {
            return; // 回合之外的更新（如会话加载时的历史回放）只留在诊断记录里
        };
        if let Some(update) = active.mapper.map(notification.update) {
            let _ = active.tx.send(update);
        }
    }
}

/// 没有权限请求的连接（能力探测）用的处理方：任何请求都拒绝。
struct NoPermissions;

impl PermissionHandler for NoPermissions {
    async fn on_permission(
        &self,
        _req: iwork_ports::agent::PermissionRequest,
    ) -> iwork_ports::agent::PermissionDecision {
        iwork_ports::agent::PermissionDecision::Deny {
            message: "能力探测连接不处理权限请求".into(),
        }
    }
}

/// 把运行时的 stdin / stdout 接成 ACP 连接，在后台任务里跑；返回连接句柄。
async fn start_connection<H: PermissionHandler + 'static>(
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    shared: Arc<Shared>,
    handler: Option<Arc<H>>,
) -> Result<ConnectionHandle, AgentError> {
    let raw = shared.clone();
    let incoming = futures::stream::unfold(BufReader::new(stdout).lines(), move |mut lines| {
        let raw = raw.clone();
        async move {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    raw.raw.record(&line);
                    Some((Ok(line), lines))
                }
                Ok(None) => None,
                Err(e) => Some((Err(e), lines)),
            }
        }
    });
    let outgoing = futures::sink::unfold(stdin, |mut w, line: String| async move {
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok::<_, std::io::Error>(w)
    });
    let transport = Lines::new(Box::pin(outgoing), Box::pin(incoming));

    let (cx_tx, cx_rx) = oneshot::channel();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let on_update = shared.clone();
    let on_request = shared.clone();
    let builder = Client
        .v1()
        .name("iwork")
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                on_update.on_update(n);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: RequestPermissionRequest, responder, cx: ConnectionTo<Agent>| {
                let Some(handler) = handler.clone() else {
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                };
                let mut epoch = on_request.cancel_epoch.subscribe();
                epoch.mark_unchanged();
                // 不在派发循环里等审批：等待期间其他消息要照常处理
                cx.spawn(async move {
                    let request = convert::permission_request(&req);
                    let decision = tokio::select! {
                        d = handler.on_permission(request) => Some(d),
                        _ = epoch.changed() => None,
                    };
                    let response = match decision {
                        Some(d) => convert::permission_response(&req.options, &d),
                        None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
                    };
                    responder.respond(response)
                })
            },
            agent_client_protocol::on_receive_request!(),
        );
    tokio::spawn(async move {
        let _ = builder
            .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
                let _ = cx_tx.send(cx.clone());
                tokio::select! {
                    _ = stop_rx => {},
                    _ = cx.terminated() => {},
                }
                Ok(())
            })
            .await;
    });
    let connection = cx_rx.await.map_err(|_| AgentError::Protocol {
        message: "ACP 连接未能建立".into(),
    })?;
    Ok(ConnectionHandle {
        connection,
        stop: Mutex::new(Some(stop_tx)),
    })
}

// ---------- 会话 ----------

/// 一条 ACP 会话。丢弃时结束运行时进程树。
#[derive(Debug)]
pub struct AcpSession {
    session_id: String,
    conn: Connection,
    next_turn: Mutex<u64>,
}

impl AcpSession {
    /// 运行时登记信息（CLI 路径与版本、适配器版本、协议版本）。
    pub fn registration(&self) -> &RuntimeRegistration {
        &self.conn.registration
    }

    /// 运行时进程号（Unix 上也是进程组号）。
    pub fn process_id(&self) -> u32 {
        self.conn.process.pid()
    }

    /// 运行时进程树是否已全部结束。
    pub fn process_tree_is_empty(&self) -> std::io::Result<bool> {
        self.conn.process.tree_is_empty()
    }

    /// 最近一次资源采样。
    pub fn latest_sample(&self) -> Option<ResourceSample> {
        self.conn.process.latest_sample()
    }

    /// 脱敏、限量后的原始消息记录，供诊断包导出。
    pub fn raw_log(&self) -> Vec<String> {
        self.conn.shared.raw.snapshot()
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        self.conn.cx.stop();
        // SupervisedProcess 随最后一个 Arc 一起丢弃并结束进程树；这里先结束，不等后台任务放手
        self.conn.process.kill_tree();
    }
}

impl AgentSession for AcpSession {
    type Updates = AcpUpdates;

    fn runtime_session_id(&self) -> Option<&str> {
        Some(&self.session_id)
    }

    async fn prompt(&self, blocks: Vec<ContentBlock>) -> Result<AcpUpdates, AgentError> {
        if let Some(exit) = self.conn.process.exit_info() {
            return Err(AgentError::ProcessExited {
                message: format!("运行时已退出（{}）", exit.description),
            });
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let turn_id = {
            let mut next = self.next_turn.lock().unwrap_or_else(|p| p.into_inner());
            *next += 1;
            *next
        };
        {
            let mut turn = self.conn.shared.lock_turn();
            if turn.is_some() {
                return Err(AgentError::Protocol {
                    message: "上一回合还没结束，不能再发 prompt".into(),
                });
            }
            *turn = Some(ActiveTurn {
                id: turn_id,
                tx,
                mapper: UpdateMapper::default(),
                cancel_requested: false,
            });
        }
        let request = PromptRequest::new(
            SessionId::new(self.session_id.clone()),
            blocks.iter().map(convert::content_to_acp).collect(),
        );
        let sent = self.conn.cx.connection.send_request(request);
        let shared = self.conn.shared.clone();
        let process = self.conn.process.clone();
        tokio::spawn(async move {
            let result = sent.block_task().await;
            let cancelled = shared
                .lock_turn()
                .as_ref()
                .is_some_and(|t| t.id == turn_id && t.cancel_requested);
            let last = match result {
                Ok(response) => AgentUpdate::Finished(convert::outcome_of(response.stop_reason)),
                // 取消后超时被结束进程树，请求随之失败：这是取消的结局，不是故障
                Err(_) if cancelled => AgentUpdate::Finished(TurnOutcome::Cancelled),
                Err(e) => AgentUpdate::Failed(failure(&process, "session/prompt", e).await),
            };
            let mut turn = shared.lock_turn();
            if turn.as_ref().is_some_and(|t| t.id == turn_id) {
                let active = turn.take().expect("刚检查过");
                let _ = active.tx.send(last);
            }
        });
        Ok(AcpUpdates { rx })
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        let turn_id = {
            let mut turn = self.conn.shared.lock_turn();
            match turn.as_mut() {
                Some(active) => {
                    active.cancel_requested = true;
                    active.id
                }
                None => return Ok(()),
            }
        };
        self.conn.shared.cancel_epoch.send_modify(|e| *e += 1);
        let notified = self
            .conn
            .cx
            .connection
            .send_notification(CancelNotification::new(SessionId::new(
                self.session_id.clone(),
            )));
        // 不管通知有没有发出去，3 秒后回合还没结束就结束进程树（技术设计 6.3 第 5 条）
        let shared = self.conn.shared.clone();
        let process = self.conn.process.clone();
        tokio::spawn(async move {
            tokio::time::sleep(GRACE_PERIOD).await;
            let still_running = shared.lock_turn().as_ref().is_some_and(|t| t.id == turn_id);
            if still_running {
                process.kill_tree();
            }
        });
        notified.map_err(|e| AgentError::Protocol {
            message: format!("发送取消通知失败：{e}"),
        })
    }

    async fn close(self) -> Result<(), AgentError> {
        self.conn.cx.stop();
        self.conn.process.shutdown().await;
        Ok(())
    }
}

/// 一次 prompt 的更新流。
#[derive(Debug)]
pub struct AcpUpdates {
    rx: mpsc::UnboundedReceiver<AgentUpdate>,
}

impl UpdateStream for AcpUpdates {
    async fn next(&mut self) -> Option<AgentUpdate> {
        self.rx.recv().await
    }
}
