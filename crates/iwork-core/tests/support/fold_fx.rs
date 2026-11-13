//! 折叠与引擎测试用的领域实体与事件构造函数：只用 `iwork-domain`，编号小、可读。

#![allow(dead_code, reason = "各测试各用一部分")]

use std::collections::BTreeSet;
use std::path::PathBuf;

use iwork_domain::action::{
    Action, ActionFlags, ActionKind, ActionTarget, CallShapeKind, CanonicalPath, SandboxPlacement,
    ToolRef,
};
use iwork_domain::approval::{
    Approval, ApprovalCallRecord, ApprovalKind, ApprovalState, ReviewState, VoidReason,
};
use iwork_domain::approval_mode::{ApprovalMode, ProjectDefaultMode, RememberScope};
use iwork_domain::assistant::{Assistant, AssistantConfig, AssistantVersion, Avatar};
use iwork_domain::common::{ModelId, ModelSelection, Sha256, Timestamp};
use iwork_domain::conversation::{Conversation, ConversationKind, ConversationState};
use iwork_domain::event::*;
use iwork_domain::id::{
    ApprovalId, AssistantId, ConversationId, MessageId, ProjectId, RawUlid, RuleId, RuntimeId, Seq,
    TurnId, UserActionId,
};
use iwork_domain::message::{Author, Message, MessageBody, MessageStatus};
use iwork_domain::project::{PathStatus, Project, ProjectSettings, VcsKind};
use iwork_domain::rule::{RuleEntry, RuleSetVersion, Verdict};
use iwork_domain::turn::{InterruptReason, Turn, TurnKind, TurnState};
use iwork_domain::user_action::{Proposal, UserActionCard, UserActionState};

fn raw(prefix: u128, n: u128) -> RawUlid {
    RawUlid::from_u128((prefix << 32) + n)
}
pub fn pid(n: u128) -> ProjectId {
    ProjectId::from_raw(raw(1, n))
}
pub fn aid(n: u128) -> AssistantId {
    AssistantId::from_raw(raw(2, n))
}
pub fn cid(n: u128) -> ConversationId {
    ConversationId::from_raw(raw(3, n))
}
pub fn tid(n: u128) -> TurnId {
    TurnId::from_raw(raw(4, n))
}
pub fn apid(n: u128) -> ApprovalId {
    ApprovalId::from_raw(raw(5, n))
}
pub fn uaid(n: u128) -> UserActionId {
    UserActionId::from_raw(raw(6, n))
}
pub fn rid(n: u128) -> RuleId {
    RuleId::from_raw(raw(7, n))
}
pub fn mid(n: u128) -> MessageId {
    MessageId::from_raw(raw(8, n))
}

fn model() -> ModelSelection {
    ModelSelection {
        model_id: ModelId("m".into()),
        effort: None,
    }
}

pub fn project(n: u128, default: ProjectDefaultMode) -> Project {
    Project {
        id: pid(n),
        root_path: PathBuf::from(format!("/work/p{n}")),
        vcs_kind: VcsKind("v".into()),
        name: format!("p{n}"),
        pinned: false,
        last_active_at: None,
        path_status: PathStatus::Ok,
        settings: ProjectSettings {
            default_approval_mode: default,
            ..ProjectSettings::default()
        },
    }
}

pub fn assistant(n: u128) -> Assistant {
    Assistant {
        id: aid(n),
        builtin: false,
        current: AssistantVersion {
            version: 1,
            config: AssistantConfig {
                name: format!("a{n}"),
                avatar: Avatar {
                    initial: "A".into(),
                    color: "#123456".into(),
                },
                prompt: "p".into(),
                runtime_id: RuntimeId::from_raw(raw(9, 1)),
                model: model(),
                disabled_groups: BTreeSet::new(),
            },
            created_at: Timestamp(0),
        },
    }
}

pub fn project_conversation(n: u128, project: u128) -> Conversation {
    Conversation {
        id: cid(n),
        kind: ConversationKind::Project {
            project_id: pid(project),
        },
        state: ConversationState::Active,
        last_read_seq: Seq(0),
        model_override: None,
        approval_mode: None,
        updated_at: Timestamp(0),
    }
}

pub fn dm(n: u128, assistant: u128, context_project: Option<u128>) -> Conversation {
    Conversation {
        id: cid(n),
        kind: ConversationKind::AssistantDm {
            assistant_id: aid(assistant),
            context_project: context_project.map(pid),
        },
        state: ConversationState::Active,
        last_read_seq: Seq(0),
        model_override: None,
        approval_mode: None,
        updated_at: Timestamp(0),
    }
}

pub fn turn(n: u128, conv: u128, assistant: u128) -> Turn {
    Turn {
        id: tid(n),
        conversation_id: cid(conv),
        assistant_id: aid(assistant),
        assistant_version: 1,
        runtime_id: RuntimeId::from_raw(raw(9, 1)),
        model: model(),
        runtime_session_id: None,
        kind: TurnKind::Assistant,
        state: TurnState::Queued,
        started_at: Timestamp(0),
        ended_at: None,
    }
}

