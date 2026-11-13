//! 订阅快照（T10a 方案 2.7、0a.3 的 S1）：全部范围的快照查询在**一个**读事务里跑，各部分同一个 head。
//! 查询就是 [`crate::handler::query::answer_in`]，和单条查询同一套（Q1）。

use std::sync::Arc;

use iwork_app::subscription::snapshot_queries;
use iwork_ipc::server::SourceError;
use iwork_ports::store::EventCodec;
use iwork_proto::pb;
use iwork_store::SqliteReadModel;

use crate::handler::query::{Place, answer_in, place};
use crate::source::SnapshotProvider;

/// 读模型上的快照提供方。
pub struct ReadSnapshots<C: EventCodec> {
    read: Arc<SqliteReadModel<C>>,
}

impl<C: EventCodec> ReadSnapshots<C> {
    pub fn new(read: Arc<SqliteReadModel<C>>) -> Self {
        Self { read }
    }
}

impl<C: EventCodec + 'static> SnapshotProvider for ReadSnapshots<C> {
    async fn snapshot(
        &self,
        scopes: &[pb::SubscriptionScope],
    ) -> Result<(pb::Snapshot, u64), SourceError> {
        let queries: Vec<pb::query::Kind> = scopes
            .iter()
            .flat_map(snapshot_queries)
            .filter_map(|q| q.kind)
            .collect();
        // 快照只由视图查询组成（登记表 snapshot_queries）；混进别的是登记表写错了
        if let Some(k) = queries.iter().find(|k| place(k) != Place::View) {
            return Err(SourceError(format!("快照里有非视图查询：{k:?}")));
        }
        let read = self.read.view(move |tx| {
            let head = tx.head()?;
            let mut results = Vec::with_capacity(queries.len());
            for kind in &queries {
                match answer_in(tx, head, kind)? {
                    Ok(r) => results.push(r),
                    // 订阅了不存在（已删）的对话：快照里没有这一项，和假后台一样；客户端随后收到删除事件或重查
                    Err(e) if e.code == i32::from(pb::ErrorCode::NotFound) => {}
                    Err(e) => return Ok(Err(e)),
                }
            }
            Ok(Ok((pb::Snapshot { results }, head.0)))
        });
        match read.await {
            Ok(Ok(snapshot)) => Ok(snapshot),
            Ok(Err(e)) => Err(SourceError(format!("快照查询被拒：{}", e.message))),
            Err(e) => Err(SourceError(format!("快照读库失败：{e}"))),
        }
    }
}
