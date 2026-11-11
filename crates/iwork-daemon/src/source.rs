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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use iwork_domain::event::{ConversationCreated, ProjectAdded, TurnQueued};

    /// 补发只留匹配范围键的事件，`next_from` 是扫描到的位置（不是最后一条匹配的 seq）；扫到库尾时是 `to`。
    #[test]
    fn replay_filters_by_routes_and_reports_scan_position() {
        let env = Env::new();
        let store = Arc::new(env.open());
        // 1 项目 1、2 对话 10（项目 1）、3 对话 11（项目 1）、4 回合 20（对话 10）、5 回合 21（对话 11）
        block(store.commit(cmd(
            1,
            vec![
                ProjectAdded { project: project(1) }.into(),
                ConversationCreated {
                    conversation: project_conversation(10, 1),
                }
                .into(),
                ConversationCreated {
                    conversation: project_conversation(11, 1),
                }
                .into(),
                TurnQueued { turn: turn(20, 10) }.into(),
                TurnQueued { turn: turn(21, 11) }.into(),
            ],
        )))
        .unwrap();
        let src = StoreSource::new(store.clone(), SnapshotNotWired);
        let conv10 = StreamKey::one("conversation", cid(10).to_string());
        let seqs = |p: &ReplayPage| p.events.iter().map(|e| e.seq()).collect::<Vec<_>>();

        let all = block(src.replay(std::slice::from_ref(&conv10), 0, 5, 100)).unwrap();
        assert_eq!(seqs(&all), [2, 4], "只有对话 10 的");
        assert_eq!(all.next_from, 5);
        for e in &all.events {
            assert!(e.routes.iter().any(|r| conv10.covers(r)));
        }

        // 一页只读 3 条：扫到 3，匹配的只有 2 —— next_from 报 3，不是 2
        let page = block(src.replay(std::slice::from_ref(&conv10), 0, 5, 3)).unwrap();
        assert_eq!(seqs(&page), [2]);
        assert_eq!(page.next_from, 3);
        let rest = block(src.replay(std::slice::from_ref(&conv10), page.next_from, 5, 3)).unwrap();
        assert_eq!(seqs(&rest), [4]);
        assert_eq!(rest.next_from, 5);

        // 范围 to 截在中间
        let cut = block(src.replay(std::slice::from_ref(&conv10), 0, 3, 100)).unwrap();
        assert_eq!(seqs(&cut), [2]);
        assert_eq!(cut.next_from, 3);

        // 对照：不匹配任何东西的键，扫完也报到 to
        let none = block(src.replay(&[StreamKey::one("conversation", "nope")], 0, 5, 100)).unwrap();
        assert!(none.events.is_empty());
        assert_eq!(none.next_from, 5);
        assert_eq!(block(src.head_seq()).unwrap(), 5);
    }
}
