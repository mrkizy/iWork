//! 事件发布接线（T04 方案 2.6；T03 方案 2.10）。
//!
//! - [`PublishObserver`]：存储写线程一批提交成功后、回执送出前回调它，按 seq 顺序把新事件转成 `RoutedEvent`
//!   交给 ipc 的 `Publisher`。顺序取自唯一知道提交顺序的地方（单写者），别处不再排。
//! - [`MessageRoutes`]：「在流的消息 → 路由键」表。流式增量只带 `message_id`，路由靠它查；键是消息所在对话的
//!   `live:<对话>`（T10a 方案 2.7 G6），不是 `MessageAppended` 的路由，侧栏因此收不到增量。
//! - [`LiveDeltas`]：`LivePublisher` 的实现（T10 的回合宿主注入编排）。
//!
//! 观察者跑在写线程上：只做内存操作与非阻塞唤醒，不做 I/O、不等任何东西（`CommitObserver` 的契约）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use iwork_app::subscription::live_key;
use iwork_domain::event::DomainEvent;
use iwork_domain::id::MessageId;
use iwork_domain::message::MessageStatus;
use iwork_ipc::server::{Published, Publisher};
use iwork_ports::live::{LivePublisher, MessageDelta};
use iwork_proto::pb;
use iwork_proto::routing::StreamKey;
use iwork_store::{CommitObserver, CommittedBatch};
use tokio::sync::watch;

use crate::source::routed;

/// 发布的去处。ipc 的 `Publisher` 是唯一的生产实现；测试换成记录器。
pub trait PublishSink: Send + Sync + 'static {
    fn publish(&self, p: Published);
}

impl PublishSink for Publisher {
    fn publish(&self, p: Published) {
        Publisher::publish(self, p);
    }
}

/// 在流的消息 → 路由键（它所在对话的 live 键）。
#[derive(Default)]
pub struct MessageRoutes(Mutex<HashMap<MessageId, Vec<StreamKey>>>);

