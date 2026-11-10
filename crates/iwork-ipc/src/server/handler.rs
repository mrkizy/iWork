//! 服务端对外接口（方案 2.10）：T04 组合、T10 实现 [`RequestHandler`]、daemon 实现 [`EventSource`]。

use std::future::Future;
use std::sync::Arc;

use iwork_proto::pb;
use iwork_proto::routing::StreamKey;
use prost::Message;

use crate::peer::UserId;

/// 一条连接的上下文，交给 handler。
#[derive(Debug, Clone)]
pub struct ConnectionContext {
    pub connection_id: u64,
    pub session_id: String,
    /// pid 与用户；路径只进审计，不给 handler。
    pub peer: PeerSummary,
    pub client_kind: pb::ClientKind,
    /// 第 2 步（签名 / 安装映像）是否真的查过。发布构建恒为 true；T10 的 `ExecuteUserAction`、
    /// `ReadArtifact` 受理断言它。
    pub signature_checked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSummary {
    pub pid: u32,
    pub user: UserId,
}

/// handler 拒绝一个分片：没有 `request_id` 可挂，ipc 发 `Response{request_id: 0, error}` 后断开。
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkRefused(pub pb::Error);

/// 命令与查询的受理方（T10）。`Subscribe` / `Unsubscribe` 不会到这里。
pub trait RequestHandler: Send + Sync + 'static {
    /// 命令与查询。对 `iwork_proto::routing::query_lane(q) == ViewQuery` 的查询，实现方必须在**一个读事务**里执行，
    /// 并把那个事务看到的 head 填进 `QueryResult.seq`——交接靠它定结果在事件流里的位置；
    /// 填错（比如事务外另读一次 head）会让结果与事件错位。
    fn handle(
        &self,
        ctx: &ConnectionContext,
        req: pb::Request,
    ) -> impl Future<Output = pb::Response> + Send;

    /// 分片上传。ipc 已校验 `data` ≤ 1MiB、`upload_id` 非空；缓冲、总量上限（20MB）、按连接清理归实现方。
    fn chunk(
        &self,
        ctx: &ConnectionContext,
        chunk: pb::Chunk,
    ) -> impl Future<Output = Result<(), ChunkRefused>> + Send;

    /// 连接关闭（任何原因）。实现方丢掉这条连接未完成的上传。
    fn connection_closed(&self, ctx: &ConnectionContext) -> impl Future<Output = ()> + Send;
}

/// 带路由的持久事件：实时推送与历史补发都是它，`routes` 由同一个 `iwork_app::subscription::routes_for` 算出。
#[derive(Debug, Clone, PartialEq)]
pub struct RoutedEvent {
    pub event: pb::PersistedEvent,
    pub routes: Vec<StreamKey>,
    encoded_len: usize,
}

impl RoutedEvent {
    pub fn new(event: pb::PersistedEvent, routes: Vec<StreamKey>) -> Self {
        let encoded_len = event.encoded_len();
        Self {
            event,
            routes,
            encoded_len,
        }
    }

    pub fn seq(&self) -> u64 {
        self.event.seq
    }

    /// `PersistedEvent` 编码后的字节数（建时算一次；预算按它计）。
    pub fn encoded_len(&self) -> usize {
        self.encoded_len
    }
}

/// 发布给 ipc 的东西。
#[derive(Debug, Clone)]
pub enum Published {
    Persisted(Arc<RoutedEvent>),
    Transient {
        event: pb::TransientEvent,
        routes: Vec<StreamKey>,
    },
}

/// 一页历史。`next_from` 是**扫描到的位置**（不是最后一条匹配事件的 seq）：下一页从它继续，
/// `next_from >= to` 表示 `(from, to]` 已经扫完。实现方按载荷过滤时靠它避免「过滤后不足一页」被误判为读完。
#[derive(Debug, Clone, Default)]
pub struct ReplayPage {
    pub events: Vec<Arc<RoutedEvent>>,
    pub next_from: u64,
}

/// 事件源取数失败（存储出错）。ipc 关闭这条连接，客户端重连续传。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SourceError(pub String);

/// 补发与快照的数据源（daemon 实现；T03 测试用内存实现）。
pub trait EventSource: Send + Sync + 'static {
    /// 存储里已持久化的最高 seq。
    fn head_seq(&self) -> impl Future<Output = Result<u64, SourceError>> + Send;

    /// 范围 → 路由键。纯映射，实现方转调 `iwork_app::subscription::scope_streams`。空表示范围无效。
    fn scope_streams(&self, scope: &pb::SubscriptionScope) -> Vec<StreamKey>;

    /// 读 `(from, to]` 里、匹配这些键的持久事件，按 seq 递增，最多 `limit` 条；每条带 `routes`，
    /// 由与实时推送同一个 `routes_for` 算出。返回的总字节建议不超过 1MiB（至少一条）。
    fn replay(
        &self,
        keys: &[StreamKey],
        from: u64,
        to: u64,
        limit: usize,
    ) -> impl Future<Output = Result<ReplayPage, SourceError>> + Send;

    /// 全部活动范围的快照与它的 seq。**要求**：所有部分在同一个读事务里取，返回那个事务看到的 head，
    /// 每个 `QueryResult.seq` 都等于它。ipc 发现各部分 seq 不同会丢弃这份快照并重取。
    fn snapshot(
        &self,
        scopes: &[pb::SubscriptionScope],
    ) -> impl Future<Output = Result<(pb::Snapshot, u64), SourceError>> + Send;
}
