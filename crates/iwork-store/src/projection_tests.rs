//! 投影器的单测（方案第 4a 步）。放在 crate 内部：审批终态、助手版本、附件没有读端口，
//! 要直接按行解快照（`snapshot.rs` 的函数是 crate 私有的）。

use std::cell::Cell;
use std::path::Path;

use iwork_app::codec::ProtoEventCodec;
use iwork_domain::approval::{Approval, ApprovalState, Decider, ReviewState, VoidReason};
use iwork_domain::approval_mode::{ApprovalMode, ProjectDefaultMode};
use iwork_domain::attachment::{Attachment, AttachmentOrigin};
use iwork_domain::common::{ArtifactArea, Timestamp};
use iwork_domain::conversation::ConversationState;
use iwork_domain::event::*;
use iwork_domain::id::{ReviewId, Seq};
use iwork_domain::machine::Finite;
use iwork_domain::message::{MessageBody, MessageStatus};
use iwork_domain::rule::{RuleLayer, RuleScope};
use iwork_domain::runtime::RuntimeState;
use iwork_domain::settings::GlobalSettings;
use iwork_domain::turn::{InterruptReason, TurnState};
use iwork_domain::user_action::{ExitStatus, UserActionState};
use iwork_ports::read_model::{MessagePage, ReadModelPort};
use iwork_ports::store::{StorageError, StoragePort};
use rusqlite::Connection;

use crate::ReadDefaults;
use crate::fixtures::*;
use crate::snapshot;

/// 依次提交命令，command_id 自动递增。
struct Feed<'a> {
    store: &'a Store,
    next: Cell<u128>,
}

impl<'a> Feed<'a> {
    fn new(store: &'a Store) -> Self {
        Self {
            store,
            next: Cell::new(1000),
        }
    }
    fn commit(&self, events: Vec<DomainEvent>) -> Result<Seq, StorageError> {
        let n = self.next.get();
        self.next.set(n + 1);
        block(self.store.commit(cmd(n, events))).map(|r| r.last_seq)
    }
    fn ok(&self, events: Vec<DomainEvent>) -> Seq {
        self.commit(events).expect("提交")
    }
}

