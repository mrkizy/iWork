//! 视图查询的纯函数（T10a 方案 2.6；T09 方案 2.2、2.9、第 3 节 G1–G4、G8、G10、G11）：
//!
//! - 请求一侧：`GetConversation` 六种取法的互斥校验与 `limit` 的口径、`ListAnchors` 的窗口；
//! - 结果一侧：读模型的原始事实 → 协议。投影字段（各种 `created_seq`、`ended_seq`、`decided_at`、`unread`、
//!   标题、首行、最近回合状态）在这里填，`convert` 里实体本身的转换不碰它们。
//!
//! 标题与摘录按字素簇截（技术设计 9.0a；T09 方案 2.9），这是唯一一处。

use iwork_domain::id::{ConversationId, MessageId, ProjectId, Seq};
use iwork_domain::message::MessageBody;
use iwork_ports::read_model::{
    AnchorKind, AnchorWindow, Anchors, ApprovalRow, ConversationFacts, ConversationPage,
    ConversationPaging, MessageRow, PendingEntry, PendingItem, ProjectSummary, RuntimeQuota,
    ToolCallRow, TurnRow, UserActionRow, ViewRefused,
};
use iwork_proto::pb;
use unicode_segmentation::UnicodeSegmentation;

use crate::convert::{
    ConvertError, action_to_proto, approval_to_proto, artifact_ref_to_proto, attachment_to_proto,
    change_summary_to_proto, conversation_to_proto, message_to_proto, project_to_proto,
    quota_window_to_proto, timestamp_to_proto, turn_to_proto, usage_to_proto,
    user_action_card_to_proto,
};

type R<T> = Result<T, ConvertError>;

/// 按消息条数取时 `limit` 为 0 的默认页长（与会话范围快照同一个数）。
pub const MESSAGE_PAGE_DEFAULT: u32 = crate::subscription::CONVERSATION_SNAPSHOT_PAGE_SIZE;
/// 一次最多取多少条消息或多少行（T09 方案 2.9：行级游标大于它按它取；按消息数取同一个上限）。
pub const PAGE_LIMIT_MAX: u32 = 1000;
/// 锚点窗口一侧最多取多少条。
pub const ANCHOR_SIDE_MAX: u32 = 1000;
/// 自动标题的字素簇数（技术设计 9.0a）。
pub const AUTO_TITLE_GRAPHEMES: usize = 20;
/// 锚点摘录的字素簇数（T09 方案 2.9）。
pub const EXCERPT_GRAPHEMES: usize = 200;

/// `INVALID_ARGUMENT`，`details.field` 点名字段。
pub fn invalid(field: &str, message: impl Into<String>) -> pb::Error {
    pb::Error {
        code: pb::ErrorCode::InvalidArgument.into(),
        message: message.into(),
        retryable: false,
        details: [("field".to_owned(), field.to_owned())].into(),
    }
}

/// 读模型拒绝的原因 → 对外错误。
pub fn refused_to_error(r: &ViewRefused) -> pb::Error {
    match r {
        ViewRefused::ConversationNotFound(id) => pb::Error {
            code: pb::ErrorCode::NotFound.into(),
            message: format!("对话不存在：{id}"),
            retryable: false,
            details: Default::default(),
        },
        ViewRefused::CursorNotInConversation { field, id } => {
            invalid(field, format!("消息不在对话里：{id}"))
        }
    }
}

fn parse<T: std::str::FromStr>(field: &str, text: &str) -> Result<T, pb::Error> {
    text.parse()
        .map_err(|_| invalid(field, format!("不是合法的编号：{text}")))
}

