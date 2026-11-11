//! 实体与值对象的转换。函数成对出现：`x_to_proto` / `x_from_proto`。

use iwork_domain::action::{
    Action, ActionFingerprint, ActionFlag, ActionFlags, ActionKind, ActionTarget, CallShapeKind,
    CanonicalPath, SandboxPlacement, ToolRef,
};
use iwork_domain::approval::{
    AbandonReason, Approval, ApprovalCallRecord, ApprovalKind, ApprovalState, Decider, ReviewPhase,
    ReviewState, StaleReason, VoidReason,
};
use iwork_domain::approval_mode::{ApprovalMode, ProjectDefaultMode, RememberScope};
use iwork_domain::assistant::{Assistant, AssistantConfig, AssistantVersion, Avatar};
use iwork_domain::attachment::{Attachment, AttachmentOrigin};
use iwork_domain::common::{
    Actor, ArtifactArea, ArtifactRef, EffortLevel, ModelId, ModelSelection, ToolGroup, Usage,
};
use iwork_domain::conversation::{Conversation, ConversationKind, ConversationState};
use iwork_domain::event::{
    ApprovalModeChangeCause, ApprovalModeTarget, ChangeSummary, DraftDecision, RulesDeletedCause,
};
use iwork_domain::id::{ReviewId, Seq, TurnId};
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::project::{AiReviewSettings, PathStatus, Project, ProjectSettings, VcsKind};
use iwork_domain::rule::{
    AskReason, DenyReason, RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleScope,
    RuleSetVersion, RuleSource, Verdict,
};
use iwork_domain::runtime::{
    LocalConfigItem, LocalConfigKind, LoginInfo, QuotaWindow, Runtime, RuntimeKind, RuntimeState,
};
use iwork_domain::security_list::SecurityList;
use iwork_domain::turn::{InterruptReason, Turn, TurnKind, TurnState};
use iwork_domain::user_action::{ExitStatus, Proposal, UserActionCard, UserActionState};
use iwork_proto::pb;

use super::{
    ConvertError, enum_pair, ids_to_strings, opt_timestamp, parse_id, parse_ids, parse_opt_id,
    path_from_proto, path_to_proto, required, required_timestamp, sha256_from_bytes,
    sha256_from_hex, sha256_to_proto, timestamp_to_proto,
};

type R<T> = Result<T, ConvertError>;

// ---------- 无载荷枚举 ----------

enum_pair!(approval_mode_to_proto, approval_mode_from_proto, ApprovalMode, pb::ApprovalMode, {
    ApprovalMode::Manual => pb::ApprovalMode::Manual,
    ApprovalMode::Auto => pb::ApprovalMode::Auto,
    ApprovalMode::FullAccess => pb::ApprovalMode::FullAccess,
});
// 项目默认档位在 proto 里共用 ApprovalMode；FULL_ACCESS 解不出来（项目默认不能全开）
enum_pair!(project_default_mode_to_proto, project_default_mode_from_proto, ProjectDefaultMode, pb::ApprovalMode, {
    ProjectDefaultMode::Manual => pb::ApprovalMode::Manual,
    ProjectDefaultMode::Auto => pb::ApprovalMode::Auto,
});
enum_pair!(remember_scope_to_proto, remember_scope_from_proto, RememberScope, pb::RememberScope, {
    RememberScope::Once => pb::RememberScope::Once,
    RememberScope::Conversation => pb::RememberScope::Conversation,
    RememberScope::Project => pb::RememberScope::Project,
    RememberScope::Forever => pb::RememberScope::Forever,
});
enum_pair!(interrupt_reason_to_proto, interrupt_reason_from_proto, InterruptReason, pb::InterruptReason, {
    InterruptReason::Crash => pb::InterruptReason::Crash,
    InterruptReason::ApprovalWaitLimit => pb::InterruptReason::ApprovalWaitLimit,
});
enum_pair!(decider_to_proto, decider_from_proto, Decider, pb::ApprovalDecider, {
    Decider::User => pb::ApprovalDecider::User,
    Decider::ModeSwitch => pb::ApprovalDecider::ModeSwitch,
    Decider::AiReview => pb::ApprovalDecider::AiReview,
});
enum_pair!(void_reason_to_proto, void_reason_from_proto, VoidReason, pb::VoidReason, {
    VoidReason::TurnCancelled => pb::VoidReason::TurnCancelled,
    VoidReason::ConversationDeleted => pb::VoidReason::ConversationDeleted,
    VoidReason::TurnAbandonedAfterInterrupt => pb::VoidReason::TurnAbandonedAfterInterrupt,
    VoidReason::AssistantDeleted => pb::VoidReason::AssistantDeleted,
    VoidReason::TurnFailed => pb::VoidReason::TurnFailed,
});
enum_pair!(review_phase_to_proto, review_phase_from_proto, ReviewPhase, pb::ReviewState, {
    ReviewPhase::NotReviewed => pb::ReviewState::NotReviewed,
    ReviewPhase::InProgress => pb::ReviewState::InProgress,
    ReviewPhase::Passed => pb::ReviewState::Passed,
    ReviewPhase::Failed => pb::ReviewState::Failed,
    ReviewPhase::Errored => pb::ReviewState::Errored,
    ReviewPhase::TimedOut => pb::ReviewState::TimedOut,
    ReviewPhase::Abandoned => pb::ReviewState::Abandoned,
});
enum_pair!(stale_reason_to_proto, stale_reason_from_proto, StaleReason, pb::StaleReason, {
    StaleReason::ReviewIdMismatch => pb::StaleReason::ReviewIdMismatch,
    StaleReason::NotInProgress => pb::StaleReason::NotInProgress,
    StaleReason::ModeNotAuto => pb::StaleReason::ModeNotAuto,
});
enum_pair!(message_status_to_proto, message_status_from_proto, MessageStatus, pb::MessageStatus, {
    MessageStatus::Sent => pb::MessageStatus::Sent,
    MessageStatus::Streaming => pb::MessageStatus::Streaming,
    MessageStatus::Completed => pb::MessageStatus::Completed,
    MessageStatus::Interrupted => pb::MessageStatus::Interrupted,
    MessageStatus::Failed => pb::MessageStatus::Failed,
});
enum_pair!(conversation_state_to_proto, conversation_state_from_proto, ConversationState, pb::ConversationState, {
    ConversationState::Active => pb::ConversationState::Active,
    ConversationState::Deleting => pb::ConversationState::Deleting,
    ConversationState::Deleted => pb::ConversationState::Deleted,
});
enum_pair!(path_status_to_proto, path_status_from_proto, PathStatus, pb::PathStatus, {
    PathStatus::Ok => pb::PathStatus::Ok,
    PathStatus::Missing => pb::PathStatus::Missing,
});
enum_pair!(tool_group_to_proto, tool_group_from_proto, ToolGroup, pb::ToolGroup, {
    ToolGroup::ReadAndSearch => pb::ToolGroup::ReadAndSearch,
    ToolGroup::EditFiles => pb::ToolGroup::EditFiles,
    ToolGroup::RunCommands => pb::ToolGroup::RunCommands,
    ToolGroup::Web => pb::ToolGroup::Web,
});
enum_pair!(security_list_to_proto, security_list_from_proto, SecurityList, pb::SecurityList, {
    SecurityList::CredentialCommands => pb::SecurityList::CredentialCommands,
    SecurityList::SensitiveDirs => pb::SecurityList::SensitiveDirs,
    SecurityList::WritableDirs => pb::SecurityList::WritableDirs,
});
enum_pair!(draft_decision_to_proto, draft_decision_from_proto, DraftDecision, pb::DraftDecision, {
    DraftDecision::Accept => pb::DraftDecision::Accept,
    DraftDecision::Discard => pb::DraftDecision::Discard,
});
enum_pair!(artifact_area_to_proto, artifact_area_from_proto, ArtifactArea, pb::ArtifactArea, {
    ArtifactArea::Artifacts => pb::ArtifactArea::Artifacts,
    ArtifactArea::Attachments => pb::ArtifactArea::Attachments,
    ArtifactArea::Private => pb::ArtifactArea::Private,
});
enum_pair!(turn_kind_to_proto, turn_kind_from_proto, TurnKind, pb::TurnKind, {
    TurnKind::Assistant => pb::TurnKind::Assistant,
    TurnKind::SystemAiReview => pb::TurnKind::SystemAiReview,
    TurnKind::SystemAgentsMdDraft => pb::TurnKind::SystemAgentsMdDraft,
});
enum_pair!(action_kind_to_proto, action_kind_from_proto, ActionKind, pb::ActionKind, {
    ActionKind::Read => pb::ActionKind::Read,
    ActionKind::Write => pb::ActionKind::Write,
    ActionKind::Execute => pb::ActionKind::Execute,
    ActionKind::OutsideSandbox => pb::ActionKind::OutsideSandbox,
    ActionKind::WriteAutorunConfig => pb::ActionKind::WriteAutorunConfig,
    ActionKind::Delete => pb::ActionKind::Delete,
    ActionKind::Push => pb::ActionKind::Push,
    ActionKind::Publish => pb::ActionKind::Publish,
    ActionKind::Network => pb::ActionKind::Network,
    ActionKind::McpCall => pb::ActionKind::McpCall,
    ActionKind::Unclassified => pb::ActionKind::Unclassified,
});
enum_pair!(action_flag_to_proto, action_flag_from_proto, ActionFlag, pb::ActionFlag, {
    ActionFlag::OutOfBounds => pb::ActionFlag::OutOfBounds,
    ActionFlag::BypassSandboxRequested => pb::ActionFlag::BypassSandboxRequested,
    ActionFlag::OpaqueScript => pb::ActionFlag::OpaqueScript,
    ActionFlag::ArgumentPath => pb::ActionFlag::ArgumentPath,
    ActionFlag::ResourceLimitHit => pb::ActionFlag::ResourceLimitHit,
    ActionFlag::Unlexable => pb::ActionFlag::Unlexable,
    ActionFlag::PathUnresolved => pb::ActionFlag::PathUnresolved,
});
enum_pair!(call_shape_to_proto, call_shape_from_proto, CallShapeKind, pb::CallShape, {
    CallShapeKind::Tool => pb::CallShape::Tool,
    CallShapeKind::SingleSimpleCommand => pb::CallShape::SingleSimpleCommand,
    CallShapeKind::Compound => pb::CallShape::Compound,
    CallShapeKind::Unparsable => pb::CallShape::Unparsable,
});
enum_pair!(sandbox_placement_to_proto, sandbox_placement_from_proto, SandboxPlacement, pb::SandboxPlacement, {
    SandboxPlacement::Inside => pb::SandboxPlacement::Inside,
    SandboxPlacement::Outside => pb::SandboxPlacement::Outside,
    SandboxPlacement::Ambiguous => pb::SandboxPlacement::Ambiguous,
});
enum_pair!(approval_kind_to_proto, approval_kind_from_proto, ApprovalKind, pb::ApprovalKind, {
    ApprovalKind::Ask => pb::ApprovalKind::Ask,
    ApprovalKind::OutsideSandbox => pb::ApprovalKind::OutsideSandbox,
});
enum_pair!(ask_reason_to_proto, ask_reason_from_proto, AskReason, pb::AskReason, {
    AskReason::Default => pb::AskReason::Default,
    AskReason::OutOfBounds => pb::AskReason::OutOfBounds,
    AskReason::MustAsk => pb::AskReason::MustAsk,
    AskReason::Unclassified => pb::AskReason::Unclassified,
    AskReason::McpNotReadOnly => pb::AskReason::McpNotReadOnly,
    AskReason::UnparsableCommand => pb::AskReason::UnparsableCommand,
    AskReason::ForcedAfterDenial => pb::AskReason::ForcedAfterDenial,
});
enum_pair!(rule_layer_to_proto, rule_layer_from_proto, RuleLayer, pb::RuleLayer, {
    RuleLayer::Baseline => pb::RuleLayer::Baseline,
    RuleLayer::DenialMemory => pb::RuleLayer::DenialMemory,
    RuleLayer::ToolConfig => pb::RuleLayer::ToolConfig,
    RuleLayer::UserDeny => pb::RuleLayer::UserDeny,
    RuleLayer::MustAsk => pb::RuleLayer::MustAsk,
    RuleLayer::Remembered => pb::RuleLayer::Remembered,
    RuleLayer::ProjectAllow => pb::RuleLayer::ProjectAllow,
    RuleLayer::Default => pb::RuleLayer::Default,
    RuleLayer::CredentialCommands => pb::RuleLayer::CredentialCommands,
    RuleLayer::AutorunConfig => pb::RuleLayer::AutorunConfig,
    RuleLayer::SensitiveDirs => pb::RuleLayer::SensitiveDirs,
    RuleLayer::WritableCache => pb::RuleLayer::WritableCache,
    RuleLayer::WritableDirs => pb::RuleLayer::WritableDirs,
    RuleLayer::RejectList => pb::RuleLayer::RejectList,
});
enum_pair!(rule_effect_to_proto, rule_effect_from_proto, RuleEffect, pb::RuleEffect, {
    RuleEffect::Allow => pb::RuleEffect::Allow,
    RuleEffect::Ask => pb::RuleEffect::Ask,
    RuleEffect::Deny => pb::RuleEffect::Deny,
    RuleEffect::UserActionEligible => pb::RuleEffect::UserActionEligible,
    RuleEffect::OutsideSandbox => pb::RuleEffect::OutsideSandbox,
});
enum_pair!(runtime_state_to_proto, runtime_state_from_proto, RuntimeState, pb::RuntimeState, {
    RuntimeState::Missing => pb::RuntimeState::Missing,
    RuntimeState::NotLoggedIn => pb::RuntimeState::NotLoggedIn,
    RuntimeState::ReadyVerified => pb::RuntimeState::ReadyVerified,
    RuntimeState::ReadyUnverified => pb::RuntimeState::ReadyUnverified,
    RuntimeState::Incompatible => pb::RuntimeState::Incompatible,
});
enum_pair!(local_config_kind_to_proto, local_config_kind_from_proto, LocalConfigKind, pb::LocalConfigKind, {
    LocalConfigKind::Mcp => pb::LocalConfigKind::Mcp,
    LocalConfigKind::Skill => pb::LocalConfigKind::Skill,
    LocalConfigKind::Plugin => pb::LocalConfigKind::Plugin,
});
enum_pair!(mode_change_cause_to_proto, mode_change_cause_from_proto, ApprovalModeChangeCause, pb::ApprovalModeChangeCause, {
    ApprovalModeChangeCause::User => pb::ApprovalModeChangeCause::User,
    ApprovalModeChangeCause::ProjectSwitch => pb::ApprovalModeChangeCause::ProjectSwitch,
    ApprovalModeChangeCause::NewConversation => pb::ApprovalModeChangeCause::NewConversation,
});
enum_pair!(rules_deleted_cause_to_proto, rules_deleted_cause_from_proto, RulesDeletedCause, pb::RulesDeletedCause, {
    RulesDeletedCause::ConversationDeleted => pb::RulesDeletedCause::ConversationDeleted,
    RulesDeletedCause::AssistantDeleted => pb::RulesDeletedCause::AssistantDeleted,
});

