//! 领域事件的转换：52 个事件逐个对应 proto `DomainEvent` 的 oneof。
//!
//! 两个方向都是穷举 `match`：domain 加变体不写映射编译不过；proto 加 oneof 字段不写映射，
//! `domain_event_kinds_match_proto_oneof` 测试红。

use std::collections::BTreeMap;

use iwork_domain::approval::{Approval, ApprovalState, Decider};
use iwork_domain::event::{
    self as ev, CloneFailure, CloneFailureReason, CloneOutcome, DomainEvent,
};
use iwork_domain::event::{McpServerId, ToolCallId};
use iwork_domain::id::{ReviewId, Seq, TurnId};
use iwork_domain::message::MessageStatus;
use iwork_domain::turn::TurnState;
use iwork_proto::pb;
use iwork_proto::pb::domain_event::Event as P;

use super::model::*;
use super::{
    ConvertError, conversation_id_from_proto, conversation_id_to_proto, ids_to_strings,
    opt_timestamp, parse_id, parse_ids, parse_opt_id, path_from_proto, path_to_proto, required,
    required_timestamp, timestamp_to_proto,
};

type R<T> = Result<T, ConvertError>;

/// 存储里读出的事件 → 推给客户端的 `PersistedEvent`（seq、时间、主体与事件本身）。
/// 实时发布（提交观察者）与历史补发都用它，两条路得到的字节相同。
pub fn persisted_event_to_proto(e: &iwork_ports::store::StoredEvent) -> R<pb::PersistedEvent> {
    Ok(pb::PersistedEvent {
        seq: e.seq.0,
        at: Some(timestamp_to_proto(e.at)),
        actor: e.actor.0.clone(),
        event: Some(event_to_proto(&e.event)?),
    })
}

