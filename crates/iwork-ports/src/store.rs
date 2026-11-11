//! 存储端口（技术设计 5.2）：事件日志、命令幂等、outbox、投影，以及事件编解码与投影两个接缝。
//!
//! 存储层拿到的是 domain 事件，编码成什么字节由注入的 [`EventCodec`] 决定（实现是 iwork-app 的
//! `ProtoEventCodec`），存储自己不依赖 proto。在线提交与历史重建走同一个 [`Projector`]。

use std::future::Future;
use std::path::PathBuf;

use iwork_domain::common::{Actor, Timestamp};
use iwork_domain::event::{DomainEvent, StreamId, StreamKind};
use iwork_domain::id::{CommandId, MessageId, Seq};

/// 编码后的事件：`kind` 进 `events.type` 列，`payload` 进 `events.payload` 列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedEvent {
    pub kind: &'static str,
    pub payload: Vec<u8>,
}

/// 事件编解码。
pub trait EventCodec: Send + Sync {
    /// 协议主版本，写进数据库 `meta` 表作破坏性变更的门（方案 3.7）。
    fn protocol_major(&self) -> u32;
    fn encode(&self, event: &DomainEvent) -> Result<EncodedEvent, CodecError>;
    fn decode(&self, kind: &str, payload: &[u8]) -> Result<DomainEvent, CodecError>;
}

/// 编解码失败。同一程序写进去的事件解不回来只可能是代码缺陷，所以都不可重试。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    #[error("未知的事件种类 {kind}")]
    UnknownKind { kind: String },
    #[error("事件 {kind} 解码失败：字段 {field}：{message}")]
    Malformed {
        kind: String,
        field: String,
        message: String,
    },
    #[error("事件 {kind} 无法编码：{message}")]
    Unencodable { kind: &'static str, message: String },
}

impl CodecError {
    pub fn retryable(&self) -> bool {
        false
    }
}

/// 投影：按事件更新查询用的表。
///
/// 在事务里写表，不是纯函数；要求**确定性**：同一事件序列在空表上重放得到同一张表，
/// 并且不触发任何对外动作（重建只更新视图，不重做对外动作）。
pub trait Projector: Send + Sync {
    /// 存储实现自己的事务句柄。
    type Tx;
    fn project(&self, tx: &mut Self::Tx, seq: Seq, event: &DomainEvent)
    -> Result<(), StorageError>;
}

/// outbox 待发消息的去向，不透明（由 app 与投递方约定）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutboxTarget(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutboxId(pub u64);

/// 投递状态（需求 12「投递与消费分开记录」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeliveryState {
    Queued,
    Dispatched,
    AcceptedByRuntime,
    Failed,
    Expired,
    Cancelled,
}

/// 随命令一起写入的待发消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOutboxItem {
    pub target: OutboxTarget,
    pub payload: Vec<u8>,
}

/// 已落盘的待发消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxItem {
    pub id: OutboxId,
    pub target: OutboxTarget,
    pub payload: Vec<u8>,
    pub state: DeliveryState,
    pub attempts: u32,
}

/// 按去向挑待投递的条目（T04 方案 2.11）。前缀按字节比，不当通配符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxSelector {
    /// `target` 以这个前缀开头的（某一个去向取自己的）。
    Prefix(String),
    /// `target` 不以其中任何一个前缀开头的（没有去向认领的）。列表为空时就是全部。
    NoneOf(Vec<String>),
}

impl OutboxSelector {
    /// 这个去向的条目是否被选中。存储实现可以在 SQL 里做同样的筛选，语义以这里为准。
    pub fn matches(&self, target: &OutboxTarget) -> bool {
        match self {
            Self::Prefix(p) => target.0.starts_with(p.as_str()),
            Self::NoneOf(ps) => !ps.iter().any(|p| target.0.starts_with(p.as_str())),
        }
    }
}

/// [`StoragePort::outbox_requeue_dispatched`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequeueReport {
    /// 改回 `Queued`、等着重投的条数。
    pub requeued: u64,
    /// 标成 `Failed` 的条数（去向不能幂等，不自动重投）。
    pub failed: u64,
}

