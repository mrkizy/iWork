//! 任意 domain 事件的生成器：每个事件变体一个策略，配上它的种类名，覆盖全部 53 个变体。

use std::path::PathBuf;

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
    Actor, ArtifactArea, ArtifactRef, EffortLevel, ModelId, ModelSelection, Sha256, Timestamp,
    ToolGroup, Usage,
};
use iwork_domain::conversation::{Conversation, ConversationKind, ConversationState};
use iwork_domain::event::{self as ev, *};
use iwork_domain::id::*;
use iwork_domain::machine::Finite;
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
use iwork_domain::session::SessionLostReason;
use iwork_domain::turn::{Turn, TurnIntentKind, TurnKind, TurnState};
use iwork_domain::user_action::{ExitStatus, Proposal, UserActionCard, UserActionState};
use proptest::prelude::*;
use proptest::sample::select;
use proptest::strategy::BoxedStrategy;

macro_rules! arb_id {
    ($name:ident, $ty:ty) => {
        pub fn $name() -> impl Strategy<Value = $ty> + Clone {
            any::<u128>().prop_map(|v| <$ty>::from_raw(RawUlid::from_u128(v)))
        }
    };
}
arb_id!(project_id, ProjectId);
arb_id!(assistant_id, AssistantId);
arb_id!(conversation_id, ConversationId);
arb_id!(message_id, MessageId);
arb_id!(turn_id, TurnId);
arb_id!(approval_id, ApprovalId);
arb_id!(user_action_id, UserActionId);
arb_id!(rule_id, RuleId);
arb_id!(attachment_id, AttachmentId);
arb_id!(runtime_id, RuntimeId);
arb_id!(job_id, JobId);

pub fn review_id() -> impl Strategy<Value = ReviewId> + Clone {
    turn_id().prop_map(ReviewId)
}

pub fn text() -> impl Strategy<Value = String> + Clone {
    ".{0,8}"
}

pub fn path() -> impl Strategy<Value = PathBuf> + Clone {
    "/[a-z0-9_/ ]{0,12}".prop_map(PathBuf::from)
}

fn canonical() -> impl Strategy<Value = CanonicalPath> + Clone {
    path().prop_map(CanonicalPath)
}

/// 毫秒精度，正负都有（proto 的秒与纳秒拆分要处理负数）。
pub fn ts() -> impl Strategy<Value = Timestamp> + Clone {
    (-1_000_000_000_000_000i64..1_000_000_000_000_000).prop_map(Timestamp)
}

fn sha() -> impl Strategy<Value = Sha256> + Clone {
    any::<[u8; 32]>().prop_map(Sha256)
}

fn pick<T: Clone + std::fmt::Debug + 'static>(values: Vec<T>) -> impl Strategy<Value = T> + Clone {
    select(values)
}

fn artifact() -> impl Strategy<Value = ArtifactRef> + Clone {
    (
        sha(),
        pick(vec![
            ArtifactArea::Artifacts,
            ArtifactArea::Attachments,
            ArtifactArea::Private,
        ]),
        any::<u64>(),
    )
        .prop_map(|(sha256, area, size)| ArtifactRef { sha256, area, size })
}

fn model() -> impl Strategy<Value = ModelSelection> + Clone {
    (text(), proptest::option::of(text())).prop_map(|(m, e)| ModelSelection {
        model_id: ModelId(m),
        effort: e.map(EffortLevel),
    })
}

fn usage() -> impl Strategy<Value = Usage> + Clone {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        // 有限值：NaN 不等于自身，往返比较会假红
        proptest::option::of(0.0f64..1e6),
    )
        .prop_map(|(a, b, c, d, cost_usd)| Usage {
            input_tokens: a,
            output_tokens: b,
            cache_read_tokens: c,
            cache_write_tokens: d,
            cost_usd,
        })
}

fn action_kind() -> impl Strategy<Value = ActionKind> + Clone {
    use ActionKind::*;
    pick(vec![
        Read,
        Write,
        Execute,
        OutsideSandbox,
        WriteAutorunConfig,
        Delete,
        Push,
        Publish,
        Network,
        McpCall,
        Unclassified,
    ])
}

fn action_target() -> impl Strategy<Value = ActionTarget> + Clone {
    prop_oneof![
        canonical().prop_map(ActionTarget::Path),
        prop::collection::vec(canonical(), 0..3).prop_map(ActionTarget::Paths),
        (
            text(),
            proptest::option::of(text()),
            prop::collection::vec(text(), 0..3)
        )
            .prop_map(|(program, subcommand, argv)| ActionTarget::Command {
                program,
                subcommand,
                argv
            }),
        text().prop_map(ActionTarget::ToolName),
        text().prop_map(ActionTarget::Remote),
        Just(ActionTarget::None),
    ]
}

fn action() -> impl Strategy<Value = Action> + Clone {
    (
        action_kind(),
        action_target(),
        text(),
        text(),
        any::<(bool, bool, bool, bool)>(),
        any::<(bool, bool, bool)>(),
    )
        .prop_map(
            |(kind, target, tool, raw_summary, (a, b, c, d), (e, f, g))| Action {
                kind,
                target,
                tool: ToolRef(tool),
                raw_summary,
                flags: ActionFlags {
                    out_of_bounds: a,
                    bypass_sandbox_requested: b,
                    opaque_script: c,
                    argument_path: d,
                    resource_limit_hit: e,
                    unlexable: f,
                    path_unresolved: g,
                },
            },
        )
}

