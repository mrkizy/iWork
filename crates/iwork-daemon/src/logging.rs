//! 结构化日志（T04 方案 2.14；设计 10.3；需求 11）。
//!
//! - 去哪：`<数据目录>/logs/daemon.log`，JSON 行；超过 10 MiB 滚动，留 5 个（含当前），总量封顶 50 MiB；权限 0600。
//! - 不拖慢业务：格式化后进有界队列（1 MiB），专用线程写文件；满了丢弃并计数，恢复后补一条「丢了 N 条」。
//! - 级别：默认 `info`；环境变量 `IWORK_LOG`（`目标=级别,…` 的写法，同 `RUST_LOG` 的常用子集）可调。
//!   审计 target（`iwork.ipc.audit`）永远不被级别过滤掉。
//! - 不落敏感内容：约定之外再叠两层——字段名黑名单（命中的值写成 `<redacted>`）与单值长度截断（512 字节）。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber, span as tspan};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// 日志文件名。
pub const LOG_FILE: &str = "daemon.log";
/// 调级别的环境变量。
pub const LOG_ENV: &str = "IWORK_LOG";
/// 一个文件到这么大就滚动。
pub const ROLL_BYTES: u64 = 10 * 1024 * 1024;
/// 保留几个文件（含当前的 `daemon.log`）。
pub const KEEP_FILES: usize = 5;
/// 队列里最多积压这么多字节，再多就丢。
pub const QUEUE_BYTES: usize = 1024 * 1024;
/// 单个字段值的长度上限（字节）。
pub const MAX_VALUE_BYTES: usize = 512;

/// 字段名黑名单：命中（整名相同，或以 `_<名字>` 结尾，不分大小写）的值写成 `<redacted>`。
pub const REDACTED_FIELDS: &[&str] = &[
    "body",
    "text",
    "content",
    "prompt",
    "input",
    "output",
    "env",
    "token",
    "secret",
    "password",
    "authorization",
    "cookie",
];

const REDACTED: &str = "<redacted>";

/// span 字段名的唯一定义处：命令、回合、审批各一个 span（卡范围 9）。
/// T04 在请求入口开 `command`；`turn`、`approval` 由 T10 在回合宿主里开（`iwork-core` 不依赖输出端）。
pub mod span {
    pub fn command(command_id: &str, kind: &str) -> tracing::Span {
        tracing::info_span!("command", command_id, kind)
    }
    pub fn turn(turn_id: &str) -> tracing::Span {
        tracing::info_span!("turn", turn_id)
    }
    pub fn approval(approval_id: &str) -> tracing::Span {
        tracing::info_span!("approval", approval_id)
    }
}

fn is_redacted(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    REDACTED_FIELDS.iter().any(|r| {
        lower == *r
            || lower
                .strip_suffix(r)
                .is_some_and(|head| head.ends_with('_'))
    })
}

/// 超过上限截断（按字符边界），并标 `…(+N)`，N 是截掉的字节数。
fn truncate(mut s: String) -> String {
    if s.len() <= MAX_VALUE_BYTES {
        return s;
    }
    let mut cut = MAX_VALUE_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = s.len() - cut;
    s.truncate(cut);
    let _ = write!(s, "…(+{dropped})");
    s
}

/// 把字段收进 JSON 对象，同时做黑名单与截断。
struct JsonVisitor<'a>(&'a mut Map<String, Value>);

impl JsonVisitor<'_> {
    fn put(&mut self, field: &Field, value: Value) {
        let name = field.name();
        let value = if is_redacted(name) {
            Value::String(REDACTED.into())
        } else {
            match value {
                Value::String(s) => Value::String(truncate(s)),
                v => v,
            }
        };
        self.0.insert(name.to_owned(), value);
    }
}

impl Visit for JsonVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::String(value.to_owned()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::from(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, Value::String(value.to_string()));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.put(field, Value::String(format!("{value:?}")));
    }
}

/// span 上收集到的字段（存在 span 的扩展里）。
struct SpanFields(Map<String, Value>);

/// 日志行去哪。
pub trait Sink: Send + Sync + 'static {
    fn write_line(&self, line: Vec<u8>);
}

