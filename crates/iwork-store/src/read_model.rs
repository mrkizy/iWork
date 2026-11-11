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
use iwork_domain::id::{ConversationId, ProjectId};
use iwork_domain::message::{Message, MessageBody, MessageStatus};
use iwork_domain::rule::RuleEntry;
use iwork_domain::runtime::{QuotaWindow, Runtime};
use iwork_domain::settings::GlobalSettings;
use iwork_domain::turn::{Turn, TurnState};
use iwork_domain::user_action::{UserActionStateKind, UserActionTriggerKind, next_kind};
use iwork_ports::read_model::{
    AgentsMdDraftCard, ConversationSummary, ConversationView, MessagePage, PendingItem,
    ProjectSummary, ReadModelPort, ResumeCard, RuntimeQuota, Unsettled,
};
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::reader::ReaderPool;
use crate::snapshot::{
    approval_state_from_columns, decode_approval, decode_assistant, decode_card,
    decode_conversation, decode_message, decode_project, decode_rule, decode_runtime, decode_turn,
};
use crate::sql::{ColumnText, enum_col, id_col, sha_col, sha_text, sql_err, u64_col};

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
fn rows<T>(
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

fn blobs(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Vec<u8>>, StorageError> {
    rows(conn, sql, params, |r| r.get(0))
}

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
fn fill_streaming_body(conn: &Connection, m: &mut Message) -> Result<(), StorageError> {
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
fn kinds_where<K: ColumnText>(keep: impl Fn(K) -> bool) -> (String, Vec<&'static str>) {
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

/// `approvals` 的一行：`(id, snapshot, state, decider, void_reason)`。
type ApprovalRow = (String, Vec<u8>, String, Option<String>, Option<String>);

fn approval_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}

fn decode_approval_row<C: EventCodec + ?Sized>(
    codec: &C,
    (id, snapshot, state, decider, void_reason): ApprovalRow,
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
        self.query(|conn, codec| {
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
        })
    }

    fn assistants(&self) -> impl Future<Output = Result<Vec<Assistant>, StorageError>> + Send {
        self.query(|conn, codec| {
            blobs(
                conn,
                "SELECT snapshot FROM assistants ORDER BY builtin DESC, id",
                [],
            )?
            .iter()
            .map(|s| decode_assistant(codec, s))
            .collect()
        })
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
            type DraftRow = (String, String, String, String, i64, i64);
            let drafts: Vec<DraftRow> = rows(
                conn,
                "SELECT card_id, project_id, content_sha256, content_area, content_size, proposed_at \
                 FROM agents_md_drafts WHERE decision IS NULL ORDER BY card_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )?;
            for (card, project, sha, area, size, at) in drafts {
                const T: &str = "agents_md_drafts";
                out.push(PendingItem::AgentsMdDraft(AgentsMdDraftCard {
                    card_id: id_col(T, "card_id", &card)?,
                    project_id: id_col(T, "project_id", &project)?,
                    content: ArtifactRef {
                        sha256: sha_col(T, "content_sha256", &sha)?,
                        area: enum_col(T, "content_area", &area)?,
                        size: u64_col(T, "content_size", size)?,
                    },
                    proposed_at: Timestamp(at),
                }));
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
            Ok(Unsettled {
                turns,
                streaming,
                cards,
            })
        })
    }

    fn quota(&self) -> impl Future<Output = Result<Vec<RuntimeQuota>, StorageError>> + Send {
        self.query(|conn, _codec| {
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
                    used_pct: u8::try_from(used_pct).map_err(|_| {
                        crate::sql::bad_column("quota_snapshots", "used_pct", used_pct)
                    })?,
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
        })
    }

    fn runtimes(&self) -> impl Future<Output = Result<Vec<Runtime>, StorageError>> + Send {
        self.query(|conn, codec| {
            blobs(conn, "SELECT snapshot FROM runtimes ORDER BY id", [])?
                .iter()
                .map(|s| decode_runtime(codec, s))
                .collect()
        })
    }

    /// `Some(p)`：全局规则 + 该项目的规则。更细的范围（助手、对话）由需要的卡再加查询。
    fn rules(
        &self,
        project: Option<ProjectId>,
    ) -> impl Future<Output = Result<Vec<RuleEntry>, StorageError>> + Send {
        self.query(move |conn, codec| {
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
        })
    }

    fn security_lists(&self) -> impl Future<Output = Result<Vec<RuleEntry>, StorageError>> + Send {
        self.query(|conn, codec| {
            blobs(
                conn,
                "SELECT snapshot FROM approval_rules WHERE layer IN \
                 ('credential_commands', 'sensitive_dirs', 'writable_dirs') ORDER BY id",
                [],
            )?
            .iter()
            .map(|s| decode_rule(codec, s))
            .collect()
        })
    }

    fn settings(&self) -> impl Future<Output = Result<GlobalSettings, StorageError>> + Send {
        let defaults = self.defaults.clone();
        self.query(move |conn, _codec| {
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

fn unread_u32(v: i64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
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
