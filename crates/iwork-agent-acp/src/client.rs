//! `AgentPort` 的 ACP 实现：每条会话一个运行时进程，后台服务经它的 stdin / stdout 说 ACP（客户端端）。
//!
//! 一条会话的数据流：
//! - 运行时的 `session/update` 在连接的派发循环里**同步**转成 [`AgentUpdate`] 放进当前回合的队列；
//! - `session/request_permission` 在派发循环外的任务里交给 [`PermissionHandler`]，不占住派发循环，
//!   所以等审批期间其他消息照常处理，而且转给处理方之前，它之前到达的更新都已进了队列（顺序保证，
//!   编排核心据此把「工具调用开始」排在「等待审批」之前）。权限任务只转发处理方的返回值，自己不选
//!   「已取消」：回合停下时让挂起的请求返回是处理方的约定（编排停止受理，T06 方案 2.6 第 3、5 条）；
//! - prompt 响应到达时推最后一条（结束或失败），然后关闭队列；
//! - 等任何请求的响应时都同时等进程退出：进程的后代可能还拿着 stdout，读端收不到 EOF，
//!   不能只靠连接断开发现崩溃；
//! - 取消：先等这条会话上的权限任务都把答复写出去（最多 3 秒），再发 `session/cancel`：已交付的「允许」
//!   因此排在取消之前到达运行时。然后等 prompt 结束（最多 3 秒），**总是**结束整棵进程树并确认清空，
//!   才推「已取消」。运行时配合取消时它起的子进程也可能还在跑，取消的完成条件是整棵树没了；
//!   会话随之失效，下一回合按运行时会话 id 续接；
//! - 运行时的上报（用量、额度、实际模型）挂在 `session/update` 通知的 `_meta` 上，解出来进当前回合；
//! - 运行时在错误响应里点名的错误码经 [`crate::bridge_error`] 映射，不回显运行时的文字。

use std::future::Future;
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
    RuntimeCapabilities, SandboxStatus, SessionSpec, TurnOutcome, UpdateStream,
};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

use crate::bridge_error::{self, ErrorContext};
use crate::convert::{self, CLI_KEY, InitMeta, META_KEY, SessionMeta};
use crate::diagnostics::RawLog;
use crate::frame::read_frame;
use crate::process::{GRACE_PERIOD, Pipes, ResourceSample, RuntimeLaunch, SupervisedProcess};
use crate::registry::{CliInfo, IncompatibleCombo, KNOWN_INCOMPATIBLE, RuntimeRegistration};
use crate::turns::TurnBook;

/// 出错后等进程退出的时限：用来区分「进程死了」和「协议出错」。
const EXIT_PROBE: Duration = Duration::from_secs(1);
/// 运行时发来的单条消息（一行）的字节上限。超过即视为运行时故障：结束进程树，不再读。
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// 开会话时 `spec.mcp` 非空的报错：MCP 的启动配置与密钥随本机导入功能（T10）接入。
pub const MCP_UNSUPPORTED: &str = "M1 本机导入功能未接入前不支持挂载 MCP";

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

    /// 起进程、握手、做兼容检查。成功时进程与连接都可用。`model` 是要开的会话请求的模型
    /// （能力探测时为空），只用来给运行时点名的错误补上模型名。
    async fn connect<H: PermissionHandler + 'static>(
        &self,
        handler: Option<Arc<H>>,
        model: &str,
    ) -> Result<Connection, AgentError> {
        let (process, pipes) =
            SupervisedProcess::spawn(&self.launch).map_err(|e| AgentError::Protocol {
                message: e.to_string(),
            })?;
        let process = Arc::new(process);
        let shared = Arc::new(Shared::default());
        let cx = start_connection(pipes, shared.clone(), process.clone(), handler).await?;
        let init = InitializeRequest::new(ProtocolVersion::V1)
            .client_capabilities(ClientCapabilities::new())
            .client_info(Implementation::new(
                "iwork",
                env!("CARGO_PKG_VERSION").to_string(),
            ));
        let sent = cx.connection.send_request(init).block_task();
        let handshake = ErrorContext {
            model: model.to_owned(),
            version: String::new(),
        };
        let response = guarded(&process, &shared, &cx, &handshake, "initialize", sent).await?;
        let parsed = registration_of(&response)
            .and_then(|r| convert::init_meta(response.meta.as_ref()).map(|m| (r, m)));
        let (registration, init_meta) = parsed.map_err(|message| {
            process.kill_tree();
            AgentError::Protocol { message }
        })?;
        if let Err(reason) = crate::registry::check_compatibility(&registration, self.incompatible)
        {
            process.kill_tree();
            return Err(AgentError::Incompatible { version: reason });
        }
        let errors = ErrorContext {
            model: model.to_owned(),
            version: registration
                .cli
                .as_ref()
                .map_or(&registration.adapter_version, |c| &c.version)
                .clone(),
        };
        Ok(Connection {
            process,
            shared,
            cx,
            registration,
            init: response,
            init_meta,
            errors,
        })
    }
}