// ---------- 带载荷的状态 ----------

/// 回合状态：proto 用 `state` + `interrupt_reason` 两个字段，INTERRUPTED 时原因必填。
pub fn turn_state_to_proto(state: TurnState) -> (i32, Option<i32>) {
    use pb::TurnState as P;
    let (p, reason) = match state {
        TurnState::Queued => (P::Queued, None),
        TurnState::Running => (P::Running, None),
        TurnState::WaitingApproval => (P::WaitingApproval, None),
        TurnState::Cancelling => (P::Cancelling, None),
        TurnState::Interrupted { reason } => {
            (P::Interrupted, Some(interrupt_reason_to_proto(reason)))
        }
        TurnState::Completed => (P::Completed, None),
        TurnState::Cancelled => (P::Cancelled, None),
        TurnState::Failed => (P::Failed, None),
    };
    (p as i32, reason)
}

/// `reason_field` 是原因所在字段名，报错时点名。非 INTERRUPTED 时原因字段被忽略。
pub fn turn_state_from_proto(
    kind: &'static str,
    field: &'static str,
    reason_field: &'static str,
    state: i32,
    reason: Option<i32>,
) -> R<TurnState> {
    use pb::TurnState as P;
    Ok(match P::try_from(state) {
        Ok(P::Queued) => TurnState::Queued,
        Ok(P::Running) => TurnState::Running,
        Ok(P::WaitingApproval) => TurnState::WaitingApproval,
        Ok(P::Cancelling) => TurnState::Cancelling,
        Ok(P::Interrupted) => TurnState::Interrupted {
            reason: interrupt_reason_from_proto(
                kind,
                reason_field,
                required(kind, reason_field, reason)?,
            )?,
        },
        Ok(P::Completed) => TurnState::Completed,
        Ok(P::Cancelled) => TurnState::Cancelled,
        Ok(P::Failed) => TurnState::Failed,
        _ => {
            return Err(ConvertError::UnsupportedEnum {
                kind,
                field,
                value: state,
            });
        }
    })
}

/// 审批主状态 → (state, decider, void_reason)。
pub fn approval_state_to_proto(state: ApprovalState) -> (i32, Option<i32>, Option<i32>) {
    use pb::ApprovalState as P;
    match state {
        ApprovalState::Pending => (P::Pending as i32, None, None),
        ApprovalState::Approved { decider } => {
            (P::Approved as i32, Some(decider_to_proto(decider)), None)
        }
        ApprovalState::Denied => (P::Denied as i32, None, None),
        ApprovalState::Voided { reason } => {
            (P::Voided as i32, None, Some(void_reason_to_proto(reason)))
        }
    }
}