fn conn(env: &Env) -> Connection {
    Connection::open_with_flags(env.db(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn load_approval(env: &Env, id: u128) -> Approval {
    let c = conn(env);
    let key = apid(id).to_string();
    let (s, state, decider, void): (Vec<u8>, String, Option<String>, Option<String>) = c
        .query_row(
            "SELECT snapshot, state, decider, void_reason FROM approvals WHERE id = ?1",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    let state =
        snapshot::approval_state_from_columns(&key, &state, decider.as_deref(), void.as_deref())
            .unwrap();
    snapshot::decode_approval(&ProtoEventCodec, &s, state).unwrap()
}

fn load_snapshot(env: &Env, table: &str, key_col: &str, key: &str) -> Vec<u8> {
    conn(env)
        .query_row(
            &format!("SELECT snapshot FROM {table} WHERE {key_col} = ?1"),
            [key],
            |r| r.get(0),
        )
        .unwrap()
}

fn updated_seq(env: &Env, table: &str, key_col: &str, key: &str) -> i64 {
    conn(env)
        .query_row(
            &format!("SELECT updated_seq FROM {table} WHERE {key_col} = ?1"),
            [key],
            |r| r.get(0),
        )
        .unwrap()
}

fn rebuild(store: &Store, env: &Env) {
    let before = dump_projections(&env.db());
    block(store.rebuild_projections()).unwrap();
    assert_eq!(dump_projections(&env.db()), before, "重建与在线逐表一致");
}

/// 一个项目 + 项目对话 + 回合，作为别的实体的上下文。
fn base(feed: &Feed<'_>) {
    feed.ok(vec![
        ProjectAdded {
            project: project(1),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(10, 1),
        }
        .into(),
        TurnQueued { turn: turn(20, 10) }.into(),
    ]);
}

// ---------- 快照覆盖范围核对表（方案 2.6） ----------

#[test]
fn snapshot_roundtrip_all_entities() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    let rm = read_model(&store);
    let codec = ProtoEventCodec;
    base(&feed);

    // Project / Assistant / AssistantVersion
    let mut a = assistant(2, true);
    feed.ok(vec![
        AssistantCreated {
            assistant: a.clone(),
        }
        .into(),
    ]);
    feed.ok(vec![
        AssistantUpdated {
            assistant_id: aid(2),
            version: 2,
            config: a.current.config.clone(),
        }
        .into(),
    ]);
    a.current.version = 2;
    a.current.created_at = Timestamp(T0);
    assert_eq!(block(rm.assistants()).unwrap(), vec![a.clone()]);
    let v = snapshot::decode_assistant_version(
        &codec,
        &conn(&env)
            .query_row(
                "SELECT snapshot FROM assistant_versions WHERE assistant_id = ?1 AND version = 2",
                [aid(2).to_string()],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (v.assistant_id, v.version, v.config),
        (aid(2), 2, a.current.config.clone())
    );
    assert_eq!(block(rm.projects()).unwrap()[0].project, project(1));

    // Conversation：两种对话 × 两种落库状态（Deleted 的行直接删）
    for (i, state) in [ConversationState::Active, ConversationState::Deleting]
        .into_iter()
        .enumerate()
    {
        let mut c = dm(100 + i as u128, 2);
        c.state = state;
        c.approval_mode = Some(ApprovalMode::Auto);
        feed.ok(vec![
            ConversationCreated {
                conversation: c.clone(),
            }
            .into(),
        ]);
        let view = block(rm.conversation(
            c.id,
            MessagePage {
                before: None,
                limit: 10,
            },
        ))
        .unwrap();
        assert_eq!(view.conversation, c);
    }

    // Message：五种状态 × 两种正文
    let bodies = [
        MessageBody::Inline("hi".into()),
        MessageBody::Artifact(artifact(3, ArtifactArea::Artifacts)),
    ];
    let mut n = 200;
    for status in MessageStatus::all() {
        for body in &bodies {
            n += 1;
            let mut m = assistant_message(n, 10, 20);
            m.status = status;
            m.body = body.clone();
            feed.ok(vec![MessageAppended { message: m.clone() }.into()]);
            let got = snapshot::decode_message(
                &codec,
                &load_snapshot(&env, "messages", "id", &m.id.to_string()),
            )
            .unwrap();
            assert_eq!(got, m, "{status:?}");
        }
    }

    // Turn：每一种状态
    for (i, state) in TurnState::all().into_iter().enumerate() {
        let mut t = turn(300 + i as u128, 10);
        t.state = state;
        t.ended_at = state.is_terminal().then_some(Timestamp(T0 + 5));
        feed.ok(vec![TurnQueued { turn: t.clone() }.into()]);
        let got = snapshot::decode_turn(
            &codec,
            &load_snapshot(&env, "turns", "id", &t.id.to_string()),
        )
        .unwrap();
        assert_eq!(got, t, "{state:?}");
    }

    // UserActionCard：每一种状态（带载荷的各取代表值）
    let states = vec![
        UserActionState::Pending,
        UserActionState::Starting,
        UserActionState::Executing,
        UserActionState::Stopping,
        UserActionState::Done {
            exit: ExitStatus::Code(3),
        },
        UserActionState::Done {
            exit: ExitStatus::Signal(9),
        },
        UserActionState::Failed {
            error: "spawn".into(),
        },
        UserActionState::Unknown {
            error: "lost".into(),
        },
        UserActionState::Ended,
        UserActionState::Dismissed,
    ]
    .into_iter()
    .chain(
        VoidReason::all()
            .into_iter()
            .map(|reason| UserActionState::Voided { reason }),
    );
    for (i, state) in states.enumerate() {
        let mut c = card(400 + i as u128, 20, 10);
        c.state = state.clone();
        c.output_artifact = Some(artifact(4, ArtifactArea::Artifacts));
        feed.ok(vec![UserActionProposed { card: c.clone() }.into()]);
        let got = snapshot::decode_card(
            &codec,
            &load_snapshot(&env, "user_actions", "id", &c.id.to_string()),
        )
        .unwrap();
        assert_eq!(got, c, "{state:?}");
    }

    // RuleEntry：四种范围
    let scopes = [
        RuleScope::Global,
        RuleScope::Project(pid(1)),
        RuleScope::Assistant(aid(2)),
        RuleScope::Conversation(cid(10)),
    ];
    for (i, scope) in scopes.into_iter().enumerate() {
        let r = rule(500 + i as u128, RuleLayer::Remembered, scope);
        feed.ok(vec![RuleRemembered { rule: r.clone() }.into()]);
        let got = snapshot::decode_rule(
            &codec,
            &load_snapshot(&env, "approval_rules", "id", &r.id.to_string()),
        )
        .unwrap();
        assert_eq!(got, r);
    }

    // Attachment：四种来源
    let origins = [
        AttachmentOrigin::RepoPath("/work/p1/a.txt".into()),
        AttachmentOrigin::Copied(artifact(5, ArtifactArea::Attachments)),
        AttachmentOrigin::Pasted(artifact(6, ArtifactArea::Attachments)),
        AttachmentOrigin::CredentialInline(artifact(7, ArtifactArea::Private)),
    ];
    for (i, origin) in origins.into_iter().enumerate() {
        let a: Attachment = attachment(600 + i as u128, 10, origin);
        feed.ok(vec![
            AttachmentStored {
                attachment: a.clone(),
            }
            .into(),
        ]);
        let got = snapshot::decode_attachment(
            &codec,
            &load_snapshot(&env, "attachments", "id", &a.id.to_string()),
        )
        .unwrap();
        assert_eq!(got, a);
    }

    // Runtime：五种状态
    for (i, state) in [
        RuntimeState::Missing,
        RuntimeState::NotLoggedIn,
        RuntimeState::ReadyVerified,
        RuntimeState::ReadyUnverified,
        RuntimeState::Incompatible,
    ]
    .into_iter()
    .enumerate()
    {
        let mut r = runtime(700 + i as u128);
        r.state = state;
        feed.ok(vec![RuntimeUpdated { runtime: r.clone() }.into()]);
        assert!(block(rm.runtimes()).unwrap().contains(&r), "{state:?}");
    }

    rebuild(&store, &env);
}

#[test]
fn approval_terminal_states_online_and_rebuilt() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    base(&feed);
    let mut expected = Vec::new();
    let mut n = 50;
    let mut push = |state: ApprovalState, event: Option<DomainEvent>, n: u128| {
        let mut a = approval(n, 20, 10);
        a.review = ReviewState::Passed {
            review_id: ReviewId(tid(99)),
        };
        feed.ok(vec![
            ApprovalRequested {
                approval: a.clone(),
            }
            .into(),
        ]);
        if let Some(e) = event {
            feed.ok(vec![e]);
        }
        a.state = state;
        expected.push((n, a));
    };
    push(ApprovalState::Pending, None, n);
    for decider in Decider::all() {
        n += 1;
        let state = ApprovalState::Approved { decider };
        let ev = ApprovalDecided {
            approval_id: apid(n),
            state,
            remember: None,
        };
        push(state, Some(ev.into()), n);
    }
    n += 1;
    let ev = ApprovalDecided {
        approval_id: apid(n),
        state: ApprovalState::Denied,
        remember: None,
    };
    push(ApprovalState::Denied, Some(ev.into()), n);
    for reason in VoidReason::all() {
        n += 1;
        let ev = ApprovalVoided {
            approval_id: apid(n),
            reason,
        };
        push(ApprovalState::Voided { reason }, Some(ev.into()), n);
    }
    assert_eq!(expected.len(), 1 + 3 + 1 + 4, "全部九种状态");
    for (id, a) in &expected {
        assert_eq!(&load_approval(&env, *id), a, "在线读取 {:?}", a.state);
    }
    rebuild(&store, &env);
    for (id, a) in &expected {
        assert_eq!(&load_approval(&env, *id), a, "重建后读取 {:?}", a.state);
    }
}

#[test]
fn approval_decided_sets_decided_at_from_envelope() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    base(&feed);
    feed.ok(vec![
        ApprovalRequested {
            approval: approval(1, 20, 10),
        }
        .into(),
    ]);
    let pending = block(read_model(&store).pending_items()).unwrap();
    assert_eq!(pending.len(), 1, "对照：待处理里有它");
    env.clock.set(T0 + 7_000);
    feed.ok(vec![
        ApprovalDecided {
            approval_id: apid(1),
            state: ApprovalState::Approved {
                decider: Decider::User,
            },
            remember: None,
        }
        .into(),
    ]);
    let at = scalar(&env.db(), "SELECT decided_at FROM approvals");
    assert_eq!(at, T0 + 7_000, "决定时间来自信封");
    assert_eq!(scalar(&env.db(), "SELECT awaits_user FROM approvals"), 0);
    assert!(
        block(read_model(&store).pending_items())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn runtime_updated_keeps_import_prompted() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    feed.ok(vec![
        RuntimeUpdated {
            runtime: runtime(1),
        }
        .into(),
    ]);
    feed.ok(vec![
        LocalImportPrompted {
            runtime_id: rtid(1),
        }
        .into(),
    ]);
    let mut r = runtime(1);
    r.state = RuntimeState::NotLoggedIn;
    feed.ok(vec![RuntimeUpdated { runtime: r.clone() }.into()]);
    let q = "SELECT import_prompted FROM runtimes";
    assert_eq!(
        scalar(&env.db(), q),
        1,
        "在线：RuntimeUpdated 不清 import_prompted"
    );
    assert_eq!(block(read_model(&store).runtimes()).unwrap(), vec![r]);
    rebuild(&store, &env);
    assert_eq!(scalar(&env.db(), q), 1, "重建后");
}

#[test]
fn rebuild_with_different_defaults_is_identical() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    feed.ok(vec![
        GlobalSettingsChanged {
            playful_ui: Some(false),
            autostart: None,
            projects_root: None,
        }
        .into(),
    ]);
    let table = dump_table(&env.db(), "global_settings", "id");
    let defaults = |root: &str| ReadDefaults {
        global_settings: GlobalSettings {
            playful_ui: true,
            autostart: true,
            projects_root: root.into(),
        },
    };
    let a = block(store.read_model(defaults("/home/a")).settings()).unwrap();
    block(store.rebuild_projections()).unwrap();
    assert_eq!(
        dump_table(&env.db(), "global_settings", "id"),
        table,
        "表逐字段相同"
    );
    let b = block(store.read_model(defaults("/home/b")).settings()).unwrap();
    assert!(!a.playful_ui && !b.playful_ui, "事件给的值两边都在");
    assert!(a.autostart && b.autostart, "没给的用默认值补");
    assert_eq!(
        (a.projects_root.to_str(), b.projects_root.to_str()),
        (Some("/home/a"), Some("/home/b"))
    );
    assert!(
        table[0].contains("Null"),
        "没设过的列在表里是 NULL：{table:?}"
    );
}

#[test]
fn no_insert_or_replace_in_projector() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/projector");
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        if text
            .to_uppercase()
            .contains(&["INSERT OR", "REPLACE"].join(" "))
        {
            hits.push(path);
        }
    }
    assert!(hits.is_empty(), "投影器不许用整行替换：{hits:?}");
}