/// 一次命令的提交：事件、outbox、`command_id` 在同一个事务里。
#[derive(Debug, Clone, PartialEq)]
pub struct CommitRequest {
    pub command_id: CommandId,
    pub actor: Actor,
    pub events: Vec<DomainEvent>,
    pub outbox: Vec<NewOutboxItem>,
}

/// 提交结果。`replayed`：同一 `command_id` 重发，什么都没写，返回上次的序号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitReceipt {
    pub first_seq: Seq,
    pub last_seq: Seq,
    pub replayed: bool,
}

/// 读出的事件（已解码）。
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    pub seq: Seq,
    pub at: Timestamp,
    pub actor: Actor,
    pub event: DomainEvent,
}

/// 按流过滤：`id` 为 `None` 表示这一类的全部流。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamSelector {
    pub kind: StreamKind,
    pub id: Option<StreamId>,
}

/// 读事件的过滤条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventFilter {
    All,
    Streams(Vec<StreamSelector>),
}

/// 重建结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildReport {
    pub events: u64,
    pub last_seq: Seq,
}

/// 存储。实现对 `C: EventCodec`、`P: Projector` 泛型，daemon 组合时注入。
pub trait StoragePort: Send + Sync {
    /// 一次命令一个事务：编码 → 写 events → 投影（同一事务）→ 写 outbox → 记 `command_id` → 提交。
    /// 同一 `command_id` 重发返回上次的序号且 `replayed = true`。
    fn commit(
        &self,
        req: CommitRequest,
    ) -> impl Future<Output = Result<CommitReceipt, StorageError>> + Send;
    fn head_seq(&self) -> impl Future<Output = Result<Seq, StorageError>> + Send;
    /// 读 `from` 之后（不含）的事件，最多 `limit` 条，已用同一个 codec 解码。
    fn read_events(
        &self,
        filter: EventFilter,
        from: Seq,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<StoredEvent>, StorageError>> + Send;
    /// 消息检查点：流式期间定期写下已收到的正文，不进事件日志（技术设计 3.5）。
    fn checkpoint_message(
        &self,
        message_id: MessageId,
        offset: u64,
        text: &str,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;
    fn outbox_pending(
        &self,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<OutboxItem>, StorageError>> + Send;
    fn outbox_mark(
        &self,
        id: OutboxId,
        state: DeliveryState,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;
    /// 把停在 `Dispatched`（派发了、没来得及记结果）的条目收尾，一个事务：`target` 以 `redeliver_prefixes`
    /// 里任一前缀开头的改回 `Queued`（接收方按 `OutboxId` 去重，可以重投）；以 `fail_prefixes` 里任一前缀开头的
    /// 标 `Failed`（不能幂等，不自动重投）；两边都不沾的原样留在 `Dispatched`，交给认识这个去向的版本按它自己的
    /// 规则收尾（T04 方案 2.11）。`attempts` 不变。
    fn outbox_requeue_dispatched(
        &self,
        redeliver_prefixes: Vec<String>,
        fail_prefixes: Vec<String>,
    ) -> impl Future<Output = Result<RequeueReport, StorageError>> + Send;
    /// 符合 `selector`、状态是 `Queued` 的条目，按 id 递增，最多 `limit` 条。筛选在存储里做，
    /// 排在别的去向后面的条目也挑得出来。
    fn outbox_pending_where(
        &self,
        selector: OutboxSelector,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<OutboxItem>, StorageError>> + Send;
    /// 历史重建：清投影表 → 按序读全部事件 → 解码 → 同一个 projector；不写 outbox。
    fn rebuild_projections(
        &self,
    ) -> impl Future<Output = Result<RebuildReport, StorageError>> + Send;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    #[error("存储读写失败：{message}")]
    Io { message: String },
    /// 写线程繁忙或数据库被锁，稍后重试即可
    #[error("存储繁忙")]
    Busy,
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("事件 {seq} 无法投影：{message}")]
    Projection { seq: Seq, message: String },
    #[error("找不到 {entity} {id}")]
    NotFound { entity: &'static str, id: String },
    /// 数据库的协议主版本与程序不符：已备份，拒绝启动（方案 3.7）
    #[error("数据库协议主版本 {database} 与程序 {program} 不符，已备份到 {backup}")]
    ProtocolMajorMismatch {
        database: u32,
        program: u32,
        backup: PathBuf,
    },
}

impl StorageError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Busy)
    }
}