fn project_settings() -> impl Strategy<Value = ProjectSettings> + Clone {
    (
        pick(ProjectDefaultMode::all()),
        proptest::option::of(model()),
        any::<bool>(),
        proptest::option::of(runtime_id()),
        proptest::option::of(model()),
        any::<u32>(),
    )
        .prop_map(
            |(mode, default_model, enabled, rt, review_model, timeout_secs)| ProjectSettings {
                default_approval_mode: mode,
                default_model,
                ai_review: AiReviewSettings {
                    enabled,
                    runtime_id: rt,
                    model: review_model,
                    timeout_secs,
                },
            },
        )
}

fn project() -> impl Strategy<Value = Project> + Clone {
    (
        project_id(),
        path(),
        text(),
        text(),
        any::<bool>(),
        proptest::option::of(ts()),
        pick(PathStatus::all()),
        project_settings(),
    )
        .prop_map(
            |(id, root_path, vcs, name, pinned, last_active_at, path_status, settings)| Project {
                id,
                root_path,
                vcs_kind: VcsKind(vcs),
                name,
                pinned,
                last_active_at,
                path_status,
                settings,
            },
        )
}

fn assistant_config() -> impl Strategy<Value = AssistantConfig> + Clone {
    use ToolGroup::*;
    (
        text(),
        text(),
        text(),
        text(),
        runtime_id(),
        model(),
        prop::collection::btree_set(pick(vec![ReadAndSearch, EditFiles, RunCommands, Web]), 0..4),
    )
        .prop_map(
            |(name, initial, color, prompt, runtime_id, model, disabled_groups)| AssistantConfig {
                name,
                avatar: Avatar { initial, color },
                prompt,
                runtime_id,
                model,
                disabled_groups,
            },
        )
}

fn approval_mode() -> impl Strategy<Value = ApprovalMode> + Clone {
    pick(ApprovalMode::all())
}

fn conversation() -> impl Strategy<Value = Conversation> + Clone {
    let kind = prop_oneof![
        project_id().prop_map(|project_id| ConversationKind::Project { project_id }),
        (assistant_id(), proptest::option::of(project_id())).prop_map(
            |(assistant_id, context_project)| {
                ConversationKind::AssistantDm {
                    assistant_id,
                    context_project,
                }
            }
        ),
    ];
    (
        conversation_id(),
        kind,
        pick(ConversationState::all()),
        any::<u64>(),
        proptest::option::of(model()),
        proptest::option::of(approval_mode()),
        ts(),
    )
        .prop_map(
            |(id, kind, state, seq, model_override, approval_mode, updated_at)| Conversation {
                id,
                kind,
                state,
                last_read_seq: Seq(seq),
                model_override,
                approval_mode,
                updated_at,
            },
        )
}

fn body() -> impl Strategy<Value = MessageBody> + Clone {
    prop_oneof![
        text().prop_map(MessageBody::Inline),
        artifact().prop_map(MessageBody::Artifact)
    ]
}

fn message() -> impl Strategy<Value = Message> + Clone {
    let author = prop_oneof![
        Just(Author::User),
        Just(Author::System),
        (assistant_id(), text())
            .prop_map(|(id, name_snapshot)| Author::Assistant { id, name_snapshot }),
    ];
    (
        message_id(),
        conversation_id(),
        author,
        proptest::option::of(project_id()),
        pick(MessageStatus::all()),
        body(),
        prop::collection::vec(attachment_id(), 0..3),
        ts(),
        proptest::option::of(turn_id()),
    )
        .prop_map(
            |(
                id,
                conversation_id,
                author,
                context_project_id,
                status,
                body,
                attachments,
                created_at,
                turn_id,
            )| {
                Message {
                    id,
                    conversation_id,
                    author,
                    context_project_id,
                    status,
                    body,
                    attachments,
                    created_at,
                    turn_id,
                }
            },
        )
}

fn turn_state() -> impl Strategy<Value = TurnState> + Clone {
    pick(TurnState::all())
}

fn turn() -> impl Strategy<Value = Turn> + Clone {
    (
        (
            turn_id(),
            conversation_id(),
            assistant_id(),
            any::<u32>(),
            runtime_id(),
        ),
        model(),
        proptest::option::of(text()),
        pick(vec![
            TurnKind::Assistant,
            TurnKind::SystemAiReview,
            TurnKind::SystemAgentsMdDraft,
        ]),
        turn_state(),
        ts(),
        proptest::option::of(ts()),
    )
        .prop_map(
            |(
                (id, conversation_id, assistant_id, assistant_version, runtime_id),
                model,
                session,
                kind,
                state,
                started_at,
                ended_at,
            )| {
                Turn {
                    id,
                    conversation_id,
                    assistant_id,
                    assistant_version,
                    runtime_id,
                    model,
                    runtime_session_id: session,
                    kind,
                    state,
                    started_at,
                    ended_at,
                }
            },
        )
}

pub fn review_state() -> impl Strategy<Value = ReviewState> + Clone {
    prop_oneof![Just(ReviewState::NotReviewed), started_review_state(),]
}

/// 审查开始之后的子状态（事件 `ApprovalReviewUpdated` 只会带这些）。
pub fn started_review_state() -> impl Strategy<Value = ReviewState> + Clone {
    prop_oneof![
        review_id().prop_map(|review_id| ReviewState::InProgress { review_id }),
        review_id().prop_map(|review_id| ReviewState::Passed {
            review_id,
            reason: "需要".into()
        }),
        (review_id(), text())
            .prop_map(|(review_id, reason)| ReviewState::Failed { review_id, reason }),
        review_id().prop_map(|review_id| ReviewState::Errored { review_id }),
        review_id().prop_map(|review_id| ReviewState::TimedOut { review_id }),
        (
            review_id(),
            pick(vec![
                AbandonReason::ModeChangedFromAuto,
                AbandonReason::ExecutorLost
            ])
        )
            .prop_map(|(review_id, reason)| ReviewState::Abandoned { review_id, reason }),
    ]
}