/// domain 事件 → proto 事件。只在路径不是合法 UTF-8、或实体的状态装不进 proto 消息时失败。
pub fn event_to_proto(event: &DomainEvent) -> R<pb::DomainEvent> {
    let e = match event {
        DomainEvent::ProjectAdded(e) => P::ProjectAdded(pb::ProjectAdded {
            project: Some(project_to_proto(&e.project)?),
        }),
        DomainEvent::ProjectUpdated(e) => P::ProjectUpdated(pb::ProjectUpdated {
            project_id: e.project_id.to_string(),
            name: e.name.clone(),
            pinned: e.pinned,
            last_active_at: e.last_active_at.map(timestamp_to_proto),
        }),
        DomainEvent::ProjectRelocated(e) => P::ProjectRelocated(pb::ProjectRelocated {
            project_id: e.project_id.to_string(),
            new_path: path_to_proto("ProjectRelocated", "new_path", &e.new_path)?,
        }),
        DomainEvent::ProjectPathStatusChanged(e) => {
            P::ProjectPathStatusChanged(pb::ProjectPathStatusChanged {
                project_id: e.project_id.to_string(),
                path_status: path_status_to_proto(e.path_status),
            })
        }
        DomainEvent::ProjectRemoved(e) => P::ProjectRemoved(pb::ProjectRemoved {
            project_id: e.project_id.to_string(),
        }),
        DomainEvent::ProjectSettingsUpdated(e) => {
            P::ProjectSettingsUpdated(pb::ProjectSettingsUpdated {
                project_id: e.project_id.to_string(),
                settings: Some(project_settings_to_proto(&e.settings)),
                previous: e.previous.as_ref().map(project_settings_to_proto),
            })
        }
        DomainEvent::RepositoryInitialized(e) => {
            P::RepositoryInitialized(pb::RepositoryInitialized {
                path: path_to_proto("RepositoryInitialized", "path", &e.path)?,
            })
        }
        DomainEvent::CloneFinished(e) => P::CloneFinished(clone_finished_to_proto(e)?),
        DomainEvent::AgentsMdDraftProposed(e) => {
            P::AgentsMdDraftProposed(pb::AgentsMdDraftProposed {
                card_id: e.card_id.to_string(),
                project_id: e.project_id.to_string(),
                content_artifact: Some(artifact_ref_to_proto(&e.content_artifact)),
            })
        }
        DomainEvent::AgentsMdDraftDecided(e) => P::AgentsMdDraftDecided(pb::AgentsMdDraftDecided {
            card_id: e.card_id.to_string(),
            project_id: e.project_id.to_string(),
            decision: draft_decision_to_proto(e.decision),
        }),
        DomainEvent::AssistantCreated(e) => P::AssistantCreated(pb::AssistantCreated {
            assistant: Some(assistant_to_proto(&e.assistant)),
        }),
        DomainEvent::AssistantUpdated(e) => P::AssistantUpdated(pb::AssistantUpdated {
            assistant_id: e.assistant_id.to_string(),
            version: e.version,
            config: Some(assistant_config_to_proto(&e.config)),
        }),
        DomainEvent::AssistantDeleted(e) => P::AssistantDeleted(pb::AssistantDeleted {
            assistant_id: e.assistant_id.to_string(),
            name_snapshot: e.name_snapshot.clone(),
        }),
        DomainEvent::AssistantReset(e) => P::AssistantReset(pb::AssistantReset {
            assistant_id: e.assistant_id.to_string(),
            version: e.version,
            config: Some(assistant_config_to_proto(&e.config)),
        }),
        DomainEvent::ConversationCreated(e) => P::ConversationCreated(pb::ConversationCreated {
            conversation: Some(conversation_to_proto(&e.conversation)),
        }),
        DomainEvent::ConversationDeleting(e) => P::ConversationDeleting(pb::ConversationDeleting {
            conversation_id: e.conversation_id.to_string(),
        }),
        DomainEvent::ConversationDeleted(e) => P::ConversationDeleted(pb::ConversationDeleted {
            conversation_id: e.conversation_id.to_string(),
        }),
        DomainEvent::ConversationModelChanged(e) => {
            P::ConversationModelChanged(pb::ConversationModelChanged {
                conversation_id: e.conversation_id.to_string(),
                model: Some(model_selection_to_proto(&e.model)),
            })
        }
        DomainEvent::ApprovalModeChanged(e) => P::ApprovalModeChanged(pb::ApprovalModeChanged {
            target: Some(mode_target_to_proto(&e.target)),
            mode: e.mode.map(approval_mode_to_proto),
            cause: mode_change_cause_to_proto(e.cause),
            released_approvals: e.released_approvals,
        }),
        DomainEvent::DmProjectSwitched(e) => P::DmProjectSwitched(pb::DmProjectSwitched {
            conversation_id: e.conversation_id.to_string(),
            project_id: e.project_id.map(|p| p.to_string()),
        }),
        DomainEvent::ConversationRead(e) => P::ConversationRead(pb::ConversationRead {
            conversation_id: e.conversation_id.to_string(),
            last_read_seq: e.last_read_seq.0,
        }),
        DomainEvent::MessageAppended(e) => P::MessageAppended(pb::MessageAppended {
            message: Some(message_to_proto(&e.message)),
        }),
        DomainEvent::MessageCompleted(e) => P::MessageCompleted(pb::MessageCompleted {
            conversation_id: e.conversation_id.to_string(),
            message_id: e.message_id.to_string(),
            body: Some(message_body_to_proto(&e.body)),
            status: completed_status_to_proto(e.status)?,
        }),
        DomainEvent::TurnQueued(e) => P::TurnQueued(pb::TurnQueued {
            turn: Some(turn_to_proto(&e.turn)),
        }),
        DomainEvent::TurnStateChanged(e) => {
            let (from, from_interrupt_reason) = turn_state_to_proto(e.from);
            let (to, interrupt_reason) = turn_state_to_proto(e.to);
            P::TurnStateChanged(pb::TurnStateChanged {
                conversation_id: conversation_id_to_proto(e.conversation_id),
                turn_id: e.turn_id.to_string(),
                from,
                to,
                reason: e.reason.clone(),
                interrupt_reason,
                from_interrupt_reason,
            })
        }
        DomainEvent::TurnFinished(e) => P::TurnFinished(pb::TurnFinished {
            conversation_id: conversation_id_to_proto(e.conversation_id),
            turn_id: e.turn_id.to_string(),
            state: finished_state_to_proto(e.state)?,
            usage: Some(usage_to_proto(&e.usage)),
            celebrate: e.celebrate,
            change_summary: e
                .change_summary
                .as_ref()
                .map(change_summary_to_proto)
                .transpose()?,
        }),
        DomainEvent::TurnModelReported(e) => P::TurnModelReported(pb::TurnModelReported {
            conversation_id: conversation_id_to_proto(e.conversation_id),
            turn_id: e.turn_id.to_string(),
            model: Some(model_selection_to_proto(&e.model)),
        }),
        DomainEvent::ApprovalDelivered(e) => P::ApprovalDelivered(pb::ApprovalDelivered {
            conversation_id: conversation_id_to_proto(e.conversation_id),
            approval_id: e.approval_id.to_string(),
            turn_id: e.turn_id.to_string(),
        }),
        DomainEvent::TurnIntentRecorded(e) => P::TurnIntentRecorded(pb::TurnIntentRecorded {
            conversation_id: conversation_id_to_proto(e.conversation_id),
            turn_id: e.turn_id.to_string(),
            intent: turn_intent_to_proto(e.intent),
        }),
        DomainEvent::ToolCallStarted(e) => P::ToolCallStarted(pb::ToolCallStarted {
            conversation_id: conversation_id_to_proto(e.conversation_id),
            turn_id: e.turn_id.to_string(),
            tool_call_id: e.tool_call_id.0.clone(),
            tool_name: e.tool_name.clone(),
            action: Some(action_to_proto(&e.action)?),
        }),
        DomainEvent::ToolCallFinished(e) => P::ToolCallFinished(pb::ToolCallFinished {
            conversation_id: conversation_id_to_proto(e.conversation_id),
            turn_id: e.turn_id.to_string(),
            tool_call_id: e.tool_call_id.0.clone(),
            ok: e.ok,
            output_artifact: e.output_artifact.as_ref().map(artifact_ref_to_proto),
        }),
        DomainEvent::TurnSessionBound(e) => P::TurnSessionBound(pb::TurnSessionBound {
            conversation_id: conversation_id_to_proto(e.conversation_id),
            turn_id: e.turn_id.to_string(),
            runtime_session_id: e.runtime_session_id.clone(),
            replaced: e.replaced.as_ref().map(|r| pb::SessionReplaced {
                old_runtime_session_id: r.old_runtime_session_id.clone(),
                reason: session_lost_reason_to_proto(r.reason),
            }),
        }),
        DomainEvent::ApprovalRequested(e) => P::ApprovalRequested(pb::ApprovalRequested {
            approval: Some(approval_to_proto(requested_state(&e.approval)?)?),
        }),
        DomainEvent::ApprovalReviewUpdated(e) => {
            let (review, review_id, reason) = review_state_to_proto(&e.review);
            P::ApprovalReviewUpdated(pb::ApprovalReviewUpdated {
                conversation_id: conversation_id_to_proto(e.conversation_id),
                approval_id: e.approval_id.to_string(),
                review_id: review_id.ok_or(ConvertError::InvalidValue {
                    kind: "ApprovalReviewUpdated",
                    field: "review",
                    reason: "审查子状态事件不会是 NotReviewed".into(),
                })?,
                review,
                reason,
            })
        }
        DomainEvent::ApprovalDecided(e) => P::ApprovalDecided(approval_decided_to_proto(e)?),
        DomainEvent::ApprovalVoided(e) => P::ApprovalVoided(pb::ApprovalVoided {
            conversation_id: conversation_id_to_proto(e.conversation_id),
            approval_id: e.approval_id.to_string(),
            reason: void_reason_to_proto(e.reason),
        }),
        DomainEvent::ApprovalEvaluated(e) => P::ApprovalEvaluated(pb::ApprovalEvaluated {
            turn_id: e.turn_id.to_string(),
            action: Some(action_to_proto(&e.action)?),
            matched_rule_ids: ids_to_strings(&e.matched_rule_ids),
            ruleset_version: e.ruleset_version.0.clone(),
            verdict: Some(verdict_to_proto(&e.verdict)?),
            layer_hit: rule_layer_to_proto(e.layer_hit),
            duration_ms: e.duration_ms,
            snapshot_lists_seq: e.snapshot_lists_seq.0,
            live_rules_seq: e.live_rules_seq.0,
            sub_actions: actions_to_proto(&e.sub_actions)?,
            call_shape: e.call_shape.map_or(0, call_shape_to_proto),
            snapshot_rule_paths: e
                .snapshot_rule_paths
                .iter()
                .map(|r| {
                    Ok(pb::RulePathResolution {
                        path: path_to_proto("RulePathResolution", "path", &r.path)?,
                        resolved: path_to_proto("RulePathResolution", "resolved", &r.resolved)?,
                    })
                })
                .collect::<R<_>>()?,
            grant_approval_id: e.grant_approval_id.map(|id| id.to_string()),
        }),
        DomainEvent::RuleRemembered(e) => P::RuleRemembered(pb::RuleRemembered {
            rule: Some(rule_to_proto(&e.rule)?),
        }),
        DomainEvent::RuleRevoked(e) => P::RuleRevoked(pb::RuleRevoked {
            rule_id: e.rule_id.to_string(),
        }),
        DomainEvent::RulesDeleted(e) => P::RulesDeleted(pb::RulesDeleted {
            rule_ids: ids_to_strings(&e.rule_ids),
            cause: rules_deleted_cause_to_proto(e.cause),
        }),
        DomainEvent::ApprovalStaleResultDropped(e) => {
            P::ApprovalStaleResultDropped(pb::ApprovalStaleResultDropped {
                approval_id: e.approval_id.to_string(),
                review_id: e.review_id.to_string(),
                result: review_phase_to_proto(e.result),
                why: stale_reason_to_proto(e.why),
            })
        }
        DomainEvent::UserActionProposed(e) => P::UserActionProposed(pb::UserActionProposed {
            card: Some(user_action_card_to_proto(&e.card)?),
        }),
        DomainEvent::UserActionStateChanged(e) => {
            let f = user_action_state_to_proto(&e.state);
            P::UserActionStateChanged(pb::UserActionStateChanged {
                conversation_id: conversation_id_to_proto(e.conversation_id),
                card_id: e.card_id.to_string(),
                state: f.state,
                exit: f.exit,
                error: f.error,
                output_artifact: e.output_artifact.as_ref().map(artifact_ref_to_proto),
                void_reason: f.void_reason,
            })
        }
        DomainEvent::SecurityListEntryAdded(e) => {
            P::SecurityListEntryAdded(pb::SecurityListEntryAdded {
                rule: Some(rule_to_proto(&e.rule)?),
            })
        }
        DomainEvent::SecurityListEntryRemoved(e) => {
            P::SecurityListEntryRemoved(pb::SecurityListEntryRemoved {
                entry_id: e.entry_id.to_string(),
            })
        }
        DomainEvent::AttachmentStored(e) => P::AttachmentStored(pb::AttachmentStored {
            attachment: Some(attachment_to_proto(&e.attachment)?),
        }),
        DomainEvent::AttachmentConfirmed(e) => P::AttachmentConfirmed(pb::AttachmentConfirmed {
            conversation_id: e.conversation_id.to_string(),
            attachment_id: e.attachment_id.to_string(),
        }),
        DomainEvent::RuntimeUpdated(e) => P::RuntimeUpdated(pb::RuntimeUpdated {
            runtime: Some(runtime_to_proto(&e.runtime)?),
        }),
        DomainEvent::QuotaObserved(e) => P::QuotaObserved(pb::QuotaObserved {
            runtime_id: e.runtime_id.to_string(),
            windows: e
                .windows
                .iter()
                .map(quota_window_to_proto)
                .collect::<R<_>>()?,
            observed_at: Some(timestamp_to_proto(e.observed_at)),
        }),
        DomainEvent::UsageReported(e) => P::UsageReported(pb::UsageReported {
            runtime_id: e.runtime_id.to_string(),
            turn_id: e.turn_id.to_string(),
            usage: Some(usage_to_proto(&e.usage)),
        }),
        DomainEvent::RuntimeSessionForgetFailed(e) => {
            use pb::runtime_session_forget_failed::Outcome;
            P::RuntimeSessionForgetFailed(pb::RuntimeSessionForgetFailed {
                conversation_id: e.conversation_id.to_string(),
                runtime_session_id: e.runtime_session_id.clone(),
                outcome: Some(match &e.outcome {
                    ev::ForgetFailure::Refused { why } => {
                        Outcome::Refused(pb::ForgetRefused { why: why.clone() })
                    }
                    ev::ForgetFailure::Failed { message } => Outcome::Failed(pb::ForgetFailed {
                        message: message.clone(),
                    }),
                }),
            })
        }
        DomainEvent::LocalImportPrompted(e) => P::LocalImportPrompted(pb::LocalImportPrompted {
            runtime_id: e.runtime_id.to_string(),
        }),
        DomainEvent::LocalConfigImported(e) => P::LocalConfigImported(pb::LocalConfigImported {
            runtime_id: e.runtime_id.to_string(),
            items: e.items.iter().map(local_config_item_to_proto).collect(),
        }),
        DomainEvent::McpSecretSet(e) => P::McpSecretSet(pb::McpSecretSet {
            mcp_server_id: e.mcp_server_id.0.clone(),
            key_name: e.key_name.clone(),
        }),
        DomainEvent::GlobalSettingsChanged(e) => {
            P::GlobalSettingsChanged(pb::GlobalSettingsChanged {
                playful_ui: e.playful_ui,
                autostart: e.autostart,
                projects_root: e
                    .projects_root
                    .as_deref()
                    .map(|p| path_to_proto("GlobalSettingsChanged", "projects_root", p))
                    .transpose()?,
            })
        }
        DomainEvent::RecoveryCompleted(e) => P::RecoveryCompleted(pb::RecoveryCompleted {
            interrupted_turn_ids: ids_to_strings(&e.interrupted_turn_ids),
        }),
    };
    Ok(pb::DomainEvent { event: Some(e) })
}

