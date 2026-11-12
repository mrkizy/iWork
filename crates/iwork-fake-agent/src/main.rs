//! 假运行时：按脚本扮演 ACP agent（stdin / stdout），让回合、审批、取消、崩溃场景不依赖厂商就能自动测试
//! （技术设计第 10 节）。脚本格式见 README.md。
//!
//! 用法：`iwork-fake-agent --script <脚本.json> [--record <记录.jsonl>]`
//! 记录文件每行一个 JSON：自己的进程号、收到的请求与通知（`{"method":..,"params":..}`）、权限答复、启动的子进程号。
//!
//! `_meta["iwork"]` 下的约定项（T06 方案 2.5）照同一份字符串写，不依赖 `iwork-agent-acp`；
//! 两边对不对得上由 `iwork-agent-acp` 对着它的端到端测试核对。

mod script;

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse, Meta,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind,
    PromptCapabilities, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SessionCapabilities, SessionId, SessionInfoUpdate,
    SessionNotification, SessionSystemPromptCapabilities, SessionUpdate, StopReason, TextContent,
    ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, Lines};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use iwork_proto::pb;
use prost::Message as _;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::script::{Script, Step};

/// iWork 在 ACP `_meta` 里占用的键。
const META_KEY: &str = "iwork";
const ALLOW_ID: &str = "allow-once";
const REJECT_ID: &str = "reject-once";
/// 崩溃前留给已发出消息写出的时间。
const CRASH_FLUSH: Duration = Duration::from_millis(200);

/// 记录文件。没给 `--record` 时什么都不写。
struct Recorder(Option<Mutex<std::fs::File>>);

impl Recorder {
    fn write(&self, value: Value) {
        if let Some(file) = &self.0 {
            let mut file = file.lock().unwrap_or_else(|p| p.into_inner());
            // 整行（含换行）拼好后一次写出：`writeln!` 会分几次写，测试并发读时可能读到半行。
            // 记录写不进去说明测试环境坏了，直接让进程失败，比静默丢记录好查
            let line = format!("{value}\n");
            file.write_all(line.as_bytes()).expect("写记录文件失败");
            file.flush().expect("写记录文件失败");
        }
    }

    fn request(&self, method: &str, params: &impl serde::Serialize) {
        let params = serde_json::to_value(params).expect("ACP 类型都能序列化");
        self.write(json!({ "method": method, "params": params }));
    }
}

struct State {
    script: Script,
    record: Recorder,
    next_turn: Mutex<usize>,
    /// 已开始的工具调用：权限请求要带上它的名字、标题与输入，和真实适配器一样
    tool_calls: Mutex<HashMap<String, ToolCallUpdateFields>>,
    /// 当前回合是否已被取消；每次 prompt 开始时清零
    cancelled: watch::Sender<bool>,
}

/// 到达记录：认出客户端发来的每一行是什么。发出的权限请求按 JSON-RPC id 记下工具调用 id，
/// 答复到达时据此点名。
#[derive(Default)]
struct Arrivals {
    permission_ids: Mutex<HashMap<String, String>>,
}

impl Arrivals {
    /// 记下发出去的权限请求。
    fn sent(&self, line: &str) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if v["method"] == "session/request_permission"
            && let Some(tool) = v["params"]["toolCall"]["toolCallId"].as_str()
        {
            self.permission_ids
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(v["id"].to_string(), tool.to_owned());
        }
    }

    /// 一行到达的东西：请求与通知是方法名，权限答复是「permission_response <工具调用 id> <选项或 cancelled>」。
    fn arrived(&self, line: &str) -> Option<String> {
        let v = serde_json::from_str::<Value>(line).ok()?;
        if let Some(method) = v["method"].as_str() {
            return Some(method.to_owned());
        }
        let tool = self
            .permission_ids
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&v["id"].to_string())?;
        let outcome = &v["result"]["outcome"];
        let picked = outcome["optionId"]
            .as_str()
            .or(outcome["outcome"].as_str())
            .unwrap_or("error");
        Some(format!("permission_response {tool} {picked}"))
    }
}

/// 一个回合怎么结束。
enum Flow {
    Done,
    Cancelled,
}

type Conn = ConnectionTo<Client>;