/// 审查子状态 → (review, review_id, reason)。
/// 「失去执行者」放弃时 `reason` 字段的固定文字（T07 Q9）。proto 没有放弃原因的枚举（方案定
/// proto 不变），用这段文字区分两种放弃；其余放弃都是离开自动，编码时不带文字。
pub const REVIEW_ABANDONED_EXECUTOR_LOST: &str = "后台服务重启，审查中止";

pub fn review_state_to_proto(review: &ReviewState) -> (i32, Option<String>, Option<String>) {
    let reason = match review {
        ReviewState::Passed { reason, .. } | ReviewState::Failed { reason, .. } => {
            Some(reason.clone())
        }
        ReviewState::Abandoned {
            reason: AbandonReason::ExecutorLost,
            ..
        } => Some(REVIEW_ABANDONED_EXECUTOR_LOST.to_owned()),
        _ => None,
    };
    (
        review_phase_to_proto(review.phase()),
        review.review_id().map(|id| id.to_string()),
        reason,
    )
}

/// 由 (review, review_id, reason) 拼回审查子状态。NOT_REVIEWED 时忽略 review_id；
/// 其余状态 review_id 必填；PASSED 兼容旧事件缺少 reason（空串）；FAILED 时 reason 必填；ABANDONED 时 reason 区分放弃原因
/// （见 [`REVIEW_ABANDONED_EXECUTOR_LOST`]）；其他状态的 reason 被忽略（domain 不存）。
pub fn review_state_from_proto(
    kind: &'static str,
    review: i32,
    review_id: Option<&str>,
    reason: Option<&str>,
) -> R<ReviewState> {
    let phase = review_phase_from_proto(kind, "review", review)?;
    if phase == ReviewPhase::NotReviewed {
        return Ok(ReviewState::NotReviewed);
    }
    let review_id = ReviewId(parse_id::<TurnId>(
        kind,
        "review_id",
        required(kind, "review_id", review_id)?,
    )?);
    Ok(match phase {
        ReviewPhase::NotReviewed => unreachable!("上面已返回"),
        ReviewPhase::InProgress => ReviewState::InProgress { review_id },
        ReviewPhase::Passed => ReviewState::Passed {
            review_id,
            reason: reason.unwrap_or_default().to_owned(),
        },
        ReviewPhase::Failed => ReviewState::Failed {
            review_id,
            reason: required(kind, "reason", reason)?.to_owned(),
        },
        ReviewPhase::Errored => ReviewState::Errored { review_id },
        ReviewPhase::TimedOut => ReviewState::TimedOut { review_id },
        ReviewPhase::Abandoned => ReviewState::Abandoned {
            review_id,
            // T07 之前只有切回手动一种放弃，旧事件的 reason 可能为空或任意说明文字
            reason: if reason == Some(REVIEW_ABANDONED_EXECUTOR_LOST) {
                AbandonReason::ExecutorLost
            } else {
                AbandonReason::ModeChangedFromAuto
            },
        },
    })
}

pub fn exit_status_to_proto(exit: ExitStatus) -> pb::ExitStatus {
    pb::ExitStatus {
        kind: Some(match exit {
            ExitStatus::Code(c) => pb::exit_status::Kind::Code(c),
            ExitStatus::Signal(s) => pb::exit_status::Kind::Signal(s),
        }),
    }
}

pub fn exit_status_from_proto(kind: &'static str, exit: Option<&pb::ExitStatus>) -> R<ExitStatus> {
    match required(kind, "exit", exit)?.kind {
        Some(pb::exit_status::Kind::Code(c)) => Ok(ExitStatus::Code(c)),
        Some(pb::exit_status::Kind::Signal(s)) => Ok(ExitStatus::Signal(s)),
        None => Err(ConvertError::MissingField {
            kind: "ExitStatus",
            field: "kind",
        }),
    }
}

/// 卡片状态在 proto 里拆成 state / exit / error / void_reason 四个字段。
pub struct UserActionStateFields {
    pub state: i32,
    pub exit: Option<pb::ExitStatus>,
    pub error: Option<String>,
    pub void_reason: Option<i32>,
}

pub fn user_action_state_to_proto(state: &UserActionState) -> UserActionStateFields {
    use pb::UserActionState as P;
    let mut f = UserActionStateFields {
        state: 0,
        exit: None,
        error: None,
        void_reason: None,
    };
    f.state = match state {
        UserActionState::Pending => P::Pending,
        UserActionState::Starting => P::Starting,
        UserActionState::Executing => P::Executing,
        UserActionState::Stopping => P::Stopping,
        UserActionState::Done { exit } => {
            f.exit = Some(exit_status_to_proto(*exit));
            P::Done
        }
        UserActionState::Failed { error } => {
            f.error = Some(error.clone());
            P::Failed
        }
        UserActionState::Unknown { error } => {
            f.error = Some(error.clone());
            P::Unknown
        }
        UserActionState::Ended => P::Ended,
        UserActionState::Dismissed => P::Dismissed,
        UserActionState::Voided { reason } => {
            f.void_reason = Some(void_reason_to_proto(*reason));
            P::Voided
        }
    } as i32;
    f
}

/// 状态需要的附带字段必填；不需要的附带字段被忽略。
pub fn user_action_state_from_proto(
    kind: &'static str,
    f: &UserActionStateFields,
) -> R<UserActionState> {
    use pb::UserActionState as P;
    let error = || required(kind, "error", f.error.clone());
    Ok(match P::try_from(f.state) {
        Ok(P::Pending) => UserActionState::Pending,
        Ok(P::Starting) => UserActionState::Starting,
        Ok(P::Executing) => UserActionState::Executing,
        Ok(P::Stopping) => UserActionState::Stopping,
        Ok(P::Done) => UserActionState::Done {
            exit: exit_status_from_proto(kind, f.exit.as_ref())?,
        },
        Ok(P::Failed) => UserActionState::Failed { error: error()? },
        Ok(P::Unknown) => UserActionState::Unknown { error: error()? },
        Ok(P::Ended) => UserActionState::Ended,
        Ok(P::Dismissed) => UserActionState::Dismissed,
        Ok(P::Voided) => UserActionState::Voided {
            reason: void_reason_from_proto(
                kind,
                "void_reason",
                required(kind, "void_reason", f.void_reason)?,
            )?,
        },
        _ => {
            return Err(ConvertError::UnsupportedEnum {
                kind,
                field: "state",
                value: f.state,
            });
        }
    })
}

// ---------- 值对象 ----------

pub fn model_selection_to_proto(m: &ModelSelection) -> pb::ModelSelection {
    pb::ModelSelection {
        model_id: m.model_id.0.clone(),
        effort: m.effort.as_ref().map(|e| e.0.clone()),
    }
}

pub fn model_selection_from_proto(m: &pb::ModelSelection) -> ModelSelection {
    ModelSelection {
        model_id: ModelId(m.model_id.clone()),
        effort: m.effort.clone().map(EffortLevel),
    }
}

pub fn required_model(
    kind: &'static str,
    field: &'static str,
    m: Option<&pb::ModelSelection>,
) -> R<ModelSelection> {
    required(kind, field, m).map(model_selection_from_proto)
}

pub fn usage_to_proto(u: &Usage) -> pb::Usage {
    pb::Usage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_tokens: u.cache_read_tokens,
        cache_write_tokens: u.cache_write_tokens,
        cost_usd: u.cost_usd,
    }
}

pub fn usage_from_proto(u: &pb::Usage) -> Usage {
    Usage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_tokens: u.cache_read_tokens,
        cache_write_tokens: u.cache_write_tokens,
        cost_usd: u.cost_usd,
    }
}

pub fn artifact_ref_to_proto(r: &ArtifactRef) -> pb::ArtifactRef {
    pb::ArtifactRef {
        sha256: sha256_to_proto(&r.sha256),
        area: artifact_area_to_proto(r.area),
        size: r.size,
    }
}

pub fn artifact_ref_from_proto(r: &pb::ArtifactRef) -> R<ArtifactRef> {
    Ok(ArtifactRef {
        sha256: sha256_from_hex("ArtifactRef", "sha256", &r.sha256)?,
        area: artifact_area_from_proto("ArtifactRef", "area", r.area)?,
        size: r.size,
    })
}

pub fn required_artifact(
    kind: &'static str,
    field: &'static str,
    r: Option<&pb::ArtifactRef>,
) -> R<ArtifactRef> {
    artifact_ref_from_proto(required(kind, field, r)?)
}

pub fn opt_artifact(r: Option<&pb::ArtifactRef>) -> R<Option<ArtifactRef>> {
    r.map(artifact_ref_from_proto).transpose()
}

