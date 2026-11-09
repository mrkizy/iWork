//! 假运行时：按脚本扮演 ACP agent（stdin / stdout），让回合、审批、取消、崩溃场景不依赖厂商就能自动测试
//! （技术设计第 10 节）。脚本格式见 README.md。
//!
//! 用法：`iwork-fake-agent --script <脚本.json> [--record <记录.jsonl>]`
//! 记录文件每行一个 JSON：自己的进程号、收到的请求与通知（`{"method":..,"params":..}`）、权限答复、启动的子进程号。

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
    InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind,
    PromptCapabilities, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SessionCapabilities, SessionId, SessionNotification,
    SessionSystemPromptCapabilities, SessionUpdate, StopReason, TextContent, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, Lines};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::script::{Script, Step};

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
            // 记录写不进去说明测试环境坏了，直接让进程失败，比静默丢记录好查
            writeln!(file, "{value}").expect("写记录文件失败");
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
                responder.respond(NewSessionResponse::new(SessionId::new("fake-session-1")))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, _cx| {
                s3.record.request("session/load", &req);
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx: Conn| {
                s4.record.request("session/prompt", &req);
                let state = s4.clone();
                // 回合里要发权限请求并等答复，不能占住派发循环
                cx.clone().spawn(async move {
                    let stop = run_prompt(&state, &cx, req.session_id).await?;
                    responder.respond(PromptResponse::new(stop))
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
    if let Some(cli) = &info.cli {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "iwork".into(),
            json!({ "cli": { "path": cli.path, "version": cli.version } }),
        );
        response = response.meta(meta);
    }
    response
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
                allowed,
                denied,
            } => {
                let request = RequestPermissionRequest::new(
                    self.session.clone(),
                    ToolCallUpdate::new(tool_call_id.clone(), self.tool_fields(tool_call_id)),
                    vec![
                        PermissionOption::new(ALLOW_ID, "允许", PermissionOptionKind::AllowOnce),
                        PermissionOption::new(REJECT_ID, "拒绝", PermissionOptionKind::RejectOnce),
                    ],
                );
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
            Step::Sleep { ms } => Ok(cancelled(!self.pause(Duration::from_millis(*ms)).await)),
            Step::Crash { exit_code } => {
                // 已发出的通知由连接在后台写出；给它一点时间写完，模拟「输出到一半时崩溃」
                tokio::time::sleep(CRASH_FLUSH).await;
                std::process::exit(*exit_code)
            }
            Step::Hang => std::future::pending().await,
            Step::SpawnChild { program, args } => {
                let child = std::process::Command::new(program)
                    .args(args)
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
        }
    }
}
