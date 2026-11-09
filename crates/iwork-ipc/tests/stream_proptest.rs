//! 断线续传 proptest（验收 4；方案 7.1）。
//!
//! 随机：提交（写入 / 删除，单路由键）、增删范围（含重叠范围、同 id 替换）、会写视图的查询、主动断线重连、
//! 小预算让服务端 `fail_connection`、快照两部分之间夹提交（必须被拒绝重取）、补发中途提交。
//! 客户端模型按方案 2.11 的契约实现：只从 `Incoming` 按到达顺序应用，按实体 id 幂等写入，快照与查询结果只替换视图
//! 成员，游标在应用时推进；连接被关就重连并一次写出全部 `Subscribe`，同游标第二次被关改用 `from_seq = 0`。
//!
//! 断言：每条连接上送出的持久事件严格递增、不重，且都在客户端游标之后（流不回退）；静止后客户端每个可见实体与
//! 服务端投影完全一致，且可见成员集合相同（没有漏掉、没有被快照或分页删错）。

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use iwork_ipc::client::{Client, Incoming, IncomingItem};
use iwork_proto::pb;
use proptest::prelude::*;
use support::*;

const ENTITIES: [&str; 5] = ["conversation:c1", "conversation:c2", "conversation:c3", "project:p1", "project:p2"];

fn scope_of(i: u8) -> pb::SubscriptionScope {
    match i % 4 {
        0 => sidebar(),
        1 => conversation("c1"),
        2 => conversation("c2"),
        _ => conversation("c3"),
    }
}

#[derive(Debug, Clone)]
enum Act {
    Commit { entity: u8, delete: bool },
    Burst { n: u8 },
    Subscribe { id: u8, scope: u8 },
    Unsubscribe { id: u8 },
    ViewQuery { conv: u8 },
    Disconnect,
    TearNextSnapshot,
    Pump,
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        4 => (0u8..5, prop::bool::weighted(0.2)).prop_map(|(entity, delete)| Act::Commit { entity, delete }),
        1 => (5u8..40).prop_map(|n| Act::Burst { n }),
        2 => (1u8..4, 0u8..4).prop_map(|(id, scope)| Act::Subscribe { id, scope }),
        1 => (1u8..4).prop_map(|id| Act::Unsubscribe { id }),
        1 => (1u8..4).prop_map(|conv| Act::ViewQuery { conv }),
        1 => Just(Act::Disconnect),
        1 => Just(Act::TearNextSnapshot),
        3 => Just(Act::Pump),
    ]
}

struct Model {
    world: World,
    scopes: BTreeMap<u32, pb::SubscriptionScope>,
    /// 视图已经对齐到游标的范围（收到过它的 Subscribed）。还没对齐的范围在重连时不能续传。
    synced: BTreeSet<u32>,
    cursor: u64,
    entities: BTreeMap<String, u64>,
    members: BTreeSet<String>,
    /// 上一次连接被关时的游标（同游标第二次被关就改快照）。
    failed_at: Option<u64>,
    /// 本连接上最后一条持久事件的 seq（检查严格递增）。
    last_on_conn: u64,
    conn: Option<(Arc<Client>, Incoming)>,
    reconnects: usize,
}

impl Model {
    fn active(&self) -> Vec<pb::SubscriptionScope> {
        self.scopes.values().cloned().collect()
    }

    fn replace_view(&mut self, at: u64, only: Option<&str>) {
        let scopes = self.active();
        let p = self.world.projection_at(at);
        let in_view = |e: &str| covers(&scopes, e) && only.is_none_or(|o| o == e);
        self.members.retain(|e| !in_view(e));
        for (e, v) in p {
            if in_view(&e) {
                self.entities.insert(e.clone(), v);
                self.members.insert(e);
            }
        }
    }