/// 消息结局只能是完成、中断、失败（`Sent` 是用户与系统消息的创建状态，`Streaming` 还没结束）。
/// 两个方向同样严格：编得进去的一定解得回来。
fn completed_status_to_proto(status: MessageStatus) -> R<i32> {
    match status {
        MessageStatus::Completed | MessageStatus::Interrupted | MessageStatus::Failed => {
            Ok(message_status_to_proto(status))
        }
        MessageStatus::Sent | MessageStatus::Streaming => Err(ConvertError::InvalidValue {
            kind: "MessageCompleted",
            field: "status",
            reason: format!("消息结局不能是 {status:?}"),
        }),
    }
}

fn completed_status_from_proto(value: i32) -> R<MessageStatus> {
    match message_status_from_proto("MessageCompleted", "status", value) {
        Ok(s @ (MessageStatus::Completed | MessageStatus::Interrupted | MessageStatus::Failed)) => {
            Ok(s)
        }
        _ => Err(ConvertError::UnsupportedEnum {
            kind: "MessageCompleted",
            field: "status",
            value,
        }),
    }
}

/// 回合结束事件只带终态（完成、取消、失败）。proto 的 `TurnFinished` 没有中断原因字段，
/// 中断也不是结束；所以非终态在编码时就拒绝，解码同样只认终态——编得进去的一定解得回来。
fn finished_state_to_proto(state: TurnState) -> R<i32> {
    if !state.is_terminal() {
        return Err(ConvertError::InvalidValue {
            kind: "TurnFinished",
            field: "state",
            reason: format!("回合结束只能是终态，不能是 {state:?}"),
        });
    }
    Ok(turn_state_to_proto(state).0)
}