fn parse_args() -> Result<(PathBuf, Option<PathBuf>), String> {
    let mut args = std::env::args_os().skip(1);
    let mut script = None;
    let mut record = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--script") => script = args.next().map(PathBuf::from),
            Some("--record") => record = args.next().map(PathBuf::from),
            _ => return Err(format!("不认识的参数：{}", arg.to_string_lossy())),
        }
    }
    let script = script.ok_or("缺少 --script <脚本.json>")?;
    Ok((script, record))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("iwork-fake-agent: {message}");
        std::process::exit(2);
    }
}

async fn run() -> Result<(), String> {
    let (script_path, record_path) = parse_args()?;
    let text = std::fs::read_to_string(&script_path)
        .map_err(|e| format!("读脚本 {} 失败：{e}", script_path.display()))?;
    let script = Script::parse(&text)?;
    let record = match record_path {
        Some(path) => Some(Mutex::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| format!("打开记录文件 {} 失败：{e}", path.display()))?,
        )),
        None => None,
    };
    let record = Recorder(record);
    record.write(json!({ "started_pid": std::process::id() }));
    let state = Arc::new(State {
        script,
        record,
        next_turn: Mutex::new(0),
        tool_calls: Mutex::new(HashMap::new()),
        cancelled: watch::channel(false).0,
    });

    // 到达记录在读到这一行时就写，早于派发：记录文件里的先后就是到达 stdin 的先后
    let arrivals = Arc::new(Arrivals::default());
    let (reader, writer) = (state.clone(), arrivals.clone());
    let incoming = futures::stream::unfold(
        (BufReader::new(tokio::io::stdin()).lines(), arrivals),
        move |(mut lines, arrivals)| {
            let state = reader.clone();
            async move {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if let Some(arrived) = arrivals.arrived(&line) {
                            state.record.write(json!({ "arrived": arrived }));
                        }
                        Some((Ok(line), (lines, arrivals)))
                    }
                    Ok(None) => None,
                    Err(e) => Some((Err(e), (lines, arrivals))),
                }
            }
        },
    );
    let outgoing = futures::sink::unfold(tokio::io::stdout(), move |mut w, line: String| {
        writer.sent(&line);
        async move {
            w.write_all(line.as_bytes()).await?;
            w.write_all(b"\n").await?;
            w.flush().await?;
            Ok::<_, std::io::Error>(w)
        }
    });
    let transport = Lines::new(Box::pin(outgoing), Box::pin(incoming));

    let (s1, s2, s3, s4, s5) = (
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
        state.clone(),
    );
    Agent
        .v1()
        .name("iwork-fake-agent")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                s1.record.request("initialize", &req);
                responder.respond(initialize_response(&s1.script))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _cx| {
                s2.record.request("session/new", &req);
                let mut response = NewSessionResponse::new(SessionId::new("fake-session-1"));
                if let Some(sandbox) = &s2.script.agent.session_sandbox {
                    response = response.meta(iwork_meta(json!({ "sandbox": sandbox })));
                }
                responder.respond(response)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, _cx| {
                s3.record.request("session/load", &req);
                let info = &s3.script.agent;
                if let Some(code) = &info.load_error {
                    return responder.respond_with_result(Err(named_error(code, None)));
                }
                let mut inner = Map::new();
                if let Some(sandbox) = &info.session_sandbox {
                    inner.insert("sandbox".into(), json!(sandbox));
                }
                if let Some(id) = &info.resumed_session_id {
                    inner.insert("cliSessionId".into(), json!(id));
                }
                let mut response = LoadSessionResponse::new();
                if !inner.is_empty() {
                    response = response.meta(iwork_meta(Value::Object(inner)));
                }
                responder.respond(response)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx: Conn| {
                s4.record.request("session/prompt", &req);
                let state = s4.clone();
                // 回合里要发权限请求并等答复，不能占住派发循环
                cx.clone().spawn(async move {
                    // 出错时回 JSON-RPC 错误，不让错误冒出任务（那会结束整个连接）
                    let result = run_prompt(&state, &cx, req.session_id)
                        .await
                        .map(PromptResponse::new);
                    responder.respond_with_result(result)
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: CancelNotification, _cx| {
                s5.record.request("session/cancel", &n);
                s5.cancelled.send_replace(true);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await
        .map_err(|e| format!("ACP 连接出错：{e}"))
}

fn initialize_response(script: &Script) -> InitializeResponse {
    let info = &script.agent;
    let mut sessions = SessionCapabilities::new();
    if info.system_prompt {
        sessions = sessions.system_prompt(SessionSystemPromptCapabilities::new());
    }
    let caps = AgentCapabilities::new()
        .load_session(info.load_session)
        .prompt_capabilities(PromptCapabilities::new().image(info.image))
        .session_capabilities(sessions);
    let mut response = InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(caps)
        .agent_info(Implementation::new(info.name.clone(), info.version.clone()));
    let mut inner = Map::new();
    if let Some(cli) = &info.cli {
        inner.insert(
            "cli".into(),
            json!({ "path": cli.path, "version": cli.version }),
        );
    }
    if let Some(v) = &info.permission_wait_limit_secs {
        inner.insert("permissionWaitLimitSecs".into(), v.clone());
    }
    if let Some(v) = &info.sandbox_platform {
        inner.insert("sandboxPlatform".into(), v.clone());
    }
    if !info.effort_levels.is_empty() {
        inner.insert("effortLevels".into(), json!(info.effort_levels));
    }
    if !info.models.is_empty() {
        let models: Vec<Value> = info
            .models
            .iter()
            .map(|m| json!({ "id": m.id, "label": m.label }))
            .collect();
        inner.insert("models".into(), Value::Array(models));
    }
    if !inner.is_empty() {
        response = response.meta(iwork_meta(Value::Object(inner)));
    }
    response
}

fn iwork_meta(inner: Value) -> Meta {
    let mut meta = Map::new();
    meta.insert(META_KEY.into(), inner);
    meta
}

fn encode_report(kind: pb::runtime_report::Kind) -> Value {
    let report = pb::RuntimeReport { kind: Some(kind) };
    Value::String(BASE64.encode(report.encode_to_vec()))
}

async fn run_prompt(
    state: &State,
    cx: &Conn,
    session: SessionId,
) -> Result<StopReason, agent_client_protocol::Error> {
    state.cancelled.send_replace(false);
    let index = {
        let mut next = state.next_turn.lock().unwrap_or_else(|p| p.into_inner());
        *next += 1;
        *next - 1
    };
    let Some(turn) = state.script.turns.get(index) else {
        return Err(agent_client_protocol::Error::internal_error()
            .data(format!("脚本只有 {} 个回合", state.script.turns.len())));
    };
    let ctx = Ctx { state, cx, session };
    Ok(match ctx.run(&turn.steps).await? {
        Flow::Cancelled => StopReason::Cancelled,
        Flow::Done => match turn.stop_reason {
            script::StopReason::EndTurn => StopReason::EndTurn,
            script::StopReason::MaxTokens => StopReason::MaxTokens,
            script::StopReason::Refusal => StopReason::Refusal,
        },
    })
}

struct Ctx<'a> {
    state: &'a State,
    cx: &'a Conn,
    session: SessionId,
}

impl Ctx<'_> {
    /// 某次工具调用的描述；脚本里没先开这个调用时为空。
    fn tool_fields(&self, id: &str) -> ToolCallUpdateFields {
        let calls = self
            .state
            .tool_calls
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        calls.get(id).cloned().unwrap_or_default()
    }

    /// 某次工具调用的权限请求（允许一次 / 拒绝一次两个选项）。
    fn permission_request(&self, tool_call_id: &str) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            self.session.clone(),
            ToolCallUpdate::new(tool_call_id.to_owned(), self.tool_fields(tool_call_id)),
            vec![
                PermissionOption::new(ALLOW_ID, "允许", PermissionOptionKind::AllowOnce),
                PermissionOption::new(REJECT_ID, "拒绝", PermissionOptionKind::RejectOnce),
            ],
        )
    }

    fn is_cancelled(&self) -> bool {
        *self.state.cancelled.borrow()
    }

    /// 等 `duration`；期间收到取消返回 `false`。
    async fn pause(&self, duration: Duration) -> bool {
        let mut rx = self.state.cancelled.subscribe();
        tokio::select! {
            _ = tokio::time::sleep(duration) => !self.is_cancelled(),
            _ = rx.wait_for(|c| *c) => false,
        }
    }

    fn send(&self, update: SessionUpdate) -> Result<(), agent_client_protocol::Error> {
        self.cx
            .send_notification(SessionNotification::new(self.session.clone(), update))
    }

    /// 上报挂在一条空的 `session_info_update` 通知的 `_meta.iwork.report` 上。
    fn report(&self, value: Value) -> Result<(), agent_client_protocol::Error> {
        self.report_on(
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new()),
            value,
        )
    }

    /// 上报挂在给定的更新上。
    fn report_on(
        &self,
        update: SessionUpdate,
        value: Value,
    ) -> Result<(), agent_client_protocol::Error> {
        let n = SessionNotification::new(self.session.clone(), update)
            .meta(iwork_meta(json!({ "report": value })));
        self.cx.send_notification(n)
    }

    fn text(&self, text: &str) -> Result<(), agent_client_protocol::Error> {
        self.send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text)),
        )))
    }

    /// 按速率输出若干片；返回是否被取消。
    async fn paced(
        &self,
        pieces: impl Iterator<Item = String>,
        rate: Option<f64>,
    ) -> Result<bool, agent_client_protocol::Error> {
        let gap = rate
            .filter(|r| *r > 0.0)
            .map(|r| Duration::from_secs_f64(1.0 / r));
        for piece in pieces {
            if self.is_cancelled() {
                return Ok(true);
            }
            self.text(&piece)?;
            if let Some(gap) = gap
                && !self.pause(gap).await
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn run<'s>(
        &'s self,
        steps: &'s [Step],
    ) -> Pin<Box<dyn Future<Output = Result<Flow, agent_client_protocol::Error>> + Send + 's>> {
        Box::pin(async move {
            for step in steps {
                if self.is_cancelled() && !matches!(step, Step::Hang) {
                    return Ok(Flow::Cancelled);
                }
                if let Flow::Cancelled = self.step(step).await? {
                    return Ok(Flow::Cancelled);
                }
            }
            Ok(Flow::Done)
        })
    }

    async fn step(&self, step: &Step) -> Result<Flow, agent_client_protocol::Error> {
        let cancelled = |c: bool| if c { Flow::Cancelled } else { Flow::Done };
        match step {
            Step::Text {
                text,
                chunk_chars,
                rate_per_sec,
            } => {
                let chars: Vec<char> = text.chars().collect();
                let size = chunk_chars.unwrap_or(chars.len()).max(1);
                let pieces = chars.chunks(size).map(|c| c.iter().collect::<String>());
                Ok(cancelled(self.paced(pieces, *rate_per_sec).await?))
            }
            Step::RepeatText {
                chunk,
                count,
                rate_per_sec,
                stamp,
            } => {
                let pieces = (0..*count).map(|_| {
                    if *stamp {
                        let nanos = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos())
                            .unwrap_or(0);
                        format!("{nanos}|{chunk}")
                    } else {
                        chunk.clone()
                    }
                });
                Ok(cancelled(self.paced(pieces, *rate_per_sec).await?))
            }
            Step::BadLine { kind, bytes } => {
                let mut line = match kind {
                    script::BadLineKind::Oversize => vec![b'x'; *bytes],
                    script::BadLineKind::InvalidUtf8 => vec![0xff, 0xfe, b'{'],
                };
                line.push(b'\n');
                // 和连接共用同一个 stdout；锁住后整行写出，不与 ACP 消息交错
                {
                    let mut out = std::io::stdout().lock();
                    let _ = out.write_all(&line);
                    let _ = out.flush();
                }
                std::future::pending().await
            }
            Step::Stderr { text } => {
                eprintln!("{text}");
                Ok(Flow::Done)
            }
            Step::ToolCall {
                id,
                name,
                title,
                input,
            } => {
                let title = title.clone().unwrap_or_else(|| name.clone());
                self.state
                    .tool_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(
                        id.clone(),
                        ToolCallUpdateFields::new()
                            .title(title.clone())
                            .name(name.clone())
                            .raw_input(input.clone()),
                    );
                let call = ToolCall::new(id.clone(), title)
                    .name(name.clone())
                    .status(ToolCallStatus::InProgress)
                    .raw_input(input.clone());
                self.send(SessionUpdate::ToolCall(call))?;
                Ok(Flow::Done)
            }
            Step::ToolResult { id, ok, output } => {
                let status = if *ok {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                };
                let fields = ToolCallUpdateFields::new().status(status).content(vec![
                    ContentBlock::Text(TextContent::new(output.clone())).into(),
                ]);
                self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    id.clone(),
                    fields,
                )))?;
                Ok(Flow::Done)
            }
            Step::Permission {
                tool_call_id,
                cwd,
                allowed,
                denied,
            } => {
                let mut request = self.permission_request(tool_call_id);
                if let Some(cwd) = cwd {
                    request = request.meta(iwork_meta(json!({ "cwd": cwd })));
                }
                let response = self.cx.send_request(request).block_task().await?;
                self.state.record.write(json!({
                    "permission_response": serde_json::to_value(&response).expect("可序列化"),
                }));
                match response.outcome {
                    RequestPermissionOutcome::Selected(s) if s.option_id.0.as_ref() == ALLOW_ID => {
                        self.run(allowed).await
                    }
                    RequestPermissionOutcome::Selected(_) => self.run(denied).await,
                    _ => Ok(Flow::Cancelled),
                }
            }
            Step::ParallelPermissions { tool_call_ids } => {
                let asks = tool_call_ids.iter().map(|id| {
                    self.cx
                        .send_request(self.permission_request(id))
                        .block_task()
                });
                for (id, response) in tool_call_ids
                    .iter()
                    .zip(futures::future::join_all(asks).await)
                {
                    self.state.record.write(json!({
                        "tool_call_id": id,
                        "permission_response": serde_json::to_value(&response?).expect("可序列化"),
                    }));
                }
                Ok(Flow::Done)
            }
            Step::Sleep { ms } => Ok(cancelled(!self.pause(Duration::from_millis(*ms)).await)),
            Step::Crash { exit_code } => {
                // 已发出的通知由连接在后台写出；给它一点时间写完，模拟「输出到一半时崩溃」
                tokio::time::sleep(CRASH_FLUSH).await;
                std::process::exit(*exit_code)
            }
            Step::Hang => std::future::pending().await,
            Step::SpawnChild { program, args } => {
                // stdin 不给子进程：ACP 的输入只属于运行时自己；stdout 照常继承（测试要的就是它拿着管道）
                let child = std::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| {
                        agent_client_protocol::Error::internal_error()
                            .data(format!("启动子进程 {program} 失败：{e}"))
                    })?;
                self.state
                    .record
                    .write(json!({ "spawned_child": child.id() }));
                Ok(Flow::Done)
            }
            Step::ReportModel {
                model_id,
                effort,
                on_text,
            } => {
                let report = encode_report(pb::runtime_report::Kind::Model(pb::ModelSelection {
                    model_id: model_id.clone(),
                    effort: effort.clone(),
                }));
                match on_text {
                    None => self.report(report)?,
                    Some(text) => self.report_on(
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new(text.clone()),
                        ))),
                        report,
                    )?,
                }
                Ok(Flow::Done)
            }
            Step::ReportUsage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                cost_usd,
            } => {
                self.report(encode_report(pb::runtime_report::Kind::Usage(pb::Usage {
                    input_tokens: *input_tokens,
                    output_tokens: *output_tokens,
                    cache_read_tokens: *cache_read_tokens,
                    cache_write_tokens: *cache_write_tokens,
                    cost_usd: *cost_usd,
                })))?;
                Ok(Flow::Done)
            }
            Step::ReportQuota { windows } => {
                let windows = windows
                    .iter()
                    .map(|w| pb::QuotaWindowReport {
                        window: w.window.clone(),
                        used_pct: w.used_pct,
                        resets_at: w
                            .resets_at_secs
                            .map(|seconds| prost_types::Timestamp { seconds, nanos: 0 }),
                    })
                    .collect();
                self.report(encode_report(pb::runtime_report::Kind::Quota(
                    pb::QuotaReport { windows },
                )))?;
                Ok(Flow::Done)
            }
            Step::ReportRaw { value } => {
                self.report(value.clone())?;
                Ok(Flow::Done)
            }
            Step::Error { code, reason } => Err(named_error(code, reason.as_deref())),
        }
    }
}

/// 带点名错误码的 JSON-RPC 错误：`data` 为 `{"iwork": {"code", "reason"?}}`。
fn named_error(code: &str, reason: Option<&str>) -> agent_client_protocol::Error {
    let mut inner = json!({ "code": code });
    if let Some(reason) = reason {
        inner["reason"] = json!(reason);
    }
    agent_client_protocol::Error::internal_error().data(json!({ META_KEY: inner }))
}