/// 一条子动作：执行 `program`。
pub fn exec(program: &str) -> Action {
    Action {
        kind: ActionKind::Execute,
        target: ActionTarget::Command {
            program: program.into(),
            subcommand: None,
            argv: vec![program.into()],
        },
        tool: ToolRef("Bash".into()),
        raw_summary: String::new(),
        flags: ActionFlags::default(),
    }
}

/// 调用记录：`program` 不同、摘要就不同。
pub fn record(program: &str) -> ApprovalCallRecord {
    let digest = |salt: u8| {
        let mut b = [salt; 32];
        for (i, c) in program.bytes().enumerate().take(31) {
            b[i + 1] = c;
        }
        Sha256(b)
    };
    ApprovalCallRecord {
        tool: ToolRef("Bash".into()),
        cwd: CanonicalPath(PathBuf::from("/work/p1")),
        shape: CallShapeKind::SingleSimpleCommand,
        placement: SandboxPlacement::Inside,
        bypass_sandbox_requested: false,
        actions: vec![exec(program)],
        representative: 0,
        raw_input_digest: digest(1),
        normalized_digest: digest(2),
    }
}

/// 一条询问审批，调用是执行 `program`。
pub fn approval(n: u128, turn: u128, conv: u128, program: &str) -> Approval {
    let mut action = exec(program);
    action.raw_summary = program.into();
    Approval {
        id: apid(n),
        turn_id: tid(turn),
        conversation_id: cid(conv),
        kind: ApprovalKind::Ask,
        action,
        call: Some(Box::new(record(program))),
        matched_rules: vec![],
        ruleset_version: RuleSetVersion("r".into()),
        state: ApprovalState::Pending,
        review: ReviewState::NotReviewed,
        offered_remember: vec![RememberScope::Once],
        remember_label: "本次".into(),
        requested_at: Timestamp(0),
    }
}

pub fn card(n: u128, turn: u128, conv: u128) -> UserActionCard {
    UserActionCard {
        id: uaid(n),
        turn_id: tid(turn),
        conversation_id: cid(conv),
        proposal: Proposal::Command {
            command: "rm -rf .git/hooks/x".into(),
            cwd: CanonicalPath(PathBuf::from("/work/p1")),
        },
        reason: "底线".into(),
        proposal_hash: Sha256([7; 32]),
        state: UserActionState::Pending,
        output_artifact: None,
    }
}

pub fn rule(n: u128) -> RuleEntry {
    use iwork_domain::rule::{RuleEffect, RuleLayer, RuleMatcher, RuleScope, RuleSource};
    RuleEntry {
        id: rid(n),
        layer: RuleLayer::Remembered,
        matcher: RuleMatcher::ProgramSubcommand {
            program: format!("p{n}"),
            subcommand: None,
        },
        effect: RuleEffect::Allow,
        source: RuleSource::User,
        scope: RuleScope::Global,
        created_at: Timestamp(0),
        label: format!("r{n}"),
        applies_in: Default::default(),
    }
}

pub fn user_message(n: u128, conv: u128) -> Message {
    Message {
        id: mid(n),
        conversation_id: cid(conv),
        author: Author::User,
        context_project_id: None,
        status: MessageStatus::Sent,
        body: MessageBody::Inline("继续".into()),
        attachments: vec![],
        created_at: Timestamp(0),
        turn_id: None,
    }
}

// ---------- 事件 ----------