fn call_shape() -> impl Strategy<Value = CallShapeKind> + Clone {
    pick(vec![
        CallShapeKind::Tool,
        CallShapeKind::SingleSimpleCommand,
        CallShapeKind::Compound,
        CallShapeKind::Unparsable,
    ])
}

fn call_record() -> impl Strategy<Value = ApprovalCallRecord> + Clone {
    (
        text(),
        canonical(),
        call_shape(),
        pick(vec![
            SandboxPlacement::Inside,
            SandboxPlacement::Outside,
            SandboxPlacement::Ambiguous,
        ]),
        any::<bool>(),
        prop::collection::vec(action(), 1..4),
        any::<u32>(),
        sha(),
        sha(),
    )
        .prop_map(
            |(tool, cwd, shape, placement, bypass, actions, rep, raw, norm)| {
                let representative = rep % actions.len() as u32;
                ApprovalCallRecord {
                    tool: ToolRef(tool),
                    cwd,
                    shape,
                    placement,
                    bypass_sandbox_requested: bypass,
                    actions,
                    representative,
                    raw_input_digest: raw,
                    normalized_digest: norm,
                }
            },
        )
}

/// `ApprovalRequested` 带的审批：待审批（或已拒绝，proto 消息也装得下）。
fn approval() -> impl Strategy<Value = Approval> + Clone {
    (
        (
            approval_id(),
            turn_id(),
            conversation_id(),
            proptest::option::of(call_record().prop_map(Box::new)),
        ),
        pick(vec![ApprovalKind::Ask, ApprovalKind::OutsideSandbox]),
        action(),
        prop::collection::vec(rule_id(), 0..3),
        text(),
        pick(vec![ApprovalState::Pending, ApprovalState::Denied]),
        review_state(),
        prop::collection::vec(pick(RememberScope::all()), 0..3),
        text(),
        ts(),
    )
        .prop_map(
            |(
                (id, turn_id, conversation_id, call),
                kind,
                action,
                matched_rules,
                version,
                state,
                review,
                offered_remember,
                remember_label,
                requested_at,
            )| {
                Approval {
                    id,
                    turn_id,
                    conversation_id,
                    kind,
                    action,
                    call,
                    matched_rules,
                    ruleset_version: RuleSetVersion(version),
                    state,
                    review,
                    offered_remember,
                    remember_label,
                    requested_at,
                }
            },
        )
}

fn exit_status() -> impl Strategy<Value = ExitStatus> + Clone {
    prop_oneof![
        any::<i32>().prop_map(ExitStatus::Code),
        any::<i32>().prop_map(ExitStatus::Signal)
    ]
}

fn user_action_state() -> impl Strategy<Value = UserActionState> + Clone {
    prop_oneof![
        Just(UserActionState::Pending),
        Just(UserActionState::Starting),
        Just(UserActionState::Executing),
        Just(UserActionState::Stopping),
        exit_status().prop_map(|exit| UserActionState::Done { exit }),
        text().prop_map(|error| UserActionState::Failed { error }),
        text().prop_map(|error| UserActionState::Unknown { error }),
        Just(UserActionState::Ended),
        Just(UserActionState::Dismissed),
        pick(VoidReason::all()).prop_map(|reason| UserActionState::Voided { reason }),
    ]
}

fn proposal() -> impl Strategy<Value = Proposal> + Clone {
    prop_oneof![
        (text(), canonical()).prop_map(|(command, cwd)| Proposal::Command { command, cwd }),
        (canonical(), text(), text()).prop_map(|(path, old, new)| Proposal::Patch {
            path,
            old,
            new
        }),
    ]
}

fn card() -> impl Strategy<Value = UserActionCard> + Clone {
    (
        (user_action_id(), turn_id(), conversation_id()),
        proposal(),
        text(),
        sha(),
        user_action_state(),
        proptest::option::of(artifact()),
    )
        .prop_map(
            |(
                (id, turn_id, conversation_id),
                proposal,
                reason,
                proposal_hash,
                state,
                output_artifact,
            )| {
                UserActionCard {
                    id,
                    turn_id,
                    conversation_id,
                    proposal,
                    reason,
                    proposal_hash,
                    state,
                    output_artifact,
                }
            },
        )
}

fn rule_scope() -> impl Strategy<Value = RuleScope> + Clone {
    prop_oneof![
        Just(RuleScope::Global),
        project_id().prop_map(RuleScope::Project),
        assistant_id().prop_map(RuleScope::Assistant),
        conversation_id().prop_map(RuleScope::Conversation),
    ]
}

fn rule_layer() -> impl Strategy<Value = RuleLayer> + Clone {
    use RuleLayer::*;
    pick(vec![
        Baseline,
        DenialMemory,
        ToolConfig,
        UserDeny,
        MustAsk,
        Remembered,
        ProjectAllow,
        Default,
        CredentialCommands,
        AutorunConfig,
        SensitiveDirs,
        WritableCache,
        WritableDirs,
        RejectList,
    ])
}