/// 把事件格式化成一行 JSON 交给 [`Sink`]。
pub struct JsonLayer<S: Sink> {
    sink: Arc<S>,
}

impl<S: Sink> JsonLayer<S> {
    pub fn new(sink: Arc<S>) -> Self {
        Self { sink }
    }
}

impl<Sub, S> Layer<Sub> for JsonLayer<S>
where
    Sub: Subscriber + for<'a> LookupSpan<'a>,
    S: Sink,
{
    fn on_new_span(&self, attrs: &tspan::Attributes<'_>, id: &tspan::Id, ctx: Context<'_, Sub>) {
        let mut map = Map::new();
        attrs.record(&mut JsonVisitor(&mut map));
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(map));
        }
    }

    fn on_record(&self, id: &tspan::Id, values: &tspan::Record<'_>, ctx: Context<'_, Sub>) {
        if let Some(span) = ctx.span(id)
            && let Some(fields) = span.extensions_mut().get_mut::<SpanFields>()
        {
            values.record(&mut JsonVisitor(&mut fields.0));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, Sub>) {
        let meta = event.metadata();
        let mut obj = Map::new();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        obj.insert("ts".into(), Value::from(ts));
        obj.insert("level".into(), Value::from(meta.level().as_str()));
        obj.insert("target".into(), Value::from(meta.target()));
        let mut fields = Map::new();
        event.record(&mut JsonVisitor(&mut fields));
        obj.insert("fields".into(), Value::Object(fields));
        if let Some(scope) = ctx.event_scope(event) {
            let mut spans = Vec::new();
            for span in scope.from_root() {
                let mut s = Map::new();
                s.insert("name".into(), Value::from(span.name()));
                if let Some(f) = span.extensions().get::<SpanFields>() {
                    s.extend(f.0.clone());
                }
                spans.push(Value::Object(s));
            }
            obj.insert("spans".into(), Value::Array(spans));
        }
        let mut line = serde_json::to_vec(&Value::Object(obj)).unwrap_or_default();
        line.push(b'\n');
        self.sink.write_line(line);
    }
}

/// ipc 审计里要交给后台自己处理的信号（方案 2.10、2.9）。日志旁路按审计 target 与事件名过滤出来，不改日志本身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditSignal {
    /// 服务端拒绝了来连的客户端（升级后新客户端被旧后台拒）：安装检查立刻跑一次。
    PeerRejectedByServer,
    /// 连接因后台自身问题被关（`cause.is_server_fault()` 那条 `error`）：`cause` 是 `FailCause::as_str()`。
    ServerFault { cause: String },
}

/// 审计旁路。走日志而不是回调，是 T03 方案 2.12 定下的接口。
pub struct AuditTap {
    tx: tokio::sync::broadcast::Sender<AuditSignal>,
}

impl AuditTap {
    pub fn new(tx: tokio::sync::broadcast::Sender<AuditSignal>) -> Self {
        Self { tx }
    }
}

#[derive(Default)]
struct AuditFields {
    event: Option<String>,
    role: Option<String>,
    cause: Option<String>,
}

impl Visit for AuditFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        let slot = match field.name() {
            "event" => &mut self.event,
            "role" => &mut self.role,
            "cause" => &mut self.cause,
            _ => return,
        };
        *slot = Some(value.to_owned());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }
}

/// 纯函数：一条审计事件对应的信号。
pub fn audit_signal(
    level: &Level,
    event: &str,
    role: Option<&str>,
    cause: Option<&str>,
) -> Option<AuditSignal> {
    use iwork_ipc::audit::{EVENT_CONNECTION_FAILED, EVENT_PEER_REJECTED};
    if event == EVENT_PEER_REJECTED && role == Some("server") {
        return Some(AuditSignal::PeerRejectedByServer);
    }
    // 后台自身问题另记一条 `error` 级别的同名事件；`warn` 那条是所有关连接都有的
    if event == EVENT_CONNECTION_FAILED && *level == Level::ERROR {
        return cause.map(|c| AuditSignal::ServerFault {
            cause: c.to_owned(),
        });
    }
    None
}