#[test]
fn approval_mode_three_targets() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    base(&feed);
    let change = |target, mode| -> DomainEvent {
        ApprovalModeChanged {
            target,
            mode,
            cause: ApprovalModeChangeCause::User,
        }
        .into()
    };
    feed.ok(vec![change(
        ApprovalModeTarget::Conversation(cid(10)),
        Some(ApprovalMode::FullAccess),
    )]);
    let rm = read_model(&store);
    let view = block(rm.conversation(
        cid(10),
        MessagePage {
            before: None,
            limit: 1,
        },
    ))
    .unwrap();
    assert_eq!(
        view.conversation.approval_mode,
        Some(ApprovalMode::FullAccess)
    );

    let member = ApprovalModeTarget::Member {
        conversation_id: cid(10),
        assistant_id: aid(1),
    };
    feed.ok(vec![change(member, Some(ApprovalMode::Auto))]);
    assert_eq!(
        dump_table(&env.db(), "conversation_member_modes", "conversation_id").len(),
        1
    );
    feed.ok(vec![change(member, Some(ApprovalMode::Manual))]);
    let rows = dump_table(&env.db(), "conversation_member_modes", "conversation_id");
    assert!(rows.len() == 1 && rows[0].contains("manual"), "{rows:?}");
    feed.ok(vec![change(member, None)]);
    assert!(
        dump_table(&env.db(), "conversation_member_modes", "conversation_id").is_empty(),
        "恢复跟随"
    );

    feed.ok(vec![change(
        ApprovalModeTarget::Project(pid(1)),
        Some(ApprovalMode::Auto),
    )]);
    let p = &block(rm.projects()).unwrap()[0].project;
    assert_eq!(p.settings.default_approval_mode, ProjectDefaultMode::Auto);
    for bad in [Some(ApprovalMode::FullAccess), None] {
        let err = feed.commit(vec![change(ApprovalModeTarget::Project(pid(1)), bad)]);
        assert!(
            matches!(err, Err(StorageError::Projection { .. })),
            "项目默认不能是 {bad:?}"
        );
    }
    rebuild(&store, &env);
}

