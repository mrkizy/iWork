//! `SqliteReadModel`：`ReadModelPort` 的 11 个查询（方案 2.7）。全部走读线程池。
//!
//! 实体从快照解出；审批的主状态由三列叠加（方案 2.6）；流式中的消息正文用检查点填（设计 3.5）；
//! 全局设置没设过的字段用注入的默认值补——默认值只在读侧出现，投影不依赖环境。

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use iwork_domain::approval::Approval;
use iwork_domain::assistant::Assistant;
use iwork_domain::common::{ArtifactRef, Timestamp};
use iwork_domain::event::{DomainEvent, StreamKind};
use iwork_domain::id::{ConversationId, ProjectId, TurnId};
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::rule::RuleEntry;
use iwork_domain::runtime::{QuotaWindow, Runtime};
use iwork_domain::settings::GlobalSettings;
use iwork_domain::turn::{Turn, TurnKind, TurnState};
use iwork_domain::user_action::{UserActionStateKind, UserActionTriggerKind, next_kind};
use iwork_ports::read_model::{
    AgentsMdDraftCard, ConversationSummary, ConversationView, MessagePage, PendingItem,
    ProjectSummary, QuitCounts, ReadModelPort, ResumeCard, RuntimeQuota, SnapshotInputs,
    SummaryLine, SummarySource, SummaryText, SummaryWho, TurnWork, Unsettled,
};
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::projector::messages::author_kind;
use crate::reader::ReaderPool;
use crate::snapshot::{
    approval_state_from_columns, decode_approval, decode_assistant, decode_card,
    decode_conversation, decode_message, decode_project, decode_rule, decode_runtime, decode_turn,
};
use crate::sql::{ColumnText, enum_col, id_col, sha_col, sha_text, sql_err, u64_col};
use crate::views::ViewTx;

/// 读侧补的默认值（依赖环境，如用户主目录）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadDefaults {
    pub global_settings: GlobalSettings,
}

/// 查询模型。与 `SqliteStore` 共用读线程池与 codec。
pub struct SqliteReadModel<C: EventCodec> {
    readers: Arc<ReaderPool>,
    codec: Arc<C>,
    defaults: Arc<ReadDefaults>,
}

impl<C: EventCodec + 'static> SqliteReadModel<C> {
    pub(crate) fn new(readers: Arc<ReaderPool>, codec: Arc<C>, defaults: ReadDefaults) -> Self {
        Self {
            readers,
            codec,
            defaults: Arc::new(defaults),
        }
    }

    /// 在**一个**读事务里跑 `f`（T10a 方案 Q1、S1）：视图查询与订阅快照用。
    pub fn view<T, F>(
        &self,
        f: F,
    ) -> impl Future<Output = Result<T, StorageError>> + Send + use<T, F, C>
    where
        T: Send + 'static,
        F: FnOnce(&ViewTx<'_, C>) -> Result<T, StorageError> + Send + 'static,
    {
        let defaults = self.defaults.clone();
        self.query(move |conn, codec| {
            f(&ViewTx {
                conn,
                codec,
                defaults: &defaults,
            })
        })
    }

    fn query<T, F>(
        &self,
        f: F,
    ) -> impl Future<Output = Result<T, StorageError>> + Send + use<T, F, C>
    where
        T: Send + 'static,
        F: FnOnce(&Connection, &C) -> Result<T, StorageError> + Send + 'static,
    {
        let codec = self.codec.clone();
        self.readers.run(move |conn| f(conn, codec.as_ref()))
    }
}

/// 跑一条查询、把每行映射出来。
pub(crate) fn rows<T>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    mut map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, StorageError> {
    let mut stmt = conn.prepare_cached(sql).map_err(sql_err)?;
    let out = stmt
        .query_map(params, |r| map(r))
        .map_err(sql_err)?
        .collect::<Result<Vec<T>, _>>()
        .map_err(sql_err)?;
    Ok(out)
}