impl AgentPort for AcpRuntime {
    type Session = AcpSession;

    /// 起一个探测进程握手后立即结束它。能力随运行时版本变，不缓存。
    async fn capabilities(&self) -> Result<RuntimeCapabilities, AgentError> {
        let conn = self.connect::<NoPermissions>(None, "").await?;
        let caps = capabilities_of(&conn.init, &conn.init_meta);
        conn.cx.stop();
        conn.process.shutdown().await;
        Ok(caps)
    }

    async fn open_session<H: PermissionHandler + 'static>(
        &self,
        spec: SessionSpec,
        on_permission: H,
    ) -> Result<AcpSession, AgentError> {
        let conn = self
            .connect(Some(Arc::new(on_permission)), &spec.model.model_id.0)
            .await?;
        match open(&conn, &spec).await {
            Ok((session_id, meta)) => Ok(AcpSession {
                session_id,
                sandbox: meta.sandbox,
                conn,
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
/// 返回运行时的会话 id 与响应里的约定项；续接后运行时换了 id 就用新的。
async fn open(conn: &Connection, spec: &SessionSpec) -> Result<(String, SessionMeta), AgentError> {
    if !spec.mcp.is_empty() {
        // 端口只给了 id 与名字；启动配置与密钥随本机导入功能（T10）接入后才能透传
        return Err(AgentError::Protocol {
            message: MCP_UNSUPPORTED.into(),
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
    let meta = convert::session_setup_meta(spec)?;
    let cx = &conn.cx.connection;
    let session_meta = |m: Option<&agent_client_protocol::v1::schema::Meta>| {
        convert::session_meta(m).map_err(|message| AgentError::Protocol { message })
    };
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
            let sent = cx.send_request(req).block_task();
            let r = conn.guarded("session/load", sent).await?;
            let meta = session_meta(r.meta.as_ref())?;
            let id = meta.cli_session_id.clone().unwrap_or_else(|| old.clone());
            Ok((id, meta))
        }
        None => {
            let req = NewSessionRequest::new(spec.cwd.clone())
                .system_prompt(system_prompt)
                .meta(meta);
            let sent = cx.send_request(req).block_task();
            let r = conn.guarded("session/new", sent).await?;
            let meta = session_meta(r.meta.as_ref())?;
            Ok((r.session_id.0.to_string(), meta))
        }
    }
}

/// 等一个请求的响应，同时等进程退出。进程先退出就停掉连接、结束进程树，报进程退出。
async fn guarded<T>(
    process: &SupervisedProcess,
    shared: &Shared,
    cx: &ConnectionHandle,
    errors: &ErrorContext,
    method: &str,
    request: impl Future<Output = Result<T, agent_client_protocol::Error>>,
) -> Result<T, AgentError> {
    tokio::select! {
        result = request => match result {
            Ok(response) => Ok(response),
            Err(e) => Err(failure(process, shared, errors, method, &e).await),
        },
        exit = process.exited() => {
            cx.stop();
            process.kill_tree();
            Err(exited(shared, method, &exit))
        }
    }
}

fn exited(shared: &Shared, method: &str, exit: &crate::process::ExitInfo) -> AgentError {
    match shared.fatal() {
        Some(reason) => AgentError::Protocol { message: reason },
        None => AgentError::ProcessExited {
            message: format!("{method} 期间运行时退出（{}）", exit.description),
        },
    }
}

/// 一次请求失败时判断原因：运行时点名了错误码就按它（[`bridge_error`]）；进程已经（或马上）
/// 退出就是进程退出，否则是协议错误。只报方法名与错误码：错误说明由运行时给出，可能夹带正文
/// 或凭据，不进事件与日志。
async fn failure(
    process: &SupervisedProcess,
    shared: &Shared,
    errors: &ErrorContext,
    method: &str,
    error: &agent_client_protocol::Error,
) -> AgentError {
    if let Some(reason) = shared.fatal() {
        return AgentError::Protocol { message: reason };
    }
    // 点名的错误码优先于「进程退出」：运行时报了错再退出（如审批兜底触发后结束 CLI）是正常次序
    if let Some(named) = bridge_error::map(error.data.as_ref(), errors) {
        return named;
    }
    match process.wait_exit(EXIT_PROBE).await {
        Some(exit) => exited(shared, method, &exit),
        None => AgentError::Protocol {
            message: format!("{method} 失败（错误码 {:?}）", error.code),
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
                // 不回显收到的值：它来自运行时，内容不可信
                _ => {
                    return Err(format!(
                        "_meta.{META_KEY}.{CLI_KEY} 须同时有字符串 path 与 version"
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

fn capabilities_of(init: &InitializeResponse, meta: &InitMeta) -> RuntimeCapabilities {
    let caps = &init.agent_capabilities;
    RuntimeCapabilities {
        supports_resume: caps.load_session,
        supports_image_input: caps.prompt_capabilities.image,
        // 思考强度、可选模型、平台沙箱 ACP 稳定版没有字段，由适配器在 `_meta` 里报（T06 方案 2.5）
        effort_levels: meta.effort_levels.clone(),
        sandbox_platform: meta.sandbox_platform,
        model_options: meta.model_options.clone(),
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
    init_meta: InitMeta,
    /// 运行时点名错误码时补上的模型与版本
    errors: ErrorContext,
}

impl Connection {
    async fn guarded<T>(
        &self,
        method: &str,
        request: impl Future<Output = Result<T, agent_client_protocol::Error>>,
    ) -> Result<T, AgentError> {
        guarded(
            &self.process,
            &self.shared,
            &self.cx,
            &self.errors,
            method,
            request,
        )
        .await
    }
}

/// 连接任务的句柄。
#[derive(Debug)]
struct ConnectionHandle {
    connection: ConnectionTo<Agent>,
    stopper: Stopper,
}

/// 让连接任务收尾退出的开关，可交给别的任务；只生效一次。
#[derive(Debug, Clone)]
struct Stopper(Arc<Mutex<Option<oneshot::Sender<()>>>>);

impl Stopper {
    fn stop(&self) {
        if let Some(tx) = self.0.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = tx.send(());
        }
    }
}

impl ConnectionHandle {
    fn stop(&self) {
        self.stopper.stop();
    }

    fn stopper(&self) -> Stopper {
        self.stopper.clone()
    }
}

/// 派发循环与会话方法共用的状态。
#[derive(Debug, Default)]
struct Shared {
    /// 当前回合的账；收尾方由它的锁决定（见 [`crate::turns`]）
    turns: TurnBook,
    /// 还没把答复写出去的权限任务数。`cancel()` 等它归零再发 `session/cancel`
    permission_tasks: watch::Sender<usize>,
    raw: RawLog,
    /// 连接层发现的致命错误（如消息超长），优先于「进程退出」报给上层
    fatal: Mutex<Option<String>>,
}

impl Shared {
    fn fatal(&self) -> Option<String> {
        self.fatal.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// 一个还没把答复写出去的权限任务：登记时加一，丢弃（答复已写出或任务结束）时减一。
struct PermissionTask(watch::Sender<usize>);

impl PermissionTask {
    fn start(count: &watch::Sender<usize>) -> Self {
        count.send_modify(|n| *n += 1);
        Self(count.clone())
    }
}

impl Drop for PermissionTask {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
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

/// 读掉运行时的 stderr，只记每行长度：内容可能是任何东西，不进后台服务的日志。
async fn drain_stderr<R: AsyncRead + Unpin>(mut stderr: R, shared: Arc<Shared>) {
    let mut buf = [0u8; 8192];
    let mut line_bytes = 0usize;
    loop {
        match stderr.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for b in &buf[..n] {
                    if *b == b'\n' {
                        shared.raw.record_stderr(line_bytes);
                        line_bytes = 0;
                    } else {
                        line_bytes += 1;
                    }
                }
            }
        }
    }
    if line_bytes > 0 {
        shared.raw.record_stderr(line_bytes);
    }
}

/// 把运行时的 stdin / stdout 接成 ACP 连接，在后台任务里跑；返回连接句柄。
async fn start_connection<H: PermissionHandler + 'static>(
    pipes: Pipes,
    shared: Arc<Shared>,
    process: Arc<SupervisedProcess>,
    handler: Option<Arc<H>>,
) -> Result<ConnectionHandle, AgentError> {
    let Pipes {
        stdin,
        stdout,
        stderr,
    } = pipes;
    tokio::spawn(drain_stderr(stderr, shared.clone()));
    let reader = shared.clone();
    let incoming = futures::stream::unfold(BufReader::new(stdout), move |mut out| {
        let shared = reader.clone();
        let process = process.clone();
        async move {
            match read_frame(&mut out, MAX_FRAME_BYTES).await {
                Ok(Some(line)) => {
                    shared.raw.record_incoming(&line);
                    Some((Ok(line), out))
                }
                Ok(None) => None,
                Err(e) => {
                    // 读坏了（超长、非 UTF-8）就不再信任这个运行时
                    shared.raw.record_rejected_frame();
                    *shared.fatal.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
                    process.kill_tree();
                    Some((Err(e), out))
                }
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
                let report = convert::report_of(n.meta.as_ref());
                // 先送通知本身的更新（上报照约定挂在空的 session_info_update 上，转换后什么都不产生；
                // 挂在别的更新上也不丢那条更新），再送上报
                on_update.turns.on_update(n.update);
                match report {
                    Some(Ok(report)) => on_update.turns.on_report(report),
                    // 解不出来的上报只记一笔诊断：它不改变回合的走向，不值得中断会话
                    Some(Err(_)) => on_update.raw.record_bad_report(),
                    None => {}
                }
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
                // 在派发循环里登记：之后才调的 cancel() 一定等得到它
                let in_flight = PermissionTask::start(&on_request.permission_tasks);
                // 在派发循环里取：此前到达的更新都已进了队列，这个数就是它们的条数
                let updates_before = on_request.turns.emitted();
                let shared = on_request.clone();
                // 不在派发循环里等审批：等待期间其他消息要照常处理
                cx.spawn(async move {
                    let _in_flight = in_flight;
                    let request = match convert::permission_request(&req, updates_before) {
                        Ok(request) => request,
                        // 约定项读不懂：不交给处理方，按拒绝答复运行时（失败即关闭）
                        Err(message) => {
                            shared.raw.record_bad_permission_meta();
                            let deny = iwork_ports::agent::PermissionDecision::Deny { message };
                            return responder
                                .respond(convert::permission_response(&req.options, &deny));
                        }
                    };
                    // 处理方返回什么就答什么：回执已经发出的结论没有第二个地方能把它换掉
                    let decision = handler.on_permission(request).await;
                    responder.respond(convert::permission_response(&req.options, &decision))
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
        stopper: Stopper(Arc::new(Mutex::new(Some(stop_tx)))),
    })
}

// ---------- 会话 ----------

/// 一条 ACP 会话。丢弃时结束运行时进程树。
#[derive(Debug)]
pub struct AcpSession {
    session_id: String,
    sandbox: SandboxStatus,
    conn: Connection,
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

    fn permission_wait_limit(&self) -> Option<Duration> {
        self.conn.init_meta.permission_wait_limit
    }

    fn sandbox(&self) -> SandboxStatus {
        self.sandbox
    }

    async fn prompt(&self, blocks: Vec<ContentBlock>) -> Result<AcpUpdates, AgentError> {
        if let Some(exit) = self.conn.process.exit_info() {
            return Err(AgentError::ProcessExited {
                message: format!("运行时已退出（{}）", exit.description),
            });
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let turn_id = self.conn.shared.turns.begin(tx)?;
        let request = PromptRequest::new(
            SessionId::new(self.session_id.clone()),
            blocks.iter().map(convert::content_to_acp).collect(),
        );
        let sent = self.conn.cx.connection.send_request(request).block_task();
        let shared = self.conn.shared.clone();
        let process = self.conn.process.clone();
        let stopper = self.conn.cx.stopper();
        let errors = self.conn.errors.clone();
        tokio::spawn(async move {
            let last = tokio::select! {
                result = sent => match result {
                    Ok(response) => AgentUpdate::Finished(convert::outcome_of(response.stop_reason)),
                    Err(e) => AgentUpdate::Failed(
                        failure(&process, &shared, &errors, "session/prompt", &e).await,
                    ),
                },
                exit = process.exited() => {
                    stopper.stop();
                    process.kill_tree();
                    AgentUpdate::Failed(exited(&shared, "session/prompt", &exit))
                }
            };
            // 取消在先时终态由取消任务推：它要等整棵树结束
            shared.turns.complete(turn_id, last);
        });
        Ok(AcpUpdates { rx })
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        // 没有进行中的回合（prompt 已先收尾）或已经在取消：什么都不做，更不去结束进程树
        let Some((turn_id, mut prompt_done)) = self.conn.shared.turns.request_cancel() else {
            return Ok(());
        };
        // 先让权限任务把答复写出去：调用方（编排）此前已停止受理，它们都已有结论、不再等编排；
        // GRACE_PERIOD 只是处理方不守约定时的上限
        let mut tasks = self.conn.shared.permission_tasks.subscribe();
        let _ = tokio::time::timeout(GRACE_PERIOD, tasks.wait_for(|n| *n == 0)).await;
        let notified = self
            .conn
            .cx
            .connection
            .send_notification(CancelNotification::new(SessionId::new(
                self.session_id.clone(),
            )));
        // 等 prompt 结束（最多 3 秒，技术设计 6.3 第 5 条），然后不论运行时是否配合，都结束整棵
        // 进程树并确认清空：运行时起的子进程不会因为 prompt 结束而停下
        let shared = self.conn.shared.clone();
        let process = self.conn.process.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(GRACE_PERIOD, prompt_done.wait_for(|done| *done)).await;
            let last = if process.kill_tree_and_wait().await {
                AgentUpdate::Finished(TurnOutcome::Cancelled)
            } else {
                AgentUpdate::Failed(AgentError::ProcessExited {
                    message: "取消后运行时进程树没能在时限内结束".into(),
                })
            };
            shared
                .turns
                .finish_cancel(turn_id, last, "运行时已在取消时结束，需要续接会话".into());
        });
        notified.map_err(|e| AgentError::Protocol {
            message: format!("发送取消通知失败（错误码 {:?}）", e.code),
        })
    }

    async fn terminate(&self) -> Result<(), AgentError> {
        self.conn.cx.stop();
        terminate_tree(self.conn.process.as_ref()).await
    }

    async fn close(self) -> Result<(), AgentError> {
        self.conn.cx.stop();
        self.conn.process.shutdown().await;
        Ok(())
    }
}

/// 结束整棵进程树并确认清空。平台实现是 [`SupervisedProcess`]；测试换成确认不了的替身，
/// 证明「清不掉」不会被当成清掉了。
trait ProcessTreeKill {
    fn kill_tree_and_wait(&self) -> impl Future<Output = bool> + Send;
}

impl ProcessTreeKill for SupervisedProcess {
    fn kill_tree_and_wait(&self) -> impl Future<Output = bool> + Send {
        SupervisedProcess::kill_tree_and_wait(self)
    }
}

async fn terminate_tree(tree: &impl ProcessTreeKill) -> Result<(), AgentError> {
    if tree.kill_tree_and_wait().await {
        Ok(())
    } else {
        Err(AgentError::ProcessExited {
            message: "运行时进程树没能在时限内结束".into(),
        })
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// 进程树确认不了清空（平台查询一直说「还有进程」）。
    struct Unkillable(AtomicUsize);

    impl ProcessTreeKill for Unkillable {
        async fn kill_tree_and_wait(&self) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst);
            false
        }
    }

    /// 清不掉的树报 `ProcessExited`，不报成功；每次调用都真的再结束一次，不缓存上次的失败。
    #[tokio::test]
    async fn terminate_reports_unkillable_tree() {
        let tree = Unkillable(AtomicUsize::new(0));
        for round in 1..=2 {
            assert_eq!(
                terminate_tree(&tree).await,
                Err(AgentError::ProcessExited {
                    message: "运行时进程树没能在时限内结束".into()
                })
            );
            assert_eq!(tree.0.load(Ordering::SeqCst), round);
        }
    }
}