impl<S: Subscriber> Layer<S> for AuditTap {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if meta.target() != iwork_ipc::audit::TARGET {
            return;
        }
        let mut f = AuditFields::default();
        event.record(&mut f);
        let Some(name) = f.event.as_deref() else {
            return;
        };
        if let Some(sig) = audit_signal(meta.level(), name, f.role.as_deref(), f.cause.as_deref()) {
            // 没有接收方（任务还没起）时丢掉：这些信号只是「加速」或「核对一次」
            let _ = self.tx.send(sig);
        }
    }
}

/// 级别过滤：`IWORK_LOG` 解析不了时用默认的 `info`（并在 stderr 说一句）；审计 target 永远放行。
pub fn filter_from(spec: Option<&str>) -> Targets {
    let base = match spec {
        Some(s) => Targets::from_str(s).unwrap_or_else(|e| {
            eprintln!("iwork-daemon：{LOG_ENV}={s} 解析失败（{e}），用默认级别 info");
            Targets::new().with_default(LevelFilter::INFO)
        }),
        None => Targets::new().with_default(LevelFilter::INFO),
    };
    base.with_target(iwork_ipc::audit::TARGET, LevelFilter::TRACE)
}

/// 按大小滚动的文件：`daemon.log` → `daemon.log.1` → … → `daemon.log.{keep-1}`，最老的删掉。
pub struct RollingFile {
    dir: PathBuf,
    roll_bytes: u64,
    keep: usize,
    file: File,
    size: u64,
}

impl RollingFile {
    pub fn open(dir: &Path, roll_bytes: u64, keep: usize) -> io::Result<Self> {
        let path = dir.join(LOG_FILE);
        let file = open_private_append(&path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            dir: dir.to_path_buf(),
            roll_bytes,
            keep: keep.max(1),
            file,
            size,
        })
    }

    fn numbered(&self, n: usize) -> PathBuf {
        if n == 0 {
            self.dir.join(LOG_FILE)
        } else {
            self.dir.join(format!("{LOG_FILE}.{n}"))
        }
    }

    pub fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        if self.size > 0 && self.size + line.len() as u64 > self.roll_bytes {
            self.roll()?;
        }
        self.file.write_all(line)?;
        self.size += line.len() as u64;
        Ok(())
    }

    fn roll(&mut self) -> io::Result<()> {
        let oldest = self.numbered(self.keep - 1);
        match std::fs::remove_file(&oldest) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        for n in (0..self.keep - 1).rev() {
            let from = self.numbered(n);
            if from.exists() {
                std::fs::rename(&from, self.numbered(n + 1))?;
            }
        }
        self.file = open_private_append(&self.numbered(0))?;
        self.size = 0;
        Ok(())
    }
}

fn open_private_append(path: &Path) -> io::Result<File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        // 不跟随链接：不顺着别人放好的链接把日志追加到别处
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
}

enum Msg {
    Line(Vec<u8>),
    /// 排空到这里为止，然后回一声
    Flush(mpsc::Sender<()>),
}

/// 有界队列 + 专用写线程。
pub struct QueueSink {
    tx: Mutex<Option<mpsc::Sender<Msg>>>,
    queued: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    limit: usize,
}