fn finished_state_from_proto(state: i32) -> R<TurnState> {
    match turn_state_from_proto("TurnFinished", "state", "state", state, None) {
        Ok(s) if s.is_terminal() => Ok(s),
        _ => Err(ConvertError::UnsupportedEnum {
            kind: "TurnFinished",
            field: "state",
            value: state,
        }),
    }
}

/// 克隆结果：成功 ↔ `ok = true` + `path`；失败 ↔ `ok = false` + `error`（错误码
/// `TARGET_NOT_EMPTY` / `CLONE_FAILED`）。只有克隆失败可重试（多半是网络）。
fn clone_finished_to_proto(e: &ev::CloneFinished) -> R<pb::CloneFinished> {
    let job_id = e.job_id.to_string();
    Ok(match &e.outcome {
        CloneOutcome::Cloned { path } => pb::CloneFinished {
            job_id,
            ok: true,
            error: None,
            path: Some(path_to_proto("CloneFinished", "path", path)?),
        },
        CloneOutcome::Failed(f) => pb::CloneFinished {
            job_id,
            ok: false,
            error: Some(pb::Error {
                code: match f.reason {
                    CloneFailureReason::TargetNotEmpty => pb::ErrorCode::TargetNotEmpty,
                    CloneFailureReason::CloneFailed => pb::ErrorCode::CloneFailed,
                } as i32,
                message: f.message.clone(),
                retryable: f.reason == CloneFailureReason::CloneFailed,
                details: BTreeMap::new(),
            }),
            path: None,
        },
    })
}

/// 失败时只认两个错误码；`retryable` 与 `details` 由错误码决定，不进 domain。
fn clone_finished_from_proto(e: &pb::CloneFinished) -> R<ev::CloneFinished> {
    const K: &str = "CloneFinished";
    let outcome = if e.ok {
        CloneOutcome::Cloned {
            path: path_from_proto(required(K, "path", e.path.as_deref())?),
        }
    } else {
        let error = required(K, "error", e.error.as_ref())?;
        let reason = match pb::ErrorCode::try_from(error.code) {
            Ok(pb::ErrorCode::TargetNotEmpty) => CloneFailureReason::TargetNotEmpty,
            Ok(pb::ErrorCode::CloneFailed) => CloneFailureReason::CloneFailed,
            _ => {
                return Err(ConvertError::UnsupportedEnum {
                    kind: "Error",
                    field: "code",
                    value: error.code,
                });
            }
        };
        CloneOutcome::Failed(CloneFailure {
            reason,
            message: error.message.clone(),
        })
    };
    Ok(ev::CloneFinished {
        job_id: parse_id(K, "job_id", &e.job_id)?,
        outcome,
    })
}