pub fn change_summary_to_proto(c: &ChangeSummary) -> R<pb::ChangeSummary> {
    Ok(pb::ChangeSummary {
        files_changed: c.files_changed,
        insertions: c.insertions,
        deletions: c.deletions,
        paths: c
            .paths
            .iter()
            .map(|p| path_to_proto("ChangeSummary", "paths", p))
            .collect::<R<_>>()?,
    })
}

pub fn change_summary_from_proto(c: &pb::ChangeSummary) -> ChangeSummary {
    ChangeSummary {
        files_changed: c.files_changed,
        insertions: c.insertions,
        deletions: c.deletions,
        paths: c.paths.iter().map(|p| path_from_proto(p)).collect(),
    }
}

// ---------- 项目 ----------

pub fn project_settings_to_proto(s: &ProjectSettings) -> pb::ProjectSettings {
    pb::ProjectSettings {
        default_approval_mode: project_default_mode_to_proto(s.default_approval_mode),
        default_model: s.default_model.as_ref().map(model_selection_to_proto),
        ai_review: Some(pb::AiReviewSettings {
            enabled: s.ai_review.enabled,
            runtime_id: s.ai_review.runtime_id.map(|id| id.to_string()),
            model: s.ai_review.model.as_ref().map(model_selection_to_proto),
            timeout_secs: s.ai_review.timeout_secs,
        }),
    }
}

pub fn project_settings_from_proto(s: &pb::ProjectSettings) -> R<ProjectSettings> {
    const K: &str = "ProjectSettings";
    let ai = required(K, "ai_review", s.ai_review.as_ref())?;
    Ok(ProjectSettings {
        default_approval_mode: project_default_mode_from_proto(
            K,
            "default_approval_mode",
            s.default_approval_mode,
        )?,
        default_model: s.default_model.as_ref().map(model_selection_from_proto),
        ai_review: AiReviewSettings {
            enabled: ai.enabled,
            runtime_id: parse_opt_id("AiReviewSettings", "runtime_id", ai.runtime_id.as_deref())?,
            model: ai.model.as_ref().map(model_selection_from_proto),
            timeout_secs: ai.timeout_secs,
        },
    })
}

/// `Project.unread` 是投影字段，domain 没有：转 proto 填 0，转 domain 忽略。
pub fn project_to_proto(p: &Project) -> R<pb::Project> {
    Ok(pb::Project {
        id: p.id.to_string(),
        root_path: path_to_proto("Project", "root_path", &p.root_path)?,
        vcs_kind: p.vcs_kind.0.clone(),
        name: p.name.clone(),
        pinned: p.pinned,
        last_active_at: p.last_active_at.map(timestamp_to_proto),
        path_status: path_status_to_proto(p.path_status),
        settings: Some(project_settings_to_proto(&p.settings)),
        unread: 0,
    })
}

pub fn project_from_proto(p: &pb::Project) -> R<Project> {
    const K: &str = "Project";
    Ok(Project {
        id: parse_id(K, "id", &p.id)?,
        root_path: path_from_proto(&p.root_path),
        vcs_kind: VcsKind(p.vcs_kind.clone()),
        name: p.name.clone(),
        pinned: p.pinned,
        last_active_at: opt_timestamp(K, "last_active_at", p.last_active_at.as_ref())?,
        path_status: path_status_from_proto(K, "path_status", p.path_status)?,
        settings: project_settings_from_proto(required(K, "settings", p.settings.as_ref())?)?,
    })
}

// ---------- 助手 ----------

pub fn assistant_config_to_proto(c: &AssistantConfig) -> pb::AssistantConfig {
    pb::AssistantConfig {
        name: c.name.clone(),
        avatar: Some(pb::Avatar {
            initial: c.avatar.initial.clone(),
            color: c.avatar.color.clone(),
        }),
        prompt: c.prompt.clone(),
        runtime_id: c.runtime_id.to_string(),
        model: Some(model_selection_to_proto(&c.model)),
        disabled_groups: c
            .disabled_groups
            .iter()
            .map(|g| tool_group_to_proto(*g))
            .collect(),
    }
}

pub fn assistant_config_from_proto(c: &pb::AssistantConfig) -> R<AssistantConfig> {
    const K: &str = "AssistantConfig";
    let avatar = required(K, "avatar", c.avatar.as_ref())?;
    Ok(AssistantConfig {
        name: c.name.clone(),
        avatar: Avatar {
            initial: avatar.initial.clone(),
            color: avatar.color.clone(),
        },
        prompt: c.prompt.clone(),
        runtime_id: parse_id(K, "runtime_id", &c.runtime_id)?,
        model: required_model(K, "model", c.model.as_ref())?,
        disabled_groups: c
            .disabled_groups
            .iter()
            .map(|g| tool_group_from_proto(K, "disabled_groups", *g))
            .collect::<R<_>>()?,
    })
}

/// `Assistant.updated_at` 对应当前版本的 `created_at`（版本创建即助手最近一次更新）。
pub fn assistant_to_proto(a: &Assistant) -> pb::Assistant {
    pb::Assistant {
        id: a.id.to_string(),
        builtin: a.builtin,
        version: a.current.version,
        config: Some(assistant_config_to_proto(&a.current.config)),
        updated_at: Some(timestamp_to_proto(a.current.created_at)),
    }
}

pub fn assistant_from_proto(a: &pb::Assistant) -> R<Assistant> {
    const K: &str = "Assistant";
    Ok(Assistant {
        id: parse_id(K, "id", &a.id)?,
        builtin: a.builtin,
        current: AssistantVersion {
            version: a.version,
            config: assistant_config_from_proto(required(K, "config", a.config.as_ref())?)?,
            created_at: required_timestamp(K, "updated_at", a.updated_at.as_ref())?,
        },
    })
}

// ---------- 对话与消息 ----------

/// `Conversation.unread` 是投影字段：转 proto 填 0，转 domain 忽略。
pub fn conversation_to_proto(c: &Conversation) -> pb::Conversation {
    pb::Conversation {
        id: c.id.to_string(),
        kind: Some(match c.kind {
            ConversationKind::Project { project_id } => {
                pb::conversation::Kind::Project(pb::ProjectConversation {
                    project_id: project_id.to_string(),
                })
            }
            ConversationKind::AssistantDm {
                assistant_id,
                context_project,
            } => pb::conversation::Kind::AssistantDm(pb::AssistantDm {
                assistant_id: assistant_id.to_string(),
                context_project_id: context_project.map(|p| p.to_string()),
            }),
        }),
        state: conversation_state_to_proto(c.state),
        last_read_seq: c.last_read_seq.0,
        unread: 0,
        model_override: c.model_override.as_ref().map(model_selection_to_proto),
        approval_mode: c.approval_mode.map(approval_mode_to_proto),
        updated_at: Some(timestamp_to_proto(c.updated_at)),
    }
}

pub fn conversation_from_proto(c: &pb::Conversation) -> R<Conversation> {
    const K: &str = "Conversation";
    Ok(Conversation {
        id: parse_id(K, "id", &c.id)?,
        kind: match required(K, "kind", c.kind.as_ref())? {
            pb::conversation::Kind::Project(p) => ConversationKind::Project {
                project_id: parse_id("ProjectConversation", "project_id", &p.project_id)?,
            },
            pb::conversation::Kind::AssistantDm(d) => ConversationKind::AssistantDm {
                assistant_id: parse_id("AssistantDm", "assistant_id", &d.assistant_id)?,
                context_project: parse_opt_id(
                    "AssistantDm",
                    "context_project_id",
                    d.context_project_id.as_deref(),
                )?,
            },
        },
        state: conversation_state_from_proto(K, "state", c.state)?,
        last_read_seq: Seq(c.last_read_seq),
        model_override: c.model_override.as_ref().map(model_selection_from_proto),
        approval_mode: c
            .approval_mode
            .map(|m| approval_mode_from_proto(K, "approval_mode", m))
            .transpose()?,
        updated_at: required_timestamp(K, "updated_at", c.updated_at.as_ref())?,
    })
}

pub fn message_body_to_proto(b: &MessageBody) -> pb::MessageBody {
    pb::MessageBody {
        content: Some(match b {
            MessageBody::Inline(text) => pb::message_body::Content::InlineText(text.clone()),
            MessageBody::Artifact(r) => {
                pb::message_body::Content::Artifact(artifact_ref_to_proto(r))
            }
        }),
    }
}

/// `MessageBody` 在，`content` 就必须在：缺了是写坏的数据，点名报错，不当成空串。
/// 显式的空内联正文 `Inline("")` 合法。
pub fn message_body_from_proto(b: &pb::MessageBody) -> R<MessageBody> {
    Ok(
        match required("MessageBody", "content", b.content.as_ref())? {
            pb::message_body::Content::InlineText(text) => MessageBody::Inline(text.clone()),
            pb::message_body::Content::Artifact(r) => {
                MessageBody::Artifact(artifact_ref_from_proto(r)?)
            }
        },
    )
}