impl MessageRoutes {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<MessageId, Vec<StreamKey>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, id: &MessageId) -> Option<Vec<StreamKey>> {
        self.lock().get(id).cloned()
    }

    /// 按一条已提交的事件维护：开始流式的登记，结束的摘掉。
    fn observe(&self, event: &DomainEvent) {
        match event {
            DomainEvent::MessageAppended(e) if e.message.status == MessageStatus::Streaming => {
                self.lock()
                    .insert(e.message.id, vec![live_key(e.message.conversation_id)]);
            }
            DomainEvent::MessageCompleted(e) => {
                self.lock().remove(&e.message_id);
            }
            _ => {}
        }
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 发布这一侧对外可读的东西：发布核对读 `last_published`，投递的各条道等 `outbox`。
#[derive(Clone)]
pub struct PublishState {
    /// 观察者交给发布方的最高 seq。
    pub last_published: Arc<AtomicU64>,
    /// `serve` 起来之前库里的 head：ipc 把它当发布水位的基线（这之前的事件靠补发）。
    pub baseline: Arc<AtomicU64>,
    pub routes: Arc<MessageRoutes>,
    /// 每有一批新写入的 outbox 条目就加一（各条道各持一个接收端，取到取不到为止后等它变）。
    pub outbox: watch::Sender<u64>,
    /// 提交了自启设置（`GlobalSettingsChanged{autostart: Some(_)}`）就叫醒对齐任务。
    pub autostart: Arc<tokio::sync::Notify>,
    /// 引擎流里有新事件就叫醒 `engine-sync`（T07 6.10.8：观察者里只叫醒，`sync()` 在独立任务里跑）。
    pub engine: Arc<tokio::sync::Notify>,
    /// 回合宿主关心的事件（[`crate::host::TURN_HOST_WAKE_KINDS`]）落盘就叫醒它（T10a 0a.6）。
    pub host: Arc<tokio::sync::Notify>,
}

impl PublishState {
    pub fn new() -> Self {
        Self {
            last_published: Arc::new(AtomicU64::new(0)),
            baseline: Arc::new(AtomicU64::new(0)),
            routes: Arc::new(MessageRoutes::default()),
            outbox: watch::Sender::new(0),
            autostart: Arc::new(tokio::sync::Notify::new()),
            engine: Arc::new(tokio::sync::Notify::new()),
            host: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn last_published(&self) -> u64 {
        self.last_published.load(Ordering::SeqCst)
    }

    pub fn set_baseline(&self, head: u64) {
        self.baseline.store(head, Ordering::SeqCst);
    }

    /// 发布水位：已发布的最高 seq 与基线中较大的那个（发布核对拿它和库里的 head 比）。
    pub fn watermark(&self) -> u64 {
        self.last_published()
            .max(self.baseline.load(Ordering::SeqCst))
    }
}

impl Default for PublishState {
    fn default() -> Self {
        Self::new()
    }
}

/// 提交观察者。
pub struct PublishObserver<S: PublishSink> {
    sink: Arc<S>,
    state: PublishState,
}

impl<S: PublishSink> PublishObserver<S> {
    pub fn new(sink: Arc<S>, state: PublishState) -> Self {
        Self { sink, state }
    }
}

impl<S: PublishSink> CommitObserver for PublishObserver<S> {
    fn committed(&mut self, batch: CommittedBatch<'_>) {
        for e in batch.events {
            // 转换失败是程序缺陷（同一程序写进去的事件转不出来）：panic 让写线程退出，存储失效 → 整体退出（方案 2.6）
            let r = routed(e).unwrap_or_else(|err| {
                tracing::error!(seq = e.seq.0, error = %err, "提交的事件转换失败");
                panic!("提交的事件 {} 转换失败：{err}", e.seq.0)
            });
            self.state.routes.observe(&e.event);
            if let DomainEvent::GlobalSettingsChanged(c) = &e.event
                && c.autostart.is_some()
            {
                // 非阻塞：只存一个许可，对齐任务醒来自己读最新的已提交设置
                self.state.autostart.notify_one();
            }
            self.sink.publish(Published::Persisted(Arc::new(r)));
            self.state.last_published.store(e.seq.0, Ordering::SeqCst);
        }
        // 只置位：不读写存储、不碰引擎的锁（不变量 6）。一批里有一条就够
        if batch
            .events
            .iter()
            .any(|e| iwork_core::approval::engine::fold::in_engine_streams(&e.event))
        {
            self.state.engine.notify_one();
        }
        if batch
            .events
            .iter()
            .any(|e| crate::host::TURN_HOST_WAKE_KINDS.contains(&e.event.kind()))
        {
            self.state.host.notify_one();
        }
        if batch.outbox_added > 0 {
            self.state.outbox.send_modify(|n| *n = n.wrapping_add(1));
        }
    }
}

/// 流式增量的出口：按消息查路由表；查不到（消息已结束）就丢掉这段。
pub struct LiveDeltas<S: PublishSink> {
    sink: Arc<S>,
    routes: Arc<MessageRoutes>,
}

impl<S: PublishSink> LiveDeltas<S> {
    pub fn new(sink: Arc<S>, routes: Arc<MessageRoutes>) -> Self {
        Self { sink, routes }
    }
}

impl<S: PublishSink> LivePublisher for LiveDeltas<S> {
    fn message_delta(&self, delta: MessageDelta) {
        let Some(routes) = self.routes.get(&delta.message_id) else {
            tracing::debug!(message_id = %delta.message_id, "增量所属的消息不在流中，丢掉");
            return;
        };
        self.sink.publish(Published::Transient {
            event: pb::TransientEvent {
                event: Some(pb::transient_event::Event::MessageDelta(pb::MessageDelta {
                    message_id: delta.message_id.to_string(),
                    offset: delta.offset,
                    text: delta.text,
                })),
            },
            routes,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use iwork_domain::common::{Actor, Timestamp};
    use iwork_domain::event::{ConversationCreated, MessageAppended, MessageCompleted};
    use iwork_domain::id::Seq;
    use iwork_domain::message::MessageBody;
    use iwork_ports::store::StoredEvent;
    use std::time::Duration;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Published>>);

    impl PublishSink for Recorder {
        fn publish(&self, p: Published) {
            self.0.lock().unwrap().push(p);
        }
    }

    fn stored(seq: u64, event: DomainEvent) -> StoredEvent {
        StoredEvent {
            seq: Seq(seq),
            at: Timestamp(T0),
            actor: Actor("user".into()),
            event,
        }
    }

    fn delta(message: u128, text: &str) -> MessageDelta {
        MessageDelta {
            message_id: mid(message),
            offset: 0,
            text: text.into(),
        }
    }

    /// 增量按「在流的消息 → 路由」表发：消息开始流式之前与结束之后的增量都丢掉；路由是消息所在对话的 live 键（G6）。
    #[test]
    fn live_delta_routed_by_message_table() {
        let rec = Arc::new(Recorder::default());
        let state = PublishState::new();
        let mut obs = PublishObserver::new(rec.clone(), state.clone());
        let live = LiveDeltas::new(rec.clone(), state.routes.clone());

        live.message_delta(delta(30, "早到"));
        let appended = stored(
            2,
            MessageAppended {
                message: assistant_message(30, 10, 20),
            }
            .into(),
        );
        obs.committed(CommittedBatch {
            events: &[
                stored(
                    1,
                    ConversationCreated {
                        conversation: project_conversation(10, 1),
                    }
                    .into(),
                ),
                appended.clone(),
            ],
            outbox_added: 0,
        });
        live.message_delta(delta(30, "中间"));
        obs.committed(CommittedBatch {
            events: &[stored(
                3,
                MessageCompleted {
                    conversation_id: cid(10),
                    message_id: mid(30),
                    body: MessageBody::Inline("全文".into()),
                    status: MessageStatus::Completed,
                }
                .into(),
            )],
            outbox_added: 0,
        });
        live.message_delta(delta(30, "晚到"));

        let got = rec.0.lock().unwrap();
        let seqs: Vec<_> = got
            .iter()
            .filter_map(|p| match p {
                Published::Persisted(e) => Some(e.seq()),
                _ => None,
            })
            .collect();
        assert_eq!(seqs, [1, 2, 3], "持久事件按 seq 顺序全部发布");
        let transients: Vec<_> = got
            .iter()
            .filter_map(|p| match p {
                Published::Transient { event, routes } => Some((event.clone(), routes.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(transients.len(), 1, "只有流式期间的那一段");
        assert_eq!(
            transients[0].1,
            [iwork_app::subscription::live_key(cid(10))],
            "不是 MessageAppended 的路由（那是整类 conversation 键能盖住的）"
        );
        match &transients[0].0.event {
            Some(pb::transient_event::Event::MessageDelta(d)) => assert_eq!(d.text, "中间"),
            other => panic!("{other:?}"),
        }
        assert_eq!(state.last_published(), 3);
        assert!(state.routes.is_empty());
    }

    /// G6（T10a 方案 2.7、第 11 节步 7 验收）：侧栏订的是整类 `conversation` 键，收不到任何对话的流式增量；
    /// 打开的那个对话的会话范围收得到，别的对话的会话范围收不到。对照：同一条消息的持久事件侧栏收得到。
    #[test]
    fn sidebar_scope_gets_no_deltas() {
        use iwork_app::subscription::{routes_for, scope_streams};
        let rec = Arc::new(Recorder::default());
        let state = PublishState::new();
        let mut obs = PublishObserver::new(rec.clone(), state.clone());
        let live = LiveDeltas::new(rec.clone(), state.routes.clone());
        let appended = stored(
            1,
            MessageAppended {
                message: assistant_message(30, 10, 20),
            }
            .into(),
        );
        obs.committed(CommittedBatch {
            events: std::slice::from_ref(&appended),
            outbox_added: 0,
        });
        live.message_delta(delta(30, "片段"));
        let routes = match rec.0.lock().unwrap().last() {
            Some(Published::Transient { routes, .. }) => routes.clone(),
            other => panic!("{other:?}"),
        };
        let scope = |kind| scope_streams(&pb::SubscriptionScope { kind: Some(kind) });
        use pb::subscription_scope::Kind as S;
        let hits = |keys: &[StreamKey], routes: &[StreamKey]| {
            keys.iter().any(|k| routes.iter().any(|r| k.covers(r)))
        };
        let sidebar = scope(S::Sidebar(pb::SidebarScope {}));
        let open = scope(S::Conversation(pb::ConversationScope {
            conversation_id: cid(10).to_string(),
        }));
        let other = scope(S::Conversation(pb::ConversationScope {
            conversation_id: cid(11).to_string(),
        }));
        assert!(!hits(&sidebar, &routes), "侧栏收到了增量：{routes:?}");
        assert!(hits(&open, &routes), "打开的对话收不到增量");
        assert!(!hits(&other, &routes), "别的对话收到了增量");
        assert!(
            hits(&sidebar, &routes_for(&appended.event)),
            "对照：持久事件侧栏收得到"
        );
    }

    /// 提交了自启设置就叫醒对齐任务；别的设置变化不叫。
    #[tokio::test]
    async fn autostart_setting_wakes_alignment() {
        use iwork_domain::event::GlobalSettingsChanged;
        let rec = Arc::new(Recorder::default());
        let state = PublishState::new();
        let mut obs = PublishObserver::new(rec, state.clone());
        let change = |autostart| -> DomainEvent {
            GlobalSettingsChanged {
                playful_ui: Some(true),
                autostart,
                projects_root: None,
            }
            .into()
        };
        obs.committed(CommittedBatch {
            events: &[stored(1, change(None))],
            outbox_added: 0,
        });
        let woke =
            tokio::time::timeout(Duration::from_millis(50), state.autostart.notified()).await;
        assert!(woke.is_err(), "别的设置不叫醒");
        obs.committed(CommittedBatch {
            events: &[stored(2, change(Some(false)))],
            outbox_added: 0,
        });
        tokio::time::timeout(Duration::from_millis(50), state.autostart.notified())
            .await
            .expect("提交了自启设置应叫醒");
    }

    #[test]
    fn outbox_added_bumps_the_wakeup() {
        let rec = Arc::new(Recorder::default());
        let state = PublishState::new();
        let mut rx = state.outbox.subscribe();
        let mut obs = PublishObserver::new(rec, state.clone());
        obs.committed(CommittedBatch {
            events: &[],
            outbox_added: 0,
        });
        assert!(!rx.has_changed().unwrap());
        obs.committed(CommittedBatch {
            events: &[],
            outbox_added: 3,
        });
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
    }
}