    /// 应用一项。返回 false 表示连接结束。
    fn apply(&mut self, item: IncomingItem) -> bool {
        match item {
            IncomingItem::Event(pb::Event { body: Some(pb::event::Body::Persisted(p)), .. }) => {
                assert!(p.seq > self.last_on_conn, "一条连接上严格递增：{} 在 {} 之后", p.seq, self.last_on_conn);
                assert!(p.seq > self.cursor, "流不回退：{} ≤ 游标 {}", p.seq, self.cursor);
                self.last_on_conn = p.seq;
                self.cursor = p.seq;
                let op = self.world.op_at(p.seq);
                // 没有活动范围对应的（退订后排着的）丢掉。
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
            IncomingItem::Event(_) => {}
            IncomingItem::Subscribed { subscribed, .. } => {
                if self.scopes.contains_key(&subscribed.subscription_id) {
                    self.synced.insert(subscribed.subscription_id);
                }
                if subscribed.snapshot.is_some() {
                    let s = subscribed.snapshot_seq;
                    assert!(s >= self.cursor, "快照不回退：{s} < {}", self.cursor);
                    self.replace_view(s, None);
                    self.cursor = s;
                    self.last_on_conn = s;
                }
            }
            IncomingItem::ViewResult { result, .. } => {
                assert!(result.seq >= self.cursor, "查询结果在流里的位置不回退");
                // 测试 handler 对 GetConversation 的结果不带内容：结果的含义是「这个视图在 S 时刻的成员与内容」。
                self.cursor = result.seq;
                self.last_on_conn = result.seq;
            }
            IncomingItem::ServerError(e) => panic!("不该有协议错误：{e:?}"),
            IncomingItem::Closed(_) => return false,
        }
        true
    }

    async fn connect(&mut self, h: &Harness, involuntary: bool) {
        let all_synced = self.scopes.keys().all(|id| self.synced.contains(id));
        let from = if !all_synced {
            // 有范围的视图还没对齐（它的 Subscribed 没到就断了）：不能声称「全部齐到游标」，走快照。
            0
        } else if involuntary {
            if self.failed_at == Some(self.cursor) {
                self.failed_at = None;
                0
            } else {
                self.failed_at = Some(self.cursor);
                self.cursor
            }
        } else {
            self.cursor
        };
        let mut cfg = client_cfg();
        cfg.incoming_limit = 16 * 1024;
        let (c, inc) = Client::connect(&h.connector, &h.endpoint, &client_own(), cfg).await.expect("重连");
        self.reconnects += 1;
        self.last_on_conn = 0;
        // 全部范围一次写出。
        for (id, s) in &self.scopes {
            let _ = c.subscribe_nowait(*id, s.clone(), from).expect("刚连上");
        }
        if from == 0 && self.scopes.is_empty() {
            self.cursor = 0;
        }
        self.conn = Some((Arc::new(c), inc));
    }

    /// 把已经到达的都应用掉；连接被关就重连。
    async fn pump(&mut self, h: &Harness) {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        loop {
            let Some((_, inc)) = self.conn.as_mut() else { return };
            match tokio::time::timeout(Duration::from_millis(1), inc.next()).await {
                Ok(Some(item)) => {
                    if !self.apply(item) {
                        self.conn = None;
                        self.connect(h, true).await;
                    }
                }
                Ok(None) => unreachable!("Closed 之后才结束"),
                Err(_) => return,
            }
        }
    }

    fn client(&self) -> Arc<Client> {
        Arc::clone(&self.conn.as_ref().expect("已连接").0)
    }
}

async fn run(acts: Vec<Act>, small_budget: bool) {
    let mut cfg = server_cfg();
    cfg.replay_page = 4;
    cfg.handover_timeout = Duration::from_secs(5);
    if small_budget {
        cfg.budget_limit = 8 * 1024;
        cfg.replay_reserve = 1024;
    }
    let h = start(cfg);
    *h.world.pad.lock().unwrap() = 200;
    let mut m = Model {
        world: h.world.clone(),
        scopes: BTreeMap::new(),
        synced: BTreeSet::new(),
        cursor: 0,
        entities: BTreeMap::new(),
        members: BTreeSet::new(),
        failed_at: None,
        last_on_conn: 0,
        conn: None,
        reconnects: 0,
    };
    m.connect(&h, false).await;
    let mut value = 0u64;
    for a in acts {
        match a {
            Act::Commit { entity, delete } => {
                value += 1;
                let e = ENTITIES[entity as usize].to_owned();
                h.publish(if delete { Op::Delete { entity: e } } else { Op::Upsert { entity: e, value } });
            }
            Act::Burst { n } => {
                // 边提交边让出：提交落在补发、快照、交接的各个中间点上。
                for i in 0..n {
                    value += 1;
                    h.upsert(ENTITIES[(i % 5) as usize], value);
                    if i % 3 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            }
            Act::Subscribe { id, scope } => {
                let s = scope_of(scope);
                m.scopes.insert(id as u32, s.clone());
                m.synced.remove(&(id as u32));
                // 新范围从快照起（from_seq 0）；同 id 替换。
                let _ = m.client().subscribe_nowait(id as u32, s, 0);
            }
            Act::Unsubscribe { id } => {
                m.synced.remove(&(id as u32));
                if m.scopes.remove(&(id as u32)).is_some() {
                    let scopes = m.active();
                    m.members.retain(|e| covers(&scopes, e));
                    // 立即写出：帧顺序必须与调用顺序一致（之后的 Subscribe 不能跑到它前面）。
                    let _ = m.client().unsubscribe_nowait(id as u32);
                }
            }
            Act::ViewQuery { conv } => {
                let c = m.client();
                tokio::spawn(async move { c.view_query(get_conversation(&format!("c{conv}"))).await });
            }
            Act::Disconnect => {
                m.conn = None;
                m.connect(&h, false).await;
            }
            Act::TearNextSnapshot => *h.world.snapshot_tear_once.lock().unwrap() = true,
            Act::Pump => m.pump(&h).await,
        }
    }

    // 静止：发一个会写视图的查询，等它的结果到达——它在流里的位置是当前 head，之前的事件都已送到。
    let mut settled = false;
    for _ in 0..50 {
        m.pump(&h).await;
        let c = m.client();
        let q = tokio::spawn(async move { c.view_query(get_conversation("c1")).await });
        let head = h.world.head();
        let mut done = false;
        while let Some((_, inc)) = m.conn.as_mut() {
            let Ok(Some(item)) = tokio::time::timeout(Duration::from_secs(10), inc.next()).await else {
                panic!("静止阶段等不到东西");
            };
            let is_final = matches!(&item, IncomingItem::ViewResult { result, .. } if result.seq == head);
            if !m.apply(item) {
                m.conn = None;
                m.connect(&h, true).await;
                break;
            }
            if is_final {
                done = true;
                break;
            }
        }
        let _ = q.await;
        if done {
            settled = true;
            break;
        }
    }
    assert!(settled, "有限次重连内应能静止（没有新提交时）");
    assert_eq!(m.cursor, h.world.head(), "游标追平");

    let scopes = m.active();
    let truth = visible(&h.world.projection_at(h.world.head()), &scopes);
    let mine: BTreeMap<String, u64> =
        m.members.iter().filter(|e| covers(&scopes, e)).map(|e| (e.clone(), m.entities[e])).collect();
    assert_eq!(mine, truth, "静止后可见视图与服务端投影一致（重连 {} 次）", m.reconnects);
}

proptest! {
    // 默认 64 例；PROPTEST_CASES=2000 跑完整一轮。
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(64),
        ..ProptestConfig::default()
    })]

    #[test]
    fn stream_never_loses_or_duplicates_across_reconnects(
        acts in prop::collection::vec(act(), 1..60),
        small_budget in prop::bool::weighted(0.4),
    ) {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(run(acts, small_budget));
    }
}