pub fn message_to_proto(m: &Message) -> pb::Message {
    pb::Message {
        id: m.id.to_string(),
        conversation_id: m.conversation_id.to_string(),
        author: Some(pb::Author {
            kind: Some(match &m.author {
                Author::User => pb::author::Kind::User(pb::UserAuthor {}),
                Author::System => pb::author::Kind::System(pb::SystemAuthor {}),
                Author::Assistant { id, name_snapshot } => {
                    pb::author::Kind::Assistant(pb::AssistantAuthor {
                        assistant_id: id.to_string(),
                        name_snapshot: name_snapshot.clone(),
                    })
                }
            }),
        }),
        context_project_id: m.context_project_id.map(|p| p.to_string()),
        status: message_status_to_proto(m.status),
        body: Some(message_body_to_proto(&m.body)),
        attachment_ids: ids_to_strings(&m.attachments),
        created_at: Some(timestamp_to_proto(m.created_at)),
        turn_id: m.turn_id.map(|t| t.to_string()),
    }
}

pub fn message_from_proto(m: &pb::Message) -> R<Message> {
    const K: &str = "Message";
    let author = match required(K, "author", m.author.as_ref().and_then(|a| a.kind.as_ref()))? {
        pb::author::Kind::User(_) => Author::User,
        pb::author::Kind::System(_) => Author::System,
        pb::author::Kind::Assistant(a) => Author::Assistant {
            id: parse_id("AssistantAuthor", "assistant_id", &a.assistant_id)?,
            name_snapshot: a.name_snapshot.clone(),
        },
    };
    let status = message_status_from_proto(K, "status", m.status)?;
    // 只有流式中的助手消息允许没有正文（正文结束时由 MessageCompleted 写入）；其余缺正文点名报错
    let body = match (&m.body, &author, status) {
        (Some(body), _, _) => message_body_from_proto(body)?,
        (None, Author::Assistant { .. }, MessageStatus::Streaming) => {
            MessageBody::Inline(String::new())
        }
        (None, _, _) => {
            return Err(ConvertError::MissingField {
                kind: K,
                field: "body",
            });
        }
    };
    Ok(Message {
        id: parse_id(K, "id", &m.id)?,
        conversation_id: parse_id(K, "conversation_id", &m.conversation_id)?,
        author,
        context_project_id: parse_opt_id(K, "context_project_id", m.context_project_id.as_deref())?,
        status,
        body,
        attachments: parse_ids(K, "attachment_ids", &m.attachment_ids)?,
        created_at: required_timestamp(K, "created_at", m.created_at.as_ref())?,
        turn_id: parse_opt_id(K, "turn_id", m.turn_id.as_deref())?,
    })
}

// ---------- 回合 ----------

/// `Turn.state_reason` 是给人看的说明，domain 的回合不存（说明随 `TurnStateChanged.reason` 记录）：
/// 转 proto 不设，转 domain 忽略。
pub fn turn_to_proto(t: &Turn) -> pb::Turn {
    let (state, interrupt_reason) = turn_state_to_proto(t.state);
    pb::Turn {
        id: t.id.to_string(),
        conversation_id: t.conversation_id.to_string(),
        assistant_id: t.assistant_id.to_string(),
        assistant_version: t.assistant_version,
        runtime_id: t.runtime_id.to_string(),
        model: Some(model_selection_to_proto(&t.model)),
        runtime_session_id: t.runtime_session_id.clone(),
        state,
        state_reason: None,
        started_at: Some(timestamp_to_proto(t.started_at)),
        ended_at: t.ended_at.map(timestamp_to_proto),
        kind: turn_kind_to_proto(t.kind),
        interrupt_reason,
    }
}

pub fn turn_from_proto(t: &pb::Turn) -> R<Turn> {
    const K: &str = "Turn";
    Ok(Turn {
        id: parse_id(K, "id", &t.id)?,
        conversation_id: parse_id(K, "conversation_id", &t.conversation_id)?,
        assistant_id: parse_id(K, "assistant_id", &t.assistant_id)?,
        assistant_version: t.assistant_version,
        runtime_id: parse_id(K, "runtime_id", &t.runtime_id)?,
        model: required_model(K, "model", t.model.as_ref())?,
        runtime_session_id: t.runtime_session_id.clone(),
        kind: turn_kind_from_proto(K, "kind", t.kind)?,
        state: turn_state_from_proto(K, "state", "interrupt_reason", t.state, t.interrupt_reason)?,
        started_at: required_timestamp(K, "started_at", t.started_at.as_ref())?,
        ended_at: opt_timestamp(K, "ended_at", t.ended_at.as_ref())?,
    })
}

// ---------- 动作与审批 ----------

fn canonical_to_proto(kind: &'static str, field: &'static str, p: &CanonicalPath) -> R<String> {
    path_to_proto(kind, field, &p.0)
}

fn canonical_from_proto(text: &str) -> CanonicalPath {
    CanonicalPath(path_from_proto(text))
}

pub fn action_target_to_proto(t: &ActionTarget) -> R<pb::ActionTarget> {
    use pb::action_target::Kind;
    const K: &str = "ActionTarget";
    Ok(pb::ActionTarget {
        kind: Some(match t {
            ActionTarget::Path(p) => Kind::Path(canonical_to_proto(K, "path", p)?),
            ActionTarget::Paths(ps) => Kind::Paths(pb::PathList {
                paths: ps
                    .iter()
                    .map(|p| canonical_to_proto(K, "paths", p))
                    .collect::<R<_>>()?,
            }),
            ActionTarget::Command {
                program,
                subcommand,
                argv,
            } => Kind::Command(pb::CommandTarget {
                program: program.clone(),
                subcommand: subcommand.clone(),
                argv: argv.clone(),
            }),
            ActionTarget::ToolName(n) => Kind::ToolName(n.clone()),
            ActionTarget::Remote(r) => Kind::Remote(r.clone()),
            ActionTarget::None => Kind::None(pb::NoTarget {}),
        }),
    })
}

pub fn action_target_from_proto(
    kind: &'static str,
    t: Option<&pb::ActionTarget>,
) -> R<ActionTarget> {
    use pb::action_target::Kind;
    Ok(
        match required(
            "ActionTarget",
            "kind",
            required(kind, "target", t)?.kind.as_ref(),
        )? {
            Kind::Path(p) => ActionTarget::Path(canonical_from_proto(p)),
            Kind::Paths(ps) => {
                ActionTarget::Paths(ps.paths.iter().map(|p| canonical_from_proto(p)).collect())
            }
            Kind::Command(c) => ActionTarget::Command {
                program: c.program.clone(),
                subcommand: c.subcommand.clone(),
                argv: c.argv.clone(),
            },
            Kind::ToolName(n) => ActionTarget::ToolName(n.clone()),
            Kind::Remote(r) => ActionTarget::Remote(r.clone()),
            Kind::None(_) => ActionTarget::None,
        },
    )
}

pub fn action_to_proto(a: &Action) -> R<pb::ActionSummary> {
    Ok(pb::ActionSummary {
        kind: action_kind_to_proto(a.kind),
        target: Some(action_target_to_proto(&a.target)?),
        tool_name: a.tool.0.clone(),
        raw_summary: a.raw_summary.clone(),
        out_of_bounds: a.flags.out_of_bounds,
        bypass_sandbox_requested: a.flags.bypass_sandbox_requested,
        opaque_script: a.flags.opaque_script,
        argument_path: a.flags.argument_path,
        resource_limit_hit: a.flags.resource_limit_hit,
        unlexable: a.flags.unlexable,
        path_unresolved: a.flags.path_unresolved,
    })
}

pub fn action_from_proto(kind: &'static str, a: Option<&pb::ActionSummary>) -> R<Action> {
    const K: &str = "ActionSummary";
    let a = required(kind, "action", a)?;
    Ok(Action {
        kind: action_kind_from_proto(K, "kind", a.kind)?,
        target: action_target_from_proto(K, a.target.as_ref())?,
        tool: ToolRef(a.tool_name.clone()),
        raw_summary: a.raw_summary.clone(),
        flags: ActionFlags {
            out_of_bounds: a.out_of_bounds,
            bypass_sandbox_requested: a.bypass_sandbox_requested,
            opaque_script: a.opaque_script,
            argument_path: a.argument_path,
            resource_limit_hit: a.resource_limit_hit,
            unlexable: a.unlexable,
            path_unresolved: a.path_unresolved,
        },
    })
}

