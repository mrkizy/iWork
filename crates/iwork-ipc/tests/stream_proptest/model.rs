use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use iwork_ipc::client::{Client, ClientError, Incoming, IncomingItem};
use iwork_proto::pb;

use crate::support::*;

/// 状态只从 Incoming 写入；这里不持有服务端 World。
#[derive(Default)]
pub struct View {
    pub scopes: BTreeMap<u32, pb::SubscriptionScope>,
    synced: BTreeSet<u32>,
    pub cursor: u64,
    entities: BTreeMap<String, u64>,
    members: BTreeSet<String>,
    pub texts: BTreeMap<String, String>,
    queries: BTreeMap<u64, String>,
}

impl View {
    pub fn active(&self) -> Vec<pb::SubscriptionScope> {
        self.scopes.values().cloned().collect()
    }

    pub fn visible(&self) -> BTreeMap<String, u64> {
        self.members
            .iter()
            .filter(|e| covers(&self.active(), e))
            .map(|e| (e.clone(), self.entities[e]))
            .collect()
    }

    fn replace(&mut self, data: BTreeMap<String, u64>, only: Option<&str>) {
        let scopes = self.active();
        self.members.retain(|e| {
            !(covers(&scopes, e)
                && only.is_none_or(|c| keys_cover(&scope_keys(&conversation(c)), e)))
        });
        for (e, v) in data {
            self.entities.insert(e.clone(), v);
            if covers(&scopes, &e) {
                self.members.insert(e);
            }
        }
    }

    fn apply(&mut self, item: IncomingItem) -> bool {
        match item {
            IncomingItem::Event(e) => match e.body.expect("事件有载荷") {
                pb::event::Body::Persisted(p) => {
                    assert!(p.seq > self.cursor, "事件不回退、不重复");
                    self.cursor = p.seq;
                    let op = decode_op(&p.actor);
                    if covers(&self.active(), op.entity()) {
                        match op {
                            Op::Upsert { entity, value } => {
                                self.entities.insert(entity.clone(), value);
                                self.members.insert(entity);
                            }
                            Op::Delete { entity } => {
                                self.entities.remove(&entity);
                                self.members.remove(&entity);
                            }
                        }
                    }
                }
                pb::event::Body::Transient(t) => {
                    let Some(pb::transient_event::Event::MessageDelta(d)) = t.event else {
                        panic!("预期 MessageDelta");
                    };
                    let text = self.texts.entry(d.message_id).or_default();
                    assert_eq!(d.offset as usize, text.len(), "增量字节偏移连续");
                    text.push_str(&d.text);
                }
            },
            IncomingItem::Subscribed { subscribed, .. } => {
                self.synced.insert(subscribed.subscription_id);
                if let Some(snapshot) = subscribed.snapshot {
                    assert!(subscribed.snapshot_seq >= self.cursor, "快照不回退");
                    self.replace(
                        snapshot.results.iter().flat_map(decode_view).collect(),
                        None,
                    );
                    self.cursor = subscribed.snapshot_seq;
                }
            }
            IncomingItem::ViewResult { request_id, result } => {
                assert!(result.seq >= self.cursor, "查询在流里的位置不回退");
                let conv = self.queries.remove(&request_id).expect("已登记查询");
                self.replace(decode_view(&result), Some(&conv));
                self.cursor = result.seq;
            }
            IncomingItem::ServerError(e) => panic!("不该有协议错误：{e:?}"),
            IncomingItem::Closed(_) => return false,
        }
        true
    }
}

/// 服务端日志只供断言使用，不能给 View 补数据。
#[derive(Default)]
struct Audit {
    start: u64,
    received: Vec<pb::PersistedEvent>,
}

impl Audit {
    fn check_segment(&self, h: &Harness, view: &View, through: u64) {
        let scopes = view.active();
        let st = h.world.state.lock().unwrap();
        let expected: Vec<_> = st
            .events
            .iter()
            .filter(|(e, op)| {
                e.seq() > self.start && e.seq() <= through && covers(&scopes, op.entity())
            })
            .map(|(e, _)| e.event.clone())
            .collect();
        assert_eq!(
            self.received, expected,
            "区间 ({}, {through}] 完整持久事件序列",
            self.start
        );
    }

    fn observe(&mut self, h: &Harness, view: &View, item: &IncomingItem) {
        match item {
            IncomingItem::Event(e) => {
                let Some(pb::event::Body::Persisted(p)) = &e.body else {
                    return;
                };
                let subscription_ids = &e.subscription_ids;
                let op = h.world.op_at(p.seq);
                let ids: Vec<_> = view
                    .scopes
                    .iter()
                    .filter(|(_, s)| covers(&[(*s).clone()], op.entity()))
                    .map(|(id, _)| *id)
                    .collect();
                assert_eq!(*subscription_ids, ids, "多路由只交付一次，携带全部匹配范围");
                self.received.push(p.clone());
            }
            IncomingItem::Subscribed { subscribed, .. } => {
                if let Some(snapshot) = &subscribed.snapshot {
                    // 快照可覆盖尚未发出的事件；快照之前已发出的部分仍须是完整前缀。
                    self.check_segment(h, view, view.cursor);
                    assert_eq!(
                        snapshot.results.len(),
                        view.scopes.len(),
                        "每个范围有一个快照部分"
                    );
                    let data: BTreeMap<_, _> = snapshot
                        .results
                        .iter()
                        .flat_map(|r| {
                            assert_eq!(r.seq, subscribed.snapshot_seq);
                            decode_view(r)
                        })
                        .collect();
                    assert_eq!(
                        data,
                        visible(
                            &h.world.projection_at(subscribed.snapshot_seq),
                            &view.active()
                        ),
                        "快照真实载荷"
                    );
                    self.start = subscribed.snapshot_seq;
                    self.received.clear();
                }
            }
            IncomingItem::ViewResult { request_id, result } => {
                self.check_segment(h, view, result.seq);
                let conv = &view.queries[request_id];
                assert_eq!(
                    decode_view(result),
                    h.world
                        .view_at(result.seq, &scope_keys(&conversation(conv))),
                    "查询真实载荷"
                );
                self.start = result.seq;
                self.received.clear();
            }
            _ => {}
        }
    }
}