/// `ApprovalRequested` 带的审批只能是待审批（或已拒绝：AI 审查前就被底线拒的那种）。`Approval` 消息本身能装下
/// 四种状态（G3），事件这一侧的约束照旧：批准与作废只经 `ApprovalDecided` / `ApprovalVoided`。
fn requested_state(a: &Approval) -> R<&Approval> {
    match a.state {
        ApprovalState::Pending | ApprovalState::Denied => Ok(a),
        other => Err(ConvertError::InvalidValue {
            kind: "ApprovalRequested",
            field: "approval.state",
            reason: format!("请求审批的事件只能带待审批或已拒绝，不能是 {other:?}"),
        }),
    }
}

/// 允许 ↔ `APPROVED` + 决定者；拒绝 ↔ `DENIED` + 决定者 `USER`（拒绝的一定是用户）。
fn approval_decided_to_proto(e: &ev::ApprovalDecided) -> R<pb::ApprovalDecided> {
    let (state, decider) = match e.state {
        ApprovalState::Approved { decider } => (pb::ApprovalState::Approved, decider),
        ApprovalState::Denied => (pb::ApprovalState::Denied, Decider::User),
        other => {
            return Err(ConvertError::InvalidValue {
                kind: "ApprovalDecided",
                field: "state",
                reason: format!("决定事件只能是批准或拒绝，不能是 {other:?}"),
            });
        }
    };
    Ok(pb::ApprovalDecided {
        conversation_id: conversation_id_to_proto(e.conversation_id),
        approval_id: e.approval_id.to_string(),
        state: state as i32,
        decider: decider_to_proto(decider),
        remember: e.remember.map(remember_scope_to_proto),
    })
}

fn approval_decided_from_proto(e: &pb::ApprovalDecided) -> R<ev::ApprovalDecided> {
    const K: &str = "ApprovalDecided";
    let decider = decider_from_proto(K, "decider", e.decider)?;
    let state = match pb::ApprovalState::try_from(e.state) {
        Ok(pb::ApprovalState::Approved) => ApprovalState::Approved { decider },
        Ok(pb::ApprovalState::Denied) if decider == Decider::User => ApprovalState::Denied,
        Ok(pb::ApprovalState::Denied) => {
            return Err(ConvertError::InvalidValue {
                kind: K,
                field: "decider",
                reason: format!("拒绝的决定者只能是用户，实际 {decider:?}"),
            });
        }
        _ => {
            return Err(ConvertError::UnsupportedEnum {
                kind: K,
                field: "state",
                value: e.state,
            });
        }
    };
    Ok(ev::ApprovalDecided {
        conversation_id: conversation_id_from_proto("ApprovalDecided", &e.conversation_id)?,
        approval_id: parse_id(K, "approval_id", &e.approval_id)?,
        state,
        remember: e
            .remember
            .map(|r| remember_scope_from_proto(K, "remember", r))
            .transpose()?,
    })
}