/// `Approval.decided_at`：domain 的审批不存决定时间（投影从 `ApprovalDecided` 事件的时间取），
/// 转 proto 不设，转 domain 忽略。主状态里的决定者与作废原因在 proto `Approval` 上没有字段，
/// 由 `ApprovalDecided` / `ApprovalVoided` 事件携带；所以经 `Approval` 消息只能无损往返
/// `Pending` 的审批（`ApprovalRequested` 只会带待审批）。终态审批转 proto 时报错，防止静默丢信息。
pub fn approval_to_proto(a: &Approval) -> R<pb::Approval> {
    const K: &str = "Approval";
    let (state, decider, void_reason) = approval_state_to_proto(a.state);
    if decider.is_some() || void_reason.is_some() {
        return Err(ConvertError::InvalidValue {
            kind: K,
            field: "state",
            reason: format!(
                "Approval 消息装不下 {:?} 的载荷，只能带待审批或已拒绝",
                a.state
            ),
        });
    }
    let (review, review_id, review_reason) = review_state_to_proto(&a.review);
    Ok(pb::Approval {
        id: a.id.to_string(),
        turn_id: a.turn_id.to_string(),
        conversation_id: a.conversation_id.to_string(),
        kind: approval_kind_to_proto(a.kind),
        action: Some(action_to_proto(&a.action)?),
        call: a.call.as_deref().map(call_record_to_proto).transpose()?,
        matched_rule_ids: ids_to_strings(&a.matched_rules),
        ruleset_version: a.ruleset_version.0.clone(),
        state,
        review,
        review_reason,
        offered_remember: a
            .offered_remember
            .iter()
            .map(|r| remember_scope_to_proto(*r))
            .collect(),
        remember_label: a.remember_label.clone(),
        requested_at: Some(timestamp_to_proto(a.requested_at)),
        decided_at: None,
        review_id,
    })
}

pub fn approval_from_proto(a: &pb::Approval) -> R<Approval> {
    const K: &str = "Approval";
    let state = match pb::ApprovalState::try_from(a.state) {
        Ok(pb::ApprovalState::Pending) => ApprovalState::Pending,
        Ok(pb::ApprovalState::Denied) => ApprovalState::Denied,
        _ => {
            return Err(ConvertError::UnsupportedEnum {
                kind: K,
                field: "state",
                value: a.state,
            });
        }
    };
    Ok(Approval {
        id: parse_id(K, "id", &a.id)?,
        turn_id: parse_id(K, "turn_id", &a.turn_id)?,
        conversation_id: parse_id(K, "conversation_id", &a.conversation_id)?,
        kind: approval_kind_from_proto(K, "kind", a.kind)?,
        action: action_from_proto(K, a.action.as_ref())?,
        call: a
            .call
            .as_ref()
            .map(|c| call_record_from_proto(c).map(Box::new))
            .transpose()?,
        matched_rules: parse_ids(K, "matched_rule_ids", &a.matched_rule_ids)?,
        ruleset_version: RuleSetVersion(a.ruleset_version.clone()),
        state,
        review: review_state_from_proto(
            K,
            a.review,
            a.review_id.as_deref(),
            a.review_reason.as_deref(),
        )?,
        offered_remember: a
            .offered_remember
            .iter()
            .map(|r| remember_scope_from_proto(K, "offered_remember", *r))
            .collect::<R<_>>()?,
        remember_label: a.remember_label.clone(),
        requested_at: required_timestamp(K, "requested_at", a.requested_at.as_ref())?,
    })
}

pub fn call_record_to_proto(c: &ApprovalCallRecord) -> R<pb::ApprovalCallRecord> {
    Ok(pb::ApprovalCallRecord {
        tool_name: c.tool.0.clone(),
        cwd: canonical_to_proto("ApprovalCallRecord", "cwd", &c.cwd)?,
        shape: call_shape_to_proto(c.shape),
        placement: sandbox_placement_to_proto(c.placement),
        bypass_sandbox_requested: c.bypass_sandbox_requested,
        actions: actions_to_proto(&c.actions)?,
        representative: c.representative,
        raw_input_digest: c.raw_input_digest.0.to_vec().into(),
        normalized_digest: c.normalized_digest.0.to_vec().into(),
    })
}

pub fn call_record_from_proto(c: &pb::ApprovalCallRecord) -> R<ApprovalCallRecord> {
    const K: &str = "ApprovalCallRecord";
    let actions = actions_from_proto(K, &c.actions)?;
    if c.representative as usize >= actions.len() {
        return Err(ConvertError::InvalidValue {
            kind: K,
            field: "representative",
            reason: format!(
                "代表动作下标 {} 超出子动作个数 {}",
                c.representative,
                actions.len()
            ),
        });
    }
    Ok(ApprovalCallRecord {
        tool: ToolRef(c.tool_name.clone()),
        cwd: canonical_from_proto(&c.cwd),
        shape: call_shape_from_proto(K, "shape", c.shape)?,
        placement: sandbox_placement_from_proto(K, "placement", c.placement)?,
        bypass_sandbox_requested: c.bypass_sandbox_requested,
        actions,
        representative: c.representative,
        raw_input_digest: sha256_from_bytes(K, "raw_input_digest", &c.raw_input_digest)?,
        normalized_digest: sha256_from_bytes(K, "normalized_digest", &c.normalized_digest)?,
    })
}

pub fn actions_to_proto(actions: &[Action]) -> R<Vec<pb::ActionSummary>> {
    actions.iter().map(action_to_proto).collect()
}

pub fn actions_from_proto(kind: &'static str, actions: &[pb::ActionSummary]) -> R<Vec<Action>> {
    actions
        .iter()
        .map(|a| action_from_proto(kind, Some(a)))
        .collect()
}

pub fn proposal_to_proto(p: &Proposal) -> R<pb::user_action_proposal::Content> {
    use pb::user_action_proposal::Content;
    Ok(match p {
        Proposal::Command { command, cwd } => Content::Command(pb::CommandProposal {
            command: command.clone(),
            cwd: canonical_to_proto("CommandProposal", "cwd", cwd)?,
        }),
        Proposal::Patch { path, old, new } => Content::Patch(pb::PatchProposal {
            path: canonical_to_proto("PatchProposal", "path", path)?,
            old_text: old.clone(),
            new_text: new.clone(),
        }),
    })
}

fn command_proposal_from_proto(c: &pb::CommandProposal) -> Proposal {
    Proposal::Command {
        command: c.command.clone(),
        cwd: canonical_from_proto(&c.cwd),
    }
}

fn patch_proposal_from_proto(p: &pb::PatchProposal) -> Proposal {
    Proposal::Patch {
        path: canonical_from_proto(&p.path),
        old: p.old_text.clone(),
        new: p.new_text.clone(),
    }
}

pub fn proposal_from_proto(
    kind: &'static str,
    p: Option<&pb::user_action_proposal::Content>,
) -> R<Proposal> {
    use pb::user_action_proposal::Content;
    Ok(match required(kind, "content", p)? {
        Content::Command(c) => command_proposal_from_proto(c),
        Content::Patch(p) => patch_proposal_from_proto(p),
    })
}

pub fn user_action_card_to_proto(c: &UserActionCard) -> R<pb::UserActionCard> {
    use pb::user_action_card::Content;
    let f = user_action_state_to_proto(&c.state);
    Ok(pb::UserActionCard {
        id: c.id.to_string(),
        turn_id: c.turn_id.to_string(),
        conversation_id: c.conversation_id.to_string(),
        content: Some(match proposal_to_proto(&c.proposal)? {
            pb::user_action_proposal::Content::Command(x) => Content::Command(x),
            pb::user_action_proposal::Content::Patch(x) => Content::Patch(x),
        }),
        reason: c.reason.clone(),
        proposal_hash: c.proposal_hash.0.to_vec().into(),
        state: f.state,
        exit: f.exit,
        output_artifact: c.output_artifact.as_ref().map(artifact_ref_to_proto),
        error: f.error,
        void_reason: f.void_reason,
    })
}

pub fn user_action_card_from_proto(c: &pb::UserActionCard) -> R<UserActionCard> {
    use pb::user_action_card::Content;
    const K: &str = "UserActionCard";
    Ok(UserActionCard {
        id: parse_id(K, "id", &c.id)?,
        turn_id: parse_id(K, "turn_id", &c.turn_id)?,
        conversation_id: parse_id(K, "conversation_id", &c.conversation_id)?,
        proposal: match required(K, "content", c.content.as_ref())? {
            Content::Command(x) => command_proposal_from_proto(x),
            Content::Patch(x) => patch_proposal_from_proto(x),
        },
        reason: c.reason.clone(),
        proposal_hash: sha256_from_bytes(K, "proposal_hash", &c.proposal_hash)?,
        state: user_action_state_from_proto(
            K,
            &UserActionStateFields {
                state: c.state,
                exit: c.exit,
                error: c.error.clone(),
                void_reason: c.void_reason,
            },
        )?,
        output_artifact: opt_artifact(c.output_artifact.as_ref())?,
    })
}

pub fn fingerprint_to_proto(f: &ActionFingerprint) -> R<pb::ActionFingerprint> {
    Ok(pb::ActionFingerprint {
        kind: action_kind_to_proto(f.kind()),
        target: Some(action_target_to_proto(f.target())?),
    })
}

