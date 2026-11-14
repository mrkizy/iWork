//! 视图查询（T10a 方案 2.6、2.7；T09 方案 2.2 行登记表、2.9）：`GetConversation` 六种取法、对话列表的原始事实、
//! 待处理项带位置、消息锚点。
//!
//! 全部方法挂在 [`ViewTx`] 上：一个 [`ViewTx`] 就是一个读事务，订阅快照把几个范围的查询放进同一个事务里跑（S1），
//! 单条查询也是一个事务（Q1）。`head` 读的是这个事务看到的最新 seq。
//!
//! 时间线上每种行的排序字段是创建它的事件 seq（行登记表）：消息 `appended_seq`、工具调用 `started_seq`、审批
//! `requested_seq`、卡片 `proposed_seq`、回合 `created_seq` 与 `ended_seq`。各有按（对话，排序字段）的索引
//! （迁移 0002），取页不扫整个对话。
//!
//! 投影上没有的值从事件里取，按 seq 主键读一条：工具调用的动作（`ToolCallStarted`）、回合结束的用量与印章
//! （`TurnFinished`）、事件的信封时间。

use std::collections::BTreeSet;

use iwork_domain::event::{DomainEvent, StreamKind};
use iwork_domain::id::{AttachmentId, ConversationId, MessageId, Seq};
use iwork_domain::message::{Author, Message, MessageBody};
use iwork_domain::turn::{Turn, TurnState};
use iwork_domain::user_action::UserActionStateKind;
use iwork_ports::read_model::{
    Anchor, AnchorKind, AnchorWindow, Anchors, ApprovalRow, ConversationFacts, ConversationPage,
    ConversationPaging, FirstUserMessage, MessageRow, PendingEntry, PendingItem, ResumeCard,
    ToolCallRow, TurnEnd, TurnRow, UserActionRow, ViewRefused,
};
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::projector::messages::author_kind;
use crate::read_model::{
    ApprovalColumns, ReadDefaults, approval_row, blobs, decode_approval_row, fill_streaming_body,
    kinds_where, rows,
};
use crate::snapshot::{
    decode_assistant, decode_attachment, decode_card, decode_conversation, decode_message,
    decode_turn,
};
use crate::sql::{ColumnText, seq_col, sql_err, u64_col};

/// 视图查询的结果：要么查到，要么因为请求本身不成立被拒（不是存储故障）。
pub type Viewed<T> = Result<Result<T, ViewRefused>, StorageError>;

/// 页的 seq 上界「没有上界」：SQLite 的整数是 i64。
const NO_UPPER: i64 = i64::MAX;

/// 一个读事务里的视图查询。由 [`crate::SqliteReadModel::view`] 交给调用方。
pub struct ViewTx<'a, C: EventCodec + ?Sized> {
    pub(crate) conn: &'a Connection,
    pub(crate) codec: &'a C,
    pub(crate) defaults: &'a ReadDefaults,
}

fn seq_i64(s: Seq) -> i64 {
    i64::try_from(s.0).unwrap_or(NO_UPPER)
}

fn seq_of(table: &str, column: &str, v: i64) -> Result<Seq, StorageError> {
    seq_col(table, column, v)
}

/// 一页里除对话事实与累计用量之外的部分。
struct PageBody {
    messages: Vec<MessageRow>,
    turns: Vec<TurnRow>,
    tool_calls: Vec<ToolCallRow>,
    approvals: Vec<ApprovalRow>,
    user_actions: Vec<UserActionRow>,
    attachments: Vec<iwork_domain::attachment::Attachment>,
    has_more: bool,
    has_newer: bool,
}

/// 一类行在行级游标里的身份。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum RowRef {
    Message(String),
    ToolCall(String, String),
    /// 审批 id、所属回合
    Approval(String, String),
    /// 卡片 id、所属回合
    UserAction(String, String),
    TurnEnd(String),
}