#[test]
fn dm_project_switched_clears_override() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    base(&feed);
    let mut c = dm(30, 1);
    c.approval_mode = Some(ApprovalMode::FullAccess);
    feed.ok(vec![ConversationCreated { conversation: c }.into()]);
    feed.ok(vec![
        DmProjectSwitched {
            conversation_id: cid(30),
            project_id: Some(pid(1)),
        }
        .into(),
    ]);
    let view = block(read_model(&store).conversation(
        cid(30),
        MessagePage {
            before: None,
            limit: 1,
        },
    ))
    .unwrap();
    assert_eq!(view.conversation.approval_mode, None);
    assert_eq!(
        view.conversation.kind,
        iwork_domain::conversation::ConversationKind::AssistantDm {
            assistant_id: aid(1),
            context_project: Some(pid(1))
        }
    );
    let err = feed.commit(vec![
        DmProjectSwitched {
            conversation_id: cid(10),
            project_id: None,
        }
        .into(),
    ]);
    assert!(
        matches!(err, Err(StorageError::Projection { .. })),
        "项目对话不能切项目"
    );
}

/// 一个对话里各种东西都放一点，返回挂着制品引用的数量。
fn populate(feed: &Feed<'_>, conv: u128, turn_n: u128, k: u8) {
    feed.ok(vec![
        TurnQueued {
            turn: turn(turn_n, conv),
        }
        .into(),
        MessageAppended {
            message: {
                let mut m = user_message(turn_n + 1, conv, "x");
                m.body = MessageBody::Artifact(artifact(k, ArtifactArea::Artifacts));
                m
            },
        }
        .into(),
        MessageAppended {
            message: assistant_message(turn_n + 2, conv, turn_n),
        }
        .into(),
        ToolCallStarted {
            turn_id: tid(turn_n),
            tool_call_id: ToolCallId("tc".into()),
            tool_name: "Bash".into(),
            action: approval(1, 1, 1).action,
        }
        .into(),
        ToolCallFinished {
            turn_id: tid(turn_n),
            tool_call_id: ToolCallId("tc".into()),
            ok: true,
            output_artifact: Some(artifact(k + 1, ArtifactArea::Artifacts)),
        }
        .into(),
        ApprovalRequested {
            approval: approval(turn_n + 3, turn_n, conv),
        }
        .into(),
        UserActionProposed {
            card: card(turn_n + 4, turn_n, conv),
        }
        .into(),
        AttachmentStored {
            attachment: attachment(
                turn_n + 5,
                conv,
                AttachmentOrigin::Copied(artifact(k + 2, ArtifactArea::Attachments)),
            ),
        }
        .into(),
        TurnFinished {
            turn_id: tid(turn_n),
            state: TurnState::Completed,
            usage: Default::default(),
            celebrate: false,
            change_summary: None,
        }
        .into(),
    ]);
    block(feed.store.checkpoint_message(mid(turn_n + 2), 1, "a")).unwrap();
}

