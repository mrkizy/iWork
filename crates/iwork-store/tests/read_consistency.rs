//! 第 1 轮审查 1/F-1：一次 `ReadModelPort` 调用里的几条 SELECT 读同一个提交时刻。
//! 用一个包装 codec 在读线程解码消息快照时同步提交 `MessageCompleted`（它会改消息、删检查点），
//! 断言这次查询返回的「状态 + 正文」是某个已提交状态，而不是新旧拼起来的 `Streaming + ""`。

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::event::*;
use iwork_domain::message::{MessageBody, MessageStatus};
use iwork_ports::read_model::{MessagePage, PendingItem, ReadModelPort};
use iwork_ports::store::{CodecError, EncodedEvent, EventCodec, StoragePort};
use iwork_store::{OpenOptions, ReadDefaults, SqliteProjector, SqliteStore};
use support::*;

type HookStore = SqliteStore<HookCodec>;

/// 在读线程第一次解码 `kind` 时先跑一次钩子。
struct HookCodec {
    kind: &'static str,
    armed: AtomicBool,
    hook: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl EventCodec for HookCodec {
    fn protocol_major(&self) -> u32 {
        ProtoEventCodec.protocol_major()
    }
    fn encode(&self, event: &DomainEvent) -> Result<EncodedEvent, CodecError> {
        ProtoEventCodec.encode(event)
    }
    fn decode(&self, kind: &str, payload: &[u8]) -> Result<DomainEvent, CodecError> {
        let on_reader = std::thread::current()
            .name()
            .is_some_and(|n| n.starts_with("iwork-store-reader"));
        if on_reader && kind == self.kind && self.armed.swap(false, Ordering::SeqCst) {
            (self.hook.get().expect("钩子已装"))();
        }
        ProtoEventCodec.decode(kind, payload)
    }
}

fn open(env: &Env, kind: &'static str) -> (Arc<HookStore>, Arc<HookCodec>) {
    let codec = Arc::new(HookCodec {
        kind,
        armed: AtomicBool::new(false),
        hook: OnceLock::new(),
    });
    let store = SqliteStore::open_with(
        &env.dir,
        codec.clone(),
        Arc::new(SqliteProjector::new(codec.clone())),
        env.clock.clone(),
        Env::config(),
        OpenOptions::default(),
    )
    .unwrap();
    (Arc::new(store), codec)
}

fn defaults() -> ReadDefaults {
    ReadDefaults {
        global_settings: iwork_domain::settings::GlobalSettings {
            playful_ui: true,
            autostart: false,
            projects_root: "/x".into(),
        },
    }
}

fn seed(store: &HookStore) {
    block(store.commit(cmd(
        1,
        vec![
            ProjectAdded { project: project(1) }.into(),
            // 回合的助手：审批与卡片写入前查归属存续（T02 方案 2.6）
            AssistantCreated { assistant: assistant(1, false) }.into(),
            ConversationCreated { conversation: project_conversation(10, 1) }.into(),
            TurnQueued { turn: turn(20, 10) }.into(),
            MessageAppended { message: assistant_message(30, 10, 20) }.into(),
        ],
    )))
    .unwrap();
    block(store.checkpoint_message(mid(30), 5, "hello")).unwrap();
}

#[test]
fn message_completed_during_read_never_mixes_states() {
    let env = Env::new();
    let (store, codec) = open(&env, "message_appended");
    seed(&store);
    let weak: Weak<HookStore> = Arc::downgrade(&store);
    let _ = codec.hook.set(Box::new(move || {
        let store = weak.upgrade().unwrap();
        let done: DomainEvent = MessageCompleted {
            conversation_id: cid(10),
            message_id: mid(30),
            body: MessageBody::Inline("hello final".into()),
            status: MessageStatus::Completed,
        }
        .into();
        block(store.commit(cmd(2, vec![done]))).unwrap();
    }));
    codec.armed.store(true, Ordering::SeqCst);

    let rm = store.read_model(defaults());
    let page = MessagePage {
        before: None,
        limit: 10,
    };
    let view = block(rm.conversation(cid(10), page)).unwrap();
    assert!(
        !codec.armed.load(Ordering::SeqCst),
        "对照：钩子确实在查询中途提交了"
    );
    let m = &view.messages[0];
    assert_eq!(
        (m.status, m.body.clone()),
        (
            MessageStatus::Streaming,
            MessageBody::Inline("hello".into())
        ),
        "这次查询读的是提交前的同一时刻"
    );
    let after = block(rm.conversation(cid(10), page)).unwrap();
    assert_eq!(
        (after.messages[0].status, after.messages[0].body.clone()),
        (
            MessageStatus::Completed,
            MessageBody::Inline("hello final".into())
        ),
        "下一次查询看到提交后的状态"
    );
}

/// 同类：`pending_items` 的几组 SELECT 也读同一时刻。解码第一条审批时提交「批准它 + 新提一张卡片」，
/// 这次返回不能「审批已不在、卡片却已出现」这样新旧拼接。
#[test]
fn pending_items_read_one_snapshot() {
    let env = Env::new();
    let (store, codec) = open(&env, "approval_requested");
    seed(&store);
    block(store.commit(cmd(
        3,
        vec![ApprovalRequested { approval: approval(40, 20, 10) }.into()],
    )))
    .unwrap();
    let weak: Weak<HookStore> = Arc::downgrade(&store);
    let _ = codec.hook.set(Box::new(move || {
        let store = weak.upgrade().unwrap();
        block(store.commit(cmd(
            4,
            vec![
                ApprovalDecided {
                    conversation_id: Some(cid(10)),
                    approval_id: apid(40),
                    state: iwork_domain::approval::ApprovalState::Denied,
                    remember: None,
                }
                .into(),
                UserActionProposed { card: card(50, 20, 10) }.into(),
            ],
        )))
        .unwrap();
    }));
    codec.armed.store(true, Ordering::SeqCst);
    let rm = store.read_model(defaults());
    let items = block(rm.pending_items()).unwrap();
    assert!(!codec.armed.load(Ordering::SeqCst), "对照：钩子确实提交了");
    assert_eq!(items.len(), 1, "只有提交前的那一项：{items:?}");
    assert!(matches!(&items[0], PendingItem::Approval(a) if a.id == apid(40)));
    let after = block(rm.pending_items()).unwrap();
    assert!(matches!(after.as_slice(), [PendingItem::UserAction(c)] if c.id == uaid(50)));
}