/// proto 事件 → domain 事件。`event` 缺失、字段不合法时点名报错。
pub fn event_from_proto(event: pb::DomainEvent) -> R<DomainEvent> {
    let e = required("DomainEvent", "event", event.event)?;
    Ok(match e {
        P::ProjectAdded(e) => ev::ProjectAdded {
            project: project_from_proto(required("ProjectAdded", "project", e.project.as_ref())?)?,
        }
        .into(),
        P::ProjectUpdated(e) => ev::ProjectUpdated {
            project_id: parse_id("ProjectUpdated", "project_id", &e.project_id)?,
            name: e.name,
            pinned: e.pinned,
            last_active_at: opt_timestamp(
                "ProjectUpdated",
                "last_active_at",
                e.last_active_at.as_ref(),
            )?,
        }
        .into(),
        P::ProjectRelocated(e) => ev::ProjectRelocated {
            project_id: parse_id("ProjectRelocated", "project_id", &e.project_id)?,
            new_path: path_from_proto(&e.new_path),
        }
        .into(),
        P::ProjectPathStatusChanged(e) => ev::ProjectPathStatusChanged {
            project_id: parse_id("ProjectPathStatusChanged", "project_id", &e.project_id)?,
            path_status: path_status_from_proto(
                "ProjectPathStatusChanged",
                "path_status",
                e.path_status,
            )?,
        }
        .into(),
        P::ProjectRemoved(e) => ev::ProjectRemoved {
            project_id: parse_id("ProjectRemoved", "project_id", &e.project_id)?,
        }
        .into(),
        P::ProjectSettingsUpdated(e) => ev::ProjectSettingsUpdated {
            project_id: parse_id("ProjectSettingsUpdated", "project_id", &e.project_id)?,
            settings: project_settings_from_proto(required(
                "ProjectSettingsUpdated",
                "settings",
                e.settings.as_ref(),
            )?)?,
            previous: e
                .previous
                .as_ref()
                .map(project_settings_from_proto)
                .transpose()?,
        }
        .into(),
        P::RepositoryInitialized(e) => ev::RepositoryInitialized {
            path: path_from_proto(&e.path),
        }
        .into(),
        P::CloneFinished(e) => clone_finished_from_proto(&e)?.into(),
        P::AgentsMdDraftProposed(e) => ev::AgentsMdDraftProposed {
            card_id: parse_id("AgentsMdDraftProposed", "card_id", &e.card_id)?,
            project_id: parse_id("AgentsMdDraftProposed", "project_id", &e.project_id)?,
            content_artifact: required_artifact(
                "AgentsMdDraftProposed",
                "content_artifact",
                e.content_artifact.as_ref(),
            )?,
        }
        .into(),
        P::AgentsMdDraftDecided(e) => ev::AgentsMdDraftDecided {
            card_id: parse_id("AgentsMdDraftDecided", "card_id", &e.card_id)?,
            project_id: parse_id("AgentsMdDraftDecided", "project_id", &e.project_id)?,
            decision: draft_decision_from_proto("AgentsMdDraftDecided", "decision", e.decision)?,
        }
        .into(),
        P::AssistantCreated(e) => ev::AssistantCreated {
            assistant: assistant_from_proto(required(
                "AssistantCreated",
                "assistant",
                e.assistant.as_ref(),
            )?)?,
        }
        .into(),
        P::AssistantUpdated(e) => ev::AssistantUpdated {
            assistant_id: parse_id("AssistantUpdated", "assistant_id", &e.assistant_id)?,
            version: e.version,
            config: assistant_config_from_proto(required(
                "AssistantUpdated",
                "config",
                e.config.as_ref(),
            )?)?,
        }
        .into(),
        P::AssistantDeleted(e) => ev::AssistantDeleted {
            assistant_id: parse_id("AssistantDeleted", "assistant_id", &e.assistant_id)?,
            name_snapshot: e.name_snapshot,
        }
        .into(),
        P::AssistantReset(e) => ev::AssistantReset {
            assistant_id: parse_id("AssistantReset", "assistant_id", &e.assistant_id)?,
            version: e.version,
            config: assistant_config_from_proto(required(
                "AssistantReset",
                "config",
                e.config.as_ref(),
            )?)?,
        }
        .into(),
        P::ConversationCreated(e) => ev::ConversationCreated {
            conversation: conversation_from_proto(required(
                "ConversationCreated",
                "conversation",
                e.conversation.as_ref(),
            )?)?,
        }
        .into(),
        P::ConversationDeleting(e) => ev::ConversationDeleting {
            conversation_id: parse_id(
                "ConversationDeleting",
                "conversation_id",
                &e.conversation_id,
            )?,
        }
        .into(),
        P::ConversationDeleted(e) => ev::ConversationDeleted {
            conversation_id: parse_id(
                "ConversationDeleted",
                "conversation_id",
                &e.conversation_id,
            )?,
        }
        .into(),
        P::ConversationModelChanged(e) => ev::ConversationModelChanged {
            conversation_id: parse_id(
                "ConversationModelChanged",
                "conversation_id",
                &e.conversation_id,
            )?,
            model: required_model("ConversationModelChanged", "model", e.model.as_ref())?,
        }
        .into(),
        P::ApprovalModeChanged(e) => ev::ApprovalModeChanged {
            target: mode_target_from_proto("ApprovalModeChanged", e.target.as_ref())?,
            mode: e
                .mode
                .map(|m| approval_mode_from_proto("ApprovalModeChanged", "mode", m))
                .transpose()?,
            cause: mode_change_cause_from_proto("ApprovalModeChanged", "cause", e.cause)?,
            released_approvals: e.released_approvals,
        }
        .into(),
        P::DmProjectSwitched(e) => ev::DmProjectSwitched {
            conversation_id: parse_id("DmProjectSwitched", "conversation_id", &e.conversation_id)?,
            project_id: parse_opt_id("DmProjectSwitched", "project_id", e.project_id.as_deref())?,
        }
        .into(),
        P::ConversationRead(e) => ev::ConversationRead {
            conversation_id: parse_id("ConversationRead", "conversation_id", &e.conversation_id)?,
            last_read_seq: Seq(e.last_read_seq),
        }
        .into(),
        P::MessageAppended(e) => ev::MessageAppended {
            message: message_from_proto(required(
                "MessageAppended",
                "message",
                e.message.as_ref(),
            )?)?,
        }
        .into(),
        P::MessageCompleted(e) => {
            const K: &str = "MessageCompleted";
            ev::MessageCompleted {
                conversation_id: parse_id(K, "conversation_id", &e.conversation_id)?,
                message_id: parse_id(K, "message_id", &e.message_id)?,
                // 完成消息必须带正文（可以是显式的空串）
                body: message_body_from_proto(required(K, "body", e.body.as_ref())?)?,
                status: completed_status_from_proto(e.status)?,
            }
            .into()
        }
        P::TurnQueued(e) => ev::TurnQueued {
            turn: turn_from_proto(required("TurnQueued", "turn", e.turn.as_ref())?)?,
        }
        .into(),
        P::TurnStateChanged(e) => {
            const K: &str = "TurnStateChanged";
            ev::TurnStateChanged {
                conversation_id: conversation_id_from_proto(
                    "TurnStateChanged",
                    &e.conversation_id,
                )?,
                turn_id: parse_id(K, "turn_id", &e.turn_id)?,
                from: turn_state_from_proto(
                    K,
                    "from",
                    "from_interrupt_reason",
                    e.from,
                    e.from_interrupt_reason,
                )?,
                to: turn_state_from_proto(K, "to", "interrupt_reason", e.to, e.interrupt_reason)?,
                reason: e.reason,
            }
            .into()
        }
        P::TurnFinished(e) => {
            const K: &str = "TurnFinished";
            ev::TurnFinished {
                conversation_id: conversation_id_from_proto("TurnFinished", &e.conversation_id)?,
                turn_id: parse_id(K, "turn_id", &e.turn_id)?,
                state: finished_state_from_proto(e.state)?,
                usage: usage_from_proto(required(K, "usage", e.usage.as_ref())?),
                celebrate: e.celebrate,
                change_summary: e.change_summary.as_ref().map(change_summary_from_proto),
            }
            .into()
        }
        P::TurnModelReported(e) => ev::TurnModelReported {
            conversation_id: conversation_id_from_proto("TurnModelReported", &e.conversation_id)?,
            turn_id: parse_id("TurnModelReported", "turn_id", &e.turn_id)?,
            model: required_model("TurnModelReported", "model", e.model.as_ref())?,
        }
        .into(),
        P::ApprovalDelivered(e) => ev::ApprovalDelivered {
            conversation_id: conversation_id_from_proto("ApprovalDelivered", &e.conversation_id)?,
            approval_id: parse_id("ApprovalDelivered", "approval_id", &e.approval_id)?,
            turn_id: parse_id("ApprovalDelivered", "turn_id", &e.turn_id)?,
        }
        .into(),
        P::TurnIntentRecorded(e) => ev::TurnIntentRecorded {
            conversation_id: conversation_id_from_proto("TurnIntentRecorded", &e.conversation_id)?,
            turn_id: parse_id("TurnIntentRecorded", "turn_id", &e.turn_id)?,
            intent: turn_intent_from_proto("TurnIntentRecorded", "intent", e.intent)?,
        }
        .into(),
        P::ToolCallStarted(e) => ev::ToolCallStarted {
            conversation_id: conversation_id_from_proto("ToolCallStarted", &e.conversation_id)?,
            turn_id: parse_id("ToolCallStarted", "turn_id", &e.turn_id)?,
            tool_call_id: ToolCallId(e.tool_call_id),
            tool_name: e.tool_name,
            action: action_from_proto("ToolCallStarted", e.action.as_ref())?,
        }
        .into(),
        P::ToolCallFinished(e) => ev::ToolCallFinished {
            conversation_id: conversation_id_from_proto("ToolCallFinished", &e.conversation_id)?,
            turn_id: parse_id("ToolCallFinished", "turn_id", &e.turn_id)?,
            tool_call_id: ToolCallId(e.tool_call_id),
            ok: e.ok,
            output_artifact: opt_artifact(e.output_artifact.as_ref())?,
        }
        .into(),
        P::TurnSessionBound(e) => ev::TurnSessionBound {
            conversation_id: conversation_id_from_proto("TurnSessionBound", &e.conversation_id)?,
            turn_id: parse_id("TurnSessionBound", "turn_id", &e.turn_id)?,
            runtime_session_id: e.runtime_session_id,
            replaced: e
                .replaced
                .map(|r| -> R<_> {
                    Ok(ev::SessionReplaced {
                        old_runtime_session_id: r.old_runtime_session_id,
                        reason: session_lost_reason_from_proto(
                            "SessionReplaced",
                            "reason",
                            r.reason,
                        )?,
                    })
                })
                .transpose()?,
        }
        .into(),
        P::ApprovalRequested(e) => ev::ApprovalRequested {
            approval: requested_state(&approval_from_proto(required(
                "ApprovalRequested",
                "approval",
                e.approval.as_ref(),
            )?)?)?
            .clone(),
        }
        .into(),
        P::ApprovalReviewUpdated(e) => {
            const K: &str = "ApprovalReviewUpdated";
            let review =
                review_state_from_proto(K, e.review, Some(&e.review_id), e.reason.as_deref())?;
            if review.review_id().is_none() {
                return Err(ConvertError::InvalidValue {
                    kind: K,
                    field: "review",
                    reason: "审查子状态事件不会是 NOT_REVIEWED".into(),
                });
            }
            ev::ApprovalReviewUpdated {
                conversation_id: conversation_id_from_proto(
                    "ApprovalReviewUpdated",
                    &e.conversation_id,
                )?,
                approval_id: parse_id(K, "approval_id", &e.approval_id)?,
                review,
            }
            .into()
        }
        P::ApprovalDecided(e) => approval_decided_from_proto(&e)?.into(),
        P::ApprovalVoided(e) => ev::ApprovalVoided {
            conversation_id: conversation_id_from_proto("ApprovalVoided", &e.conversation_id)?,
            approval_id: parse_id("ApprovalVoided", "approval_id", &e.approval_id)?,
            reason: void_reason_from_proto("ApprovalVoided", "reason", e.reason)?,
        }
        .into(),
        P::ApprovalEvaluated(e) => {
            const K: &str = "ApprovalEvaluated";
            ev::ApprovalEvaluated {
                turn_id: parse_id(K, "turn_id", &e.turn_id)?,
                action: action_from_proto(K, e.action.as_ref())?,
                matched_rule_ids: parse_ids(K, "matched_rule_ids", &e.matched_rule_ids)?,
                ruleset_version: iwork_domain::rule::RuleSetVersion(e.ruleset_version),
                verdict: verdict_from_proto(K, e.verdict.as_ref())?,
                layer_hit: rule_layer_from_proto(K, "layer_hit", e.layer_hit)?,
                duration_ms: e.duration_ms,
                snapshot_lists_seq: iwork_domain::id::Seq(e.snapshot_lists_seq),
                live_rules_seq: iwork_domain::id::Seq(e.live_rules_seq),
                sub_actions: actions_from_proto(K, &e.sub_actions)?,
                // T07 之前的审计事件不带形状（未指定 = 0）
                call_shape: match e.call_shape {
                    0 => None,
                    v => Some(call_shape_from_proto(K, "call_shape", v)?),
                },
                snapshot_rule_paths: e
                    .snapshot_rule_paths
                    .iter()
                    .map(|r| ev::RulePathResolution {
                        path: path_from_proto(&r.path),
                        resolved: path_from_proto(&r.resolved),
                    })
                    .collect(),
                grant_approval_id: parse_opt_id(
                    K,
                    "grant_approval_id",
                    e.grant_approval_id.as_deref(),
                )?,
            }
            .into()
        }
        P::RuleRemembered(e) => ev::RuleRemembered {
            rule: rule_from_proto(required("RuleRemembered", "rule", e.rule.as_ref())?)?,
        }
        .into(),
        P::RuleRevoked(e) => ev::RuleRevoked {
            rule_id: parse_id("RuleRevoked", "rule_id", &e.rule_id)?,
        }
        .into(),
        P::RulesDeleted(e) => ev::RulesDeleted {
            rule_ids: parse_ids("RulesDeleted", "rule_ids", &e.rule_ids)?,
            cause: rules_deleted_cause_from_proto("RulesDeleted", "cause", e.cause)?,
        }
        .into(),
        P::ApprovalStaleResultDropped(e) => {
            const K: &str = "ApprovalStaleResultDropped";
            ev::ApprovalStaleResultDropped {
                approval_id: parse_id(K, "approval_id", &e.approval_id)?,
                review_id: ReviewId(parse_id::<TurnId>(K, "review_id", &e.review_id)?),
                result: review_phase_from_proto(K, "result", e.result)?,
                why: stale_reason_from_proto(K, "why", e.why)?,
            }
            .into()
        }
        P::UserActionProposed(e) => ev::UserActionProposed {
            card: user_action_card_from_proto(required(
                "UserActionProposed",
                "card",
                e.card.as_ref(),
            )?)?,
        }
        .into(),
        P::UserActionStateChanged(e) => {
            const K: &str = "UserActionStateChanged";
            ev::UserActionStateChanged {
                conversation_id: conversation_id_from_proto(
                    "UserActionStateChanged",
                    &e.conversation_id,
                )?,
                card_id: parse_id(K, "card_id", &e.card_id)?,
                state: user_action_state_from_proto(
                    K,
                    &UserActionStateFields {
                        state: e.state,
                        exit: e.exit,
                        error: e.error,
                        void_reason: e.void_reason,
                    },
                )?,
                output_artifact: opt_artifact(e.output_artifact.as_ref())?,
            }
            .into()
        }
        P::SecurityListEntryAdded(e) => ev::SecurityListEntryAdded {
            rule: rule_from_proto(required("SecurityListEntryAdded", "rule", e.rule.as_ref())?)?,
        }
        .into(),
        P::SecurityListEntryRemoved(e) => ev::SecurityListEntryRemoved {
            entry_id: parse_id("SecurityListEntryRemoved", "entry_id", &e.entry_id)?,
        }
        .into(),
        P::AttachmentStored(e) => ev::AttachmentStored {
            attachment: attachment_from_proto(required(
                "AttachmentStored",
                "attachment",
                e.attachment.as_ref(),
            )?)?,
        }
        .into(),
        P::AttachmentConfirmed(e) => ev::AttachmentConfirmed {
            conversation_id: parse_id(
                "AttachmentConfirmed",
                "conversation_id",
                &e.conversation_id,
            )?,
            attachment_id: parse_id("AttachmentConfirmed", "attachment_id", &e.attachment_id)?,
        }
        .into(),
        P::RuntimeUpdated(e) => ev::RuntimeUpdated {
            runtime: runtime_from_proto(required(
                "RuntimeUpdated",
                "runtime",
                e.runtime.as_ref(),
            )?)?,
        }
        .into(),
        P::QuotaObserved(e) => ev::QuotaObserved {
            runtime_id: parse_id("QuotaObserved", "runtime_id", &e.runtime_id)?,
            windows: e
                .windows
                .iter()
                .map(quota_window_from_proto)
                .collect::<R<_>>()?,
            observed_at: required_timestamp(
                "QuotaObserved",
                "observed_at",
                e.observed_at.as_ref(),
            )?,
        }
        .into(),
        P::UsageReported(e) => ev::UsageReported {
            runtime_id: parse_id("UsageReported", "runtime_id", &e.runtime_id)?,
            turn_id: parse_id("UsageReported", "turn_id", &e.turn_id)?,
            usage: usage_from_proto(required("UsageReported", "usage", e.usage.as_ref())?),
        }
        .into(),
        P::RuntimeSessionForgetFailed(e) => {
            use pb::runtime_session_forget_failed::Outcome;
            const KIND: &str = "RuntimeSessionForgetFailed";
            ev::RuntimeSessionForgetFailed {
                conversation_id: parse_id(KIND, "conversation_id", &e.conversation_id)?,
                runtime_session_id: e.runtime_session_id,
                outcome: match required(KIND, "outcome", e.outcome)? {
                    Outcome::Refused(r) => ev::ForgetFailure::Refused { why: r.why },
                    Outcome::Failed(f) => ev::ForgetFailure::Failed { message: f.message },
                },
            }
            .into()
        }
        P::LocalImportPrompted(e) => ev::LocalImportPrompted {
            runtime_id: parse_id("LocalImportPrompted", "runtime_id", &e.runtime_id)?,
        }
        .into(),
        P::LocalConfigImported(e) => ev::LocalConfigImported {
            runtime_id: parse_id("LocalConfigImported", "runtime_id", &e.runtime_id)?,
            items: e
                .items
                .iter()
                .map(local_config_item_from_proto)
                .collect::<R<_>>()?,
        }
        .into(),
        P::McpSecretSet(e) => ev::McpSecretSet {
            mcp_server_id: McpServerId(e.mcp_server_id),
            key_name: e.key_name,
        }
        .into(),
        P::GlobalSettingsChanged(e) => ev::GlobalSettingsChanged {
            playful_ui: e.playful_ui,
            autostart: e.autostart,
            projects_root: e.projects_root.as_deref().map(path_from_proto),
        }
        .into(),
        P::RecoveryCompleted(e) => ev::RecoveryCompleted {
            interrupted_turn_ids: parse_ids(
                "RecoveryCompleted",
                "interrupted_turn_ids",
                &e.interrupted_turn_ids,
            )?,
        }
        .into(),
    })
}
