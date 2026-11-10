//! `SqliteProjector`：按事件更新投影表（方案 2.6）。
//!
//! 规则：
//! - **不跑状态机**，只有一处例外：审批与亲手执行卡片在写入前校验状态前提与归属存续（`owner.rs`，判断在
//!   `iwork-domain`）；不通过报 `Projection`、整条命令回滚。别的实体的状态照事件直接写。
//! - **只由事件决定**：不持有任何依赖环境的默认值（默认值在读模型补），同一事件序列在空表上
//!   重放得到同一张表。
//! - **缺行报错**：更新事件找不到目标行 → `Projection{seq}`，命令回滚，不吞。
//! - **每列一个写者**：同一张表被多个事件写时只更新自己拥有的列；不用 `OR REPLACE` 冲突策略整行替换（会把别的事件写的列清回默认值）。
//! - **`updated_seq`**：凡是写到一行，就把那一行的 `updated_seq` 设为当前事件的 seq。
//!
//! 加一个事件：下面 `project` 的穷举 `match` 一处 + `artifact_refs::refs_in` 的穷举 `match` 一处
//! （漏了都编译不过）+ 带新实体时 `snapshot.rs` 登记一行。

mod approvals;
mod artifact_refs;
mod assistants;
mod attachments;
mod conversations;
mod messages;
pub(crate) mod owner;
mod projects;
mod rules;
mod runtimes;
mod settings;
mod turns;
mod unread;
mod user_actions;

use std::sync::Arc;

use iwork_domain::event::DomainEvent;
use iwork_domain::id::Seq;
use iwork_ports::store::{EventCodec, Projector, StorageError};
use rusqlite::{Connection, OptionalExtension, Params};

use crate::sql::{expect_hit, sql_err};
use crate::writer::{Envelope, WriteTx};

pub use artifact_refs::artifact_refs_in;

/// 投影器。在线提交与重建走同一个实例（方案 5「投影与重建同一个 projector 实例」）。
pub struct SqliteProjector<C: EventCodec> {
    codec: Arc<C>,
}

impl<C: EventCodec> SqliteProjector<C> {
    pub fn new(codec: Arc<C>) -> Self {
        Self { codec }
    }
}

impl<C: EventCodec> Projector for SqliteProjector<C> {
    type Tx = WriteTx;

    fn project(&self, tx: &mut WriteTx, seq: Seq, event: &DomainEvent) -> Result<(), StorageError> {
        let seq_sql =
            crate::sql::seq_val("事件 seq", seq).map_err(|e| StorageError::Projection {
                seq,
                message: e.to_string(),
            })?;
        let cx = Cx {
            conn: &tx.conn,
            codec: self.codec.as_ref(),
            seq,
            seq_sql,
            env: &tx.envelope,
        };
        dispatch(&cx, event)?;
        artifact_refs::record(&cx, event)?;
        unread::bump(&cx, event)
    }
}