fn rule() -> impl Strategy<Value = RuleEntry> + Clone {
    use RuleEffect::*;
    let matcher = prop_oneof![
        path().prop_map(RuleMatcher::PathUnder),
        path().prop_map(RuleMatcher::PathIs),
        (text(), proptest::option::of(text())).prop_map(|(program, subcommand)| {
            RuleMatcher::ProgramSubcommand {
                program,
                subcommand,
            }
        }),
        text().prop_map(RuleMatcher::ToolName),
        action_kind().prop_map(RuleMatcher::ActionKindIs),
        Just(RuleMatcher::Any),
        pick(vec![
            ActionFlag::OutOfBounds,
            ActionFlag::BypassSandboxRequested,
            ActionFlag::OpaqueScript,
            ActionFlag::ArgumentPath,
            ActionFlag::ResourceLimitHit,
            ActionFlag::Unlexable,
            ActionFlag::PathUnresolved
        ])
        .prop_map(RuleMatcher::Flag),
    ];
    // 组合条件可以嵌套：叶子之上再套两层 All
    let matcher = matcher.prop_recursive(2, 8, 3, |inner| {
        prop::collection::vec(inner, 0..3).prop_map(RuleMatcher::All)
    });
    let source = prop_oneof![
        text().prop_map(|v| RuleSource::Builtin {
            ruleset_version: RuleSetVersion(v)
        }),
        (approval_id(), text()).prop_map(|(approval_id, by)| RuleSource::Remembered {
            approval_id,
            by: Actor(by)
        }),
        Just(RuleSource::ProjectConfig),
        Just(RuleSource::User),
    ];
    (
        rule_id(),
        rule_layer(),
        matcher,
        pick(vec![Allow, Ask, Deny, UserActionEligible, OutsideSandbox]),
        source,
        rule_scope(),
        ts(),
        text(),
        prop::collection::btree_set(approval_mode(), 0..3),
    )
        .prop_map(
            |(id, layer, matcher, effect, source, scope, created_at, label, applies_in)| {
                RuleEntry {
                    id,
                    layer,
                    matcher,
                    effect,
                    source,
                    scope,
                    created_at,
                    label,
                    applies_in,
                }
            },
        )
}

fn ask_reason() -> impl Strategy<Value = AskReason> + Clone {
    use AskReason::*;
    pick(vec![
        Default,
        OutOfBounds,
        MustAsk,
        Unclassified,
        McpNotReadOnly,
        UnparsableCommand,
        ForcedAfterDenial,
    ])
}

fn verdict() -> impl Strategy<Value = Verdict> + Clone {
    let deny = prop_oneof![
        rule_id().prop_map(DenyReason::Baseline),
        (action_kind(), action_target()).prop_map(|(kind, target)| DenyReason::DenialMemory(
            ActionFingerprint::new(kind, target)
        )),
        Just(DenyReason::ToolDisabled),
        rule_id().prop_map(DenyReason::DenyRule),
        Just(DenyReason::SystemTurnOutOfToolSet),
        Just(DenyReason::ToolSetExcluded),
    ];
    prop_oneof![
        Just(Verdict::Allow),
        ask_reason().prop_map(Verdict::Ask),
        deny.prop_map(Verdict::Deny),
        proposal().prop_map(Verdict::UserAction),
        ask_reason().prop_map(Verdict::OutsideSandbox),
    ]
}

fn runtime() -> impl Strategy<Value = Runtime> + Clone {
    use RuntimeState::*;
    let login = (any::<bool>(), text(), text(), proptest::option::of(ts())).prop_map(
        |(logged_in, auth_method, subscription_type, login_expires_at)| LoginInfo {
            logged_in,
            auth_method,
            subscription_type,
            login_expires_at,
        },
    );
    (
        runtime_id(),
        text(),
        text(),
        proptest::option::of(path()),
        proptest::option::of(text()),
        text(),
        pick(vec![
            Missing,
            NotLoggedIn,
            ReadyVerified,
            ReadyUnverified,
            Incompatible,
        ]),
        proptest::option::of(login),
    )
        .prop_map(
            |(id, kind, display_name, cli_path, cli_version, adapter_version, state, login)| {
                Runtime {
                    id,
                    kind: RuntimeKind(kind),
                    display_name,
                    cli_path,
                    cli_version,
                    adapter_version,
                    state,
                    login,
                }
            },
        )
}

fn quota_window() -> impl Strategy<Value = QuotaWindow> + Clone {
    (text(), 0u8..=100, proptest::option::of(ts()), ts()).prop_map(
        |(window, used_pct, resets_at, observed_at)| QuotaWindow {
            window,
            used_pct,
            resets_at,
            observed_at,
        },
    )
}

fn local_item() -> impl Strategy<Value = LocalConfigItem> + Clone {
    (
        text(),
        pick(vec![
            LocalConfigKind::Mcp,
            LocalConfigKind::Skill,
            LocalConfigKind::Plugin,
        ]),
        text(),
        text(),
        text(),
        any::<bool>(),
    )
        .prop_map(
            |(id, kind, name, description, source, imported)| LocalConfigItem {
                id,
                kind,
                name,
                description,
                source,
                imported,
            },
        )
}

fn boxed<S: Strategy<Value = T> + 'static, T: Into<DomainEvent> + std::fmt::Debug + 'static>(
    s: S,
) -> BoxedStrategy<DomainEvent> {
    // 带所属对话的十二种事件（G5）：各变体的策略里填 None，这里统一随机成有或没有（没有 = 加字段之前的旧事件）
    (
        s.prop_map(Into::into),
        proptest::option::of(conversation_id()),
    )
        .prop_map(|(e, c)| with_conversation(e, c))
        .boxed()
}

