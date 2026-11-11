//! 折叠与查询：各 `*_seq` 取创建事件的 seq、G1/G3/G4/G7/G8、未读、键集分页。不起服务端，直接折叠事件。

use iwork_fake_daemon::fixtures as f;
use iwork_fake_daemon::scenario::{self, Step};
use iwork_fake_daemon::{Tables, envelope_time};
use iwork_proto::pb;
use iwork_proto::pb::TurnState as T;
use iwork_proto::pb::query::Kind as Q;
use iwork_proto::pb::query_result::Kind as R;

/// 依次折叠，seq 从 1 起；返回表与 head。
struct World {
    tables: Tables,
    head: u64,
}

impl World {
    fn new() -> Self {
        World {
            tables: Tables::default(),
            head: 0,
        }
    }

    fn push(&mut self, e: pb::DomainEvent) -> u64 {
        self.head += 1;
        self.tables.fold(
            self.head,
            &envelope_time(self.head),
            e.event.as_ref().unwrap(),
        );
        self.head
    }

    fn steps(&mut self, steps: Vec<Step>) {
        for s in steps {
            match s {
                Step::Append(e) => {
                    self.push(*e);
                }
                Step::Mutate(m) => m(&mut self.tables),
                Step::Transient(_)
                | Step::Sleep(_)
                | Step::DisconnectAll
                | Step::DisconnectForSnapshot
                | Step::StopListening
                | Step::OpenHere => {}
            }
        }
    }

    fn query(&self, kind: Q) -> R {
        let r = self
            .tables
            .query(self.head, &pb::Query { kind: Some(kind) })
            .unwrap();
        assert_eq!(r.seq, self.head, "QueryResult.seq = head");
        r.kind.unwrap()
    }

    fn conversation(
        &self,
        id: &str,
        before: Option<&str>,
        limit: u32,
    ) -> pb::GetConversationResult {
        match self.query(Q::GetConversation(pb::GetConversation {
            conversation_id: id.into(),
            before_message_id: before.map(Into::into),
            after_message_id: None,
            around_seq: None,
            limit,
        })) {
            R::GetConversation(g) => g,
            other => panic!("{other:?}"),
        }
    }

    fn list(&self, l: pb::ListConversations) -> pb::ListConversationsResult {
        match self.query(Q::ListConversations(l)) {
            R::ListConversations(x) => x,
            other => panic!("{other:?}"),
        }
    }

    fn pending(&self) -> Vec<pb::PendingItem> {
        match self.query(Q::ListPendingItems(pb::ListPendingItems {})) {
            R::ListPendingItems(x) => x.items,
            other => panic!("{other:?}"),
        }
    }
}

fn base() -> World {
    let mut w = World::new();
    w.steps(scenario::base());
    w
}

#[test]
fn every_seq_field_is_the_creating_event_seq() {
    let mut w = base();
    w.push(f::project_conversation("c", scenario::PROJECT));
    let m = w.push(f::user_message("m1", "c", "你好"));
    let tq = w.push(f::turn_queued("t", "c", f::IWORK));
    let tc = w.push(f::tool_started("t", "tc", "Bash", "ls"));
    let ap = w.push(f::approval(
        "a",
        "c",
        "t",
        f::command_action("ls"),
        pb::ApprovalKind::Ask,
        &[],
    ));
    let ua = w.push(f::user_action("u", "c", "t", "ssh-add -l"));
    let fin = w.push(f::turn_finished(
        "t",
        T::Completed,
        true,
        Some(f::changes(&["x"], 1, 0)),
    ));
    let g = w.conversation("c", None, 0);
    assert_eq!(g.messages[0].created_seq, m);
    assert_eq!(g.turns[0].created_seq, tq);
    assert_eq!(g.turns[0].ended_seq, fin);
    assert!(g.turns[0].celebrate);
    assert_eq!(g.tool_calls[0].created_seq, tc);
    assert_eq!(g.approvals[0].created_seq, ap);
    assert_eq!(g.user_actions[0].created_seq, ua);
    assert_eq!(g.usage_total.unwrap().input_tokens, 1200);
    // 对照：这几个 seq 互不相同，上面不是拿同一个数比了几遍。
    assert_eq!(
        [m, tq, tc, ap, ua, fin]
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        6
    );
}

#[test]
fn resume_card_seq_is_when_turn_was_interrupted() {
    let mut w = base();
    w.steps(
        scenario::get("resume-card")
            .unwrap()
            .steps()
            .into_iter()
            .skip(scenario::base().len())
            .collect(),
    );
    let items = w.pending();
    let Some(pb::pending_item::Item::Resume(r)) = &items[0].item else {
        panic!("{items:?}")
    };
    assert_eq!(r.created_seq, w.head, "进入 INTERRUPTED 是最后一条事件");
    assert_eq!(r.reason, i32::from(pb::InterruptReason::Crash));
}