pub(crate) fn blobs(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Vec<u8>>, StorageError> {
    rows(conn, sql, params, |r| r.get(0))
}

/// 三张安全清单的全部条目。
pub(crate) const SECURITY_LISTS: &str = "SELECT snapshot FROM approval_rules WHERE layer IN \
     ('credential_commands', 'sensitive_dirs', 'writable_dirs') ORDER BY id";

/// 最新一页（无游标）。
const PAGE_LATEST: &str =
    "SELECT snapshot FROM messages WHERE conversation_id = ?1 ORDER BY id DESC LIMIT ?2";
/// 游标之前的一页。
const PAGE_BEFORE: &str = "SELECT snapshot FROM messages WHERE conversation_id = ?1 AND id < ?2 \
                           ORDER BY id DESC LIMIT ?3";

/// 一页消息；流式中的消息正文用检查点填，没有检查点就是空串。
fn message_page<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
    conversation: &str,
    page: MessagePage,
) -> Result<Vec<Message>, StorageError> {
    // 有无游标拆成两条语句：`? IS NULL OR id < ?` 会让规划器用不上 id 的范围约束（方案 2.7）
    let snapshots = match page.before {
        None => blobs(conn, PAGE_LATEST, params![conversation, page.limit])?,
        Some(before) => blobs(
            conn,
            PAGE_BEFORE,
            params![conversation, before.to_string(), page.limit],
        )?,
    };
    let mut out = Vec::with_capacity(snapshots.len());
    for s in snapshots.iter().rev() {
        let mut m = decode_message(codec, s)?;
        fill_streaming_body(conn, &mut m)?;
        out.push(m);
    }
    Ok(out)
}

/// 流式中的消息正文用检查点填，没有检查点就是空串；别的消息不动。
pub(crate) fn fill_streaming_body(conn: &Connection, m: &mut Message) -> Result<(), StorageError> {
    if m.status == MessageStatus::Streaming {
        let text: Option<String> = conn
            .prepare_cached("SELECT text FROM message_checkpoints WHERE message_id = ?1")
            .and_then(|mut st| st.query_row([m.id.to_string()], |r| r.get(0)).optional())
            .map_err(sql_err)?;
        m.body = MessageBody::Inline(text.unwrap_or_default());
    }
    Ok(())
}

/// 列值 `IN (…)` 的占位符与参数：从登记表里挑出满足领域谓词的变体，不在 SQL 里手写状态名。
pub(crate) fn kinds_where<K: ColumnText>(keep: impl Fn(K) -> bool) -> (String, Vec<&'static str>) {
    let texts: Vec<&'static str> = K::ALL
        .iter()
        .copied()
        .filter(|k| keep(*k))
        .map(ColumnText::to_text)
        .collect();
    (vec!["?"; texts.len()].join(", "), texts)
}

/// 这一页消息涉及的回合，加上该对话还没结束的回合，按 id 升序。
fn turns_for<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
    conversation: &str,
    messages: &[Message],
) -> Result<Vec<Turn>, StorageError> {
    let mut ids: Vec<String> = messages
        .iter()
        .filter_map(|m| m.turn_id.map(|t| t.to_string()))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let terminal: Vec<&str> = [
        TurnState::Completed,
        TurnState::Cancelled,
        TurnState::Failed,
    ]
    .into_iter()
    .map(TurnState::to_text)
    .collect();
    let in_list = vec!["?"; ids.len()].join(", ");
    let sql = format!(
        "SELECT snapshot FROM turns WHERE conversation_id = ? AND \
         (state NOT IN (?, ?, ?) OR id IN ({in_list})) ORDER BY id"
    );
    let mut args: Vec<String> = vec![conversation.to_owned()];
    args.extend(terminal.iter().map(|s| (*s).to_owned()));
    args.extend(ids);
    blobs(conn, &sql, params_from_iter(args))?
        .iter()
        .map(|s| decode_turn(codec, s))
        .collect()
}

/// 还没决定的 AGENTS.md 草稿卡片，按 id 升序，连同提出它的 seq（没决定前 `updated_seq` 就是提出时的）。
pub(crate) fn pending_drafts(
    conn: &Connection,
) -> Result<Vec<(AgentsMdDraftCard, i64)>, StorageError> {
    type DraftRow = (String, String, String, String, i64, i64, i64);
    let drafts: Vec<DraftRow> = rows(
        conn,
        "SELECT card_id, project_id, content_sha256, content_area, content_size, proposed_at, \
         updated_seq FROM agents_md_drafts WHERE decision IS NULL ORDER BY card_id",
        [],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        },
    )?;
    let mut out = Vec::with_capacity(drafts.len());
    for (card, project, sha, area, size, at, seq) in drafts {
        const T: &str = "agents_md_drafts";
        out.push((
            AgentsMdDraftCard {
                card_id: id_col(T, "card_id", &card)?,
                project_id: id_col(T, "project_id", &project)?,
                content: ArtifactRef {
                    sha256: sha_col(T, "content_sha256", &sha)?,
                    area: enum_col(T, "content_area", &area)?,
                    size: u64_col(T, "content_size", size)?,
                },
                proposed_at: Timestamp(at),
            },
            seq,
        ));
    }
    Ok(out)
}

