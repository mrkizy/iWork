//! 摘要与侧栏派生（方案 2.2 摘要、第 7 节「摘要」与「其他」里不连后台的各条）。

mod support;

use std::time::{Duration, Instant, SystemTime};

use iwork_client_core::derive::{self, Ctx};
use iwork_client_core::store::{QueryPurpose, Store, SummaryTable};
use iwork_proto::pb;
use pb::domain_event::Event as E;
use support::*;

fn ctx() -> Ctx {
    Ctx {
        now: SystemTime::UNIX_EPOCH + Duration::from_secs(1_762_100_000),
    }
}

fn project(unread: u32) -> pb::Project {
    pb::Project {
        id: PID.into(),
        name: "web".into(),
        unread,
        ..Default::default()
    }
}

fn summary_result(
    seq: u64,
    table: SummaryTable,
    projects: &[pb::Project],
    convs: &[pb::Conversation],
) -> pb::QueryResult {
    use pb::query_result::Kind as K;
    let kind = match table {
        SummaryTable::Projects => K::ListProjects(pb::ListProjectsResult {
            projects: projects.to_vec(),
        }),
        SummaryTable::Conversations => K::ListConversations(pb::ListConversationsResult {
            conversations: convs.to_vec(),
            has_more: false,
        }),
        SummaryTable::Assistants => K::ListAssistants(pb::ListAssistantsResult::default()),
        SummaryTable::Pending => K::ListPendingItems(pb::ListPendingItemsResult::default()),
        SummaryTable::List => unreachable!("这些用例不开列表页"),
    };
    pb::QueryResult {
        seq,
        kind: Some(kind),
    }
}

/// 跑完一轮重查：按会话层的方式取 `due`、逐条应用结果。
fn run_round(
    s: &mut Store,
    seq: u64,
    projects: &[pb::Project],
    convs: &[pb::Conversation],
    now: Instant,
) {
    let (round, tables) = s.refresh.due(now, false).expect("该发一轮");
    for table in tables {
        s.apply(view_result(
            QueryPurpose::Summary { round, table },
            summary_result(seq, table, projects, convs),
        ));
    }
}

/// S5：只来一条 `ConversationRead`，对话与项目两级徽标最终为 0（客户端不推演，靠失效重查）。
#[test]
fn read_event_alone_clears_both_badges() {
    let mut s = store_with_open();
    let t = Instant::now();
    s.apply(snapshot(
        SIDEBAR,
        5,
        vec![
            summary_result(5, SummaryTable::Projects, &[project(3)], &[]),
            summary_result(
                5,
                SummaryTable::Conversations,
                &[],
                &[conversation(CID, "修登录", 3)],
            ),
        ],
    ));
    let before = derive::sidebar::sidebar(&s, ctx());
    assert_eq!(
        badge(&before.projects[0].unread_badge),
        "3",
        "对照：读之前有徽标"
    );
    assert_eq!(badge(&before.projects[0].recent[0].unread_badge), "3");

    s.apply(persisted(
        6,
        &[SIDEBAR, CONV],
        E::ConversationRead(pb::ConversationRead {
            conversation_id: CID.into(),
            last_read_seq: 6,
        }),
    ));
    assert!(s.refresh.is_stale(), "命中侧栏订阅就失效，不挑事件类型");
    // 失效还没重查时，徽标仍是后台上一次给的值（客户端不自己清）。
    assert_eq!(
        badge(&derive::sidebar::sidebar(&s, ctx()).projects[0].unread_badge),
        "3"
    );
    run_round(
        &mut s,
        7,
        &[project(0)],
        &[conversation(CID, "修登录", 0)],
        t,
    );
    let after = derive::sidebar::sidebar(&s, ctx());
    assert_eq!(after.projects[0].unread_badge, None);
    assert_eq!(after.projects[0].recent[0].unread_badge, None);
    assert!(!s.refresh.is_stale());
}