pub fn e_project(n: u128, default: ProjectDefaultMode) -> DomainEvent {
    ProjectAdded {
        project: project(n, default),
    }
    .into()
}
pub fn e_assistant(n: u128) -> DomainEvent {
    AssistantCreated {
        assistant: assistant(n),
    }
    .into()
}
pub fn e_assistant_deleted(n: u128) -> DomainEvent {
    AssistantDeleted {
        assistant_id: aid(n),
        name_snapshot: format!("a{n}"),
    }
    .into()
}
pub fn e_conversation(c: Conversation) -> DomainEvent {
    ConversationCreated { conversation: c }.into()
}
pub fn e_deleting(n: u128) -> DomainEvent {
    ConversationDeleting {
        conversation_id: cid(n),
    }
    .into()
}
pub fn e_deleted(n: u128) -> DomainEvent {
    ConversationDeleted {
        conversation_id: cid(n),
    }
    .into()
}
pub fn e_turn(n: u128, conv: u128, assistant: u128) -> DomainEvent {
    TurnQueued {
        turn: turn(n, conv, assistant),
    }
    .into()
}
pub fn e_turn_state(n: u128, from: TurnState, to: TurnState) -> DomainEvent {
    TurnStateChanged {
        turn_id: tid(n),
        from,
        to,
        reason: None,
    }
    .into()
}
pub fn e_waiting(n: u128) -> DomainEvent {
    e_turn_state(n, TurnState::Running, TurnState::WaitingApproval)
}
pub fn e_running(n: u128) -> DomainEvent {
    e_turn_state(n, TurnState::WaitingApproval, TurnState::Running)
}
pub fn e_interrupted(n: u128) -> DomainEvent {
    e_turn_state(
        n,
        TurnState::WaitingApproval,
        TurnState::Interrupted {
            reason: InterruptReason::ApprovalWaitLimit,
        },
    )
}
pub fn e_resumed(n: u128) -> DomainEvent {
    e_turn_state(
        n,
        TurnState::Interrupted {
            reason: InterruptReason::ApprovalWaitLimit,
        },
        TurnState::Running,
    )
}
pub fn e_turn_finished(n: u128, state: TurnState) -> DomainEvent {
    TurnFinished {
        turn_id: tid(n),
        state,
        usage: Default::default(),
        celebrate: false,
        change_summary: None,
    }
    .into()
}
pub fn e_requested(a: Approval) -> DomainEvent {
    ApprovalRequested { approval: a }.into()
}
pub fn e_review(n: u128, review: ReviewState) -> DomainEvent {
    ApprovalReviewUpdated {
        approval_id: apid(n),
        review,
    }
    .into()
}
pub fn e_decided(n: u128, state: ApprovalState) -> DomainEvent {
    ApprovalDecided {
        approval_id: apid(n),
        state,
        remember: None,
    }
    .into()
}
pub fn e_allowed(n: u128) -> DomainEvent {
    e_decided(
        n,
        ApprovalState::Approved {
            decider: iwork_domain::approval::Decider::User,
        },
    )
}
pub fn e_denied(n: u128) -> DomainEvent {
    e_decided(n, ApprovalState::Denied)
}
pub fn e_voided(n: u128, reason: VoidReason) -> DomainEvent {
    ApprovalVoided {
        approval_id: apid(n),
        reason,
    }
    .into()
}
pub fn e_delivered(n: u128, turn: u128) -> DomainEvent {
    ApprovalDelivered {
        approval_id: apid(n),
        turn_id: tid(turn),
    }
    .into()
}
/// 审计事件：这次求值命中了 `grant`，结果 `verdict`，求值所见游标 `seen`。
pub fn e_grant_audit(turn: u128, grant: u128, verdict: Verdict, seen: Seq) -> DomainEvent {
    ApprovalEvaluated {
        turn_id: tid(turn),
        action: exec("x"),
        matched_rule_ids: vec![],
        ruleset_version: RuleSetVersion("r".into()),
        verdict,
        layer_hit: iwork_domain::rule::RuleLayer::Remembered,
        duration_ms: 0,
        snapshot_lists_seq: Seq(0),
        live_rules_seq: seen,
        sub_actions: vec![],
        call_shape: None,
        snapshot_rule_paths: vec![],
        grant_approval_id: Some(apid(grant)),
    }
    .into()
}
pub fn e_card(c: UserActionCard) -> DomainEvent {
    UserActionProposed { card: c }.into()
}
pub fn e_card_state(n: u128, state: UserActionState) -> DomainEvent {
    UserActionStateChanged {
        card_id: uaid(n),
        state,
        output_artifact: None,
    }
    .into()
}
pub fn e_user_message(n: u128, conv: u128) -> DomainEvent {
    MessageAppended {
        message: user_message(n, conv),
    }
    .into()
}
pub fn e_mode(target: ApprovalModeTarget, mode: Option<ApprovalMode>) -> DomainEvent {
    ApprovalModeChanged {
        target,
        mode,
        cause: ApprovalModeChangeCause::User,
    }
    .into()
}

/// 引擎测试的布景：一个项目（手动默认）、这些助手、一个项目对话（对话级档位 `mode`）。id 由调用方给。
pub fn world_with(
    project_id: ProjectId,
    conversation_id: ConversationId,
    assistants: &[AssistantId],
    mode: Option<ApprovalMode>,
) -> Vec<DomainEvent> {
    let mut p = project(1, ProjectDefaultMode::Manual);
    p.id = project_id;
    let mut c = project_conversation(1, 1);
    c.id = conversation_id;
    c.kind = ConversationKind::Project { project_id };
    c.approval_mode = mode;
    let mut events: Vec<DomainEvent> = vec![ProjectAdded { project: p }.into()];
    for a in assistants {
        let mut x = assistant(1);
        x.id = *a;
        events.push(AssistantCreated { assistant: x }.into());
    }
    events.push(ConversationCreated { conversation: c }.into());
    events
}

/// 扮演编排：回合登记并进入等审批。
pub fn waiting_turn(
    t: TurnId,
    conversation_id: ConversationId,
    a: AssistantId,
) -> Vec<DomainEvent> {
    let mut x = turn(1, 1, 1);
    x.id = t;
    x.conversation_id = conversation_id;
    x.assistant_id = a;
    vec![
        TurnQueued { turn: x }.into(),
        TurnStateChanged {
            turn_id: t,
            from: TurnState::Queued,
            to: TurnState::Running,
            reason: None,
        }
        .into(),
        TurnStateChanged {
            turn_id: t,
            from: TurnState::Running,
            to: TurnState::WaitingApproval,
            reason: None,
        }
        .into(),
    ]
}