/// `GetConversation` 的取法（T09 方案 G11）：五个取页字段至多设一个；按消息数取的 `limit` 为 0 取默认页长；
/// 行级游标的游标与 `limit` 为 0 拒绝；都按 [`PAGE_LIMIT_MAX`] 封顶。
pub fn conversation_paging(
    g: &pb::GetConversation,
) -> Result<(ConversationId, ConversationPaging), pb::Error> {
    let id = parse("conversation_id", &g.conversation_id)?;
    let set = [
        g.before_message_id.is_some(),
        g.after_message_id.is_some(),
        g.around_seq.is_some(),
        g.before_seq.is_some(),
        g.after_seq.is_some(),
    ]
    .into_iter()
    .filter(|b| *b)
    .count();
    if set > 1 {
        return Err(invalid(
            "get_conversation",
            "before_message_id、after_message_id、around_seq、before_seq、after_seq 至多设一个",
        ));
    }
    let messages = if g.limit == 0 {
        MESSAGE_PAGE_DEFAULT
    } else {
        g.limit.min(PAGE_LIMIT_MAX)
    };
    let rows = |field: &str, seq: u64| -> Result<(Seq, u32), pb::Error> {
        if seq == 0 {
            return Err(invalid(field, "行级游标不能为 0"));
        }
        if g.limit == 0 {
            return Err(invalid("limit", "行级游标的 limit 不能为 0"));
        }
        Ok((Seq(seq), g.limit.min(PAGE_LIMIT_MAX)))
    };
    let paging = if let Some(m) = &g.before_message_id {
        ConversationPaging::BeforeMessage {
            message: parse::<MessageId>("before_message_id", m)?,
            limit: messages,
        }
    } else if let Some(m) = &g.after_message_id {
        ConversationPaging::AfterMessage {
            message: parse::<MessageId>("after_message_id", m)?,
            limit: messages,
        }
    } else if let Some(seq) = g.around_seq {
        ConversationPaging::Around {
            seq: Seq(seq),
            limit: messages,
        }
    } else if let Some(s) = g.before_seq {
        let (seq, limit) = rows("before_seq", s)?;
        ConversationPaging::BeforeRow { seq, limit }
    } else if let Some(s) = g.after_seq {
        let (seq, limit) = rows("after_seq", s)?;
        ConversationPaging::AfterRow { seq, limit }
    } else {
        ConversationPaging::Latest { limit: messages }
    };
    Ok((id, paging))
}

/// `ListAnchors` 的窗口：两侧各封顶 [`ANCHOR_SIDE_MAX`]；`around_seq` 为 0 = 从最新往回。
pub fn anchor_window(q: &pb::ListAnchors) -> Result<(ConversationId, AnchorWindow), pb::Error> {
    Ok((
        parse("conversation_id", &q.conversation_id)?,
        AnchorWindow {
            around: (q.around_seq != 0).then_some(Seq(q.around_seq)),
            before: q.before.min(ANCHOR_SIDE_MAX),
            after: q.after.min(ANCHOR_SIDE_MAX),
        },
    ))
}

// ---------- 文字 ----------

/// 前 `n` 个字素簇；截掉了返回 `true`。
fn take_graphemes(text: &str, n: usize) -> (&str, bool) {
    match text.grapheme_indices(true).nth(n) {
        Some((i, _)) => (&text[..i], true),
        None => (text, false),
    }
}

/// 项目对话的自动标题（技术设计 9.0a）：换行与连续空白折叠成一个空格，取前 20 个字素簇，超出加「…」；
/// 文本为空（只有附件、或正文存成了制品）时取第一个附件的文件名。
pub fn auto_title(body: &MessageBody, first_attachment: Option<&str>) -> String {
    let text = match body {
        MessageBody::Inline(t) => t.as_str(),
        MessageBody::Artifact(_) => "",
    };
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return first_attachment.unwrap_or("").to_owned();
    }
    match take_graphemes(&collapsed, AUTO_TITLE_GRAPHEMES) {
        (head, true) => format!("{head}…"),
        (head, false) => head.to_owned(),
    }
}

/// 正文的首个非空行（去掉首尾空白）；存成制品的为空串。
pub fn first_line(body: &MessageBody) -> String {
    match body {
        MessageBody::Inline(t) => t
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_owned(),
        MessageBody::Artifact(_) => String::new(),
    }
}

/// 锚点摘录：正文开头 200 个字素簇。
pub fn excerpt(text: &str) -> String {
    take_graphemes(text, EXCERPT_GRAPHEMES).0.to_owned()
}

// ---------- 对话 ----------

/// 对话的查询视图（G1、G4、G8）：私聊固定标题取助手名；项目对话自动标题取第一条用户消息。
/// 重命名（`USER`）随 T10b 的 `ConversationRenamed` 进来。
pub fn conversation_view(f: &ConversationFacts) -> pb::Conversation {
    let (title, source) = match &f.dm_assistant_name {
        Some(name) => (name.clone(), pb::TitleSource::Fixed),
        None => (
            f.first_user_message
                .as_ref()
                .map(|m| auto_title(&m.body, m.first_attachment_name.as_deref()))
                .unwrap_or_default(),
            pb::TitleSource::Auto,
        ),
    };
    pb::Conversation {
        unread: f.unread,
        title,
        title_source: source.into(),
        last_turn_state: f
            .last_turn_state
            .map(|s| crate::convert::turn_state_to_proto(s).0),
        last_message_first_line: f
            .last_message_body
            .as_ref()
            .map(first_line)
            .unwrap_or_default(),
        ..conversation_to_proto(&f.conversation)
    }
}