/// 改名后侧栏随重查更新。T10 补上 `ConversationRenamed` 前，用一条已有的、命中侧栏订阅的事件触发失效
/// （假后台场景 `rename-via-summary-refresh-until-t10`）；要守的是「失效 → 重查 → 新标题」这条通用路。
#[test]
fn rename_refreshes_sidebar() {
    let mut s = store_with_open();
    s.apply(snapshot(
        SIDEBAR,
        5,
        vec![
            summary_result(5, SummaryTable::Projects, &[project(0)], &[]),
            summary_result(
                5,
                SummaryTable::Conversations,
                &[],
                &[conversation(CID, "旧标题", 0)],
            ),
        ],
    ));
    assert_eq!(
        derive::sidebar::sidebar(&s, ctx()).projects[0].recent[0].title,
        "旧标题"
    );
    s.apply(persisted(
        6,
        &[SIDEBAR],
        E::ConversationRead(pb::ConversationRead {
            conversation_id: CID.into(),
            last_read_seq: 0,
        }),
    ));
    let mut renamed = conversation(CID, "新标题", 0);
    renamed.title_source = pb::TitleSource::User as i32;
    run_round(&mut s, 7, &[project(0)], &[renamed], Instant::now());
    let item = &derive::sidebar::sidebar(&s, ctx()).projects[0].recent[0];
    assert_eq!(item.title, "新标题");
    assert!(item.can_rename, "改过的标题仍可再改");
    // 标题栏读的也是摘要里的标题。
    let tl = s.open.as_ref().unwrap();
    assert_eq!(derive::conversation::header(&s, tl).title, "新标题");
}

#[test]
fn event_during_refresh_triggers_another_round() {
    let mut s = store_with_open();
    let t = Instant::now();
    s.apply(persisted(
        6,
        &[SIDEBAR],
        E::ConversationRead(pb::ConversationRead::default()),
    ));
    let (round, tables) = s.refresh.due(t, false).unwrap();
    // 第一条结果回来后又来一个事件。
    s.apply(view_result(
        QueryPurpose::Summary {
            round,
            table: tables[0],
        },
        summary_result(7, tables[0], &[], &[]),
    ));
    s.apply(persisted(
        8,
        &[PENDING],
        E::ConversationRead(pb::ConversationRead::default()),
    ));
    for table in &tables[1..] {
        s.apply(view_result(
            QueryPurpose::Summary {
                round,
                table: *table,
            },
            summary_result(9, *table, &[], &[]),
        ));
    }
    assert!(s.refresh.is_stale(), "这一轮在飞期间有事件：不清标记");
    assert!(
        s.refresh
            .due(t + Duration::from_millis(250), false)
            .is_some(),
        "过了节流间隔再查一轮"
    );
}

/// 一轮里有查询失败（会话层调 abandon_round）：标记还在，整轮重发。
#[test]
fn refresh_one_query_fails_retries_round() {
    let mut s = store_with_open();
    let t = Instant::now();
    s.apply(persisted(
        6,
        &[SIDEBAR],
        E::ConversationRead(pb::ConversationRead::default()),
    ));
    let (round, tables) = s.refresh.due(t, false).unwrap();
    s.apply(view_result(
        QueryPurpose::Summary {
            round,
            table: tables[0],
        },
        summary_result(7, tables[0], &[], &[]),
    ));
    s.refresh.abandon_round();
    assert!(s.refresh.is_stale());
    let (_, again) = s
        .refresh
        .due(t + Duration::from_millis(250), false)
        .unwrap();
    assert_eq!(again.len(), 4, "整轮重发");
}