/// `approvals` 的一行：`(id, snapshot, state, decider, void_reason)`。
pub(crate) type ApprovalColumns = (String, Vec<u8>, String, Option<String>, Option<String>);

pub(crate) fn approval_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalColumns> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}

pub(crate) fn decode_approval_row<C: EventCodec + ?Sized>(
    codec: &C,
    (id, snapshot, state, decider, void_reason): ApprovalColumns,
) -> Result<Approval, StorageError> {
    let state =
        approval_state_from_columns(&id, &state, decider.as_deref(), void_reason.as_deref())?;
    decode_approval(codec, &snapshot, state)
}

impl<C: EventCodec + 'static> ReadModelPort for SqliteReadModel<C> {
    fn conversation(
        &self,
        id: ConversationId,
        page: MessagePage,
    ) -> impl Future<Output = Result<ConversationView, StorageError>> + Send {
        self.query(move |conn, codec| {
            let key = id.to_string();
            let row: Option<(Vec<u8>, i64)> = conn
                .prepare_cached("SELECT snapshot, unread FROM conversations WHERE id = ?1")
                .and_then(|mut s| {
                    s.query_row([&key], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()
                })
                .map_err(sql_err)?;
            let (snapshot, unread) = row.ok_or_else(|| StorageError::NotFound {
                entity: "conversation",
                id: key.clone(),
            })?;
            let messages = message_page(conn, codec, &key, page)?;
            let turns = turns_for(conn, codec, &key, &messages)?;
            Ok(ConversationView {
                conversation: decode_conversation(codec, &snapshot)?,
                unread: unread_u32(unread),
                messages,
                turns,
            })
        })
    }

    fn conversations(
        &self,
        project: Option<ProjectId>,
    ) -> impl Future<Output = Result<Vec<ConversationSummary>, StorageError>> + Send {
        self.query(move |conn, codec| {
            let raw: Vec<(Vec<u8>, i64)> = match project {
                None => rows(
                    conn,
                    "SELECT snapshot, unread FROM conversations ORDER BY updated_at DESC, id DESC",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?,
                Some(p) => rows(
                    conn,
                    "SELECT snapshot, unread FROM conversations WHERE project_id = ?1 \
                     ORDER BY updated_at DESC, id DESC",
                    [p.to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?,
            };
            raw.into_iter()
                .map(|(s, unread)| {
                    Ok(ConversationSummary {
                        conversation: decode_conversation(codec, &s)?,
                        unread: unread_u32(unread),
                    })
                })
                .collect()
        })
    }

    fn projects(&self) -> impl Future<Output = Result<Vec<ProjectSummary>, StorageError>> + Send {
        self.query(|conn, codec| projects_on(conn, codec))
    }

    fn assistants(&self) -> impl Future<Output = Result<Vec<Assistant>, StorageError>> + Send {
        self.query(|conn, codec| assistants_on(conn, codec))
    }

    fn pending_items(&self) -> impl Future<Output = Result<Vec<PendingItem>, StorageError>> + Send {
        self.query(|conn, codec| {
            let mut out = Vec::new();
            for row in rows(
                conn,
                "SELECT id, snapshot, state, decider, void_reason FROM approvals \
                 WHERE awaits_user = 1 ORDER BY id",
                [],
                approval_row,
            )? {
                out.push(PendingItem::Approval(decode_approval_row(codec, row)?));
            }
            // Unknown 也待处理：等用户「我已核对」（T01 4.3）
            for s in blobs(
                conn,
                "SELECT snapshot FROM user_actions WHERE state_kind IN ('pending', 'unknown') ORDER BY id",
                [],
            )? {
                out.push(PendingItem::UserAction(decode_card(codec, &s)?));
            }
            for (draft, _seq) in pending_drafts(conn)? {
                out.push(PendingItem::AgentsMdDraft(draft));
            }
            for s in blobs(
                conn,
                "SELECT snapshot FROM turns WHERE state LIKE 'interrupted_%' ORDER BY id",
                [],
            )? {
                let t = decode_turn(codec, &s)?;
                let TurnState::Interrupted { reason } = t.state else {
                    return Err(crate::sql::bad_column("turns", "state", format!("{} 列与快照不符", t.id)));
                };
                out.push(PendingItem::Resume(ResumeCard {
                    turn_id: t.id,
                    conversation_id: t.conversation_id,
                    reason,
                }));
            }
            Ok(out)
        })
    }

    fn unsettled(&self) -> impl Future<Output = Result<Unsettled, StorageError>> + Send {
        self.query(|conn, codec| {
            let (turn_in, turn_states) = kinds_where(TurnState::holds_process);
            let mut turns = Vec::new();
            for s in blobs(
                conn,
                &format!("SELECT snapshot FROM turns WHERE state IN ({turn_in}) ORDER BY id"),
                params_from_iter(&turn_states),
            )? {
                turns.push(decode_turn(codec, &s)?);
            }
            // 消息表没有按回合的索引：按对话取（有索引），再按回合与状态筛
            let (msg_in, msg_states) = kinds_where(|m: MessageStatus| !m.is_terminal());
            let mut streaming = Vec::new();
            for t in &turns {
                let mut args: Vec<String> = vec![t.conversation_id.to_string(), t.id.to_string()];
                args.extend(msg_states.iter().map(|s| s.to_string()));
                for s in blobs(
                    conn,
                    &format!(
                        "SELECT snapshot FROM messages WHERE conversation_id = ? AND turn_id = ? \
                         AND status IN ({msg_in}) ORDER BY id"
                    ),
                    params_from_iter(&args),
                )? {
                    let mut m = decode_message(codec, &s)?;
                    fill_streaming_body(conn, &mut m)?;
                    streaming.push(m);
                }
            }
            streaming.sort_by_key(|m| m.id);
            let (card_in, card_kinds) = kinds_where(|k: UserActionStateKind| {
                next_kind(k, UserActionTriggerKind::ProcessLost).is_ok()
            });
            let mut cards = Vec::new();
            for s in blobs(
                conn,
                &format!(
                    "SELECT snapshot FROM user_actions WHERE state_kind IN ({card_in}) ORDER BY id"
                ),
                params_from_iter(&card_kinds),
            )? {
                cards.push(decode_card(codec, &s)?);
            }
            let mut recorded_usage = std::collections::BTreeMap::new();
            for t in &turns {
                if let Some(u) = recorded_usage_on(conn, &t.id.to_string())? {
                    recorded_usage.insert(t.id, u);
                }
            }
            Ok(Unsettled {
                turns,
                recorded_usage,
                streaming,
                cards,
            })
        })
    }

    fn quota(&self) -> impl Future<Output = Result<Vec<RuntimeQuota>, StorageError>> + Send {
        self.query(|conn, _codec| quota_on(conn))
    }

    fn runtimes(&self) -> impl Future<Output = Result<Vec<Runtime>, StorageError>> + Send {
        self.query(|conn, codec| runtimes_on(conn, codec))
    }

    /// `Some(p)`：全局规则 + 该项目的规则。更细的范围（助手、对话）由需要的卡再加查询。
    fn rules(
        &self,
        project: Option<ProjectId>,
    ) -> impl Future<Output = Result<Vec<RuleEntry>, StorageError>> + Send {
        self.query(move |conn, codec| rules_on(conn, codec, project))
    }

    fn security_lists(&self) -> impl Future<Output = Result<Vec<RuleEntry>, StorageError>> + Send {
        self.query(|conn, codec| {
            blobs(conn, SECURITY_LISTS, [])?
                .iter()
                .map(|s| decode_rule(codec, s))
                .collect()
        })
    }

    fn settings(&self) -> impl Future<Output = Result<GlobalSettings, StorageError>> + Send {
        let defaults = self.defaults.clone();
        self.query(move |conn, _codec| settings_on(conn, &defaults))
    }

    fn summary_source(
        &self,
        turn: TurnId,
        line: Option<ProjectId>,
        recent_limit: u32,
    ) -> impl Future<Output = Result<SummarySource, StorageError>> + Send {
        self.query(move |conn, codec| summary_source(conn, codec, turn, line, recent_limit))
    }

    fn turn_work(&self) -> impl Future<Output = Result<TurnWork, StorageError>> + Send {
        self.query(|conn, codec| crate::host_queries::turn_work(conn, codec))
    }

    fn last_session_binding(
        &self,
        conversation: ConversationId,
    ) -> impl Future<Output = Result<Option<String>, StorageError>> + Send {
        self.query(move |conn, codec| {
            crate::host_queries::last_session_binding(conn, codec, conversation)
        })
    }

    fn session_ids_of(
        &self,
        conversation: ConversationId,
    ) -> impl Future<Output = Result<Vec<String>, StorageError>> + Send {
        self.query(move |conn, codec| {
            crate::host_queries::session_ids_of(conn, codec, conversation)
        })
    }

    fn quit_counts(&self) -> impl Future<Output = Result<QuitCounts, StorageError>> + Send {
        self.query(|conn, _codec| crate::host_queries::quit_counts(conn))
    }

    fn turn_request(
        &self,
        turn: TurnId,
    ) -> impl Future<Output = Result<Option<Message>, StorageError>> + Send {
        self.query(move |conn, codec| turn_request(conn, codec, turn))
    }

    fn recorded_usage(
        &self,
        turn: TurnId,
    ) -> impl Future<Output = Result<iwork_domain::common::Usage, StorageError>> + Send {
        self.query(move |conn, _codec| {
            Ok(recorded_usage_on(conn, &turn.to_string())?.unwrap_or_default())
        })
    }

    fn turn_with_conversation(
        &self,
        turn: TurnId,
    ) -> impl Future<
        Output = Result<Option<(Turn, iwork_domain::conversation::Conversation)>, StorageError>,
    > + Send {
        self.query(move |conn, codec| {
            crate::host_queries::turn_with_conversation(conn, codec, turn)
        })
    }

    fn snapshot_inputs(
        &self,
        conversation: ConversationId,
    ) -> impl Future<Output = Result<SnapshotInputs, StorageError>> + Send {
        self.query(move |conn, codec| {
            crate::host_queries::snapshot_inputs(conn, codec, conversation)
        })
    }

    fn artifact_is_referenced(
        &self,
        r: &ArtifactRef,
    ) -> impl Future<Output = Result<bool, StorageError>> + Send {
        let (area, sha) = (r.area.to_text(), sha_text(&r.sha256));
        self.query(move |conn, _codec| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM artifact_refs WHERE area = ?1 AND sha256 = ?2)",
                params![area, sha],
                |r| r.get(0),
            )
            .map_err(sql_err)
        })
    }
}