/// `ListConversations`（G8）：`all` 已按最近活动倒序；标题筛选（不区分大小写的包含）、`before` 游标与 `limit`
/// 在这里做。`before` 指的对话不在筛过的列表里时拒绝。
pub fn list_conversations(
    all: &[ConversationFacts],
    q: &pb::ListConversations,
) -> Result<pb::ListConversationsResult, pb::Error> {
    let needle = q.title_contains.as_deref().map(str::to_lowercase);
    let mut rows: Vec<pb::Conversation> = all
        .iter()
        .map(conversation_view)
        .filter(|c| {
            needle
                .as_ref()
                .is_none_or(|n| c.title.to_lowercase().contains(n.as_str()))
        })
        .collect();
    if let Some(before) = &q.before {
        let pos = rows
            .iter()
            .position(|c| &c.id == before)
            .ok_or_else(|| invalid("before", format!("游标对话不在列表里：{before}")))?;
        rows.drain(..=pos);
    }
    let mut has_more = false;
    if q.limit > 0 && rows.len() > q.limit as usize {
        rows.truncate(q.limit as usize);
        has_more = true;
    }
    Ok(pb::ListConversationsResult {
        conversations: rows,
        has_more,
    })
}

/// `ListConversations.project_id` → 读模型的过滤。
pub fn project_filter(q: &pb::ListConversations) -> Result<Option<ProjectId>, pb::Error> {
    q.project_id
        .as_deref()
        .map(|p| parse("project_id", p))
        .transpose()
}

// ---------- 一页 ----------

pub fn message_row_to_proto(m: &MessageRow) -> pb::Message {
    pb::Message {
        created_seq: m.created_seq.0,
        ..message_to_proto(&m.message)
    }
}

/// 回合连同投影字段（G2、G3）：两个排序字段、结束时 `TurnFinished` 带的用量、印章与改动摘要、状态说明。
pub fn turn_row_to_proto(t: &TurnRow) -> R<pb::Turn> {
    let mut p = turn_to_proto(&t.turn);
    p.created_seq = t.created_seq.0;
    p.state_reason = t.state_reason.clone();
    if let Some(end) = &t.end {
        p.ended_seq = end.seq.0;
        p.usage = Some(usage_to_proto(&end.usage));
        p.celebrate = end.celebrate;
        p.change_summary = end
            .change_summary
            .as_ref()
            .map(change_summary_to_proto)
            .transpose()?;
    }
    Ok(p)
}

pub fn tool_call_row_to_proto(c: &ToolCallRow) -> R<pb::ToolCall> {
    Ok(pb::ToolCall {
        turn_id: c.turn_id.to_string(),
        tool_call_id: c.tool_call_id.0.clone(),
        tool_name: c.tool_name.clone(),
        action: Some(action_to_proto(&c.action)?),
        finished: c.ok.is_some(),
        ok: c.ok.unwrap_or(false),
        output_artifact: c.output_artifact.as_ref().map(artifact_ref_to_proto),
        created_seq: c.created_seq.0,
    })
}

/// 审批连同投影字段（G2、G3）：排序字段与决定时间；决定者与作废原因由 `approval_to_proto` 照终态契约填。
pub fn approval_row_to_proto(a: &ApprovalRow) -> R<pb::Approval> {
    let mut p = approval_to_proto(&a.approval)?;
    p.created_seq = a.created_seq.0;
    p.decided_at = a.decided_at.map(timestamp_to_proto);
    Ok(p)
}

pub fn user_action_row_to_proto(c: &UserActionRow) -> R<pb::UserActionCard> {
    let mut p = user_action_card_to_proto(&c.card)?;
    p.created_seq = c.created_seq.0;
    Ok(p)
}

/// `GetConversation` 的结果。
pub fn conversation_page_to_proto(p: &ConversationPage) -> R<pb::GetConversationResult> {
    Ok(pb::GetConversationResult {
        conversation: Some(conversation_view(&p.conversation)),
        messages: p.messages.iter().map(message_row_to_proto).collect(),
        turns: p.turns.iter().map(turn_row_to_proto).collect::<R<_>>()?,
        has_more: p.has_more,
        tool_calls: p
            .tool_calls
            .iter()
            .map(tool_call_row_to_proto)
            .collect::<R<_>>()?,
        approvals: p
            .approvals
            .iter()
            .map(approval_row_to_proto)
            .collect::<R<_>>()?,
        user_actions: p
            .user_actions
            .iter()
            .map(user_action_row_to_proto)
            .collect::<R<_>>()?,
        attachments: p
            .attachments
            .iter()
            .map(attachment_to_proto)
            .collect::<R<_>>()?,
        usage_total: p.usage_total.as_ref().map(usage_to_proto),
        has_newer: p.has_newer,
    })
}