#[test]
fn auto_title_is_first_user_message_and_never_changes() {
    let mut w = base();
    w.push(f::project_conversation("c", scenario::PROJECT));
    let empty = w.list(pb::ListConversations::default());
    let c = empty.conversations.iter().find(|c| c.id == "c").unwrap();
    assert_eq!(c.title, "", "发首条消息前标题为空，界面显示「新对话」");
    w.push(f::user_message(
        "m1",
        "c",
        "第一行\n  第二行   把这一段写得比二十个字更长一些",
    ));
    w.push(f::user_message("m2", "c", "不该变成标题"));
    let l = w.list(pb::ListConversations::default());
    let c = l.conversations.iter().find(|c| c.id == "c").unwrap();
    assert_eq!(c.title, "第一行 第二行 把这一段写得比二十个字更…");
    assert_eq!(c.title.chars().count(), 21);
    assert_eq!(c.title_source, i32::from(pb::TitleSource::Auto));
    assert_eq!(c.last_message_first_line, "不该变成标题");
}

#[test]
fn dm_title_is_fixed_to_assistant_name() {
    let w = base();
    let l = w.list(pb::ListConversations::default());
    let dm = l
        .conversations
        .iter()
        .find(|c| c.id == scenario::IWORK_DM)
        .unwrap();
    assert_eq!(dm.title, "iWork");
    assert_eq!(dm.title_source, i32::from(pb::TitleSource::Fixed));
}

#[test]
fn approval_terminal_states_follow_the_contract_table() {
    let mut w = base();
    w.steps(
        scenario::get("approval-states")
            .unwrap()
            .steps()
            .into_iter()
            .skip(scenario::base().len())
            .collect(),
    );
    let g = w.conversation("c-approval", None, 0);
    let get = |id: &str| g.approvals.iter().find(|a| a.id == id).unwrap().clone();
    let user = i32::from(pb::ApprovalDecider::User);
    let pending = get("ap-pending");
    assert_eq!(
        (pending.decider, pending.void_reason, pending.decided_at),
        (None, None, None)
    );
    let approved = get("ap-approved");
    assert_eq!(approved.decider, Some(user));
    assert!(approved.void_reason.is_none() && approved.decided_at.is_some());
    let switched = get("ap-mode-switch");
    assert_eq!(
        switched.decider,
        Some(i32::from(pb::ApprovalDecider::ModeSwitch))
    );
    let denied = get("ap-denied");
    assert!(
        denied.decider.is_none() && denied.void_reason.is_none() && denied.decided_at.is_some()
    );
    let voided = get("ap-voided");
    assert_eq!(
        voided.void_reason,
        Some(i32::from(pb::VoidReason::TurnCancelled))
    );
    assert!(voided.decider.is_none() && voided.decided_at.is_some());
    // 待处理只列 PENDING 的。
    let pending_ids: Vec<String> = w
        .pending()
        .into_iter()
        .filter_map(|i| match i.item {
            Some(pb::pending_item::Item::Approval(a)) => Some(a.id),
            _ => None,
        })
        .collect();
    assert_eq!(pending_ids, ["ap-must-ask", "ap-outside", "ap-pending"]);
}

#[test]
fn last_turn_state_follows_most_recent_turn() {
    let mut w = base();
    w.steps(
        scenario::get("sidebar-dots")
            .unwrap()
            .steps()
            .into_iter()
            .skip(scenario::base().len())
            .collect(),
    );
    let l = w.list(pb::ListConversations::default());
    let state = |id: &str| {
        l.conversations
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .last_turn_state
    };
    assert_eq!(state("c-idle"), Some(i32::from(T::Completed)));
    assert_eq!(state("c-running"), Some(i32::from(T::Running)));
    assert_eq!(state("c-error"), Some(i32::from(T::Failed)));
    assert_eq!(state(scenario::IWORK_DM), None, "没有回合就不设");
}

#[test]
fn unread_counts_assistant_messages_and_pending_cards_until_read() {
    let mut w = base();
    w.steps(
        scenario::get("sidebar-dots")
            .unwrap()
            .steps()
            .into_iter()
            .skip(scenario::base().len())
            .collect(),
    );
    let unread = |w: &World, id: &str| {
        w.list(pb::ListConversations::default())
            .conversations
            .into_iter()
            .find(|c| c.id == id)
            .unwrap()
            .unread
    };
    // c-waiting：一条助手消息 + 一张待定审批；用户消息不算。
    assert_eq!(unread(&w, "c-waiting"), 2);
    let projects = match w.query(Q::ListProjects(pb::ListProjects {})) {
        R::ListProjects(p) => p.projects,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        projects[0].unread, 5,
        "项目未读 = 四个对话之和（1 + 1 + 2 + 1）"
    );
    let head = w.head;
    w.push(f::conversation_read("c-waiting", head));
    assert_eq!(unread(&w, "c-waiting"), 0);
}