fn rows_for(env: &Env, conv: u128) -> i64 {
    let c = cid(conv).to_string();
    let q = |t: &str| {
        scalar(
            &env.db(),
            &format!("SELECT COUNT(*) FROM {t} WHERE conversation_id = '{c}'"),
        )
    };
    [
        "messages",
        "turns",
        "turn_usage",
        "approvals",
        "user_actions",
        "attachments",
    ]
    .iter()
    .map(|t| q(t))
    .sum()
}

#[test]
fn conversation_deleted_cascades() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    base(&feed);
    feed.ok(vec![
        ConversationCreated {
            conversation: project_conversation(11, 1),
        }
        .into(),
    ]);
    populate(&feed, 10, 100, 10);
    populate(&feed, 11, 200, 20);
    let refs = |env: &Env| scalar(&env.db(), "SELECT COUNT(*) FROM artifact_refs");
    assert_eq!(refs(&env), 6, "对照：两个对话各 3 个引用");
    assert!(rows_for(&env, 10) > 0);

    feed.ok(vec![
        ConversationDeleted {
            conversation_id: cid(10),
        }
        .into(),
    ]);
    assert_eq!(rows_for(&env, 10), 0);
    assert_eq!(
        scalar(
            &env.db(),
            &format!(
                "SELECT COUNT(*) FROM conversations WHERE id = '{}'",
                cid(10)
            )
        ),
        0
    );
    assert_eq!(
        scalar(
            &env.db(),
            &format!(
                "SELECT COUNT(*) FROM tool_calls WHERE turn_id = '{}'",
                tid(100)
            )
        ),
        0
    );
    assert_eq!(refs(&env), 3, "只剩另一个对话的引用");
    assert_eq!(
        scalar(&env.db(), "SELECT COUNT(*) FROM message_checkpoints"),
        1,
        "检查点随消息删"
    );
    assert!(rows_for(&env, 11) > 0, "别的对话不受影响");
    rebuild(&store, &env);
}