// ---------- 待处理与锚点 ----------

/// 一项「待你处理」（卡片与继续卡片带时间线上的位置）。
pub fn pending_entry_to_proto(e: &PendingEntry) -> R<pb::PendingItem> {
    use pb::pending_item::Item;
    let item = match &e.item {
        PendingItem::Approval(a) => {
            let mut p = approval_to_proto(a)?;
            p.created_seq = e.created_seq.0;
            Item::Approval(p)
        }
        PendingItem::UserAction(c) => {
            let mut p = user_action_card_to_proto(c)?;
            p.created_seq = e.created_seq.0;
            Item::UserAction(p)
        }
        PendingItem::AgentsMdDraft(d) => Item::AgentsMdDraft(pb::AgentsMdDraftCard {
            card_id: d.card_id.to_string(),
            project_id: d.project_id.to_string(),
            content: Some(artifact_ref_to_proto(&d.content)),
            proposed_at: Some(timestamp_to_proto(d.proposed_at)),
        }),
        PendingItem::Resume(r) => Item::Resume(pb::ResumeCard {
            turn_id: r.turn_id.to_string(),
            conversation_id: r.conversation_id.to_string(),
            reason: crate::convert::interrupt_reason_to_proto(r.reason),
            created_seq: e.created_seq.0,
        }),
    };
    Ok(pb::PendingItem { item: Some(item) })
}

pub fn anchors_to_proto(a: &Anchors) -> pb::ListAnchorsResult {
    use pb::anchor::Kind;
    pb::ListAnchorsResult {
        anchors: a
            .anchors
            .iter()
            .map(|x| pb::Anchor {
                created_seq: x.created_seq.0,
                at: Some(timestamp_to_proto(x.at)),
                kind: Some(match &x.kind {
                    AnchorKind::UserMessage { message_id, text } => {
                        Kind::UserMessage(pb::UserMessageAnchor {
                            message_id: message_id.to_string(),
                            excerpt: excerpt(text),
                        })
                    }
                    AnchorKind::Waiting { pending_item_id } => Kind::Waiting(pb::WaitingAnchor {
                        pending_item_id: pending_item_id.clone(),
                    }),
                    AnchorKind::FailedTurn {
                        turn_id,
                        reason,
                        had_output,
                    } => Kind::FailedTurn(pb::FailedTurnAnchor {
                        turn_id: turn_id.to_string(),
                        reason: reason.clone(),
                        had_output: *had_output,
                    }),
                }),
            })
            .collect(),
        has_before: a.has_before,
        has_after: a.has_after,
        waiting_count: a.waiting_count,
        failed_count: a.failed_count,
    }
}

// ---------- 别的列表 ----------

pub fn project_summary_to_proto(p: &ProjectSummary) -> R<pb::Project> {
    Ok(pb::Project {
        unread: p.unread,
        ..project_to_proto(&p.project)?
    })
}

pub fn runtime_quota_to_proto(q: &RuntimeQuota) -> R<pb::RuntimeQuota> {
    Ok(pb::RuntimeQuota {
        runtime_id: q.runtime_id.to_string(),
        windows: q
            .windows
            .iter()
            .map(quota_window_to_proto)
            .collect::<R<_>>()?,
        observed_at: Some(timestamp_to_proto(q.observed_at)),
    })
}