#[derive(Default)]
pub struct Model {
    pub view: View,
    audit: Audit,
    conn: Option<(Arc<Client>, Incoming)>,
    failed_at: Option<u64>,
    pub reconnects: usize,
}

impl Model {
    pub async fn connect(&mut self, h: &Harness, involuntary: bool) {
        self.audit.check_segment(h, &self.view, self.view.cursor);
        self.audit = Audit {
            start: self.view.cursor,
            received: Vec::new(),
        };
        let from = if self
            .view
            .scopes
            .keys()
            .any(|id| !self.view.synced.contains(id))
        {
            0
        } else if involuntary && self.failed_at == Some(self.view.cursor) {
            self.failed_at = None;
            0
        } else {
            if involuntary {
                self.failed_at = Some(self.view.cursor);
            }
            self.view.cursor
        };
        self.conn = None;
        self.view.queries.clear();
        self.view.texts.clear();
        let mut cfg = client_cfg();
        cfg.incoming_limit = 16 * 1024;
        let (c, inc) = Client::connect(&h.connector, &h.endpoint, &client_own(), cfg)
            .await
            .expect("重连");
        for (id, s) in &self.view.scopes {
            drop(c.subscribe_nowait(*id, s.clone(), from).expect("刚连上"));
        }
        self.conn = Some((Arc::new(c), inc));
        self.reconnects += 1;
    }

    pub fn client(&self) -> Arc<Client> {
        Arc::clone(&self.conn.as_ref().expect("已连接").0)
    }

    pub fn query(&mut self, conv: &str) -> Option<u64> {
        let (id, rx) = match self.client().view_query_nowait(get_conversation(conv)) {
            Ok(sent) => sent,
            // 写端已经得知关闭，Incoming 里仍可能有尚未应用的事件：先按顺序读完到 Closed，再重连。
            Err(ClientError::Closed) => return None,
            Err(e) => panic!("发查询：{e:?}"),
        };
        drop(rx);
        self.view.queries.insert(id, conv.into());
        Some(id)
    }

    async fn next(&mut self, h: &Harness, wait: Duration) -> Option<u64> {
        let item = match tokio::time::timeout(wait, self.conn.as_mut().unwrap().1.next()).await {
            Ok(Some(item)) => item,
            Ok(None) => panic!("Closed 之后才结束"),
            Err(_) => panic!("静止阶段等不到指定查询结果"),
        };
        self.audit.observe(h, &self.view, &item);
        let id = match &item {
            IncomingItem::ViewResult { request_id, .. } => Some(*request_id),
            _ => None,
        };
        if !self.view.apply(item) {
            self.connect(h, true).await;
        }
        id
    }

    pub async fn pump(&mut self, h: &Harness) {
        // 超时只结束本次抽取，不用时间推断一个区间已经交付完整。
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        while let Ok(Some(item)) = tokio::time::timeout(
            Duration::from_millis(1),
            self.conn.as_mut().unwrap().1.next(),
        )
        .await
        {
            self.audit.observe(h, &self.view, &item);
            if !self.view.apply(item) {
                self.connect(h, true).await;
            }
        }
    }

    /// 用明确的 request_id 作为区间末尾，不能用相同 seq 的其他查询冒充屏障。
    pub async fn settle(&mut self, h: &Harness) {
        for _ in 0..50 {
            let generation = self.reconnects;
            let id = self.query("c1");
            while generation == self.reconnects {
                let received = self.next(h, Duration::from_secs(10)).await;
                if id.is_some() && received == id {
                    assert_eq!(
                        self.view.visible(),
                        visible(&h.world.projection_at(h.world.head()), &self.view.active()),
                        "可见视图与独立投影一致"
                    );
                    return;
                }
            }
        }
        panic!("有限次重连内应能静止");
    }

    pub async fn subscribe(&mut self, h: &Harness, id: u32, scope: pb::SubscriptionScope) {
        self.settle(h).await;
        self.view.scopes.insert(id, scope.clone());
        self.view.synced.remove(&id);
        drop(self.client().subscribe_nowait(id, scope, 0).expect("订阅"));
        self.settle(h).await;
    }

    pub async fn unsubscribe(&mut self, h: &Harness, id: u32) {
        self.settle(h).await;
        if self.view.scopes.remove(&id).is_some() {
            self.view.synced.remove(&id);
            drop(self.client().unsubscribe_nowait(id).expect("退订"));
        }
        self.settle(h).await;
    }
}