#[test]
fn assistant_deleted_removes_dm_but_keeps_project_messages() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    base(&feed);
    feed.ok(vec![
        AssistantCreated {
            assistant: assistant(1, false),
        }
        .into(),
        ConversationCreated {
            conversation: dm(30, 1),
        }
        .into(),
        MessageAppended {
            message: user_message(31, 30, "hi"),
        }
        .into(),
        MessageAppended {
            message: assistant_message(32, 10, 20),
        }
        .into(),
    ]);
    feed.ok(vec![
        AssistantDeleted {
            assistant_id: aid(1),
            name_snapshot: "a1".into(),
        }
        .into(),
    ]);
    let rm = read_model(&store);
    assert!(block(rm.assistants()).unwrap().is_empty());
    assert!(matches!(
        block(rm.conversation(
            cid(30),
            MessagePage {
                before: None,
                limit: 5
            }
        )),
        Err(StorageError::NotFound { .. })
    ));
    let view = block(rm.conversation(
        cid(10),
        MessagePage {
            before: None,
            limit: 5,
        },
    ))
    .unwrap();
    assert_eq!(view.messages.len(), 1, "项目对话里它的消息保留");
    assert_eq!(
        scalar(&env.db(), "SELECT COUNT(*) FROM assistant_versions"),
        0
    );
    rebuild(&store, &env);
}