fn dispatch<C: EventCodec + ?Sized>(
    cx: &Cx<'_, C>,
    event: &DomainEvent,
) -> Result<(), StorageError> {
    use DomainEvent as E;
    match event {
        E::ProjectAdded(e) => projects::added(cx, e),
        E::ProjectUpdated(e) => projects::updated(cx, e),
        E::ProjectRelocated(e) => projects::relocated(cx, e),
        E::ProjectPathStatusChanged(e) => projects::path_status_changed(cx, e),
        E::ProjectRemoved(e) => projects::removed(cx, e),
        E::ProjectSettingsUpdated(e) => projects::settings_updated(cx, e),
        E::AgentsMdDraftProposed(e) => projects::draft_proposed(cx, e),
        E::AgentsMdDraftDecided(e) => projects::draft_decided(cx, e),
        E::AssistantCreated(e) => assistants::created(cx, e),
        E::AssistantUpdated(e) => assistants::new_version(cx, e.assistant_id, e.version, &e.config),
        E::AssistantReset(e) => assistants::new_version(cx, e.assistant_id, e.version, &e.config),
        E::AssistantDeleted(e) => assistants::deleted(cx, e),
        E::ConversationCreated(e) => conversations::created(cx, e),
        E::ConversationDeleting(e) => conversations::deleting(cx, e),
        E::ConversationDeleted(e) => conversations::deleted(cx, e.conversation_id),
        E::ConversationModelChanged(e) => conversations::model_changed(cx, e),
        E::ApprovalModeChanged(e) => conversations::approval_mode_changed(cx, e),
        E::DmProjectSwitched(e) => conversations::dm_project_switched(cx, e),
        E::ConversationRead(e) => unread::conversation_read(cx, e),
        E::MessageAppended(e) => messages::appended(cx, e),
        E::MessageCompleted(e) => messages::completed(cx, e),
        E::TurnQueued(e) => turns::queued(cx, e),
        E::TurnStateChanged(e) => turns::state_changed(cx, e),
        E::TurnFinished(e) => turns::usage(cx, e.turn_id, &e.usage),
        E::UsageReported(e) => turns::usage(cx, e.turn_id, &e.usage),
        E::TurnModelReported(e) => turns::model_reported(cx, e),
        E::ToolCallStarted(e) => turns::tool_call_started(cx, e),
        E::ToolCallFinished(e) => turns::tool_call_finished(cx, e),
        E::ApprovalRequested(e) => approvals::requested(cx, e, event),
        E::ApprovalReviewUpdated(e) => approvals::review_updated(cx, e, event),
        E::ApprovalDecided(e) => approvals::decided(cx, e.approval_id, e.state, event),
        E::ApprovalVoided(e) => approvals::voided(cx, e, event),
        E::RuleRemembered(e) => rules::added(cx, &e.rule),
        E::SecurityListEntryAdded(e) => rules::added(cx, &e.rule),
        E::RuleRevoked(e) => rules::removed(cx, e.rule_id),
        E::SecurityListEntryRemoved(e) => rules::removed(cx, e.entry_id),
        E::RulesDeleted(e) => e.rule_ids.iter().try_for_each(|id| rules::removed(cx, *id)),
        E::UserActionProposed(e) => user_actions::proposed(cx, e, event),
        E::UserActionStateChanged(e) => user_actions::state_changed(cx, e, event),
        E::AttachmentStored(e) => attachments::stored(cx, e),
        E::AttachmentConfirmed(e) => attachments::confirmed(cx, e),
        E::RuntimeUpdated(e) => runtimes::updated(cx, e),
        E::QuotaObserved(e) => runtimes::quota_observed(cx, e),
        E::LocalImportPrompted(e) => runtimes::import_prompted(cx, e),
        E::LocalConfigImported(e) => runtimes::config_imported(cx, e),
        E::GlobalSettingsChanged(e) => settings::changed(cx, e),
        // 没有查询读它们：审计流、密钥键名、恢复完成、作业结果（克隆结果由 ProjectAdded 落地）
        E::ApprovalEvaluated(_)
        | E::ApprovalStaleResultDropped(_)
        | E::McpSecretSet(_)
        | E::RecoveryCompleted(_)
        | E::RepositoryInitialized(_)
        | E::CloneFinished(_) => Ok(()),
    }
}

/// 一次 `project` 调用的上下文：连接、codec、当前 seq 与信封。
pub(crate) struct Cx<'a, C: ?Sized> {
    pub(crate) conn: &'a Connection,
    pub(crate) codec: &'a C,
    pub(crate) seq: Seq,
    /// `seq` 的 SQL 值，构造时已校验过范围
    seq_sql: i64,
    pub(crate) env: &'a Envelope,
}

impl<C: ?Sized> Cx<'_, C> {
    /// 当前 seq 的 SQL 值（写 `updated_seq` 等列）。
    pub(crate) fn seq(&self) -> i64 {
        self.seq_sql
    }

    /// 事件里带的 seq（如已读位置）→ SQL 值；超出范围是这条事件不合法，命令回滚、写线程照常。
    pub(crate) fn seq_arg(&self, what: &str, value: Seq) -> Result<i64, StorageError> {
        crate::sql::seq_val(what, value).map_err(|e| self.invalid(e.to_string()))
    }

    pub(crate) fn exec<P: Params>(&self, sql: &str, params: P) -> Result<usize, StorageError> {
        self.conn
            .prepare_cached(sql)
            .and_then(|mut s| s.execute(params))
            .map_err(sql_err)
    }

    /// 执行并要求命中至少一行；0 行 → `Projection{seq, "<table> 里没有 <id>"}`。
    pub(crate) fn exec_hit<P: Params>(
        &self,
        sql: &str,
        params: P,
        table: &str,
        id: impl std::fmt::Display,
    ) -> Result<(), StorageError> {
        let changed = self.exec(sql, params)?;
        expect_hit(changed, self.seq, table, id)
    }

    /// 读一行的快照；行不存在 → `Projection`。
    pub(crate) fn snapshot(
        &self,
        table: &str,
        id_column: &str,
        id: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let found: Option<Vec<u8>> = self
            .conn
            .prepare_cached(&format!(
                "SELECT snapshot FROM {table} WHERE {id_column} = ?1"
            ))
            .and_then(|mut s| s.query_row([id], |r| r.get(0)).optional())
            .map_err(sql_err)?;
        found.ok_or_else(|| self.missing(table, id))
    }

    pub(crate) fn missing(&self, table: &str, id: impl std::fmt::Display) -> StorageError {
        StorageError::Projection {
            seq: self.seq,
            message: format!("{table} 里没有 {id}"),
        }
    }

    pub(crate) fn invalid(&self, message: String) -> StorageError {
        StorageError::Projection {
            seq: self.seq,
            message,
        }
    }
}