/// 给带所属对话的十二种事件（G5）换上 `c`；别的事件原样返回。
pub fn with_conversation(mut e: DomainEvent, c: Option<ConversationId>) -> DomainEvent {
    use DomainEvent as E;
    let slot = match &mut e {
        E::TurnStateChanged(x) => &mut x.conversation_id,
        E::TurnFinished(x) => &mut x.conversation_id,
        E::TurnModelReported(x) => &mut x.conversation_id,
        E::TurnSessionBound(x) => &mut x.conversation_id,
        E::TurnIntentRecorded(x) => &mut x.conversation_id,
        E::ToolCallStarted(x) => &mut x.conversation_id,
        E::ToolCallFinished(x) => &mut x.conversation_id,
        E::ApprovalDelivered(x) => &mut x.conversation_id,
        E::ApprovalReviewUpdated(x) => &mut x.conversation_id,
        E::ApprovalDecided(x) => &mut x.conversation_id,
        E::ApprovalVoided(x) => &mut x.conversation_id,
        E::UserActionStateChanged(x) => &mut x.conversation_id,
        _ => return e,
    };
    *slot = c;
    e
}

/// 每个事件变体一个策略，配上种类名。测试据此逐个变体跑、并核对覆盖了全部 KINDS。
pub fn event_strategies() -> Vec<(&'static str, BoxedStrategy<DomainEvent>)> {
    vec![
        (
            "project_added",
            boxed(project().prop_map(|project| ProjectAdded { project })),
        ),
        (
            "project_updated",
            boxed(
                (
                    project_id(),
                    proptest::option::of(text()),
                    proptest::option::of(any::<bool>()),
                    proptest::option::of(ts()),
                )
                    .prop_map(|(project_id, name, pinned, last_active_at)| {
                        ProjectUpdated {
                            project_id,
                            name,
                            pinned,
                            last_active_at,
                        }
                    }),
            ),
        ),
        (
            "project_relocated",
            boxed(
                (project_id(), path()).prop_map(|(project_id, new_path)| ProjectRelocated {
                    project_id,
                    new_path,
                }),
            ),
        ),
        (
            "project_path_status_changed",
            boxed(
                (project_id(), pick(PathStatus::all())).prop_map(|(project_id, path_status)| {
                    ProjectPathStatusChanged {
                        project_id,
                        path_status,
                    }
                }),
            ),
        ),
        (
            "project_removed",
            boxed(project_id().prop_map(|project_id| ProjectRemoved { project_id })),
        ),
        (
            "project_settings_updated",
            boxed(
                (
                    project_id(),
                    proptest::option::of(project_settings()),
                    project_settings(),
                )
                    .prop_map(|(project_id, previous, settings)| {
                        ProjectSettingsUpdated {
                            project_id,
                            previous,
                            settings,
                        }
                    }),
            ),
        ),
        (
            "repository_initialized",
            boxed(path().prop_map(|path| RepositoryInitialized { path })),
        ),
        (
            "clone_finished",
            boxed(
                (
                    job_id(),
                    prop_oneof![
                        path().prop_map(|path| CloneOutcome::Cloned { path }),
                        (
                            pick(vec![
                                CloneFailureReason::TargetNotEmpty,
                                CloneFailureReason::CloneFailed
                            ]),
                            text()
                        )
                            .prop_map(|(reason, message)| {
                                CloneOutcome::Failed(CloneFailure { reason, message })
                            }),
                    ],
                )
                    .prop_map(|(job_id, outcome)| CloneFinished { job_id, outcome }),
            ),
        ),
        (
            "agents_md_draft_proposed",
            boxed((job_id(), project_id(), artifact()).prop_map(
                |(card_id, project_id, content_artifact)| AgentsMdDraftProposed {
                    card_id,
                    project_id,
                    content_artifact,
                },
            )),
        ),
        (
            "agents_md_draft_decided",
            boxed(
                (
                    job_id(),
                    project_id(),
                    pick(vec![DraftDecision::Accept, DraftDecision::Discard]),
                )
                    .prop_map(|(card_id, project_id, decision)| {
                        AgentsMdDraftDecided {
                            card_id,
                            project_id,
                            decision,
                        }
                    }),
            ),
        ),
        (
            "assistant_created",
            boxed(
                (
                    assistant_id(),
                    any::<bool>(),
                    any::<u32>(),
                    assistant_config(),
                    ts(),
                )
                    .prop_map(|(id, builtin, version, config, created_at)| {
                        AssistantCreated {
                            assistant: Assistant {
                                id,
                                builtin,
                                current: AssistantVersion {
                                    version,
                                    config,
                                    created_at,
                                },
                            },
                        }
                    }),
            ),
        ),
        (
            "assistant_updated",
            boxed((assistant_id(), any::<u32>(), assistant_config()).prop_map(
                |(assistant_id, version, config)| AssistantUpdated {
                    assistant_id,
                    version,
                    config,
                },
            )),
        ),
        (
            "assistant_deleted",
            boxed(
                (assistant_id(), text()).prop_map(|(assistant_id, name_snapshot)| {
                    AssistantDeleted {
                        assistant_id,
                        name_snapshot,
                    }
                }),
            ),
        ),
        (
            "assistant_reset",
            boxed((assistant_id(), any::<u32>(), assistant_config()).prop_map(
                |(assistant_id, version, config)| AssistantReset {
                    assistant_id,
                    version,
                    config,
                },
            )),
        ),
        (
            "conversation_created",
            boxed(conversation().prop_map(|conversation| ConversationCreated { conversation })),
        ),
        (
            "conversation_deleting",
            boxed(
                conversation_id()
                    .prop_map(|conversation_id| ConversationDeleting { conversation_id }),
            ),
        ),
        (
            "conversation_deleted",
            boxed(
                conversation_id()
                    .prop_map(|conversation_id| ConversationDeleted { conversation_id }),
            ),
        ),
        (
            "conversation_model_changed",
            boxed(
                (conversation_id(), model()).prop_map(|(conversation_id, model)| {
                    ConversationModelChanged {
                        conversation_id,
                        model,
                    }
                }),
            ),
        ),
        (
            "approval_mode_changed",
            boxed(
                (
                    prop_oneof![
                        conversation_id().prop_map(ApprovalModeTarget::Conversation),
                        (conversation_id(), assistant_id()).prop_map(
                            |(conversation_id, assistant_id)| ApprovalModeTarget::Member {
                                conversation_id,
                                assistant_id
                            }
                        ),
                        project_id().prop_map(ApprovalModeTarget::Project),
                    ],
                    proptest::option::of(approval_mode()),
                    pick(vec![
                        ApprovalModeChangeCause::User,
                        ApprovalModeChangeCause::ProjectSwitch,
                        ApprovalModeChangeCause::NewConversation,
                    ]),
                    proptest::option::of(0u32..50),
                )
                    .prop_map(|(target, mode, cause, released_approvals)| {
                        ApprovalModeChanged {
                            target,
                            mode,
                            cause,
                            released_approvals,
                        }
                    }),
            ),
        ),
        (
            "dm_project_switched",
            boxed(
                (conversation_id(), proptest::option::of(project_id())).prop_map(
                    |(conversation_id, project_id)| DmProjectSwitched {
                        conversation_id,
                        project_id,
                    },
                ),
            ),
        ),
        (
            "conversation_read",
            boxed(
                (conversation_id(), any::<u64>()).prop_map(|(conversation_id, seq)| {
                    ConversationRead {
                        conversation_id,
                        last_read_seq: Seq(seq),
                    }
                }),
            ),
        ),
        (
            "message_appended",
            boxed(message().prop_map(|message| MessageAppended { message })),
        ),
        (
            "message_completed",
            boxed(
                (
                    conversation_id(),
                    message_id(),
                    body(),
                    pick(vec![
                        MessageStatus::Completed,
                        MessageStatus::Interrupted,
                        MessageStatus::Failed,
                    ]),
                )
                    .prop_map(|(conversation_id, message_id, body, status)| {
                        MessageCompleted {
                            conversation_id,
                            message_id,
                            body,
                            status,
                        }
                    }),
            ),
        ),
        (
            "turn_queued",
            boxed(turn().prop_map(|turn| TurnQueued { turn })),
        ),
        (
            "turn_state_changed",
            boxed(
                (
                    turn_id(),
                    turn_state(),
                    turn_state(),
                    proptest::option::of(text()),
                )
                    .prop_map(|(turn_id, from, to, reason)| TurnStateChanged {
                        conversation_id: None,
                        turn_id,
                        from,
                        to,
                        reason,
                    }),
            ),
        ),
        (
            "turn_finished",
            boxed(
                (
                    turn_id(),
                    pick(vec![
                        TurnState::Completed,
                        TurnState::Cancelled,
                        TurnState::Failed,
                    ]),
                    usage(),
                    any::<bool>(),
                    proptest::option::of(
                        (
                            any::<u32>(),
                            any::<u32>(),
                            any::<u32>(),
                            prop::collection::vec(path(), 0..3),
                        )
                            .prop_map(
                                |(files_changed, insertions, deletions, paths)| ChangeSummary {
                                    files_changed,
                                    insertions,
                                    deletions,
                                    paths,
                                },
                            ),
                    ),
                )
                    .prop_map(
                        |(turn_id, state, usage, celebrate, change_summary)| TurnFinished {
                            conversation_id: None,
                            turn_id,
                            state,
                            usage,
                            celebrate,
                            change_summary,
                        },
                    ),
            ),
        ),
        (
            "turn_model_reported",
            boxed(
                (turn_id(), model()).prop_map(|(turn_id, model)| TurnModelReported {
                    conversation_id: None,
                    turn_id,
                    model,
                }),
            ),
        ),
        (
            "tool_call_started",
            boxed((turn_id(), text(), text(), action()).prop_map(
                |(turn_id, id, tool_name, action)| ToolCallStarted {
                    conversation_id: None,
                    turn_id,
                    tool_call_id: ToolCallId(id),
                    tool_name,
                    action,
                },
            )),
        ),
        (
            "tool_call_finished",
            boxed(
                (
                    turn_id(),
                    text(),
                    any::<bool>(),
                    proptest::option::of(artifact()),
                )
                    .prop_map(|(turn_id, id, ok, output_artifact)| {
                        ToolCallFinished {
                            conversation_id: None,
                            turn_id,
                            tool_call_id: ToolCallId(id),
                            ok,
                            output_artifact,
                        }
                    }),
            ),
        ),
        (
            "turn_session_bound",
            boxed(
                (turn_id(), text(), prop::option::of(session_replaced())).prop_map(
                    |(turn_id, runtime_session_id, replaced)| TurnSessionBound {
                        conversation_id: None,
                        turn_id,
                        runtime_session_id,
                        replaced,
                    },
                ),
            ),
        ),
        (
            "approval_delivered",
            boxed(
                (approval_id(), turn_id()).prop_map(|(approval_id, turn_id)| ApprovalDelivered {
                    conversation_id: None,
                    approval_id,
                    turn_id,
                }),
            ),
        ),
        (
            "turn_intent_recorded",
            boxed(
                (
                    turn_id(),
                    prop_oneof![Just(TurnIntentKind::Cancel), Just(TurnIntentKind::Resume)],
                )
                    .prop_map(|(turn_id, intent)| TurnIntentRecorded {
                        conversation_id: None,
                        turn_id,
                        intent,
                    }),
            ),
        ),
        (
            "approval_requested",
            boxed(approval().prop_map(|approval| ApprovalRequested { approval })),
        ),
        (
            "approval_review_updated",
            boxed(
                (approval_id(), started_review_state()).prop_map(|(approval_id, review)| {
                    ApprovalReviewUpdated {
                        conversation_id: None,
                        approval_id,
                        review,
                    }
                }),
            ),
        ),
        (
            "approval_decided",
            boxed(
                (
                    approval_id(),
                    prop_oneof![
                        pick(vec![Decider::User, Decider::ModeSwitch, Decider::AiReview])
                            .prop_map(|decider| ApprovalState::Approved { decider }),
                        Just(ApprovalState::Denied),
                    ],
                    proptest::option::of(pick(RememberScope::all())),
                )
                    .prop_map(|(approval_id, state, remember)| ApprovalDecided {
                        conversation_id: None,
                        approval_id,
                        state,
                        remember,
                    }),
            ),
        ),
        (
            "approval_voided",
            boxed(
                (approval_id(), pick(VoidReason::all())).prop_map(|(approval_id, reason)| {
                    ApprovalVoided {
                        conversation_id: None,
                        approval_id,
                        reason,
                    }
                }),
            ),
        ),
        (
            "approval_evaluated",
            boxed(
                (
                    turn_id(),
                    action(),
                    prop::collection::vec(rule_id(), 0..3),
                    text(),
                    verdict(),
                    rule_layer(),
                    any::<u32>(),
                    (
                        any::<u64>(),
                        any::<u64>(),
                        prop::collection::vec(action(), 0..3),
                        proptest::option::of(call_shape()),
                        prop::collection::vec(
                            (text(), text()).prop_map(|(a, b)| RulePathResolution {
                                path: a.into(),
                                resolved: b.into(),
                            }),
                            0..3,
                        ),
                        proptest::option::of(approval_id()),
                    ),
                )
                    .prop_map(
                        |(
                            turn_id,
                            action,
                            matched_rule_ids,
                            v,
                            verdict,
                            layer_hit,
                            duration_ms,
                            (
                                snapshot_lists_seq,
                                live_rules_seq,
                                sub_actions,
                                call_shape,
                                snapshot_rule_paths,
                                grant_approval_id,
                            ),
                        )| ApprovalEvaluated {
                            turn_id,
                            action,
                            matched_rule_ids,
                            ruleset_version: RuleSetVersion(v),
                            verdict,
                            layer_hit,
                            duration_ms,
                            snapshot_lists_seq: Seq(snapshot_lists_seq),
                            live_rules_seq: Seq(live_rules_seq),
                            sub_actions,
                            call_shape,
                            snapshot_rule_paths,
                            grant_approval_id,
                        },
                    ),
            ),
        ),
        (
            "rule_remembered",
            boxed(rule().prop_map(|rule| RuleRemembered { rule })),
        ),
        (
            "rule_revoked",
            boxed(rule_id().prop_map(|rule_id| RuleRevoked { rule_id })),
        ),
        (
            "rules_deleted",
            boxed(
                (
                    prop::collection::vec(rule_id(), 0..3),
                    pick(vec![
                        RulesDeletedCause::ConversationDeleted,
                        RulesDeletedCause::AssistantDeleted,
                    ]),
                )
                    .prop_map(|(rule_ids, cause)| RulesDeleted { rule_ids, cause }),
            ),
        ),
        (
            "approval_stale_result_dropped",
            boxed(
                (
                    approval_id(),
                    review_id(),
                    pick(vec![
                        ReviewPhase::Passed,
                        ReviewPhase::Failed,
                        ReviewPhase::Errored,
                        ReviewPhase::TimedOut,
                    ]),
                    pick(vec![
                        StaleReason::ReviewIdMismatch,
                        StaleReason::NotInProgress,
                        StaleReason::ModeNotAuto,
                    ]),
                )
                    .prop_map(|(approval_id, review_id, result, why)| {
                        ApprovalStaleResultDropped {
                            approval_id,
                            review_id,
                            result,
                            why,
                        }
                    }),
            ),
        ),
        (
            "user_action_proposed",
            boxed(card().prop_map(|card| UserActionProposed { card })),
        ),
        (
            "user_action_state_changed",
            boxed(
                (
                    user_action_id(),
                    user_action_state(),
                    proptest::option::of(artifact()),
                )
                    .prop_map(|(card_id, state, output_artifact)| {
                        UserActionStateChanged {
                            conversation_id: None,
                            card_id,
                            state,
                            output_artifact,
                        }
                    }),
            ),
        ),
        (
            "security_list_entry_added",
            // 清单条目就是规则条目：取清单层的规则
            boxed(
                (
                    rule(),
                    pick(vec![
                        SecurityList::CredentialCommands,
                        SecurityList::SensitiveDirs,
                        SecurityList::WritableDirs,
                    ]),
                )
                    .prop_map(|(mut rule, list)| {
                        rule.layer = list.layer();
                        SecurityListEntryAdded { rule }
                    }),
            ),
        ),
        (
            "security_list_entry_removed",
            boxed(rule_id().prop_map(|entry_id| SecurityListEntryRemoved { entry_id })),
        ),
        (
            "attachment_stored",
            boxed(
                (
                    (attachment_id(), conversation_id(), sha(), any::<u64>()),
                    text(),
                    text(),
                    prop_oneof![
                        path().prop_map(AttachmentOrigin::RepoPath),
                        artifact().prop_map(AttachmentOrigin::Copied),
                        artifact().prop_map(AttachmentOrigin::Pasted),
                        artifact().prop_map(AttachmentOrigin::CredentialInline),
                    ],
                    any::<bool>(),
                    ts(),
                )
                    .prop_map(
                        |(
                            (id, conversation_id, sha256, size),
                            mime,
                            name,
                            origin,
                            needs_confirmation,
                            created_at,
                        )| AttachmentStored {
                            attachment: Attachment {
                                id,
                                conversation_id,
                                sha256,
                                size,
                                mime,
                                name,
                                origin,
                                needs_confirmation,
                                created_at,
                            },
                        },
                    ),
            ),
        ),
        (
            "attachment_confirmed",
            boxed((conversation_id(), attachment_id()).prop_map(
                |(conversation_id, attachment_id)| AttachmentConfirmed {
                    conversation_id,
                    attachment_id,
                },
            )),
        ),
        (
            "runtime_updated",
            boxed(runtime().prop_map(|runtime| RuntimeUpdated { runtime })),
        ),
        (
            "quota_observed",
            boxed(
                (
                    runtime_id(),
                    prop::collection::vec(quota_window(), 0..3),
                    ts(),
                )
                    .prop_map(|(runtime_id, windows, observed_at)| QuotaObserved {
                        runtime_id,
                        windows,
                        observed_at,
                    }),
            ),
        ),
        (
            "usage_reported",
            boxed(
                (runtime_id(), turn_id(), usage()).prop_map(|(runtime_id, turn_id, usage)| {
                    UsageReported {
                        runtime_id,
                        turn_id,
                        usage,
                    }
                }),
            ),
        ),
        (
            "runtime_session_forget_failed",
            boxed(
                (
                    conversation_id(),
                    text(),
                    prop_oneof![
                        text().prop_map(|why| ForgetFailure::Refused { why }),
                        text().prop_map(|message| ForgetFailure::Failed { message }),
                    ],
                )
                    .prop_map(|(conversation_id, runtime_session_id, outcome)| {
                        RuntimeSessionForgetFailed {
                            conversation_id,
                            runtime_session_id,
                            outcome,
                        }
                    }),
            ),
        ),
        (
            "local_import_prompted",
            boxed(runtime_id().prop_map(|runtime_id| LocalImportPrompted { runtime_id })),
        ),
        (
            "local_config_imported",
            boxed(
                (runtime_id(), prop::collection::vec(local_item(), 0..3))
                    .prop_map(|(runtime_id, items)| LocalConfigImported { runtime_id, items }),
            ),
        ),
        (
            "mcp_secret_set",
            boxed(
                (text(), text()).prop_map(|(id, key_name)| ev::McpSecretSet {
                    mcp_server_id: McpServerId(id),
                    key_name,
                }),
            ),
        ),
        (
            "global_settings_changed",
            boxed(
                (
                    proptest::option::of(any::<bool>()),
                    proptest::option::of(any::<bool>()),
                    proptest::option::of(path()),
                )
                    .prop_map(|(playful_ui, autostart, projects_root)| {
                        GlobalSettingsChanged {
                            playful_ui,
                            autostart,
                            projects_root,
                        }
                    }),
            ),
        ),
        (
            "recovery_completed",
            boxed(
                prop::collection::vec(turn_id(), 0..3).prop_map(|interrupted_turn_ids| {
                    RecoveryCompleted {
                        interrupted_turn_ids,
                    }
                }),
            ),
        ),
    ]
}