pub fn verdict_to_proto(v: &Verdict) -> R<pb::Verdict> {
    use pb::deny_reason::Kind as D;
    use pb::verdict::Kind;
    Ok(pb::Verdict {
        kind: Some(match v {
            Verdict::Allow => Kind::Allow(pb::VerdictAllow {}),
            Verdict::Ask(r) => Kind::Ask(ask_reason_to_proto(*r)),
            Verdict::OutsideSandbox(r) => Kind::OutsideSandbox(ask_reason_to_proto(*r)),
            Verdict::UserAction(p) => Kind::UserAction(pb::UserActionProposal {
                content: Some(proposal_to_proto(p)?),
            }),
            Verdict::Deny(d) => Kind::Deny(pb::DenyReason {
                kind: Some(match d {
                    DenyReason::Baseline(id) => D::BaselineRuleId(id.to_string()),
                    DenyReason::DenialMemory(f) => D::DenialMemory(fingerprint_to_proto(f)?),
                    DenyReason::ToolDisabled => D::ToolDisabled(pb::ToolDisabled {}),
                    DenyReason::DenyRule(id) => D::DenyRuleId(id.to_string()),
                    DenyReason::SystemTurnOutOfToolSet => {
                        D::SystemTurnOutOfToolSet(pb::SystemTurnOutOfToolSet {})
                    }
                    DenyReason::ToolSetExcluded => D::ToolSetExcluded(pb::ToolSetExcluded {}),
                }),
            }),
        }),
    })
}

pub fn verdict_from_proto(kind: &'static str, v: Option<&pb::Verdict>) -> R<Verdict> {
    use pb::deny_reason::Kind as D;
    use pb::verdict::Kind;
    const K: &str = "Verdict";
    Ok(
        match required(K, "kind", required(kind, "verdict", v)?.kind.as_ref())? {
            Kind::Allow(_) => Verdict::Allow,
            Kind::Ask(r) => Verdict::Ask(ask_reason_from_proto(K, "ask", *r)?),
            Kind::OutsideSandbox(r) => {
                Verdict::OutsideSandbox(ask_reason_from_proto(K, "outside_sandbox", *r)?)
            }
            Kind::UserAction(p) => Verdict::UserAction(proposal_from_proto(
                "UserActionProposal",
                p.content.as_ref(),
            )?),
            Kind::Deny(d) => {
                Verdict::Deny(match required("DenyReason", "kind", d.kind.as_ref())? {
                    D::BaselineRuleId(id) => {
                        DenyReason::Baseline(parse_id("DenyReason", "baseline_rule_id", id)?)
                    }
                    D::DenialMemory(f) => DenyReason::DenialMemory(ActionFingerprint::new(
                        action_kind_from_proto("ActionFingerprint", "kind", f.kind)?,
                        action_target_from_proto("ActionFingerprint", f.target.as_ref())?,
                    )),
                    D::ToolDisabled(_) => DenyReason::ToolDisabled,
                    D::DenyRuleId(id) => {
                        DenyReason::DenyRule(parse_id("DenyReason", "deny_rule_id", id)?)
                    }
                    D::SystemTurnOutOfToolSet(_) => DenyReason::SystemTurnOutOfToolSet,
                    D::ToolSetExcluded(_) => DenyReason::ToolSetExcluded,
                })
            }
        },
    )
}

// ---------- 规则与清单 ----------

pub fn scope_to_proto(s: &RuleScope) -> pb::Scope {
    use pb::scope::Kind;
    pb::Scope {
        kind: Some(match s {
            RuleScope::Global => Kind::Global(pb::GlobalScope {}),
            RuleScope::Project(id) => Kind::ProjectId(id.to_string()),
            RuleScope::Assistant(id) => Kind::AssistantId(id.to_string()),
            RuleScope::Conversation(id) => Kind::ConversationId(id.to_string()),
        }),
    }
}

pub fn scope_from_proto(kind: &'static str, s: Option<&pb::Scope>) -> R<RuleScope> {
    use pb::scope::Kind;
    const K: &str = "Scope";
    Ok(
        match required(K, "kind", required(kind, "scope", s)?.kind.as_ref())? {
            Kind::Global(_) => RuleScope::Global,
            Kind::ProjectId(id) => RuleScope::Project(parse_id(K, "project_id", id)?),
            Kind::AssistantId(id) => RuleScope::Assistant(parse_id(K, "assistant_id", id)?),
            Kind::ConversationId(id) => {
                RuleScope::Conversation(parse_id(K, "conversation_id", id)?)
            }
        },
    )
}

pub fn rule_matcher_to_proto(m: &RuleMatcher) -> R<pb::RuleMatcher> {
    use pb::rule_matcher::Kind as M;
    const K: &str = "RuleMatcher";
    Ok(pb::RuleMatcher {
        kind: Some(match m {
            RuleMatcher::PathUnder(p) => M::PathUnder(path_to_proto(K, "path_under", p)?),
            RuleMatcher::PathIs(p) => M::PathIs(path_to_proto(K, "path_is", p)?),
            RuleMatcher::ProgramSubcommand {
                program,
                subcommand,
            } => M::ProgramSubcommand(pb::ProgramSubcommand {
                program: program.clone(),
                subcommand: subcommand.clone(),
            }),
            RuleMatcher::ToolName(n) => M::ToolName(n.clone()),
            RuleMatcher::ActionKindIs(k) => M::ActionKindIs(action_kind_to_proto(*k)),
            RuleMatcher::Any => M::Any(pb::AnyMatcher {}),
            RuleMatcher::All(all) => M::All(pb::AllMatcher {
                matchers: all.iter().map(rule_matcher_to_proto).collect::<R<_>>()?,
            }),
            RuleMatcher::Flag(f) => M::Flag(action_flag_to_proto(*f)),
        }),
    })
}

pub fn rule_matcher_from_proto(m: &pb::RuleMatcher) -> R<RuleMatcher> {
    use pb::rule_matcher::Kind as M;
    const K: &str = "RuleMatcher";
    Ok(match required(K, "kind", m.kind.as_ref())? {
        M::PathUnder(p) => RuleMatcher::PathUnder(path_from_proto(p)),
        M::PathIs(p) => RuleMatcher::PathIs(path_from_proto(p)),
        M::ProgramSubcommand(ps) => RuleMatcher::ProgramSubcommand {
            program: ps.program.clone(),
            subcommand: ps.subcommand.clone(),
        },
        M::ToolName(n) => RuleMatcher::ToolName(n.clone()),
        M::ActionKindIs(k) => {
            RuleMatcher::ActionKindIs(action_kind_from_proto(K, "action_kind_is", *k)?)
        }
        M::Any(_) => RuleMatcher::Any,
        M::All(all) => RuleMatcher::All(
            all.matchers
                .iter()
                .map(rule_matcher_from_proto)
                .collect::<R<_>>()?,
        ),
        M::Flag(f) => RuleMatcher::Flag(action_flag_from_proto(K, "flag", *f)?),
    })
}

pub fn rule_to_proto(r: &RuleEntry) -> R<pb::Rule> {
    use pb::rule_source::Kind as S;
    Ok(pb::Rule {
        id: r.id.to_string(),
        layer: rule_layer_to_proto(r.layer),
        matcher: Some(rule_matcher_to_proto(&r.matcher)?),
        effect: rule_effect_to_proto(r.effect),
        source: Some(pb::RuleSource {
            kind: Some(match &r.source {
                RuleSource::Builtin { ruleset_version } => S::Builtin(pb::BuiltinRuleSource {
                    ruleset_version: ruleset_version.0.clone(),
                }),
                RuleSource::Remembered { approval_id, by } => {
                    S::Remembered(pb::RememberedRuleSource {
                        approval_id: approval_id.to_string(),
                        by: by.0.clone(),
                    })
                }
                RuleSource::ProjectConfig => S::ProjectConfig(pb::ProjectConfigRuleSource {}),
                RuleSource::User => S::User(pb::UserRuleSource {}),
            }),
        }),
        scope: Some(scope_to_proto(&r.scope)),
        created_at: Some(timestamp_to_proto(r.created_at)),
        label: r.label.clone(),
        applies_in: r
            .applies_in
            .iter()
            .map(|m| approval_mode_to_proto(*m))
            .collect(),
    })
}

pub fn rule_from_proto(r: &pb::Rule) -> R<RuleEntry> {
    use pb::rule_source::Kind as S;
    const K: &str = "Rule";
    let matcher = rule_matcher_from_proto(required(K, "matcher", r.matcher.as_ref())?)?;
    let source = required(
        "RuleSource",
        "kind",
        required(K, "source", r.source.as_ref())?.kind.as_ref(),
    )?;
    Ok(RuleEntry {
        id: parse_id(K, "id", &r.id)?,
        layer: rule_layer_from_proto(K, "layer", r.layer)?,
        matcher,
        effect: rule_effect_from_proto(K, "effect", r.effect)?,
        source: match source {
            S::Builtin(b) => RuleSource::Builtin {
                ruleset_version: RuleSetVersion(b.ruleset_version.clone()),
            },
            S::Remembered(m) => RuleSource::Remembered {
                approval_id: parse_id("RememberedRuleSource", "approval_id", &m.approval_id)?,
                by: Actor(m.by.clone()),
            },
            S::ProjectConfig(_) => RuleSource::ProjectConfig,
            S::User(_) => RuleSource::User,
        },
        scope: scope_from_proto(K, r.scope.as_ref())?,
        created_at: required_timestamp(K, "created_at", r.created_at.as_ref())?,
        label: r.label.clone(),
        applies_in: r
            .applies_in
            .iter()
            .map(|m| approval_mode_from_proto(K, "applies_in", *m))
            .collect::<R<_>>()?,
    })
}