impl<C: EventCodec + ?Sized> ViewTx<'_, C> {
    /// 这个事务看到的最新 seq。
    pub fn head(&self) -> Result<Seq, StorageError> {
        let head: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))
            .map_err(sql_err)?;
        seq_of("events", "seq", head)
    }

    // ---------- 已有的列表查询（和 `ReadModelPort` 的同名方法同一份查询体） ----------

    pub fn projects(&self) -> Result<Vec<iwork_ports::read_model::ProjectSummary>, StorageError> {
        crate::read_model::projects_on(self.conn, self.codec)
    }

    pub fn assistants(&self) -> Result<Vec<iwork_domain::assistant::Assistant>, StorageError> {
        crate::read_model::assistants_on(self.conn, self.codec)
    }

    pub fn quota(&self) -> Result<Vec<iwork_ports::read_model::RuntimeQuota>, StorageError> {
        crate::read_model::quota_on(self.conn)
    }

    pub fn runtimes(&self) -> Result<Vec<iwork_domain::runtime::Runtime>, StorageError> {
        crate::read_model::runtimes_on(self.conn, self.codec)
    }

    pub fn rules(
        &self,
        project: Option<iwork_domain::id::ProjectId>,
    ) -> Result<Vec<iwork_domain::rule::RuleEntry>, StorageError> {
        crate::read_model::rules_on(self.conn, self.codec, project)
    }

    pub fn settings(&self) -> Result<iwork_domain::settings::GlobalSettings, StorageError> {
        crate::read_model::settings_on(self.conn, self.defaults)
    }

    /// 一个项目；不存在为 `None`（`GetSettings` 带项目设置用）。
    pub fn project(
        &self,
        id: iwork_domain::id::ProjectId,
    ) -> Result<Option<iwork_domain::project::Project>, StorageError> {
        let s: Option<Vec<u8>> = self
            .conn
            .prepare_cached("SELECT snapshot FROM projects WHERE id = ?1")
            .and_then(|mut st| st.query_row([id.to_string()], |r| r.get(0)).optional())
            .map_err(sql_err)?;
        s.map(|s| crate::snapshot::decode_project(self.codec, &s))
            .transpose()
    }

    // ---------- 对话 ----------

    /// 一个对话的原始事实；对话不存在为 `None`。
    pub fn conversation_facts(
        &self,
        id: ConversationId,
    ) -> Result<Option<ConversationFacts>, StorageError> {
        let row: Option<(Vec<u8>, i64)> = self
            .conn
            .prepare_cached("SELECT snapshot, unread FROM conversations WHERE id = ?1")
            .and_then(|mut s| {
                s.query_row([id.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()
            })
            .map_err(sql_err)?;
        row.map(|(snapshot, unread)| self.facts(&snapshot, unread))
            .transpose()
    }

    /// 全部对话（或某个项目的）的原始事实，按最近活动倒序（`updated_at`，同时刻按 id 倒序）。
    pub fn conversations(
        &self,
        project: Option<iwork_domain::id::ProjectId>,
    ) -> Result<Vec<ConversationFacts>, StorageError> {
        let raw: Vec<(Vec<u8>, i64)> = match project {
            None => rows(
                self.conn,
                "SELECT snapshot, unread FROM conversations ORDER BY updated_at DESC, id DESC",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?,
            Some(p) => rows(
                self.conn,
                "SELECT snapshot, unread FROM conversations WHERE project_id = ?1 \
                 ORDER BY updated_at DESC, id DESC",
                [p.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?,
        };
        raw.iter().map(|(s, u)| self.facts(s, *u)).collect()
    }

    fn facts(&self, snapshot: &[u8], unread: i64) -> Result<ConversationFacts, StorageError> {
        use iwork_domain::conversation::ConversationKind;
        let conversation = decode_conversation(self.codec, snapshot)?;
        let key = conversation.id.to_string();
        let dm_assistant_name = match &conversation.kind {
            ConversationKind::AssistantDm { assistant_id, .. } => {
                let s: Option<Vec<u8>> = self
                    .conn
                    .prepare_cached("SELECT snapshot FROM assistants WHERE id = ?1")
                    .and_then(|mut st| {
                        st.query_row([assistant_id.to_string()], |r| r.get(0))
                            .optional()
                    })
                    .map_err(sql_err)?;
                // 私聊随助手一起删（级联），助手不在只可能是投影坏了：点名报错，不当成空名
                let s = s.ok_or_else(|| {
                    crate::sql::bad_column(
                        "conversations",
                        "assistant_id",
                        format!("私聊 {key} 的助手 {assistant_id} 不在"),
                    )
                })?;
                Some(decode_assistant(self.codec, &s)?.current.config.name)
            }
            ConversationKind::Project { .. } => None,
        };
        let first_user: Option<Vec<u8>> = self
            .conn
            .prepare_cached(
                "SELECT snapshot FROM messages WHERE conversation_id = ?1 AND author_kind = ?2 \
                 ORDER BY appended_seq LIMIT 1",
            )
            .and_then(|mut st| {
                st.query_row(params![key, author_kind(&Author::User)], |r| r.get(0))
                    .optional()
            })
            .map_err(sql_err)?;
        let first_user_message = first_user
            .map(|s| -> Result<FirstUserMessage, StorageError> {
                let m = decode_message(self.codec, &s)?;
                let first_attachment_name = match m.attachments.first() {
                    None => None,
                    Some(a) => self
                        .conn
                        .prepare_cached("SELECT name FROM attachments WHERE id = ?1")
                        .and_then(|mut st| {
                            st.query_row([a.to_string()], |r| r.get::<_, String>(0))
                                .optional()
                        })
                        .map_err(sql_err)?,
                };
                Ok(FirstUserMessage {
                    body: m.body,
                    first_attachment_name,
                })
            })
            .transpose()?;
        let last_turn: Option<Vec<u8>> = self
            .conn
            .prepare_cached(
                "SELECT snapshot FROM turns WHERE conversation_id = ?1 \
                 ORDER BY created_seq DESC LIMIT 1",
            )
            .and_then(|mut st| st.query_row([&key], |r| r.get(0)).optional())
            .map_err(sql_err)?;
        let last_turn_state = last_turn
            .map(|s| decode_turn(self.codec, &s).map(|t| t.state))
            .transpose()?;
        let last_message: Option<Vec<u8>> = self
            .conn
            .prepare_cached(
                "SELECT snapshot FROM messages WHERE conversation_id = ?1 \
                 ORDER BY appended_seq DESC LIMIT 1",
            )
            .and_then(|mut st| st.query_row([&key], |r| r.get(0)).optional())
            .map_err(sql_err)?;
        let last_message_body = last_message
            .map(|s| -> Result<MessageBody, StorageError> {
                let mut m = decode_message(self.codec, &s)?;
                fill_streaming_body(self.conn, &mut m)?;
                Ok(m.body)
            })
            .transpose()?;
        Ok(ConversationFacts {
            conversation,
            unread: unread_u32(unread),
            dm_assistant_name,
            first_user_message,
            last_turn_state,
            last_message_body,
        })
    }

    /// `GetConversation` 的一页（T09 方案 2.9）。对话不存在、游标消息不在这个对话里时拒绝。
    pub fn conversation_page(
        &self,
        id: ConversationId,
        paging: ConversationPaging,
    ) -> Viewed<ConversationPage> {
        let Some(conversation) = self.conversation_facts(id)? else {
            return Ok(Err(ViewRefused::ConversationNotFound(id)));
        };
        let key = id.to_string();
        let body = match paging {
            ConversationPaging::BeforeRow { seq, limit } => {
                self.row_page(&key, RowSide::Before, seq, limit)?
            }
            ConversationPaging::AfterRow { seq, limit } => {
                self.row_page(&key, RowSide::After, seq, limit)?
            }
            _ => match self.message_page(&key, paging)? {
                Ok(p) => p,
                Err(refused) => return Ok(Err(refused)),
            },
        };
        Ok(Ok(ConversationPage {
            conversation,
            messages: body.messages,
            turns: body.turns,
            tool_calls: body.tool_calls,
            approvals: body.approvals,
            user_actions: body.user_actions,
            attachments: body.attachments,
            usage_total: self.usage_total(&key)?,
            has_more: body.has_more,
            has_newer: body.has_newer,
        }))
    }

    /// 消息游标在这个对话里的排序字段。
    fn cursor_seq(
        &self,
        conversation: &str,
        message: MessageId,
    ) -> Result<Option<i64>, StorageError> {
        self.conn
            .prepare_cached(
                "SELECT appended_seq FROM messages WHERE id = ?1 AND conversation_id = ?2",
            )
            .and_then(|mut st| {
                st.query_row(params![message.to_string(), conversation], |r| r.get(0))
                    .optional()
            })
            .map_err(sql_err)
    }

    /// `(snapshot, appended_seq)` 一段消息。`older`：取不大于（`inclusive`）或小于 `bound` 的，从新到旧；否则取大于的，
    /// 从旧到新。
    fn messages_from(
        &self,
        conversation: &str,
        bound: i64,
        older: bool,
        inclusive: bool,
        limit: u32,
    ) -> Result<Vec<(Vec<u8>, i64)>, StorageError> {
        let sql = match (older, inclusive) {
            (true, true) => {
                "SELECT snapshot, appended_seq FROM messages WHERE conversation_id = ?1 \
                 AND appended_seq <= ?2 ORDER BY appended_seq DESC LIMIT ?3"
            }
            (true, false) => {
                "SELECT snapshot, appended_seq FROM messages WHERE conversation_id = ?1 \
                 AND appended_seq < ?2 ORDER BY appended_seq DESC LIMIT ?3"
            }
            (false, _) => {
                "SELECT snapshot, appended_seq FROM messages WHERE conversation_id = ?1 \
                 AND appended_seq > ?2 ORDER BY appended_seq ASC LIMIT ?3"
            }
        };
        rows(self.conn, sql, params![conversation, bound, limit], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
    }

    /// 按消息条数取的四种取法（假后台 `paging::page_range` 的语义）：先定这一页的消息，再按页的 seq 范围
    /// `[lo, hi)` 带上回合、工具调用、审批、卡片。范围的界：前面还有消息时从本页第一条起，否则从 0；后面还有消息时
    /// 到下一条消息为止，否则没有上界。
    fn message_page(&self, conversation: &str, paging: ConversationPaging) -> Viewed<PageBody> {
        type Raw = Vec<(Vec<u8>, i64)>;
        // (本页从旧到新, 前面还有, 后面还有, 后面下一条的 seq)
        let (page, has_more, has_newer, next_seq): (Raw, bool, bool, Option<i64>) = match paging {
            ConversationPaging::Latest { limit } => {
                let mut got = self.messages_from(conversation, NO_UPPER, true, true, limit + 1)?;
                let more = got.len() > limit as usize;
                got.truncate(limit as usize);
                got.reverse();
                (got, more, false, None)
            }
            ConversationPaging::BeforeMessage { message, limit } => {
                let Some(s) = self.cursor_seq(conversation, message)? else {
                    return Ok(Err(ViewRefused::CursorNotInConversation {
                        field: "before_message_id",
                        id: message.to_string(),
                    }));
                };
                let mut got = self.messages_from(conversation, s, true, false, limit + 1)?;
                let more = got.len() > limit as usize;
                got.truncate(limit as usize);
                got.reverse();
                // 游标那条消息本身就在后面
                (got, more, true, Some(s))
            }
            ConversationPaging::AfterMessage { message, limit } => {
                let Some(s) = self.cursor_seq(conversation, message)? else {
                    return Ok(Err(ViewRefused::CursorNotInConversation {
                        field: "after_message_id",
                        id: message.to_string(),
                    }));
                };
                let mut got = self.messages_from(conversation, s, false, false, limit + 1)?;
                let next = got.get(limit as usize).map(|(_, seq)| *seq);
                got.truncate(limit as usize);
                // 游标那条消息本身就在前面
                (got, true, next.is_some(), next)
            }
            ConversationPaging::Around { seq, limit } => {
                let around = seq_i64(seq);
                let before = self.messages_from(conversation, around, true, true, limit + 1)?;
                let after = self.messages_from(conversation, around, false, false, limit + 1)?;
                let half = (limit / 2) as usize;
                let limit = limit as usize;
                let b = before.len().min(limit);
                let a = after.len().min(limit);
                // 前面（含）取一半，后面补满；后面不够时往前挪，页长尽量不变
                let mut nb = b.min(half);
                let na = a.min(limit - nb);
                if na < limit - nb {
                    nb = b.min(limit - na);
                }
                let more = before.len() > nb;
                let next = after.get(na).map(|(_, s)| *s);
                let mut page: Raw = before.into_iter().take(nb).collect();
                page.reverse();
                page.extend(after.into_iter().take(na));
                (page, more, next.is_some(), next)
            }
            ConversationPaging::BeforeRow { .. } | ConversationPaging::AfterRow { .. } => {
                unreachable!("行级游标走 row_page")
            }
        };
        let lo = if has_more {
            // 往后翻到头、这一页是空的：没有属于这一页的行（游标之前的归游标所在的那一页）
            page.first().map_or(NO_UPPER, |(_, s)| *s)
        } else {
            0
        };
        let hi = next_seq.unwrap_or(NO_UPPER);
        let mut messages = Vec::with_capacity(page.len());
        for (snapshot, seq) in &page {
            messages.push(self.message_row(snapshot, *seq)?);
        }
        let mut turn_ids: BTreeSet<String> = messages
            .iter()
            .filter_map(|m| m.message.turn_id.map(|t| t.to_string()))
            .collect();
        for id in rows(
            self.conn,
            "SELECT id FROM turns WHERE conversation_id = ?1 AND \
             ((created_seq >= ?2 AND created_seq < ?3) OR \
              (ended_seq != 0 AND ended_seq >= ?2 AND ended_seq < ?3))",
            params![conversation, lo, hi],
            |r| r.get::<_, String>(0),
        )? {
            turn_ids.insert(id);
        }
        let tool_keys: Vec<(String, String)> = rows(
            self.conn,
            "SELECT turn_id, tool_call_id FROM tool_calls WHERE conversation_id = ?1 \
             AND started_seq >= ?2 AND started_seq < ?3 ORDER BY started_seq",
            params![conversation, lo, hi],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let approval_ids: Vec<String> = rows(
            self.conn,
            "SELECT id FROM approvals WHERE conversation_id = ?1 \
             AND requested_seq >= ?2 AND requested_seq < ?3 ORDER BY requested_seq",
            params![conversation, lo, hi],
            |r| r.get(0),
        )?;
        let card_ids: Vec<String> = rows(
            self.conn,
            "SELECT id FROM user_actions WHERE conversation_id = ?1 \
             AND proposed_seq >= ?2 AND proposed_seq < ?3 ORDER BY proposed_seq",
            params![conversation, lo, hi],
            |r| r.get(0),
        )?;
        Ok(Ok(self.assemble(
            conversation,
            messages,
            turn_ids,
            &tool_keys,
            &approval_ids,
            &card_ids,
            has_more,
            has_newer,
        )?))
    }

    /// 行级游标的一页（T09 方案 2.9「行级游标的语义」）：五路各按排序字段取紧挨游标的 `limit + 1` 行，归并后取
    /// `limit` 行。回合结束行和客户端同一条件：结束了（`ended_seq` 不是 0）并且到了终态（不在跑、不是中断）。
    fn row_page(
        &self,
        conversation: &str,
        side: RowSide,
        cursor: Seq,
        limit: u32,
    ) -> Result<PageBody, StorageError> {
        let s = seq_i64(cursor);
        let n = limit + 1;
        let mut candidates: Vec<(i64, RowRef)> = Vec::new();
        for lane in lanes() {
            let sql = lane.select(side);
            let (ending_in, ending_states) = kinds_where(TurnState::is_terminal);
            let found: Vec<(i64, RowRef)> = if lane.turn_end {
                let mut args: Vec<rusqlite::types::Value> =
                    vec![conversation.to_owned().into(), s.into()];
                args.extend(ending_states.iter().map(|t| (*t).to_owned().into()));
                args.push(i64::from(n).into());
                rows(
                    self.conn,
                    &sql.replace("{ending}", &ending_in),
                    params_from_iter(args),
                    |r| Ok((r.get(0)?, (lane.make)(r)?)),
                )?
            } else {
                rows(self.conn, &sql, params![conversation, s, n], |r| {
                    Ok((r.get(0)?, (lane.make)(r)?))
                })?
            };
            candidates.extend(found);
        }
        // 紧挨游标的在前
        match side {
            RowSide::Before => candidates.sort_by_key(|c| std::cmp::Reverse(c.0)),
            RowSide::After => candidates.sort_by_key(|c| c.0),
        }
        let beyond = candidates.len() > limit as usize;
        candidates.truncate(limit as usize);
        // 结果不空时另一侧从游标算起（游标那一行本身在另一侧）；结果为空时按严格比较（T09 方案 2.9「两个布尔值」）
        let behind = self.any_row_on_other_side(conversation, side, s, candidates.is_empty())?;
        let (has_more, has_newer) = match side {
            RowSide::Before => (beyond, behind),
            RowSide::After => (behind, beyond),
        };
        candidates.sort_by_key(|c| c.0);

        let mut messages = Vec::new();
        let mut turn_ids = BTreeSet::new();
        let mut tool_keys = Vec::new();
        let mut approval_ids = Vec::new();
        let mut card_ids = Vec::new();
        for (seq, row) in candidates {
            match row {
                RowRef::Message(id) => {
                    let snapshot: Vec<u8> = self
                        .conn
                        .prepare_cached("SELECT snapshot FROM messages WHERE id = ?1")
                        .and_then(|mut st| st.query_row([&id], |r| r.get(0)))
                        .map_err(sql_err)?;
                    let m = self.message_row(&snapshot, seq)?;
                    turn_ids.extend(m.message.turn_id.map(|t| t.to_string()));
                    messages.push(m);
                }
                RowRef::ToolCall(turn, call) => {
                    turn_ids.insert(turn.clone());
                    tool_keys.push((turn, call));
                }
                // 选中行所属的回合随页带上（T09 方案 2.9「结果里放什么」）
                RowRef::Approval(id, turn) => {
                    turn_ids.insert(turn);
                    approval_ids.push(id);
                }
                RowRef::UserAction(id, turn) => {
                    turn_ids.insert(turn);
                    card_ids.push(id);
                }
                RowRef::TurnEnd(id) => {
                    turn_ids.insert(id);
                }
            }
        }
        self.assemble(
            conversation,
            messages,
            turn_ids,
            &tool_keys,
            &approval_ids,
            &card_ids,
            has_more,
            has_newer,
        )
    }

    /// 游标另一侧（`Before` 时比游标新、`After` 时比游标旧）还有没有任何一类行。`strict`：游标那一行本身算不算
    /// 在另一侧——结果不空时算（它不在结果里，在结果的另一侧），结果为空时不算（T09 方案 2.9「两个布尔值」）。
    fn any_row_on_other_side(
        &self,
        conversation: &str,
        side: RowSide,
        s: i64,
        strict: bool,
    ) -> Result<bool, StorageError> {
        let cmp = match (side, strict) {
            (RowSide::Before, false) => ">=",
            (RowSide::Before, true) => ">",
            (RowSide::After, false) => "<=",
            (RowSide::After, true) => "<",
        };
        let (ending_in, ending_states) = kinds_where(TurnState::is_terminal);
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE conversation_id = ?1 AND appended_seq {cmp} ?2) \
             OR EXISTS (SELECT 1 FROM tool_calls WHERE conversation_id = ?1 AND started_seq {cmp} ?2) \
             OR EXISTS (SELECT 1 FROM approvals WHERE conversation_id = ?1 AND requested_seq {cmp} ?2) \
             OR EXISTS (SELECT 1 FROM user_actions WHERE conversation_id = ?1 AND proposed_seq {cmp} ?2) \
             OR EXISTS (SELECT 1 FROM turns WHERE conversation_id = ?1 AND ended_seq != 0 \
                        AND ended_seq {cmp} ?2 AND state IN ({ending_in}))"
        );
        let mut args: Vec<rusqlite::types::Value> = vec![conversation.to_owned().into(), s.into()];
        args.extend(ending_states.iter().map(|t| (*t).to_owned().into()));
        self.conn
            .prepare_cached(&sql)
            .and_then(|mut st| st.query_row(params_from_iter(args), |r| r.get(0)))
            .map_err(sql_err)
    }

    /// 选好的行 → 一页：回合、工具调用、审批、卡片按排序字段升序；附件取本页消息的。
    #[allow(
        clippy::too_many_arguments,
        reason = "一页的五路拼在一起，各路各自的列表"
    )]
    /// 选好的行 → 一页。`turns` 在调用方给的（消息引用的、结束行被选中的、范围里开始或结束的）之外，再补两类：
    /// 页里工具、审批、卡片所属的回合（行要读它）；这个对话所有还没到终态的回合（之后它们的状态变化、工具、
    /// 结束事件到了，客户端要在 `turns` 里找得到，否则丢掉——审查 2026-11-13-m1-t10a-d7 1/F-1）。
    fn assemble(
        &self,
        conversation: &str,
        messages: Vec<MessageRow>,
        mut turn_ids: BTreeSet<String>,
        tool_keys: &[(String, String)],
        approval_ids: &[String],
        card_ids: &[String],
        has_more: bool,
        has_newer: bool,
    ) -> Result<PageBody, StorageError> {
        let mut tool_calls = Vec::with_capacity(tool_keys.len());
        for (turn, call) in tool_keys {
            tool_calls.push(self.tool_call_row(turn, call)?);
        }
        tool_calls.sort_by_key(|t| t.created_seq);
        let mut approvals = Vec::with_capacity(approval_ids.len());
        for id in approval_ids {
            approvals.push(self.approval_row(id)?);
        }
        approvals.sort_by_key(|a| a.created_seq);
        let mut user_actions = Vec::with_capacity(card_ids.len());
        for id in card_ids {
            user_actions.push(self.user_action_row(id)?);
        }
        user_actions.sort_by_key(|c| c.created_seq);
        turn_ids.extend(tool_calls.iter().map(|c| c.turn_id.to_string()));
        turn_ids.extend(approvals.iter().map(|a| a.approval.turn_id.to_string()));
        turn_ids.extend(user_actions.iter().map(|c| c.card.turn_id.to_string()));
        let (open_in, open_states) = kinds_where(|t: TurnState| !t.is_terminal());
        let mut args: Vec<rusqlite::types::Value> = vec![conversation.to_owned().into()];
        args.extend(open_states.iter().map(|t| (*t).to_owned().into()));
        turn_ids.extend(rows(
            self.conn,
            &format!("SELECT id FROM turns WHERE conversation_id = ? AND state IN ({open_in})"),
            params_from_iter(args),
            |r| r.get::<_, String>(0),
        )?);
        let mut turns = Vec::with_capacity(turn_ids.len());
        for id in &turn_ids {
            turns.push(self.turn_row(id)?);
        }
        turns.sort_by_key(|t| t.created_seq);
        let attachment_ids: Vec<AttachmentId> = messages
            .iter()
            .flat_map(|m| m.message.attachments.iter().copied())
            .collect();
        let mut attachments = Vec::with_capacity(attachment_ids.len());
        for id in attachment_ids {
            let s: Option<Vec<u8>> = self
                .conn
                .prepare_cached("SELECT snapshot FROM attachments WHERE id = ?1")
                .and_then(|mut st| st.query_row([id.to_string()], |r| r.get(0)).optional())
                .map_err(sql_err)?;
            // 附件随对话一起删；消息引用的附件不在只可能是投影坏了
            let s = s.ok_or_else(|| {
                crate::sql::bad_column("messages", "attachments", format!("附件 {id} 不在"))
            })?;
            attachments.push(decode_attachment(self.codec, &s)?);
        }
        Ok(PageBody {
            messages,
            turns,
            tool_calls,
            approvals,
            user_actions,
            attachments,
            has_more,
            has_newer,
        })
    }

    fn message_row(&self, snapshot: &[u8], seq: i64) -> Result<MessageRow, StorageError> {
        let mut message = decode_message(self.codec, snapshot)?;
        fill_streaming_body(self.conn, &mut message)?;
        Ok(MessageRow {
            message,
            created_seq: seq_of("messages", "appended_seq", seq)?,
        })
    }

    /// 按 seq 读一条事件。
    fn event_at(&self, seq: i64) -> Result<DomainEvent, StorageError> {
        let (kind, payload): (String, Vec<u8>) = self
            .conn
            .prepare_cached("SELECT type, payload FROM events WHERE seq = ?1")
            .and_then(|mut st| st.query_row([seq], |r| Ok((r.get(0)?, r.get(1)?))))
            .map_err(sql_err)?;
        Ok(self.codec.decode(&kind, &payload)?)
    }

    /// 一条事件的信封时间。
    fn event_time(&self, seq: Seq) -> Result<iwork_domain::common::Timestamp, StorageError> {
        let at: i64 = self
            .conn
            .prepare_cached("SELECT at FROM events WHERE seq = ?1")
            .and_then(|mut st| st.query_row([seq_i64(seq)], |r| r.get(0)))
            .map_err(sql_err)?;
        Ok(iwork_domain::common::Timestamp(at))
    }

    fn turn_row(&self, id: &str) -> Result<TurnRow, StorageError> {
        let (snapshot, created, ended): (Vec<u8>, i64, i64) = self
            .conn
            .prepare_cached("SELECT snapshot, created_seq, ended_seq FROM turns WHERE id = ?1")
            .and_then(|mut st| st.query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))
            .map_err(sql_err)?;
        let turn = decode_turn(self.codec, &snapshot)?;
        let end = if ended == 0 {
            None
        } else {
            match self.event_at(ended)? {
                DomainEvent::TurnFinished(f) => Some(TurnEnd {
                    seq: seq_of("turns", "ended_seq", ended)?,
                    usage: f.usage,
                    celebrate: f.celebrate,
                    change_summary: f.change_summary,
                }),
                other => {
                    return Err(crate::sql::bad_column(
                        "turns",
                        "ended_seq",
                        format!("{id} 的 ended_seq 指向 {}", other.kind()),
                    ));
                }
            }
        };
        Ok(TurnRow {
            state_reason: self.state_reason(&turn)?,
            created_seq: seq_of("turns", "created_seq", created)?,
            turn,
            end,
        })
    }

    /// 最近一条 `TurnStateChanged` 的说明。只给停下来的回合取（终态与中断）：这时最近一条状态变化就在回合流的末尾，
    /// 按 `events_by_stream` 倒着读几条就到；还在跑的回合没有说明，不去翻它前面的几千条工具调用事件。
    fn state_reason(&self, turn: &Turn) -> Result<Option<String>, StorageError> {
        if !(turn.state.is_terminal() || matches!(turn.state, TurnState::Interrupted { .. })) {
            return Ok(None);
        }
        let found: Option<(String, Vec<u8>)> = self
            .conn
            .prepare_cached(
                "SELECT type, payload FROM events WHERE stream_kind = ?1 AND stream_id = ?2 \
                 AND type = 'turn_state_changed' ORDER BY seq DESC LIMIT 1",
            )
            .and_then(|mut st| {
                st.query_row(
                    params![StreamKind::Turn.to_text(), turn.id.to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
            })
            .map_err(sql_err)?;
        Ok(match found {
            Some((kind, payload)) => match self.codec.decode(&kind, &payload)? {
                DomainEvent::TurnStateChanged(c) => c.reason,
                _ => None,
            },
            None => None,
        })
    }

    fn tool_call_row(&self, turn: &str, call: &str) -> Result<ToolCallRow, StorageError> {
        type Row = (
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<i64>,
            i64,
        );
        let (ok, sha, area, size, started): Row = self
            .conn
            .prepare_cached(
                "SELECT ok, output_sha256, output_area, output_size, started_seq FROM tool_calls \
                 WHERE turn_id = ?1 AND tool_call_id = ?2",
            )
            .and_then(|mut st| {
                st.query_row([turn, call], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
            })
            .map_err(sql_err)?;
        let DomainEvent::ToolCallStarted(e) = self.event_at(started)? else {
            return Err(crate::sql::bad_column(
                "tool_calls",
                "started_seq",
                format!("{turn}/{call} 的 started_seq 不是 ToolCallStarted"),
            ));
        };
        const T: &str = "tool_calls";
        let output_artifact = match (sha, area, size) {
            (Some(sha), Some(area), Some(size)) => Some(iwork_domain::common::ArtifactRef {
                sha256: crate::sql::sha_col(T, "output_sha256", &sha)?,
                area: crate::sql::enum_col(T, "output_area", &area)?,
                size: u64_col(T, "output_size", size)?,
            }),
            _ => None,
        };
        Ok(ToolCallRow {
            turn_id: e.turn_id,
            tool_call_id: e.tool_call_id,
            tool_name: e.tool_name,
            action: e.action,
            ok: ok.map(|v| v != 0),
            output_artifact,
            created_seq: seq_of(T, "started_seq", started)?,
        })
    }

    fn approval_row(&self, id: &str) -> Result<ApprovalRow, StorageError> {
        let (row, requested, decided): (ApprovalColumns, i64, Option<i64>) = self
            .conn
            .prepare_cached(
                "SELECT id, snapshot, state, decider, void_reason, requested_seq, decided_at \
                 FROM approvals WHERE id = ?1",
            )
            .and_then(|mut st| st.query_row([id], |r| Ok((approval_row(r)?, r.get(5)?, r.get(6)?))))
            .map_err(sql_err)?;
        Ok(ApprovalRow {
            approval: decode_approval_row(self.codec, row)?,
            created_seq: seq_of("approvals", "requested_seq", requested)?,
            decided_at: decided.map(iwork_domain::common::Timestamp),
        })
    }

    fn user_action_row(&self, id: &str) -> Result<UserActionRow, StorageError> {
        let (snapshot, proposed): (Vec<u8>, i64) = self
            .conn
            .prepare_cached("SELECT snapshot, proposed_seq FROM user_actions WHERE id = ?1")
            .and_then(|mut st| st.query_row([id], |r| Ok((r.get(0)?, r.get(1)?))))
            .map_err(sql_err)?;
        Ok(UserActionRow {
            card: decode_card(self.codec, &snapshot)?,
            created_seq: seq_of("user_actions", "proposed_seq", proposed)?,
        })
    }

    /// 整个对话的累计用量；一个结束的回合都没有时为 `None`。
    /// 本对话用量：已结束的回合各自的用量相加。事件里的用量是回合累计（续接接着加），`turn_usage` 按回合取最后
    /// 一条；回合结束时结束事件的累计覆盖中断时 `UsageReported` 记下的。没结束的回合不算：客户端只在结束事件上
    /// 加用量，快照里先算进去，结束时就会再加一次（审查 2026-11-13-m1-t10a-d7 1/F-3）。
    fn usage_total(
        &self,
        conversation: &str,
    ) -> Result<Option<iwork_domain::common::Usage>, StorageError> {
        type Row = (i64, i64, i64, i64, i64, Option<f64>);
        let (n, input, output, read, write, cost): Row = self
            .conn
            .prepare_cached(
                "SELECT COUNT(*), COALESCE(SUM(u.input_tokens), 0), COALESCE(SUM(u.output_tokens), 0), \
                 COALESCE(SUM(u.cache_read_tokens), 0), COALESCE(SUM(u.cache_write_tokens), 0), \
                 SUM(u.cost_usd) FROM turn_usage u JOIN turns t ON t.id = u.turn_id \
                 WHERE u.conversation_id = ?1 AND t.ended_seq != 0",
            )
            .and_then(|mut st| {
                st.query_row([conversation], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })
            })
            .map_err(sql_err)?;
        if n == 0 {
            return Ok(None);
        }
        const T: &str = "turn_usage";
        Ok(Some(iwork_domain::common::Usage {
            input_tokens: u64_col(T, "input_tokens", input)?,
            output_tokens: u64_col(T, "output_tokens", output)?,
            cache_read_tokens: u64_col(T, "cache_read_tokens", read)?,
            cache_write_tokens: u64_col(T, "cache_write_tokens", write)?,
            cost_usd: cost,
        }))
    }

    // ---------- 诊断 ----------

    /// 这个对话里每一次会话替换，按 seq 升序（诊断用，任务卡范围 2）。
    pub fn session_replacements(
        &self,
        conversation: ConversationId,
    ) -> Result<Vec<iwork_ports::read_model::SessionReplacement>, StorageError> {
        crate::host_queries::session_replacements(self.conn, self.codec, conversation)
    }

    // ---------- 待处理 ----------

    /// 「待你处理」的全部项，按在时间线上的位置升序（同位置按种类、id）。哪些算待处理和
    /// `ReadModelPort::pending_items` 同一组条件。
    pub fn pending(&self) -> Result<Vec<PendingEntry>, StorageError> {
        self.pending_in(None)
    }

    /// `conversation` 给定时只取这个对话的（草稿卡片没有对话，不在里面）。
    fn pending_in(&self, conversation: Option<&str>) -> Result<Vec<PendingEntry>, StorageError> {
        let filter = if conversation.is_some() {
            " AND conversation_id = ?1"
        } else {
            ""
        };
        let args: Vec<&str> = conversation.into_iter().collect();
        let mut out = Vec::new();
        for (row, seq) in rows(
            self.conn,
            &format!(
                "SELECT id, snapshot, state, decider, void_reason, requested_seq FROM approvals \
                 WHERE awaits_user = 1{filter}"
            ),
            params_from_iter(&args),
            |r| Ok((approval_row(r)?, r.get::<_, i64>(5)?)),
        )? {
            out.push(PendingEntry {
                item: PendingItem::Approval(decode_approval_row(self.codec, row)?),
                created_seq: seq_of("approvals", "requested_seq", seq)?,
            });
        }
        let (card_in, card_kinds) = kinds_where(|k: UserActionStateKind| {
            matches!(
                k,
                UserActionStateKind::Pending | UserActionStateKind::Unknown
            )
        });
        let mut card_args: Vec<&str> = args.clone();
        card_args.extend(card_kinds.iter().copied());
        let card_sql = if conversation.is_some() {
            format!(
                "SELECT snapshot, proposed_seq FROM user_actions WHERE conversation_id = ? \
                 AND state_kind IN ({card_in})"
            )
        } else {
            format!(
                "SELECT snapshot, proposed_seq FROM user_actions WHERE state_kind IN ({card_in})"
            )
        };
        for (s, seq) in rows(self.conn, &card_sql, params_from_iter(&card_args), |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
        })? {
            out.push(PendingEntry {
                item: PendingItem::UserAction(decode_card(self.codec, &s)?),
                created_seq: seq_of("user_actions", "proposed_seq", seq)?,
            });
        }
        if conversation.is_none() {
            for (draft, seq) in crate::read_model::pending_drafts(self.conn)? {
                out.push(PendingEntry {
                    item: PendingItem::AgentsMdDraft(draft),
                    created_seq: seq_of("agents_md_drafts", "updated_seq", seq)?,
                });
            }
        }
        for (s, seq) in rows(
            self.conn,
            &format!(
                "SELECT snapshot, interrupted_seq FROM turns WHERE state LIKE 'interrupted_%'{filter}"
            ),
            params_from_iter(&args),
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Option<i64>>(1)?)),
        )? {
            let t = decode_turn(self.codec, &s)?;
            let TurnState::Interrupted { reason } = t.state else {
                return Err(crate::sql::bad_column(
                    "turns",
                    "state",
                    format!("{} 列与快照不符", t.id),
                ));
            };
            let seq = seq.ok_or_else(|| {
                crate::sql::bad_column(
                    "turns",
                    "interrupted_seq",
                    format!("中断的回合 {} 没有", t.id),
                )
            })?;
            out.push(PendingEntry {
                item: PendingItem::Resume(ResumeCard {
                    turn_id: t.id,
                    conversation_id: t.conversation_id,
                    reason,
                }),
                created_seq: seq_of("turns", "interrupted_seq", seq)?,
            });
        }
        out.sort_by_key(|e| e.created_seq);
        Ok(out)
    }

    // ---------- 锚点 ----------

    /// `ListAnchors`（T09 方案 2.9、G10）：用户消息、等你处理的事、失败的回合按排序字段归并，以 `around` 为中心
    /// 前后各取若干条。两个计数算整个对话的。对话不存在时拒绝。
    pub fn anchors(&self, id: ConversationId, window: AnchorWindow) -> Viewed<Anchors> {
        let key = id.to_string();
        let exists: bool = self
            .conn
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM conversations WHERE id = ?1)")
            .and_then(|mut st| st.query_row([&key], |r| r.get(0)))
            .map_err(sql_err)?;
        if !exists {
            return Ok(Err(ViewRefused::ConversationNotFound(id)));
        }
        // 等你处理与失败的回合在一个对话里不多：全取，计数也要全量
        let mut small: Vec<Anchor> = Vec::new();
        for e in self.pending_in(Some(&key))? {
            let pending_item_id = match &e.item {
                PendingItem::Approval(a) => a.id.to_string(),
                PendingItem::UserAction(c) => c.id.to_string(),
                PendingItem::Resume(r) => r.turn_id.to_string(),
                PendingItem::AgentsMdDraft(d) => d.card_id.to_string(),
            };
            small.push(Anchor {
                created_seq: e.created_seq,
                at: self.event_time(e.created_seq)?,
                kind: AnchorKind::Waiting { pending_item_id },
            });
        }
        let waiting_count = small.len();
        for (snapshot, created, ended) in rows(
            self.conn,
            "SELECT snapshot, created_seq, ended_seq FROM turns WHERE conversation_id = ?1 AND state = ?2",
            params![key, TurnState::Failed.to_text()],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )? {
            let turn = decode_turn(self.codec, &snapshot)?;
            let seq = seq_of(
                "turns",
                "ended_seq",
                if ended != 0 { ended } else { created },
            )?;
            small.push(Anchor {
                created_seq: seq,
                at: self.event_time(seq)?,
                kind: AnchorKind::FailedTurn {
                    turn_id: turn.id,
                    reason: self.state_reason(&turn)?.unwrap_or_default(),
                    had_output: self.had_output(&key, &turn, created, ended)?,
                },
            });
        }
        let failed_count = small.len() - waiting_count;

        let around = window.around.map_or(NO_UPPER, seq_i64);
        let user = author_kind(&Author::User);
        let before_users = rows(
            self.conn,
            "SELECT snapshot, appended_seq FROM messages WHERE conversation_id = ?1 \
             AND author_kind = ?2 AND appended_seq <= ?3 ORDER BY appended_seq DESC LIMIT ?4",
            params![key, user, around, window.before + 1],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
        )?;
        let after_users = rows(
            self.conn,
            "SELECT snapshot, appended_seq FROM messages WHERE conversation_id = ?1 \
             AND author_kind = ?2 AND appended_seq > ?3 ORDER BY appended_seq ASC LIMIT ?4",
            params![key, user, around, window.after + 1],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
        )?;
        let mut all: Vec<Anchor> = small;
        for (snapshot, seq) in before_users.iter().chain(after_users.iter()) {
            let m = decode_message(self.codec, snapshot)?;
            let seq = seq_of("messages", "appended_seq", *seq)?;
            all.push(Anchor {
                created_seq: seq,
                at: self.event_time(seq)?,
                kind: AnchorKind::UserMessage {
                    message_id: m.id,
                    text: match m.body {
                        MessageBody::Inline(t) => t,
                        MessageBody::Artifact(_) => String::new(),
                    },
                },
            });
        }
        all.sort_by_key(|a| a.created_seq);
        // 用户消息只取了窗口两侧各 n + 1 条，两侧再往外的不在 all 里；它们只影响 has_before / has_after，
        // 由取到的条数是否多于窗口判断（多出来的那一条就是「还有」的证据）
        let pivot = all.partition_point(|a| seq_i64(a.created_seq) <= around);
        let start = pivot.saturating_sub(window.before as usize);
        let end = (pivot + window.after as usize).min(all.len());
        Ok(Ok(Anchors {
            has_before: start > 0,
            has_after: end < all.len(),
            anchors: all[start..end].to_vec(),
            waiting_count: u32::try_from(waiting_count).unwrap_or(u32::MAX),
            failed_count: u32::try_from(failed_count).unwrap_or(u32::MAX),
        }))
    }

    /// 失败的回合失败前有没有输出过内容：它名下有没有非空的助手消息。只扫这个回合存续的那一段消息。
    fn had_output(
        &self,
        conversation: &str,
        turn: &Turn,
        created: i64,
        ended: i64,
    ) -> Result<bool, StorageError> {
        let upper = if ended == 0 { NO_UPPER } else { ended };
        for s in blobs(
            self.conn,
            "SELECT snapshot FROM messages WHERE conversation_id = ?1 AND appended_seq >= ?2 \
             AND appended_seq <= ?3 AND turn_id = ?4",
            params![conversation, created, upper, turn.id.to_string()],
        )? {
            let mut m: Message = decode_message(self.codec, &s)?;
            fill_streaming_body(self.conn, &mut m)?;
            let spoke = match &m.body {
                MessageBody::Inline(t) => !t.is_empty(),
                MessageBody::Artifact(_) => true,
            };
            if spoke {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// 行级游标往哪边取。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowSide {
    Before,
    After,
}

/// 行级游标的一路：表、排序字段、怎么造出行的身份。
struct Lane {
    /// `{cmp}`、`{order}` 由 [`Lane::select`] 换掉；回合结束那一路另有 `{ending}`
    sql: &'static str,
    turn_end: bool,
    make: fn(&rusqlite::Row<'_>) -> rusqlite::Result<RowRef>,
}

impl Lane {
    fn select(&self, side: RowSide) -> String {
        let (cmp, order) = match side {
            RowSide::Before => ("<", "DESC"),
            RowSide::After => (">", "ASC"),
        };
        self.sql.replace("{cmp}", cmp).replace("{order}", order)
    }
}

/// 五路（T09 方案第 3 节要点 ②）：加一种行 = 加一项，各自走自己的（对话，排序字段）索引。
fn lanes() -> [Lane; 5] {
    [
        Lane {
            sql: "SELECT appended_seq, id FROM messages WHERE conversation_id = ?1 \
                  AND appended_seq {cmp} ?2 ORDER BY appended_seq {order} LIMIT ?3",
            turn_end: false,
            make: |r| Ok(RowRef::Message(r.get(1)?)),
        },
        Lane {
            sql: "SELECT started_seq, turn_id, tool_call_id FROM tool_calls WHERE conversation_id = ?1 \
                  AND started_seq {cmp} ?2 ORDER BY started_seq {order} LIMIT ?3",
            turn_end: false,
            make: |r| Ok(RowRef::ToolCall(r.get(1)?, r.get(2)?)),
        },
        Lane {
            sql: "SELECT requested_seq, id, turn_id FROM approvals WHERE conversation_id = ?1 \
                  AND requested_seq {cmp} ?2 ORDER BY requested_seq {order} LIMIT ?3",
            turn_end: false,
            make: |r| Ok(RowRef::Approval(r.get(1)?, r.get(2)?)),
        },
        Lane {
            sql: "SELECT proposed_seq, id, turn_id FROM user_actions WHERE conversation_id = ?1 \
                  AND proposed_seq {cmp} ?2 ORDER BY proposed_seq {order} LIMIT ?3",
            turn_end: false,
            make: |r| Ok(RowRef::UserAction(r.get(1)?, r.get(2)?)),
        },
        // 回合结束行：结束了、到了终态（中断的回合显示继续卡片，没有结束行）
        Lane {
            sql: "SELECT ended_seq, id FROM turns WHERE conversation_id = ? AND ended_seq != 0 \
                  AND ended_seq {cmp} ? AND state IN ({ending}) ORDER BY ended_seq {order} LIMIT ?",
            turn_end: true,
            make: |r| Ok(RowRef::TurnEnd(r.get(1)?)),
        },
    ]
}

fn unread_u32(v: i64) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(u32::MAX)
}