/// 比 [`event_strategies`] 更宽的生成器：故意包含 proto 装不下、或语义上不该出现的值
/// （回合结束在非终态、额度超过 100、审批请求带已决定的状态……），用来验证
/// 「编码成功的值一定能无损解回；装不下的值在编码边界就报错」。
pub fn boundary_strategies() -> Vec<(&'static str, BoxedStrategy<DomainEvent>)> {
    vec![
        (
            "turn_finished",
            boxed((turn_id(), turn_state(), usage(), any::<bool>()).prop_map(
                |(turn_id, state, usage, celebrate)| TurnFinished {
                    conversation_id: None,
                    turn_id,
                    state,
                    usage,
                    celebrate,
                    change_summary: None,
                },
            )),
        ),
        (
            "quota_observed",
            boxed((runtime_id(), text(), any::<u8>(), ts()).prop_map(
                |(runtime_id, window, used_pct, at)| QuotaObserved {
                    runtime_id,
                    windows: vec![QuotaWindow {
                        window,
                        used_pct,
                        resets_at: None,
                        observed_at: at,
                    }],
                    observed_at: at,
                },
            )),
        ),
        (
            "approval_requested",
            boxed(
                (approval(), pick(ApprovalState::all())).prop_map(|(mut approval, state)| {
                    approval.state = state;
                    ApprovalRequested { approval }
                }),
            ),
        ),
        (
            "approval_review_updated",
            boxed(
                (approval_id(), review_state()).prop_map(|(approval_id, review)| {
                    ApprovalReviewUpdated {
                        conversation_id: None,
                        approval_id,
                        review,
                    }
                }),
            ),
        ),
        (
            "approval_decided",
            boxed(
                (approval_id(), pick(ApprovalState::all())).prop_map(|(approval_id, state)| {
                    ApprovalDecided {
                        conversation_id: None,
                        approval_id,
                        state,
                        remember: None,
                    }
                }),
            ),
        ),
        (
            "message_completed",
            boxed(
                (
                    conversation_id(),
                    message_id(),
                    body(),
                    pick(MessageStatus::all()),
                )
                    .prop_map(|(conversation_id, message_id, body, status)| {
                        MessageCompleted {
                            conversation_id,
                            message_id,
                            body,
                            status,
                        }
                    }),
            ),
        ),
    ]
}

fn session_replaced() -> impl Strategy<Value = SessionReplaced> {
    (
        text(),
        select(vec![
            SessionLostReason::NotFound,
            SessionLostReason::Corrupt,
            SessionLostReason::Incompatible,
        ]),
    )
        .prop_map(|(old_runtime_session_id, reason)| SessionReplaced {
            old_runtime_session_id,
            reason,
        })
}