/// 续接摘要的原料（T06 方案 5.2a）。一个读连接里取完：
/// 1. 回合所在的对话；
/// 2. 上界 = 排这个回合的那次提交（`TurnQueued` 是回合流上的第一条事件）的起始 seq。这一轮的请求和
///    `TurnQueued` 同一次提交写下，自然在界外；这一轮之后追加的（被中断那一轮的半截回复、之后别的回合）也在界外。
///    不是由用户消息触发的回合（系统回合）同样有这个界，不靠认出请求；
/// 3. 界内、这条会话线上（`context_project_id == line`）的用户消息与助手回复，按 id 升序扫一遍。
///    这条路只在续接失败时走一次，全扫一个对话的消息可以接受；系统消息与正文为空的不取。
fn summary_source<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
    turn: TurnId,
    line: Option<ProjectId>,
    recent_limit: u32,
) -> Result<SummarySource, StorageError> {
    let turn_key = turn.to_string();
    let conversation: String = conn
        .prepare_cached("SELECT conversation_id FROM turns WHERE id = ?1")
        .and_then(|mut s| s.query_row([&turn_key], |r| r.get(0)).optional())
        .map_err(sql_err)?
        .ok_or_else(|| StorageError::NotFound {
            entity: "turn",
            id: turn_key.clone(),
        })?;
    let (bound, _) = queued_commit(conn, codec, &turn_key, &conversation)?;
    let snapshots = blobs(
        conn,
        "SELECT snapshot FROM messages WHERE conversation_id = ?1 AND author_kind <> ?2 \
         AND appended_seq < ?3 ORDER BY id",
        params![conversation, author_kind(&Author::System), bound],
    )?;
    let mut first_user = None;
    let mut after_first = Vec::new();
    for s in snapshots {
        let mut m = decode_message(codec, &s)?;
        if m.context_project_id != line {
            continue;
        }
        fill_streaming_body(conn, &mut m)?;
        let Some(entry) = summary_line(conn, &m)? else {
            continue;
        };
        // 第一条用户消息之前的（如添加项目时系统回合的草稿）不算这条线上的对话
        match (&first_user, &entry.who) {
            (None, SummaryWho::User) => first_user = Some(entry),
            (None, SummaryWho::Assistant { .. }) => {}
            (Some(_), _) => after_first.push(entry),
        }
    }
    let keep = after_first.len().min(recent_limit as usize);
    let unfetched = u32::try_from(after_first.len() - keep).unwrap_or(u32::MAX);
    let earlier = after_first.into_iter().rev().take(keep).collect();
    Ok(SummarySource {
        first_user,
        earlier,
        unfetched,
    })
}

