//! 预置「上次崩溃会留下的状态」：用真存储直接写进沙箱的数据目录，再起后台。
//!
//! 预置数据只验证「恢复怎么标」；「真回合崩溃后真是这些状态」由前置任务的 `turn_dispatch_crash.rs` 证明。

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::event::*;
use iwork_domain::turn::TurnState;
use iwork_domain::user_action::UserActionState;
use iwork_ports::store::{CommitRequest, DeliveryState, StoragePort, StoredEvent};
use iwork_store::{DataDir, SqliteProjector, SqliteStore};
use rusqlite::Connection;

use crate::fixtures::*;
use crate::support::Sandbox;

pub type Store = SqliteStore<ProtoEventCodec>;

/// 打开沙箱里的库（假时钟）。
pub fn open(sb: &Sandbox) -> Store {
    let codec = Arc::new(ProtoEventCodec);
    SqliteStore::open(
        &DataDir::from_root(sb.data_root()),
        codec.clone(),
        Arc::new(SqliteProjector::new(codec)),
        Arc::new(FakeClock::new(T0)),
        Env::config(),
    )
    .expect("打开沙箱里的库")
}

/// 回合编号与预置状态。
pub const RUNNING_WITH_SESSION: u128 = 20;
pub const QUEUED_FRESH: u128 = 21;
pub const QUEUED_OPENED_BEFORE_COMMIT: u128 = 22;
pub const RUNNING_NO_SESSION: u128 = 23;
pub const WAITING_APPROVAL: u128 = 24;
pub const CANCELLING: u128 = 25;
pub const STREAMING_MESSAGE: u128 = 30;
pub const PENDING_APPROVAL: u128 = 40;
pub const CARDS: [u128; 3] = [51, 52, 53];
pub const DELETING_CONVERSATION: u128 = 11;
pub const CHECKPOINT: &str = "检查点里的半截正文";

fn turn_in(n: u128, state: TurnState, session: Option<&str>) -> DomainEvent {
    let mut t = turn(n, 10);
    t.state = state;
    t.runtime_session_id = session.map(Into::into);
    TurnQueued { turn: t }.into()
}

/// 方案 7.1 第 3 行的预置状态。返回预置完的 head。
pub fn seed_crash_state(sb: &Sandbox) -> u64 {
    let store = open(sb);
    let mut card_events: Vec<DomainEvent> = CARDS
        .iter()
        .zip([
            UserActionState::Starting,
            UserActionState::Executing,
            UserActionState::Stopping,
        ])
        .map(|(n, state)| {
            let mut c = card(*n, RUNNING_WITH_SESSION, 10);
            c.state = state;
            UserActionProposed { card: c }.into()
        })
        .collect();
    let mut events: Vec<DomainEvent> = vec![
        ProjectAdded {
            project: project(1),
        }
        .into(),
        AssistantCreated {
            assistant: assistant(1, false),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(10, 1),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(DELETING_CONVERSATION, 1),
        }
        .into(),
        turn_in(
            RUNNING_WITH_SESSION,
            TurnState::Running,
            Some("rt-session-1"),
        ),
        turn_in(QUEUED_FRESH, TurnState::Queued, None),
        turn_in(QUEUED_OPENED_BEFORE_COMMIT, TurnState::Queued, None),
        turn_in(RUNNING_NO_SESSION, TurnState::Running, None),
        turn_in(
            WAITING_APPROVAL,
            TurnState::WaitingApproval,
            Some("rt-session-2"),
        ),
        turn_in(CANCELLING, TurnState::Cancelling, Some("rt-session-3")),
        MessageAppended {
            message: assistant_message(STREAMING_MESSAGE, 10, RUNNING_WITH_SESSION),
        }
        .into(),
        ApprovalRequested {
            approval: approval(PENDING_APPROVAL, WAITING_APPROVAL, 10),
        }
        .into(),
    ];
    events.append(&mut card_events);
    events.push(
        ConversationDeleting {
            conversation_id: cid(DELETING_CONVERSATION),
        }
        .into(),
    );
    let mut req: CommitRequest = cmd(1, events);
    req.outbox = vec![
        outbox("test:a", b"1"),
        outbox("test:a", b"2"),
        outbox("test:b", b"3"),
        outbox("test:b", b"4"),
    ];
    block(store.commit(req)).expect("预置提交");
    block(store.checkpoint_message(mid(STREAMING_MESSAGE), CHECKPOINT.len() as u64, CHECKPOINT))
        .expect("写检查点");
    let first = block(store.outbox_pending(10)).unwrap()[0].id;
    block(store.outbox_mark(first, DeliveryState::Dispatched)).unwrap();
    let head = block(store.head_seq()).unwrap().0;
    store.close().unwrap();
    head
}

/// `from` 之后的全部事件。
pub fn events_after(sb: &Sandbox, from: u64) -> Vec<StoredEvent> {
    let store = open(sb);
    let ev = block(store.read_events(
        iwork_ports::store::EventFilter::All,
        iwork_domain::id::Seq(from),
        10_000,
    ))
    .unwrap();
    store.close().unwrap();
    ev
}

pub fn sql_text(db: &Path, sql: &str, id: String) -> String {
    let conn = Connection::open(db).unwrap();
    conn.query_row(sql, [id], |r| r.get(0)).unwrap()
}

pub fn turn_state(sb: &Sandbox, n: u128) -> String {
    sql_text(
        &sb.db(),
        "SELECT state FROM turns WHERE id = ?1",
        tid(n).to_string(),
    )
}

pub fn outbox_states(sb: &Sandbox) -> Vec<String> {
    let conn = Connection::open(sb.db()).unwrap();
    let mut stmt = conn
        .prepare("SELECT state FROM outbox ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// 逐表导出整个库的内容（事件、命令去重、outbox、投影、检查点、制品登记、meta）：断言「库的内容不变」用。
pub fn dump_all(sb: &Sandbox) -> Vec<(String, Vec<String>)> {
    let conn = Connection::open(sb.db()).unwrap();
    let mut names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names.retain(|n| !n.starts_with("sqlite_"));
    drop(conn);
    names
        .into_iter()
        .map(|t| {
            let mut rows = dump_table(&sb.db(), &t, "1");
            rows.sort();
            (t, rows)
        })
        .collect()
}

/// 往 `events` 表直接追加一条本版本不认识的事件（模拟更新的版本写的）。
pub fn append_future_event(sb: &Sandbox) {
    let conn = Connection::open(sb.db()).unwrap();
    // 字段号 9999、长度 0 的子消息：本版本 DomainEvent 的 oneof 里没有它
    let payload: Vec<u8> = vec![0xfa, 0xf0, 0x04, 0x00];
    conn.execute(
        "INSERT INTO events (seq, stream_kind, stream_id, type, payload, command_id, actor, at) \
         VALUES ((SELECT COALESCE(MAX(seq), 0) + 1 FROM events), 'system', NULL, 'zz_future_kind', ?1, 'future-cmd', 'system', 0)",
        [payload],
    )
    .unwrap();
}

pub fn remove_future_event(sb: &Sandbox) {
    let conn = Connection::open(sb.db()).unwrap();
    conn.execute("DELETE FROM events WHERE type = 'zz_future_kind'", [])
        .unwrap();
}