impl Sink for QueueSink {
    fn write_line(&self, line: Vec<u8>) {
        let len = line.len();
        if self.queued.fetch_add(len, Ordering::SeqCst) + len > self.limit {
            self.queued.fetch_sub(len, Ordering::SeqCst);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        match tx.as_ref().map(|tx| tx.send(Msg::Line(line))) {
            Some(Ok(())) => {}
            _ => {
                self.queued.fetch_sub(len, Ordering::SeqCst);
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// 日志的去处：滚动文件，或者（日志目录写不了时）stderr。
pub trait LineWriter: Send + 'static {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()>;
}

impl LineWriter for RollingFile {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        RollingFile::write_line(self, line)
    }
}

struct Stderr;

impl LineWriter for Stderr {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        io::stderr().write_all(line)
    }
}

impl QueueSink {
    /// 起写线程。
    pub fn start(mut out: Box<dyn LineWriter>, limit: usize) -> (Arc<Self>, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel::<Msg>();
        let queued = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let sink = Arc::new(Self {
            tx: Mutex::new(Some(tx)),
            queued: queued.clone(),
            dropped: dropped.clone(),
            limit,
        });
        let join = std::thread::Builder::new()
            .name("iwork-log".into())
            .spawn(move || {
                let mut pending_flush: VecDeque<mpsc::Sender<()>> = VecDeque::new();
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::Line(line) => {
                            queued.fetch_sub(line.len(), Ordering::SeqCst);
                            // 写不进去（磁盘满）也不能让业务停下：丢掉这一行
                            let _ = out.write_line(&line);
                            let lost = dropped.swap(0, Ordering::Relaxed);
                            if lost > 0 {
                                let note = format!(
                                    "{{\"level\":\"WARN\",\"target\":\"iwork_daemon::logging\",\"fields\":{{\"message\":\"日志队列满，丢了 {lost} 条\"}}}}\n"
                                );
                                let _ = out.write_line(note.as_bytes());
                            }
                        }
                        Msg::Flush(done) => pending_flush.push_back(done),
                    }
                    while let Some(done) = pending_flush.pop_front() {
                        let _ = done.send(());
                    }
                }
            })
            .expect("起日志线程失败");
        (sink, join)
    }

    /// 等队列里已有的行都写出去。
    pub fn flush(&self) {
        let (done_tx, done_rx) = mpsc::channel();
        let sent = {
            let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
            tx.as_ref()
                .is_some_and(|tx| tx.send(Msg::Flush(done_tx)).is_ok())
        };
        if sent {
            let _ = done_rx.recv_timeout(std::time::Duration::from_secs(1));
        }
    }

    fn close(&self) {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// 全局日志的队列：硬上限强退（`std::process::exit` 不跑析构）之前用它排空日志。
static GLOBAL_SINK: std::sync::OnceLock<Arc<QueueSink>> = std::sync::OnceLock::new();

/// 把全局日志队列里已有的行写出去（最多等 1s）。没装全局日志时什么都不做。
pub fn flush_global() {
    if let Some(sink) = GLOBAL_SINK.get() {
        sink.flush();
    }
}

/// 装好的日志。丢弃时把队列排空、停写线程（停机第 7 步「日志线程排空」）。
pub struct LogGuard {
    sink: Arc<QueueSink>,
    join: Option<JoinHandle<()>>,
    /// 实际写到哪个目录；写不了日志目录、退到 stderr 时为 `None`。
    pub dir: Option<PathBuf>,
    /// 审计旁路的发送端：要收信号的任务各 `subscribe` 一个。
    pub audit: tokio::sync::broadcast::Sender<AuditSignal>,
}

impl LogGuard {
    pub fn flush(&self) {
        self.sink.flush();
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        self.sink.flush();
        self.sink.close();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// 装全局日志：写 `dir/daemon.log`；打不开就只写 stderr（启动表第 4 步：日志目录写不了时继续）。
/// 同时装 panic 钩子：记线程名、位置、panic 信息后照常展开。
pub fn init(dir: &Path) -> LogGuard {
    let (out, used): (Box<dyn LineWriter>, Option<PathBuf>) =
        match RollingFile::open(dir, ROLL_BYTES, KEEP_FILES) {
            Ok(f) => (Box::new(f), Some(dir.to_path_buf())),
            Err(e) => {
                eprintln!(
                    "iwork-daemon：日志目录 {} 写不了（{e}），只写 stderr",
                    dir.display()
                );
                (Box::new(Stderr), None)
            }
        };
    let (sink, join) = QueueSink::start(out, QUEUE_BYTES);
    let spec = std::env::var(LOG_ENV).ok();
    let (audit, _) = tokio::sync::broadcast::channel(64);
    let subscriber = tracing_subscriber::registry()
        .with(JsonLayer::new(sink.clone()).with_filter(filter_from(spec.as_deref())))
        .with(AuditTap::new(audit.clone()));
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        eprintln!("iwork-daemon：全局日志已经装过，这次不再装");
    } else {
        let _ = GLOBAL_SINK.set(sink.clone());
    }
    install_panic_hook();
    LogGuard {
        sink,
        join: Some(join),
        dir: used,
        audit,
    }
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let task = thread.name().unwrap_or("<unnamed>");
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        tracing::error!(task, location, panic = %payload, "panic");
        previous(info);
    }));
}