/// 这一轮的请求：和回合的 `TurnQueued` 同一次提交写下的那条用户消息（受理 `SendMessage` 的形状）。
/// 系统回合没有触发它的用户消息，返回 `None`；回合不存在报 `NotFound`。
fn turn_request<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
    turn: TurnId,
) -> Result<Option<Message>, StorageError> {
    let turn_key = turn.to_string();
    let row: Option<(String, Vec<u8>)> = conn
        .prepare_cached("SELECT conversation_id, snapshot FROM turns WHERE id = ?1")
        .and_then(|mut s| {
            s.query_row([&turn_key], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
        })
        .map_err(sql_err)?;
    let Some((conversation, snapshot)) = row else {
        return Err(StorageError::NotFound {
            entity: "turn",
            id: turn_key,
        });
    };
    if decode_turn(codec, &snapshot)?.kind != TurnKind::Assistant {
        return Ok(None);
    }
    let (first, last) = queued_commit(conn, codec, &turn_key, &conversation)?;
    let snapshots = blobs(
        conn,
        "SELECT snapshot FROM messages WHERE conversation_id = ?1 AND author_kind = ?2 \
         AND appended_seq BETWEEN ?3 AND ?4 ORDER BY id",
        params![conversation, author_kind(&Author::User), first, last],
    )?;
    match snapshots.as_slice() {
        [one] => Ok(Some(decode_message(codec, one)?)),
        _ => Err(crate::sql::bad_column(
            "messages",
            "appended_seq",
            format!(
                "排回合 {turn_key} 的那次提交（seq {first}–{last}）里有 {} 条用户消息，应当恰好一条",
                snapshots.len()
            ),
        )),
    }
}

/// 排这个回合的那次提交的 seq 区间（起、止）。回合流上的第一条事件必须是 `TurnQueued`，不是就报错点名。
///
/// 「这一轮的请求落在界外」靠的是请求和 `TurnQueued` 同一次提交写下（受理 `SendMessage` 的形状，T10a 核对）。
/// 所以用户消息触发的回合（`TurnKind::Assistant`）在这次提交的 seq 区间里必须有这个对话的一条用户消息，
/// 没有就报错点名回合：分两次提交时请求会落在界内、被当成「已完成」收进摘要，不能悄悄放过。系统回合没有触发
/// 它的用户消息，不查。
fn queued_commit<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
    turn_key: &str,
    conversation: &str,
) -> Result<(i64, i64), StorageError> {
    let first: Option<(String, Vec<u8>, i64, i64)> = conn
        .prepare_cached(
            "SELECT e.type, e.payload, c.first_seq, c.last_seq FROM events e \
             JOIN commands c ON c.command_id = e.command_id \
             WHERE e.stream_kind = ?1 AND e.stream_id = ?2 ORDER BY e.seq LIMIT 1",
        )
        .and_then(|mut s| {
            s.query_row(params![StreamKind::Turn.to_text(), turn_key], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()
        })
        .map_err(sql_err)?;
    let Some((kind, payload, first_seq, last_seq)) = first else {
        return Err(crate::sql::bad_column(
            "events",
            "stream_id",
            format!("回合 {turn_key} 在投影里、事件日志里却没有它的流"),
        ));
    };
    let DomainEvent::TurnQueued(queued) = codec.decode(&kind, &payload)? else {
        return Err(crate::sql::bad_column(
            "events",
            "type",
            format!("回合 {turn_key} 流上的第一条事件是 {kind}，不是 turn_queued"),
        ));
    };
    if queued.turn.kind == TurnKind::Assistant {
        let requests: i64 = conn
            .prepare_cached(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND author_kind = ?2 \
                 AND appended_seq BETWEEN ?3 AND ?4",
            )
            .and_then(|mut s| {
                s.query_row(
                    params![
                        conversation,
                        author_kind(&Author::User),
                        first_seq,
                        last_seq
                    ],
                    |r| r.get(0),
                )
            })
            .map_err(sql_err)?;
        if requests == 0 {
            return Err(crate::sql::bad_column(
                "commands",
                "first_seq",
                format!(
                    "回合 {turn_key} 由用户消息触发，排它的那次提交（seq {first_seq}–{last_seq}）里却没有用户消息"
                ),
            ));
        }
    }
    Ok((first_seq, last_seq))
}

/// 一条消息在摘要原料里的样子；系统消息与正文、附件都为空的返回 `None`。附件只取文件名。
fn summary_line(conn: &Connection, m: &Message) -> Result<Option<SummaryLine>, StorageError> {
    let who = match &m.author {
        Author::User => SummaryWho::User,
        Author::Assistant { name_snapshot, .. } => SummaryWho::Assistant {
            name: name_snapshot.clone(),
        },
        Author::System => return Ok(None),
    };
    let text = match &m.body {
        MessageBody::Inline(t) => SummaryText::Inline(t.clone()),
        MessageBody::Artifact(_) => SummaryText::Stored,
    };
    if text == SummaryText::Inline(String::new()) && m.attachments.is_empty() {
        return Ok(None);
    }
    let mut attachments = Vec::with_capacity(m.attachments.len());
    for id in &m.attachments {
        let key = id.to_string();
        let name: String = conn
            .prepare_cached("SELECT name FROM attachments WHERE id = ?1")
            .and_then(|mut s| s.query_row([&key], |r| r.get(0)).optional())
            .map_err(sql_err)?
            .ok_or(StorageError::NotFound {
                entity: "attachment",
                id: key,
            })?;
        attachments.push(name);
    }
    Ok(Some(SummaryLine {
        who,
        text,
        attachments,
    }))
}

/// 一个回合已记下的用量累计（`turn_usage` 里那一行；事件里的用量是回合累计，这一行就是最后一条）。没记过为 `None`。
pub(crate) fn recorded_usage_on(
    conn: &Connection,
    turn_id: &str,
) -> Result<Option<iwork_domain::common::Usage>, StorageError> {
    type Row = (i64, i64, i64, i64, Option<f64>);
    let row: Option<Row> = conn
        .prepare_cached(
            "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd \
             FROM turn_usage WHERE turn_id = ?1",
        )
        .and_then(|mut st| {
            st.query_row([turn_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()
        })
        .map_err(sql_err)?;
    const T: &str = "turn_usage";
    row.map(|(input, output, read, write, cost)| {
        Ok(iwork_domain::common::Usage {
            input_tokens: u64_col(T, "input_tokens", input)?,
            output_tokens: u64_col(T, "output_tokens", output)?,
            cache_read_tokens: u64_col(T, "cache_read_tokens", read)?,
            cache_write_tokens: u64_col(T, "cache_write_tokens", write)?,
            cost_usd: cost,
        })
    })
    .transpose()
}

fn unread_u32(v: i64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

pub(crate) fn projects_on<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
) -> Result<Vec<ProjectSummary>, StorageError> {
    rows(
        conn,
        "SELECT p.snapshot, COALESCE((SELECT SUM(c.unread) FROM conversations c \
             WHERE c.project_id = p.id), 0) FROM projects p ORDER BY p.id",
        [],
        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
    )?
    .into_iter()
    .map(|(s, unread)| {
        Ok(ProjectSummary {
            project: decode_project(codec, &s)?,
            unread: unread_u32(unread),
        })
    })
    .collect()
}

pub(crate) fn assistants_on<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
) -> Result<Vec<Assistant>, StorageError> {
    blobs(
        conn,
        "SELECT snapshot FROM assistants ORDER BY builtin DESC, id",
        [],
    )?
    .iter()
    .map(|s| decode_assistant(codec, s))
    .collect()
}

pub(crate) fn quota_on(conn: &Connection) -> Result<Vec<RuntimeQuota>, StorageError> {
    type Row = (String, String, i64, Option<i64>, i64);
    let all: Vec<Row> = rows(
        conn,
        "SELECT runtime_id, window, used_pct, resets_at, observed_at FROM quota_snapshots \
             ORDER BY runtime_id, window",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let mut out: Vec<RuntimeQuota> = Vec::new();
    for (runtime, window, used_pct, resets_at, observed_at) in all {
        let runtime_id = id_col("quota_snapshots", "runtime_id", &runtime)?;
        let w = QuotaWindow {
            window,
            used_pct: u8::try_from(used_pct)
                .map_err(|_| crate::sql::bad_column("quota_snapshots", "used_pct", used_pct))?,
            resets_at: resets_at.map(Timestamp),
            observed_at: Timestamp(observed_at),
        };
        match out.last_mut() {
            Some(q) if q.runtime_id == runtime_id => {
                q.observed_at = q.observed_at.max(w.observed_at);
                q.windows.push(w);
            }
            _ => out.push(RuntimeQuota {
                runtime_id,
                observed_at: w.observed_at,
                windows: vec![w],
            }),
        }
    }
    Ok(out)
}

pub(crate) fn runtimes_on<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
) -> Result<Vec<Runtime>, StorageError> {
    blobs(conn, "SELECT snapshot FROM runtimes ORDER BY id", [])?
        .iter()
        .map(|s| decode_runtime(codec, s))
        .collect()
}

pub(crate) fn rules_on<C: EventCodec + ?Sized>(
    conn: &Connection,
    codec: &C,
    project: Option<ProjectId>,
) -> Result<Vec<RuleEntry>, StorageError> {
    let snapshots = match project {
        None => blobs(conn, "SELECT snapshot FROM approval_rules ORDER BY id", [])?,
        Some(p) => blobs(
            conn,
            "SELECT snapshot FROM approval_rules WHERE scope_kind = 'global' \
                 OR (scope_kind = 'project' AND scope_id = ?1) ORDER BY id",
            [p.to_string()],
        )?,
    };
    snapshots.iter().map(|s| decode_rule(codec, s)).collect()
}

pub(crate) fn settings_on(
    conn: &Connection,
    defaults: &ReadDefaults,
) -> Result<GlobalSettings, StorageError> {
    let row: Option<(Option<bool>, Option<bool>, Option<String>)> = conn
        .query_row(
            "SELECT playful_ui, autostart, projects_root FROM global_settings WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let d = &defaults.global_settings;
    let (playful_ui, autostart, root) = row.unwrap_or((None, None, None));
    Ok(GlobalSettings {
        playful_ui: playful_ui.unwrap_or(d.playful_ui),
        autostart: autostart.unwrap_or(d.autostart),
        projects_root: root.map_or_else(|| d.projects_root.clone(), PathBuf::from),
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::StatementStatus;

    use super::{PAGE_BEFORE, PAGE_LATEST};
    use crate::fixtures::*;
    use iwork_domain::event::{ConversationCreated, MessageAppended, ProjectAdded};
    use iwork_ports::store::StoragePort;

    /// 方案 S9 / 第 4b 步：灌 10 万条消息，深分页（游标在中段）走索引的范围约束，
    /// 不全表扫描，代价与第 1 页同量级。
    #[test]
    fn conversation_deep_page_uses_range_constraint() {
        let env = Env::new();
        let store = env.open();
        block(store.commit(cmd(
            1,
            vec![
                ProjectAdded { project: project(1) }.into(),
                ConversationCreated { conversation: project_conversation(10, 1) }.into(),
                ConversationCreated { conversation: project_conversation(11, 1) }.into(),
                MessageAppended { message: user_message(1, 10, "x") }.into(),
            ],
        )))
        .unwrap();
        // 直接灌行（只关心查询计划与扫描量，快照用同一份）：两个对话各 5 万条交错
        let conn = rusqlite::Connection::open(env.db()).unwrap();
        conn.execute_batch("BEGIN").unwrap();
        {
            let mut ins = conn
                .prepare(
                    "INSERT INTO messages(id, conversation_id, author_kind, status, turn_id, appended_seq, \
                     created_at, snapshot, updated_seq) SELECT ?1, ?2, author_kind, status, NULL, 1, 0, snapshot, 1 \
                     FROM messages WHERE id = ?3",
                )
                .unwrap();
            let seed = mid(1).to_string();
            for n in 2..=100_000u128 {
                let conv = if n % 2 == 0 { cid(10) } else { cid(11) };
                ins.execute(rusqlite::params![
                    mid(n).to_string(),
                    conv.to_string(),
                    seed
                ])
                .unwrap();
            }
        }
        conn.execute_batch("COMMIT; ANALYZE;").unwrap();

        let conv = cid(10).to_string();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {PAGE_BEFORE}"))
            .unwrap()
            .query_map(rusqlite::params![conv, "x", 100], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter().any(|p| p
                .contains("USING INDEX messages_by_conversation (conversation_id=? AND id<?)")
                || p.contains(
                    "USING COVERING INDEX messages_by_conversation (conversation_id=? AND id<?)"
                )),
            "范围约束在用：{plan:?}"
        );

        let run = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> (usize, i32, i32) {
            let mut stmt = conn.prepare(sql).unwrap();
            let n = stmt.query_map(params, |_| Ok(())).unwrap().count();
            (
                n,
                stmt.get_status(StatementStatus::FullscanStep),
                stmt.get_status(StatementStatus::VmStep),
            )
        };
        let (n1, scan1, vm1) = run(PAGE_LATEST, &[&conv, &100]);
        // 第 999 页附近：游标在中段
        let cursor = mid(50_000).to_string();
        let (n2, scan2, vm2) = run(PAGE_BEFORE, &[&conv, &cursor, &100]);
        assert_eq!((n1, n2), (100, 100), "对照：两页都取满");
        assert_eq!((scan1, scan2), (0, 0), "没有全表扫描步");
        assert!(
            vm2 < vm1 * 10,
            "深分页的 VM 步数与第 1 页同量级：{vm1} vs {vm2}"
        );

        let started = std::time::Instant::now();
        let page = block(iwork_ports::read_model::ReadModelPort::conversation(
            &read_model(&store),
            cid(10),
            iwork_ports::read_model::MessagePage {
                before: Some(mid(50_000)),
                limit: 100,
            },
        ))
        .unwrap();
        assert_eq!(page.messages.len(), 100);
        assert!(
            started.elapsed().as_millis() < 50,
            "单次分页 {:?}",
            started.elapsed()
        );
    }
}