#[test]
fn missing_row_is_projection_error() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    base(&feed);
    let cases: Vec<DomainEvent> = vec![
        MessageCompleted {
            conversation_id: cid(10),
            message_id: mid(999),
            body: MessageBody::Inline("x".into()),
            status: MessageStatus::Completed,
        }
        .into(),
        TurnStateChanged {
            turn_id: tid(999),
            from: TurnState::Queued,
            to: TurnState::Running,
            reason: None,
        }
        .into(),
        ApprovalDecided {
            approval_id: apid(999),
            state: ApprovalState::Denied,
            remember: None,
        }
        .into(),
        LocalImportPrompted {
            runtime_id: rtid(999),
        }
        .into(),
        ProjectUpdated {
            project_id: pid(999),
            name: Some("x".into()),
            pinned: None,
            last_active_at: None,
        }
        .into(),
        RuleRevoked { rule_id: rid(999) }.into(),
        ConversationDeleted {
            conversation_id: cid(999),
        }
        .into(),
    ];
    let head = block(store.head_seq()).unwrap();
    for event in cases {
        let kind = event.kind();
        match feed.commit(vec![event]) {
            Err(StorageError::Projection { seq, message }) => {
                assert_eq!(seq, Seq(head.0 + 1), "{kind}: 点名事件 seq");
                assert!(message.contains("没有"), "{kind}: {message}");
            }
            other => panic!("{kind} 应为 Projection 错误：{other:?}"),
        }
    }
    assert_eq!(block(store.head_seq()).unwrap(), head, "失败的命令整条回滚");
}

#[test]
fn updated_seq_tracks_last_event_touching_the_row() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    base(&feed); // seq 1..=3
    let p = pid(1).to_string();
    assert_eq!(updated_seq(&env, "projects", "id", &p), 1);
    let s = feed.ok(vec![
        ProjectUpdated {
            project_id: pid(1),
            name: None,
            pinned: Some(true),
            last_active_at: None,
        }
        .into(),
    ]);
    assert_eq!(updated_seq(&env, "projects", "id", &p), s.0 as i64);
    let t = tid(20).to_string();
    assert_eq!(updated_seq(&env, "turns", "id", &t), 3);
    let s = feed.ok(vec![
        TurnStateChanged {
            turn_id: tid(20),
            from: TurnState::Queued,
            to: TurnState::Interrupted {
                reason: InterruptReason::Crash,
            },
            reason: None,
        }
        .into(),
    ]);
    assert_eq!(updated_seq(&env, "turns", "id", &t), s.0 as i64);
    // 中断计入未读：对话行也被这条事件改了
    assert_eq!(
        updated_seq(&env, "conversations", "id", &cid(10).to_string()),
        s.0 as i64
    );
    rebuild(&store, &env);
}

#[test]
fn unread_counts_messages_and_cards_and_recomputes_on_read() {
    let env = Env::new();
    let store = env.open();
    let feed = Feed::new(&store);
    base(&feed);
    let unread = || {
        block(read_model(&store).conversation(
            cid(10),
            MessagePage {
                before: None,
                limit: 1,
            },
        ))
        .unwrap()
        .unread
    };
    feed.ok(vec![
        MessageAppended {
            message: user_message(1, 10, "q"),
        }
        .into(),
    ]);
    assert_eq!(unread(), 0, "用户自己的消息不计");
    feed.ok(vec![
        MessageAppended {
            message: assistant_message(2, 10, 20),
        }
        .into(),
    ]);
    feed.ok(vec![
        ApprovalRequested {
            approval: approval(3, 20, 10),
        }
        .into(),
    ]);
    feed.ok(vec![
        UserActionProposed {
            card: card(4, 20, 10),
        }
        .into(),
    ]);
    let read_to = feed.ok(vec![
        TurnStateChanged {
            turn_id: tid(20),
            from: TurnState::Running,
            to: TurnState::Interrupted {
                reason: InterruptReason::Crash,
            },
            reason: None,
        }
        .into(),
    ]);
    assert_eq!(unread(), 4);
    assert_eq!(
        block(read_model(&store).projects()).unwrap()[0].unread,
        4,
        "项目未读是对话之和"
    );
    feed.ok(vec![
        MessageAppended {
            message: assistant_message(5, 10, 20),
        }
        .into(),
    ]);
    feed.ok(vec![
        ConversationRead {
            conversation_id: cid(10),
            last_read_seq: read_to,
        }
        .into(),
    ]);
    assert_eq!(unread(), 1, "已读位置之后只剩一条");
    rebuild(&store, &env);
    assert_eq!(unread(), 1);
}