/// 一条日志的级别是否会被写出（给测试与调用方查）。
pub fn enabled(filter: &Targets, target: &str, level: Level) -> bool {
    filter.would_enable(target, &level)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 日志文件的位置上是指向别处的链接 → 打开失败，链接指向的文件不变。
    #[cfg(unix)]
    #[test]
    fn log_file_symlink_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let victim = d.path().join("victim");
        std::fs::write(&victim, "keep me").unwrap();
        let log = d.path().join("daemon.log");
        std::os::unix::fs::symlink(&victim, &log).unwrap();
        assert!(open_private_append(&log).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
        // 对照：普通文件照常打开
        assert!(open_private_append(&d.path().join("other.log")).is_ok());
    }

    #[derive(Default)]
    struct Memory(Mutex<Vec<String>>);

    impl Sink for Memory {
        fn write_line(&self, line: Vec<u8>) {
            self.0
                .lock()
                .unwrap()
                .push(String::from_utf8(line).unwrap());
        }
    }

    fn capture(f: impl FnOnce()) -> Vec<Value> {
        keep_callsites_interested();
        let mem = Arc::new(Memory::default());
        let sub = tracing_subscriber::registry().with(JsonLayer::new(mem.clone()));
        tracing::subscriber::with_default(sub, f);
        let lines = mem.0.lock().unwrap().clone();
        lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// 本线程装的订阅者（`with_default` / `set_default`）收不全事件的竞态，在这里堵上；整个测试程序只装一次。
    ///
    /// tracing 按「第一次命中某个 callsite 时有哪些订阅者」算 interest 并缓存。只有一个分发器时，
    /// 它用命中线程自己的默认订阅者来算：别的测试线程（比如 `handler` 的测试走 `span::command`、
    /// 或碰到 ipc 审计）没装订阅者，先命中就会把 callsite 缓存成 never，本线程的订阅者从此收不到。
    /// 全局装一个对谁都答 sometimes、自己不收事件的订阅者，没装订阅者的线程算出来的也不再是 never。
    /// 同一个竞态在 iwork-ipc 有一个必红的复现：`crates/iwork-ipc/tests/audit_capture.rs`。
    fn keep_callsites_interested() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::subscriber::set_global_default(KeepInterest)
                .expect("iwork-daemon 的单元测试里只有这里装全局订阅者");
        });
    }

    /// 对每个 callsite 都答 sometimes、自己一条事件都不收的订阅者。
    struct KeepInterest;

    impl tracing::Subscriber for KeepInterest {
        fn register_callsite(
            &self,
            _: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::sometimes()
        }
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            false
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// 对拍（方案 2.10）：用内存传输真的触发一次服务端拒绝与一次交接超时，旁路都收得到；
    /// 对照：被拒的另一端（客户端）那条 `peer_rejected` 不算。
    #[tokio::test]
    async fn audit_tap_sees_real_ipc_faults() {
        use iwork_ipc::client::{Client, ClientConfig};
        use iwork_ipc::peer::{
            ImageId, Manifest, PeerIdentity, SelfIdentity, SignerIdentity, UserId,
        };
        use iwork_ipc::server::{EventSource, ReplayPage, Server, ServerConfig, SourceError};
        use iwork_ipc::transport::memory;
        use iwork_proto::pb;
        use iwork_proto::routing::StreamKey;
        use std::path::PathBuf;
        use std::time::Duration;

        struct NeverSnapshot;
        impl EventSource for NeverSnapshot {
            async fn head_seq(&self) -> Result<u64, SourceError> {
                Ok(0)
            }
            fn scope_streams(&self, scope: &pb::SubscriptionScope) -> Vec<StreamKey> {
                iwork_app::subscription::scope_streams(scope)
            }
            async fn replay(
                &self,
                _: &[StreamKey],
                _: u64,
                to: u64,
                _: usize,
            ) -> Result<ReplayPage, SourceError> {
                Ok(ReplayPage {
                    events: vec![],
                    next_from: to,
                })
            }
            async fn snapshot(
                &self,
                _: &[pb::SubscriptionScope],
            ) -> Result<(pb::Snapshot, u64), SourceError> {
                std::future::pending().await
            }
        }

        const UID: u32 = 501;
        fn image(b: u8) -> ImageId {
            ImageId::Sha256([b; 32])
        }
        let manifest = Manifest::parse(&format!(
            r#"{{"manifest_version":1,"app_version":"1.0.0","binaries":{{"iwork":["{}","{}"]}}}}"#,
            image(0x51),
            image(0xC1)
        ))
        .unwrap();
        let own = |b: u8| SelfIdentity {
            user: UserId::Uid(UID),
            signer: SignerIdentity::Image(image(b)),
            exe_dir: PathBuf::from("/opt/iwork"),
            manifest: Some(manifest.clone()),
        };
        fn peer(uid: u32, b: u8) -> PeerIdentity {
            PeerIdentity {
                pid: 7,
                user: UserId::Uid(uid),
                exe: Some(PathBuf::from("/opt/iwork/iwork")),
                signer: SignerIdentity::Image(image(b)),
            }
        }

        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        keep_callsites_interested();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(AuditTap::new(tx)),
        );
        let mut cfg = ServerConfig::new("t");
        cfg.handover_timeout = Duration::from_millis(100);
        let (server, _publisher) =
            Server::new(cfg, own(0x51), crate::handler::NotWired, NeverSnapshot);
        let (listener, good) =
            memory::endpoint("tap", peer(UID, 0xC1), Arc::new(move |_| peer(UID, 0x51)));
        let token = tokio_util::sync::CancellationToken::new();
        let serve = tokio::spawn(server.serve(listener, token.clone()));

        // 一次交接超时：订阅要快照，快照永远不返回
        let (client, _incoming) = Client::connect(
            &good,
            &iwork_ipc::transport::Endpoint::Memory("tap".into()),
            &own(0xC1),
            ClientConfig::new(pb::ClientKind::Desktop, "t"),
        )
        .await
        .expect("握手");
        let scope = pb::SubscriptionScope {
            kind: Some(pb::subscription_scope::Kind::Settings(pb::SettingsScope {})),
        };
        let _ = client.subscribe(1, scope, 0).await;
        // 一次服务端拒绝：另一个用户来连
        let stranger = good.with_client_identity(peer(UID + 1, 0xC1));
        let _ = Client::connect(
            &stranger,
            &iwork_ipc::transport::Endpoint::Memory("tap".into()),
            &own(0xC1),
            ClientConfig::new(pb::ClientKind::Desktop, "t"),
        )
        .await;

        let mut got = Vec::new();
        while got.len() < 2 {
            let sig = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("旁路没收到信号")
                .unwrap();
            got.push(sig);
        }
        got.sort_by_key(|s| format!("{s:?}"));
        assert_eq!(
            got,
            [
                AuditSignal::PeerRejectedByServer,
                AuditSignal::ServerFault {
                    cause: "HandoverTimeout".into()
                },
            ]
        );
        token.cancel();
        let _ = serve.await;
    }

    #[test]
    fn audit_signal_table() {
        use iwork_ipc::audit::{EVENT_CONNECTION_FAILED, EVENT_PEER_REJECTED};
        assert_eq!(
            audit_signal(&Level::WARN, EVENT_PEER_REJECTED, Some("server"), None),
            Some(AuditSignal::PeerRejectedByServer)
        );
        assert_eq!(
            audit_signal(&Level::WARN, EVENT_PEER_REJECTED, Some("client"), None),
            None
        );
        assert_eq!(
            audit_signal(
                &Level::ERROR,
                EVENT_CONNECTION_FAILED,
                None,
                Some("PublishStalled")
            ),
            Some(AuditSignal::ServerFault {
                cause: "PublishStalled".into()
            })
        );
        // `warn` 那条是所有关连接都有的，不算后台故障
        assert_eq!(
            audit_signal(
                &Level::WARN,
                EVENT_CONNECTION_FAILED,
                None,
                Some("QueueFull")
            ),
            None
        );
    }

    #[test]
    fn redacts_listed_fields_and_truncates() {
        let long = "x".repeat(2000);
        let lines = capture(|| {
            let s = span::command("cmd-1", "send_message");
            let _e = s.enter();
            tracing::info!(
                body = "SECRET-BODY",
                auth_token = "SECRET-TOKEN",
                context = "keep-me",
                note = %long,
                "hello"
            );
        });
        assert_eq!(lines.len(), 1);
        let text = lines[0].to_string();
        assert!(!text.contains("SECRET-BODY"), "{text}");
        assert!(!text.contains("SECRET-TOKEN"), "{text}");
        let fields = &lines[0]["fields"];
        assert_eq!(fields["body"], REDACTED);
        assert_eq!(fields["auth_token"], REDACTED);
        // 对照：名字只是含有黑名单词、但不以 `_词` 结尾的不脱敏
        assert_eq!(fields["context"], "keep-me");
        let note = fields["note"].as_str().unwrap();
        assert!(note.starts_with(&"x".repeat(MAX_VALUE_BYTES)));
        assert!(note.ends_with(&format!("…(+{})", 2000 - MAX_VALUE_BYTES)));
        // span 字段在 spans 里
        assert_eq!(lines[0]["spans"][0]["name"], "command");
        assert_eq!(lines[0]["spans"][0]["command_id"], "cmd-1");
        assert_eq!(lines[0]["spans"][0]["kind"], "send_message");
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "汉".repeat(300); // 900 字节
        let t = truncate(s);
        assert!(t.ends_with("…(+390)"), "{t}");
    }

    #[test]
    fn rolls_at_size_and_caps_total() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = RollingFile::open(dir.path(), 100, 3).unwrap();
        let line = [b'a'; 40];
        for _ in 0..20 {
            f.write_line(&line).unwrap();
        }
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["daemon.log", "daemon.log.1", "daemon.log.2"]);
        let total: u64 = names
            .iter()
            .map(|n| std::fs::metadata(dir.path().join(n)).unwrap().len())
            .sum();
        assert!(total <= 300, "总量 {total} 超过 3 个文件的上限");
        for n in &names {
            assert!(std::fs::metadata(dir.path().join(n)).unwrap().len() <= 100);
        }
    }

    #[test]
    fn audit_target_is_never_filtered_out() {
        let f = filter_from(Some("off"));
        assert!(enabled(&f, iwork_ipc::audit::TARGET, Level::WARN));
        assert!(!enabled(&f, "iwork_daemon", Level::ERROR));
        let d = filter_from(None);
        assert!(enabled(&d, "iwork_daemon", Level::INFO));
        assert!(!enabled(&d, "iwork_daemon", Level::DEBUG));
    }

    struct Slow(Arc<Mutex<Vec<Vec<u8>>>>, mpsc::Receiver<()>);
    impl LineWriter for Slow {
        fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
            let _ = self.1.recv();
            self.0.lock().unwrap().push(line.to_vec());
            Ok(())
        }
    }

    /// 队列满了丢弃并计数，恢复后补一条「丢了 N 条」。
    #[test]
    fn full_queue_drops_and_reports() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let (gate_tx, gate_rx) = mpsc::channel();
        let (sink, join) = QueueSink::start(Box::new(Slow(written.clone(), gate_rx)), 100);
        for _ in 0..10 {
            sink.write_line(vec![b'z'; 40]);
        }
        // 放行足够多次写
        for _ in 0..20 {
            let _ = gate_tx.send(());
        }
        sink.flush();
        sink.close();
        drop(gate_tx);
        join.join().unwrap();
        let w = written.lock().unwrap();
        let notes: Vec<_> = w
            .iter()
            .filter(|l| String::from_utf8_lossy(l).contains("丢了"))
            .collect();
        assert_eq!(notes.len(), 1);
        let kept = w.len() - 1;
        assert!(kept < 10, "应有被丢的：写出 {kept}");
        let lost = 10 - kept;
        assert!(String::from_utf8_lossy(notes[0]).contains(&format!("丢了 {lost} 条")));
    }
}
