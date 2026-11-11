//! `EventSource` 实现（T04 方案 2.6；T03 方案 2.10）：补发从存储读，路由用和实时发布同一个 `routes_for`。
//!
//! 快照由注入的 [`SnapshotProvider`] 给（T10 实现：一个读事务里跑快照查询的受理）。T04 注入 [`SnapshotNotWired`]：
//! ipc 拿到错误会关掉那条连接，所以 T10 接入之前只有续传能用（阶段缺口）。

use std::future::Future;
use std::sync::Arc;

use iwork_app::convert::persisted_event_to_proto;
use iwork_app::subscription::{routes_for, scope_streams};
use iwork_domain::id::Seq;
use iwork_ipc::server::{EventSource, ReplayPage, RoutedEvent, SourceError};
use iwork_ports::store::{EventFilter, StoragePort, StoredEvent};
use iwork_proto::pb;
use iwork_proto::routing::StreamKey;

/// 一页补发的建议字节上限（至少一条）。
const PAGE_BYTES: usize = 1024 * 1024;

/// 快照的提供方（T10 实现）。
pub trait SnapshotProvider: Send + Sync + 'static {
    /// 全部范围在**一个读事务**里取，返回那个事务看到的 head（要求见 `EventSource::snapshot`）。
    fn snapshot(
        &self,
        scopes: &[pb::SubscriptionScope],
    ) -> impl Future<Output = Result<(pb::Snapshot, u64), SourceError>> + Send;
}

/// T04 交付时的快照提供方。
#[derive(Debug, Clone, Copy, Default)]
pub struct SnapshotNotWired;

impl SnapshotProvider for SnapshotNotWired {
    async fn snapshot(
        &self,
        _scopes: &[pb::SubscriptionScope],
    ) -> Result<(pb::Snapshot, u64), SourceError> {
        Err(SourceError("快照查询未接入".into()))
    }
}

/// 把一条存储事件变成带路由的发布单元。转换失败是程序缺陷（同一程序写进去的事件转不出来）。
pub fn routed(e: &StoredEvent) -> Result<RoutedEvent, SourceError> {
    let proto = persisted_event_to_proto(e)
        .map_err(|err| SourceError(format!("事件 {} 转换失败：{err}", e.seq.0)))?;
    Ok(RoutedEvent::new(proto, routes_for(&e.event)))
}

pub struct StoreSource<St, P> {
    store: Arc<St>,
    snapshots: P,
}

impl<St, P> StoreSource<St, P> {
    pub fn new(store: Arc<St>, snapshots: P) -> Self {
        Self { store, snapshots }
    }
}

fn matches(keys: &[StreamKey], routes: &[StreamKey]) -> bool {
    keys.iter().any(|k| routes.iter().any(|r| k.covers(r)))
}

impl<St, P> EventSource for StoreSource<St, P>
where
    St: StoragePort + Send + Sync + 'static,
    P: SnapshotProvider,
{
    async fn head_seq(&self) -> Result<u64, SourceError> {
        self.store
            .head_seq()
            .await
            .map(|s| s.0)
            .map_err(|e| SourceError(e.to_string()))
    }

    fn scope_streams(&self, scope: &pb::SubscriptionScope) -> Vec<StreamKey> {
        scope_streams(scope)
    }

    /// 读 `(from, to]`：一次从存储读一批（最多 `limit` 条），只留匹配 `keys` 的；`next_from` 是扫描到的位置。
    async fn replay(
        &self,
        keys: &[StreamKey],
        from: u64,
        to: u64,
        limit: usize,
    ) -> Result<ReplayPage, SourceError> {
        if from >= to {
            return Ok(ReplayPage {
                events: Vec::new(),
                next_from: to,
            });
        }
        let want = limit
            .max(1)
            .min(usize::try_from(to - from).unwrap_or(usize::MAX));
        let batch = self
            .store
            .read_events(EventFilter::All, Seq(from), want)
            .await
            .map_err(|e| SourceError(e.to_string()))?;
        let mut events = Vec::new();
        let mut bytes = 0usize;
        let mut next_from = from;
        let mut cut = false;
        for e in &batch {
            if e.seq.0 > to {
                next_from = to;
                cut = true;
                break;
            }
            next_from = e.seq.0;
            let routes = routes_for(&e.event);
            if matches(keys, &routes) {
                let r = Arc::new(routed(e)?);
                bytes += r.encoded_len();
                events.push(r);
                if bytes >= PAGE_BYTES {
                    cut = true;
                    break;
                }
            }
        }
        // 存储返回的比要的少：已经读到库的末尾，`(from, to]` 扫完了
        if !cut && batch.len() < want {
            next_from = to;
        }
        Ok(ReplayPage { events, next_from })
    }

    fn snapshot(
        &self,
        scopes: &[pb::SubscriptionScope],
    ) -> impl Future<Output = Result<(pb::Snapshot, u64), SourceError>> + Send {
        self.snapshots.snapshot(scopes)
    }
}