pub fn global_settings_to_proto(s: &iwork_domain::settings::GlobalSettings) -> pb::GlobalSettings {
    pb::GlobalSettings {
        playful_ui: s.playful_ui,
        autostart: s.autostart,
        projects_root: s.projects_root.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inline(t: &str) -> MessageBody {
        MessageBody::Inline(t.into())
    }

    #[test]
    fn auto_title_counts_graphemes_and_collapses_space() {
        // 20 个「é」（e + 组合重音，两个码点一个字素簇）：不截；21 个：截到 20 个加「…」
        let e = "e\u{301}";
        assert_eq!(auto_title(&inline(&e.repeat(20)), None), e.repeat(20));
        assert_eq!(
            auto_title(&inline(&e.repeat(21)), None),
            format!("{}…", e.repeat(20))
        );
        assert_eq!(auto_title(&inline("  a\n\n b\tc "), None), "a b c");
        assert_eq!(auto_title(&inline(" \n"), Some("图.png")), "图.png");
        assert_eq!(auto_title(&inline(""), None), "");
    }

    #[test]
    fn excerpt_keeps_200_graphemes() {
        let flag = "🇨🇳"; // 两个码点一个字素簇
        assert_eq!(excerpt(&flag.repeat(200)), flag.repeat(200));
        assert_eq!(excerpt(&flag.repeat(201)), flag.repeat(200));
    }

    fn facts(n: u128, title_text: &str) -> ConversationFacts {
        use iwork_domain::conversation::{Conversation, ConversationKind, ConversationState};
        use iwork_domain::id::RawUlid;
        ConversationFacts {
            conversation: Conversation {
                id: ConversationId::from_raw(RawUlid::from_u128(n)),
                kind: ConversationKind::Project {
                    project_id: ProjectId::from_raw(RawUlid::from_u128(1)),
                },
                state: ConversationState::Active,
                last_read_seq: Seq(0),
                model_override: None,
                approval_mode: None,
                updated_at: iwork_domain::common::Timestamp(0),
            },
            unread: 0,
            dm_assistant_name: None,
            first_user_message: Some(iwork_ports::read_model::FirstUserMessage {
                body: inline(title_text),
                first_attachment_name: None,
            }),
            last_turn_state: None,
            last_message_body: None,
        }
    }

    /// G8：标题筛选不区分大小写、在游标之后取 limit 条、`has_more` 照实；游标不在筛过的列表里拒绝。
    #[test]
    fn list_conversations_filters_then_pages() {
        let all: Vec<_> = (1..=5)
            .map(|n| facts(n, if n % 2 == 1 { "Fix Bug" } else { "写文档" }))
            .collect();
        let ids = |r: &pb::ListConversationsResult| -> Vec<String> {
            r.conversations.iter().map(|c| c.id.clone()).collect()
        };
        let id = |n: usize| all[n - 1].conversation.id.to_string();
        let q = |title: Option<&str>, before: Option<String>, limit| pb::ListConversations {
            project_id: None,
            title_contains: title.map(str::to_owned),
            before,
            limit,
        };
        let r = list_conversations(&all, &q(None, None, 0)).unwrap();
        assert_eq!(
            ids(&r),
            (1..=5).map(id).collect::<Vec<_>>(),
            "不分页取全部，顺序不变"
        );
        assert!(!r.has_more);
        let r = list_conversations(&all, &q(Some("fix"), None, 2)).unwrap();
        assert_eq!(ids(&r), [id(1), id(3)]);
        assert!(r.has_more);
        let r = list_conversations(&all, &q(Some("FIX"), Some(id(3)), 2)).unwrap();
        assert_eq!(ids(&r), [id(5)]);
        assert!(!r.has_more);
        let e = list_conversations(&all, &q(Some("fix"), Some(id(2)), 2)).unwrap_err();
        assert_eq!(e.details["field"], "before", "游标对话被筛掉了");
    }

    #[test]
    fn first_line_skips_blank_lines() {
        assert_eq!(first_line(&inline("\n  \n  你好 \n世界")), "你好");
        assert_eq!(first_line(&inline("")), "");
    }

    fn get(f: impl FnOnce(&mut pb::GetConversation)) -> pb::GetConversation {
        let mut g = pb::GetConversation {
            conversation_id: ConversationId::from_raw(iwork_domain::id::RawUlid::from_u128(1))
                .to_string(),
            ..Default::default()
        };
        f(&mut g);
        g
    }

    #[test]
    fn paging_ways_are_exclusive_and_limits_are_clamped() {
        use ConversationPaging as P;
        assert_eq!(
            conversation_paging(&get(|_| {})).unwrap().1,
            P::Latest {
                limit: MESSAGE_PAGE_DEFAULT
            }
        );
        assert_eq!(
            conversation_paging(&get(|g| {
                g.after_seq = Some(5);
                g.limit = 5000;
            }))
            .unwrap()
            .1,
            P::AfterRow {
                seq: Seq(5),
                limit: PAGE_LIMIT_MAX
            }
        );
        let bad = |f: fn(&mut pb::GetConversation), field: &str| {
            let e = conversation_paging(&get(f)).unwrap_err();
            assert_eq!(e.code, i32::from(pb::ErrorCode::InvalidArgument));
            assert_eq!(e.details["field"], field);
        };
        bad(
            |g| {
                g.before_seq = Some(3);
                g.around_seq = Some(3);
                g.limit = 1;
            },
            "get_conversation",
        );
        bad(
            |g| {
                g.before_seq = Some(0);
                g.limit = 1;
            },
            "before_seq",
        );
        bad(|g| g.after_seq = Some(3), "limit");
        bad(
            |g| g.before_message_id = Some("不是编号".into()),
            "before_message_id",
        );
    }
}