// ---------- 附件 ----------

pub fn attachment_to_proto(a: &Attachment) -> R<pb::Attachment> {
    use pb::attachment_origin::Kind;
    Ok(pb::Attachment {
        id: a.id.to_string(),
        conversation_id: a.conversation_id.to_string(),
        sha256: sha256_to_proto(&a.sha256),
        size: a.size,
        mime: a.mime.clone(),
        name: a.name.clone(),
        origin: Some(pb::AttachmentOrigin {
            kind: Some(match &a.origin {
                AttachmentOrigin::RepoPath(p) => {
                    Kind::RepoPath(path_to_proto("AttachmentOrigin", "repo_path", p)?)
                }
                AttachmentOrigin::Copied(r) => Kind::Copied(artifact_ref_to_proto(r)),
                AttachmentOrigin::Pasted(r) => Kind::Pasted(artifact_ref_to_proto(r)),
                AttachmentOrigin::CredentialInline(r) => {
                    Kind::CredentialInline(artifact_ref_to_proto(r))
                }
            }),
        }),
        needs_confirmation: a.needs_confirmation,
        created_at: Some(timestamp_to_proto(a.created_at)),
    })
}

pub fn attachment_from_proto(a: &pb::Attachment) -> R<Attachment> {
    use pb::attachment_origin::Kind;
    const K: &str = "Attachment";
    let origin = required(
        "AttachmentOrigin",
        "kind",
        required(K, "origin", a.origin.as_ref())?.kind.as_ref(),
    )?;
    Ok(Attachment {
        id: parse_id(K, "id", &a.id)?,
        conversation_id: parse_id(K, "conversation_id", &a.conversation_id)?,
        sha256: sha256_from_hex(K, "sha256", &a.sha256)?,
        size: a.size,
        mime: a.mime.clone(),
        name: a.name.clone(),
        origin: match origin {
            Kind::RepoPath(p) => AttachmentOrigin::RepoPath(path_from_proto(p)),
            Kind::Copied(r) => AttachmentOrigin::Copied(artifact_ref_from_proto(r)?),
            Kind::Pasted(r) => AttachmentOrigin::Pasted(artifact_ref_from_proto(r)?),
            Kind::CredentialInline(r) => {
                AttachmentOrigin::CredentialInline(artifact_ref_from_proto(r)?)
            }
        },
        needs_confirmation: a.needs_confirmation,
        created_at: required_timestamp(K, "created_at", a.created_at.as_ref())?,
    })
}

// ---------- 运行时、额度、导入 ----------

pub fn runtime_to_proto(r: &Runtime) -> R<pb::RuntimeStatus> {
    Ok(pb::RuntimeStatus {
        runtime_id: r.id.to_string(),
        kind: r.kind.0.clone(),
        display_name: r.display_name.clone(),
        cli_path: r
            .cli_path
            .as_deref()
            .map(|p| path_to_proto("RuntimeStatus", "cli_path", p))
            .transpose()?,
        cli_version: r.cli_version.clone(),
        adapter_version: r.adapter_version.clone(),
        state: runtime_state_to_proto(r.state),
        // 只有四个字段（安全不变量）：这里逐个列出，LoginInfo 加字段时编译器会在两边都提醒
        login: r.login.as_ref().map(|l| pb::LoginInfo {
            logged_in: l.logged_in,
            auth_method: l.auth_method.clone(),
            subscription_type: l.subscription_type.clone(),
            login_expires_at: l.login_expires_at.map(timestamp_to_proto),
        }),
    })
}

pub fn runtime_from_proto(r: &pb::RuntimeStatus) -> R<Runtime> {
    const K: &str = "RuntimeStatus";
    Ok(Runtime {
        id: parse_id(K, "runtime_id", &r.runtime_id)?,
        kind: RuntimeKind(r.kind.clone()),
        display_name: r.display_name.clone(),
        cli_path: r.cli_path.as_deref().map(path_from_proto),
        cli_version: r.cli_version.clone(),
        adapter_version: r.adapter_version.clone(),
        state: runtime_state_from_proto(K, "state", r.state)?,
        login: r
            .login
            .as_ref()
            .map(|l| -> R<LoginInfo> {
                Ok(LoginInfo {
                    logged_in: l.logged_in,
                    auth_method: l.auth_method.clone(),
                    subscription_type: l.subscription_type.clone(),
                    login_expires_at: opt_timestamp(
                        "LoginInfo",
                        "login_expires_at",
                        l.login_expires_at.as_ref(),
                    )?,
                })
            })
            .transpose()?,
    })
}

/// 使用率是 0–100 的百分比：两个方向都拒绝超过 100 的值（编得进去的一定解得回来）。
const MAX_USED_PCT: u8 = 100;

pub fn quota_window_to_proto(w: &QuotaWindow) -> R<pb::QuotaWindow> {
    if w.used_pct > MAX_USED_PCT {
        return Err(ConvertError::InvalidValue {
            kind: "QuotaWindow",
            field: "used_pct",
            reason: format!("{} 不在 0–100 之间", w.used_pct),
        });
    }
    Ok(pb::QuotaWindow {
        window: w.window.clone(),
        used_pct: u32::from(w.used_pct),
        resets_at: w.resets_at.map(timestamp_to_proto),
        observed_at: Some(timestamp_to_proto(w.observed_at)),
    })
}

pub fn quota_window_from_proto(w: &pb::QuotaWindow) -> R<QuotaWindow> {
    const K: &str = "QuotaWindow";
    let used_pct = u8::try_from(w.used_pct)
        .ok()
        .filter(|p| *p <= MAX_USED_PCT)
        .ok_or_else(|| ConvertError::InvalidValue {
            kind: K,
            field: "used_pct",
            reason: format!("{} 不在 0–100 之间", w.used_pct),
        })?;
    Ok(QuotaWindow {
        window: w.window.clone(),
        used_pct,
        resets_at: opt_timestamp(K, "resets_at", w.resets_at.as_ref())?,
        observed_at: required_timestamp(K, "observed_at", w.observed_at.as_ref())?,
    })
}

pub fn local_config_item_to_proto(i: &LocalConfigItem) -> pb::LocalConfigItem {
    pb::LocalConfigItem {
        id: i.id.clone(),
        kind: local_config_kind_to_proto(i.kind),
        name: i.name.clone(),
        description: i.description.clone(),
        source: i.source.clone(),
        imported: i.imported,
    }
}

pub fn local_config_item_from_proto(i: &pb::LocalConfigItem) -> R<LocalConfigItem> {
    Ok(LocalConfigItem {
        id: i.id.clone(),
        kind: local_config_kind_from_proto("LocalConfigItem", "kind", i.kind)?,
        name: i.name.clone(),
        description: i.description.clone(),
        source: i.source.clone(),
        imported: i.imported,
    })
}

// ---------- 档位作用对象 ----------

pub fn mode_target_to_proto(t: &ApprovalModeTarget) -> pb::ApprovalModeTarget {
    use pb::approval_mode_target::Kind;
    pb::ApprovalModeTarget {
        kind: Some(match t {
            ApprovalModeTarget::Conversation(id) => Kind::ConversationId(id.to_string()),
            ApprovalModeTarget::Member {
                conversation_id,
                assistant_id,
            } => Kind::Member(pb::MemberTarget {
                conversation_id: conversation_id.to_string(),
                assistant_id: assistant_id.to_string(),
            }),
            ApprovalModeTarget::Project(id) => Kind::ProjectId(id.to_string()),
        }),
    }
}

pub fn mode_target_from_proto(
    kind: &'static str,
    t: Option<&pb::ApprovalModeTarget>,
) -> R<ApprovalModeTarget> {
    use pb::approval_mode_target::Kind;
    const K: &str = "ApprovalModeTarget";
    Ok(
        match required(K, "kind", required(kind, "target", t)?.kind.as_ref())? {
            Kind::ConversationId(id) => {
                ApprovalModeTarget::Conversation(parse_id(K, "conversation_id", id)?)
            }
            Kind::Member(m) => ApprovalModeTarget::Member {
                conversation_id: parse_id("MemberTarget", "conversation_id", &m.conversation_id)?,
                assistant_id: parse_id("MemberTarget", "assistant_id", &m.assistant_id)?,
            },
            Kind::ProjectId(id) => ApprovalModeTarget::Project(parse_id(K, "project_id", id)?),
        },
    )
}