#[test]
fn title_placeholder_and_rename_rules() {
    let s = Store::new();
    let fresh = conversation(CID, "", 0);
    assert_eq!(derive::conversation_title(&s, &fresh), "新对话");
    assert!(derive::can_rename(&fresh));
    let mut fixed = fresh.clone();
    fixed.title_source = pb::TitleSource::Fixed as i32;
    assert!(!derive::can_rename(&fixed));
    let dm = pb::Conversation {
        kind: Some(pb::conversation::Kind::AssistantDm(pb::AssistantDm {
            assistant_id: "a1".into(),
            context_project_id: None,
        })),
        title_source: pb::TitleSource::Fixed as i32,
        ..fresh
    };
    assert!(!derive::can_rename(&dm), "私聊没有重命名");
}

fn assistant(id: &str, name: &str, builtin: bool) -> pb::Assistant {
    pb::Assistant {
        id: id.into(),
        builtin,
        config: Some(pb::AssistantConfig {
            name: name.into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn dm(id: &str, assistant_id: &str, unread: u32) -> pb::Conversation {
    pb::Conversation {
        id: id.into(),
        kind: Some(pb::conversation::Kind::AssistantDm(pb::AssistantDm {
            assistant_id: assistant_id.into(),
            context_project_id: None,
        })),
        unread,
        title: "小林".into(),
        title_source: pb::TitleSource::Fixed as i32,
        ..Default::default()
    }
}

fn with_summary(
    projects: Vec<pb::Project>,
    convs: Vec<pb::Conversation>,
    assistants: Vec<pb::Assistant>,
    acts: Vec<pb::AssistantActivity>,
    pending: Vec<pb::PendingItem>,
) -> Store {
    use pb::query_result::Kind as K;
    let mut s = store_with_open();
    s.apply(snapshot(
        SIDEBAR,
        5,
        vec![
            pb::QueryResult {
                seq: 5,
                kind: Some(K::ListProjects(pb::ListProjectsResult { projects })),
            },
            pb::QueryResult {
                seq: 5,
                kind: Some(K::ListConversations(pb::ListConversationsResult {
                    conversations: convs,
                    has_more: false,
                })),
            },
            pb::QueryResult {
                seq: 5,
                kind: Some(K::ListAssistants(pb::ListAssistantsResult {
                    assistants,
                    activities: acts,
                })),
            },
            pb::QueryResult {
                seq: 5,
                kind: Some(K::ListPendingItems(pb::ListPendingItemsResult {
                    items: pending,
                })),
            },
        ],
    ));
    s
}

/// 助手未读只算它那条私聊（2a 第 3 条）：项目对话里的未读不算到助手上。
#[test]
fn assistant_unread_is_dm_only() {
    let s = with_summary(
        vec![project(9)],
        vec![conversation(CID, "项目对话", 9), dm("d1", "a1", 2)],
        vec![assistant("a1", "小林", false)],
        vec![],
        vec![],
    );
    let v = derive::sidebar::sidebar(&s, ctx());
    assert_eq!(badge(&v.assistants[0].unread_badge), "2");
    assert_eq!(v.assistants[0].dm_conversation_id, "d1");
}

#[test]
fn assistant_activity_line() {
    let approval = pb::Approval {
        id: "ap".into(),
        conversation_id: CID.into(),
        action: Some(pb::ActionSummary {
            raw_summary: "npm publish".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let act = |kind: pb::AssistantActivityKind,
               conv: &str,
               item: Option<&str>,
               line: Option<&str>| pb::AssistantActivity {
        assistant_id: "a1".into(),
        kind: kind as i32,
        conversation_id: conv.into(),
        pending_item_id: item.map(Into::into),
        message_first_line: line.map(Into::into),
    };
    let build = |a: Option<pb::AssistantActivity>| {
        with_summary(
            vec![project(0)],
            vec![conversation(CID, "修登录", 0), dm("d1", "a1", 0)],
            vec![assistant("a1", "小林", false)],
            a.into_iter().collect(),
            vec![pb::PendingItem {
                item: Some(pb::pending_item::Item::Approval(approval.clone())),
            }],
        )
    };
    let line = |s: &Store| {
        derive::sidebar::sidebar(s, ctx()).assistants[0]
            .activity_line
            .clone()
    };
    // 三种 kind；项目对话加「标题：」前缀。
    assert_eq!(
        line(&build(Some(act(
            pb::AssistantActivityKind::Waiting,
            CID,
            Some("ap"),
            None
        ))))
        .as_deref(),
        Some("修登录：审批：npm publish")
    );
    assert_eq!(
        line(&build(Some(act(
            pb::AssistantActivityKind::Running,
            CID,
            None,
            None
        ))))
        .as_deref(),
        Some("修登录：工作中")
    );
    // 私聊不加前缀；多行只取首行。
    assert_eq!(
        line(&build(Some(act(
            pb::AssistantActivityKind::Message,
            "d1",
            None,
            Some("改好了\n第二行")
        ))))
        .as_deref(),
        Some("改好了")
    );
    // 从没说过话：没有第二行。
    assert_eq!(line(&build(None)), None);
}

/// R10：验证状态只在设置 › 运行时（与引导页、诊断页）出现，侧栏与标题栏的视图模型里没有。
#[test]
fn no_verification_label_outside_runtime_views() {
    let mut s = with_summary(
        vec![project(0)],
        vec![conversation(CID, "x", 0)],
        vec![assistant("a1", "iWork", true)],
        vec![],
        vec![],
    );
    let rt = pb::RuntimeStatus {
        runtime_id: "r1".into(),
        display_name: "Claude Code".into(),
        state: pb::RuntimeState::ReadyUnverified as i32,
        ..Default::default()
    };
    s.apply(persisted(
        9,
        &[SIDEBAR],
        E::RuntimeUpdated(pb::RuntimeUpdated { runtime: Some(rt) }),
    ));
    let settings = derive::settings::settings(&s);
    assert_eq!(
        settings.runtimes[0].verification_label.as_deref(),
        Some("验证中")
    );
    let sidebar = format!("{:?}", derive::sidebar::sidebar(&s, ctx()));
    let header = format!(
        "{:?}",
        derive::conversation::header(&s, s.open.as_ref().unwrap())
    );
    for v in [&sidebar, &header] {
        assert!(!v.contains("验证"), "{v}");
    }
}

/// 额度取不到时显示「—」，悬停写原因；有值时写窗口类型（验收 5 的 Rust 侧）。
#[test]
fn quota_dash_and_tooltip() {
    let mut s = store_with_open();
    s.apply(persisted(
        3,
        &[SIDEBAR],
        E::RuntimeUpdated(pb::RuntimeUpdated {
            runtime: Some(pb::RuntimeStatus {
                runtime_id: "r1".into(),
                display_name: "Claude Code".into(),
                ..Default::default()
            }),
        }),
    ));
    let cell = &derive::sidebar::sidebar(&s, ctx()).quota[0];
    assert_eq!(cell.value_text, "—");
    let quota = pb::QuotaObserved {
        runtime_id: "r1".into(),
        windows: vec![pb::QuotaWindow {
            window: "5 小时".into(),
            used_pct: 37,
            resets_at: Some(prost_types::Timestamp {
                seconds: 1_762_100_000 + 7200,
                nanos: 0,
            }),
            observed_at: None,
        }],
        observed_at: None,
    };
    // 额度事件在 runtime 流上，订额度范围的订阅收到；这里用侧栏订阅号代替，只要有活动范围对应就应用。
    s.apply(persisted(4, &[SIDEBAR], E::QuotaObserved(quota)));
    let cell = &derive::sidebar::sidebar(&s, ctx()).quota[0];
    assert_eq!(cell.value_text, "37%");
    assert_eq!(cell.tooltip, "5 小时：已用 37% · 约 2 小时后重置");
}

/// 徽标上的字；没有徽标时为空串。
fn badge(b: &Option<pb::CountBadge>) -> &str {
    b.as_ref().map_or("", |b| b.text.as_str())
}