#[test]
fn activities_prefer_waiting_then_running_then_message() {
    let mut w = base();
    w.steps(
        scenario::get("assistants-20")
            .unwrap()
            .steps()
            .into_iter()
            .skip(scenario::base().len())
            .collect(),
    );
    let acts = match w.query(Q::ListAssistants(pb::ListAssistants {})) {
        R::ListAssistants(a) => a.activities,
        other => panic!("{other:?}"),
    };
    let kind = |id: &str| acts.iter().find(|a| a.assistant_id == id).map(|a| a.kind);
    use pb::AssistantActivityKind as K;
    assert_eq!(kind("as-03"), Some(i32::from(K::Message)));
    assert_eq!(
        kind("as-15"),
        Some(i32::from(K::Running)),
        "15 是 3 与 5 的倍数：回合没结束"
    );
    assert_eq!(kind("as-01"), None, "从没说过话就没有");
    let msg = acts.iter().find(|a| a.assistant_id == "as-03").unwrap();
    assert_eq!(
        msg.message_first_line.as_deref(),
        Some("在的，有什么要做？")
    );
    // WAITING 压过 RUNNING。
    w.push(f::approval(
        "ap-x",
        "c-dm-15",
        "t-as-15",
        f::command_action("ls"),
        pb::ApprovalKind::Ask,
        &[],
    ));
    let acts = match w.query(Q::ListAssistants(pb::ListAssistants {})) {
        R::ListAssistants(a) => a.activities,
        other => panic!("{other:?}"),
    };
    let a15 = acts.iter().find(|a| a.assistant_id == "as-15").unwrap();
    assert_eq!(a15.kind, i32::from(K::Waiting));
    assert_eq!(a15.pending_item_id.as_deref(), Some("ap-x"));
}

#[test]
fn list_conversations_pages_by_recent_activity_and_filters_by_title() {
    let mut w = base();
    for i in 0..5 {
        w.push(f::project_conversation(&format!("c{i}"), scenario::PROJECT));
        w.push(f::user_message(
            &format!("m{i}"),
            &format!("c{i}"),
            &format!("标题 {i}"),
        ));
    }
    // c1 最近有活动：排到最前。
    w.push(f::user_message("m1b", "c1", "再问一句"));
    let page = |before: Option<&str>| {
        w.list(pb::ListConversations {
            project_id: Some(scenario::PROJECT.into()),
            title_contains: None,
            before: before.map(Into::into),
            limit: 2,
        })
    };
    let p1 = page(None);
    let ids = |r: &pb::ListConversationsResult| {
        r.conversations
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&p1), ["c1", "c4"]);
    assert!(p1.has_more);
    let p2 = page(Some("c4"));
    assert_eq!(ids(&p2), ["c3", "c2"]);
    let p3 = page(Some("c2"));
    assert_eq!(ids(&p3), ["c0"]);
    assert!(!p3.has_more);
    let filtered = w.list(pb::ListConversations {
        project_id: None,
        title_contains: Some("标题 3".into()),
        before: None,
        limit: 0,
    });
    assert_eq!(ids(&filtered), ["c3"]);
}

#[test]
fn get_conversation_pages_by_key_and_rows_follow_page_range() {
    let mut w = base();
    w.push(f::project_conversation("c", scenario::PROJECT));
    for i in 0..120 {
        w.push(f::user_message(&format!("m{i:03}"), "c", "x"));
    }
    let latest = w.conversation("c", None, 0);
    assert_eq!(latest.messages.len(), 50);
    assert_eq!(latest.messages[0].id, "m070");
    assert!(latest.has_more);
    let older = w.conversation("c", Some("m070"), 50);
    assert_eq!(older.messages.first().unwrap().id, "m020");
    assert_eq!(older.messages.last().unwrap().id, "m069");
    let oldest = w.conversation("c", Some("m020"), 50);
    assert_eq!(oldest.messages.len(), 20);
    assert!(!oldest.has_more);
}

#[test]
fn deleting_conversation_waits_for_unknown_card() {
    let mut w = base();
    w.steps(
        scenario::get("user-action-unknown-deleting")
            .unwrap()
            .steps()
            .into_iter()
            .skip(scenario::base().len())
            .collect(),
    );
    let l = w.list(pb::ListConversations::default());
    let c = l
        .conversations
        .iter()
        .find(|c| c.id == "c-ua-deleting")
        .unwrap();
    assert_eq!(c.state, i32::from(pb::ConversationState::Deleting));
}
