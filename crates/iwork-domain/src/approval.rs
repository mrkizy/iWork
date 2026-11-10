//! 审批：主状态 `ApprovalState` 与 AI 审查子状态 `ReviewState`（需求 12；技术设计 7.2、7.3）。
//!
//! 两张表：主状态表 [`APPROVAL_TRANSITIONS`] 与审查子状态表 [`REVIEW_TRANSITIONS`]。
//! 状态与触发都能穷举，表按「种类」写；`review_id`、不通过的理由这些不可穷举的载荷
//! 由 [`Approval::apply`] 填进去。AI 审查结果先过三条生效条件，不满足的是 `Stale`
//! （预料之中的乱序，丢弃并审计），不是表外转换（程序缺陷）。

use std::collections::BTreeSet;

use crate::action::{
    Action, ActionFingerprint, CallShapeKind, CanonicalPath, SandboxPlacement, ToolRef,
};
use crate::approval_mode::{ApprovalMode, RememberScope};
use crate::common::{Sha256, Timestamp};
use crate::id::{ApprovalId, ConversationId, ReviewId, RuleId, TurnId};
use crate::machine::{IllegalTransition, Transition, finite_enum, lookup};
use crate::rule::RuleSetVersion;

finite_enum! {
    /// 谁批准的。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum Decider {
        User,
        /// 切到全开时自动放行，界面文案「模式切换自动放行」
        ModeSwitch,
        AiReview,
    }
}

finite_enum! {
    /// 审批与亲手执行卡片作废的原因，与需求 12、技术设计 7.3 第 5 条、9.0、9.1a 逐条对应。
    ///
    /// 没有「等待上限」这个原因：hook 超时防护引起的中断不作废审批，类型上就写不出来。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum VoidReason {
        TurnCancelled,
        ConversationDeleted,
        TurnAbandonedAfterInterrupt,
        AssistantDeleted,
        /// 所属回合失败（如等审批时运行时进程意外退出）：回合已是终态，审批再批也没有回合可续
        TurnFailed,
    }
}

finite_enum! {
    /// 审批主状态。终态：`Approved`、`Denied`、`Voided`。审批不超时。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum ApprovalState {
        Pending,
        Approved { decider: Decider },
        Denied,
        Voided { reason: VoidReason },
    }
}

impl ApprovalState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

finite_enum! {
    /// 审批触发的种类（去掉 `review_id`、理由等载荷），转换表按它写。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum ApprovalTriggerKind {
        UserAllow,
        UserDeny,
        ModeSwitchAllows,
        Void { reason: VoidReason },
        AiReviewStarted,
        AiReviewPassed,
        AiReviewFailed,
        AiReviewErrored,
        AiReviewTimedOut,
        /// 有效档位离开自动：手动与全开都不经 AI 审查，未放行的审查转问用户
        ModeChangedFromAuto,
        /// 进行中的审查失去执行者（后台服务崩溃重启）：放弃、转问用户（T07 Q9）
        AiReviewAbandoned,
    }
}

use ApprovalState as A;
use ApprovalTriggerKind as AT;

/// 审批主状态表（方案 4.3）。
///
/// AI 审查相关的触发在主状态上是 `Pending` 自环：它们只能发生在待审批期间，
/// 主状态的变化（`AiReviewPassed` 放行除外）都在审查子状态表里。
pub const APPROVAL_TRANSITIONS: &[Transition<ApprovalState, ApprovalTriggerKind>] = &[
    (
        A::Pending,
        AT::UserAllow,
        A::Approved {
            decider: Decider::User,
        },
    ),
    (A::Pending, AT::UserDeny, A::Denied),
    (
        A::Pending,
        AT::ModeSwitchAllows,
        A::Approved {
            decider: Decider::ModeSwitch,
        },
    ),
    (
        A::Pending,
        AT::AiReviewPassed,
        A::Approved {
            decider: Decider::AiReview,
        },
    ),
    (
        A::Pending,
        AT::Void {
            reason: VoidReason::TurnCancelled,
        },
        A::Voided {
            reason: VoidReason::TurnCancelled,
        },
    ),
    (
        A::Pending,
        AT::Void {
            reason: VoidReason::ConversationDeleted,
        },
        A::Voided {
            reason: VoidReason::ConversationDeleted,
        },
    ),
    (
        A::Pending,
        AT::Void {
            reason: VoidReason::TurnAbandonedAfterInterrupt,
        },
        A::Voided {
            reason: VoidReason::TurnAbandonedAfterInterrupt,
        },
    ),
    (
        A::Pending,
        AT::Void {
            reason: VoidReason::AssistantDeleted,
        },
        A::Voided {
            reason: VoidReason::AssistantDeleted,
        },
    ),
    (
        A::Pending,
        AT::Void {
            reason: VoidReason::TurnFailed,
        },
        A::Voided {
            reason: VoidReason::TurnFailed,
        },
    ),
    (A::Pending, AT::AiReviewStarted, A::Pending),
    (A::Pending, AT::AiReviewFailed, A::Pending),
    (A::Pending, AT::AiReviewErrored, A::Pending),
    (A::Pending, AT::AiReviewTimedOut, A::Pending),
    (A::Pending, AT::ModeChangedFromAuto, A::Pending),
    (A::Pending, AT::AiReviewAbandoned, A::Pending),
];

pub const APPROVAL_MACHINE: &str = "approval";

/// 审批主状态的表外转换错误。
pub type ApprovalIllegal = IllegalTransition<ApprovalState, ApprovalTriggerKind>;

/// 按 [`APPROVAL_TRANSITIONS`] 求主状态的下一状态。
pub fn next(
    state: ApprovalState,
    trigger: ApprovalTriggerKind,
) -> Result<ApprovalState, ApprovalIllegal> {
    lookup(APPROVAL_MACHINE, APPROVAL_TRANSITIONS, state, trigger)
}

finite_enum! {
    /// AI 审查子状态的种类（去掉 `review_id` 与理由）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum ReviewPhase {
        NotReviewed,
        InProgress,
        Passed,
        Failed,
        Errored,
        TimedOut,
        Abandoned,
    }
}

finite_enum! {
    /// 改变审查子状态的触发。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum ReviewTrigger {
        Started,
        Passed,
        Failed,
        Errored,
        TimedOut,
        ModeChangedFromAuto,
        /// 审查回合失去执行者（T07 Q9）
        ExecutorLost,
    }
}

use ReviewPhase as R;
use ReviewTrigger as RT;

/// 审查子状态表（方案 4.3）。只在主状态 `Pending` 期间变化；不通过、出错、超时、
/// 被手动档作废之后都转为问用户。
pub const REVIEW_TRANSITIONS: &[Transition<ReviewPhase, ReviewTrigger>] = &[
    (R::NotReviewed, RT::Started, R::InProgress),
    (R::InProgress, RT::Passed, R::Passed),
    (R::InProgress, RT::Failed, R::Failed),
    (R::InProgress, RT::Errored, R::Errored),
    (R::InProgress, RT::TimedOut, R::TimedOut),
    (R::InProgress, RT::ModeChangedFromAuto, R::Abandoned),
    (R::InProgress, RT::ExecutorLost, R::Abandoned),
];

pub const REVIEW_MACHINE: &str = "approval_review";

/// 审查子状态的表外转换错误。
pub type ReviewIllegal = IllegalTransition<ReviewPhase, ReviewTrigger>;

/// 按 [`REVIEW_TRANSITIONS`] 求审查子状态的下一状态。
pub fn next_review(
    phase: ReviewPhase,
    trigger: ReviewTrigger,
) -> Result<ReviewPhase, ReviewIllegal> {
    lookup(REVIEW_MACHINE, REVIEW_TRANSITIONS, phase, trigger)
}

impl ApprovalTriggerKind {
    /// 这个触发是否改变审查子状态；不改变的返回 `None`。
    pub fn review_trigger(self) -> Option<ReviewTrigger> {
        match self {
            AT::UserAllow | AT::UserDeny | AT::ModeSwitchAllows | AT::Void { .. } => None,
            AT::AiReviewStarted => Some(RT::Started),
            AT::AiReviewPassed => Some(RT::Passed),
            AT::AiReviewFailed => Some(RT::Failed),
            AT::AiReviewErrored => Some(RT::Errored),
            AT::AiReviewTimedOut => Some(RT::TimedOut),
            AT::ModeChangedFromAuto => Some(RT::ModeChangedFromAuto),
            AT::AiReviewAbandoned => Some(RT::ExecutorLost),
        }
    }

    /// 是否是 AI 审查回来的结果。结果要先过三条生效条件，不满足即 `Stale`。
    pub fn is_review_result(self) -> bool {
        matches!(
            self,
            AT::AiReviewPassed | AT::AiReviewFailed | AT::AiReviewErrored | AT::AiReviewTimedOut
        )
    }
}

/// AI 审查子状态（带审查身份）。审查开始后的每个状态都留着 `review_id`，
/// 落盘后重建投影也能判「迟到结果作废」。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReviewState {
    NotReviewed,
    InProgress {
        review_id: ReviewId,
    },
    Passed {
        review_id: ReviewId,
        reason: String,
    },
    Failed {
        review_id: ReviewId,
        reason: String,
    },
    Errored {
        review_id: ReviewId,
    },
    TimedOut {
        review_id: ReviewId,
    },
    Abandoned {
        review_id: ReviewId,
        reason: AbandonReason,
    },
}

/// 进行中的审查为什么被放弃。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbandonReason {
    /// 有效档位离开自动
    ModeChangedFromAuto,
    /// 审查回合失去执行者：后台服务在审查进行中崩溃重启（T07 Q9）
    ExecutorLost,
}

impl ReviewState {
    pub fn phase(&self) -> ReviewPhase {
        match self {
            Self::NotReviewed => R::NotReviewed,
            Self::InProgress { .. } => R::InProgress,
            Self::Passed { .. } => R::Passed,
            Self::Failed { .. } => R::Failed,
            Self::Errored { .. } => R::Errored,
            Self::TimedOut { .. } => R::TimedOut,
            Self::Abandoned { .. } => R::Abandoned,
        }
    }

    pub fn review_id(&self) -> Option<ReviewId> {
        match self {
            Self::NotReviewed => None,
            Self::InProgress { review_id }
            | Self::Passed { review_id, .. }
            | Self::Failed { review_id, .. }
            | Self::Errored { review_id }
            | Self::TimedOut { review_id }
            | Self::Abandoned { review_id, .. } => Some(*review_id),
        }
    }
}

/// 审批触发（带载荷）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ApprovalTrigger {
    UserAllow {
        remember: RememberScope,
    },
    UserDeny,
    ModeSwitchAllows,
    Void {
        reason: VoidReason,
    },
    AiReviewStarted {
        review_id: ReviewId,
    },
    AiReviewPassed {
        review_id: ReviewId,
        reason: String,
    },
    AiReviewFailed {
        review_id: ReviewId,
        reason: String,
    },
    AiReviewErrored {
        review_id: ReviewId,
    },
    AiReviewTimedOut {
        review_id: ReviewId,
    },
    ModeChangedFromAuto,
    /// 恢复时发现审查进行中但没有执行者（T07 Q9）。`review_id` 必须是当前进行中的那次
    AiReviewAbandoned {
        review_id: ReviewId,
    },
}

impl ApprovalTrigger {
    pub fn kind(&self) -> ApprovalTriggerKind {
        match self {
            Self::UserAllow { .. } => AT::UserAllow,
            Self::UserDeny => AT::UserDeny,
            Self::ModeSwitchAllows => AT::ModeSwitchAllows,
            Self::Void { reason } => AT::Void { reason: *reason },
            Self::AiReviewStarted { .. } => AT::AiReviewStarted,
            Self::AiReviewPassed { .. } => AT::AiReviewPassed,
            Self::AiReviewFailed { .. } => AT::AiReviewFailed,
            Self::AiReviewErrored { .. } => AT::AiReviewErrored,
            Self::AiReviewTimedOut { .. } => AT::AiReviewTimedOut,
            Self::ModeChangedFromAuto => AT::ModeChangedFromAuto,
            Self::AiReviewAbandoned { .. } => AT::AiReviewAbandoned,
        }
    }

    /// 审查相关触发带的审查身份。
    pub fn review_id(&self) -> Option<ReviewId> {
        match self {
            Self::AiReviewStarted { review_id }
            | Self::AiReviewPassed { review_id, .. }
            | Self::AiReviewFailed { review_id, .. }
            | Self::AiReviewErrored { review_id }
            | Self::AiReviewTimedOut { review_id }
            | Self::AiReviewAbandoned { review_id } => Some(*review_id),
            Self::UserAllow { .. }
            | Self::UserDeny
            | Self::ModeSwitchAllows
            | Self::Void { .. }
            | Self::ModeChangedFromAuto => None,
        }
    }
}

/// 审批种类：沙箱外执行是询问的一种，批准后命令在沙箱外运行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApprovalKind {
    Ask,
    OutsideSandbox,
}

/// 恢复与复查所需的最小调用记录（T07 Q8、方案 6.2）。全部是归一后的值，不含原始 JSON 全文。
///
/// 指纹集合不单独存，由 `actions` 算（[`ApprovalCallRecord::fingerprints`]），一处真值。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApprovalCallRecord {
    pub tool: ToolRef,
    pub cwd: CanonicalPath,
    pub shape: CallShapeKind,
    pub placement: SandboxPlacement,
    pub bypass_sandbox_requested: bool,
    /// 全部子动作，保持顺序；代表动作是 `actions[representative]`。子动作的 `raw_summary` 为空，
    /// 命令原文只在 `Approval.action` 上存一份（T07 7/F-2）
    pub actions: Vec<Action>,
    pub representative: u32,
    /// sha256(工具名 ‖ 规范化 JSON)：防文字层面的改写
    pub raw_input_digest: Sha256,
    /// sha256(有序子动作 ‖ 形状 ‖ 沙箱位置 ‖ cwd)：防同文字不同解析
    pub normalized_digest: Sha256,
}

impl ApprovalCallRecord {
    /// 全部子动作的指纹，去重（拒绝记忆按它工作）。
    pub fn fingerprints(&self) -> BTreeSet<ActionFingerprint> {
        self.actions.iter().map(Action::fingerprint).collect()
    }
}

/// 审批实体。状态只经 [`Approval::apply`] 改变。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub id: ApprovalId,
    pub turn_id: TurnId,
    pub conversation_id: ConversationId,
    pub kind: ApprovalKind,
    /// 代表动作，给卡片展示
    pub action: Action,
    /// 被审批调用的完整记录（T07 Q8）。旧事件没有：拒绝记忆退化为只记 `action`，恢复后不发一次性放行
    /// 装箱：记录有几百字节，审批常被整条放进集合与枚举
    pub call: Option<Box<ApprovalCallRecord>>,
    pub matched_rules: Vec<RuleId>,
    pub ruleset_version: RuleSetVersion,
    pub state: ApprovalState,
    pub review: ReviewState,
    /// 卡片上提供的记住范围；必问与清单外沙箱外执行时为空（技术设计 7.3 第 2 条）
    pub offered_remember: Vec<RememberScope>,
    /// 「本项目内 `npm test …` 不再问」这类文案，由后台生成，网页只显示
    pub remember_label: String,
    pub requested_at: Timestamp,
}

/// 应用触发时调用方提供的当下事实。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalContext {
    /// 此刻这条审批所在成员的有效档位（[`crate::approval_mode::effective_mode`] 的结果）
    pub mode_now: ApprovalMode,
}

/// 触发生效后哪些部分变了，调用方据此决定发哪些事件：
/// 主状态变了发 `ApprovalDecided` / `ApprovalVoided`，子状态变了发 `ApprovalReviewUpdated`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub state_changed: bool,
    pub review_changed: bool,
}

/// 迟到或不对号的审查结果为什么被丢弃，对应三条生效条件（按 ②①③ 的顺序判，报第一条不满足的）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StaleReason {
    /// 审批已不在「待审批 + 审查进行中」：超时转交用户、被手动档作废、用户已决定、模式切换已放行
    NotInProgress,
    /// 结果对应的不是当前进行中的这一次审查
    ReviewIdMismatch,
    /// 当前有效档位不是自动：用户已切离自动档，AI 审查不算数
    ModeNotAuto,
}

/// [`Approval::apply`] 的失败。两种失败的性质不同，调用方必须分开处理。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalApplyError {
    /// 预料之中的乱序结果：审批不变，只写一条 audit 流的 `ApprovalStaleResultDropped`。
    #[error("审查结果作废（{trigger:?}，review_id {review_id}）：{why:?}")]
    Stale {
        trigger: ApprovalTriggerKind,
        review_id: ReviewId,
        why: StaleReason,
    },
    /// 程序缺陷：主状态表外转换。
    #[error(transparent)]
    IllegalTransition(#[from] ApprovalIllegal),
    /// 程序缺陷：审查子状态表外转换。
    #[error(transparent)]
    IllegalReviewTransition(#[from] ReviewIllegal),
}

impl Approval {
    /// 应用一个触发。失败时审批的任何字段都不变。
    ///
    /// AI 审查结果（通过、不通过、出错、超时）只在三条同时满足时生效，否则返回
    /// [`ApprovalApplyError::Stale`]：① `review_id` 是当前进行中的这次审查；
    /// ② 主状态仍是 `Pending` 且子状态仍是 `InProgress`；③ `ctx.mode_now` 是 `Auto`。
    pub fn apply(
        &mut self,
        trigger: ApprovalTrigger,
        ctx: &ApprovalContext,
    ) -> Result<Applied, ApprovalApplyError> {
        let kind = trigger.kind();
        if let ApprovalTrigger::AiReviewAbandoned { review_id } = trigger
            && self.review.review_id() != Some(review_id)
            && self.review.phase() == R::InProgress
        {
            return Err(ApprovalApplyError::Stale {
                trigger: kind,
                review_id,
                why: StaleReason::ReviewIdMismatch,
            });
        }
        if kind.is_review_result() {
            let review_id = trigger
                .review_id()
                .expect("审查结果触发一定带 review_id（见 ApprovalTrigger::review_id）");
            if let Some(why) = self.stale_reason(review_id, ctx) {
                return Err(ApprovalApplyError::Stale {
                    trigger: kind,
                    review_id,
                    why,
                });
            }
        }

        let state = next(self.state, kind)?;
        let review = match kind.review_trigger() {
            Some(review_trigger) => {
                let phase = next_review(self.review.phase(), review_trigger)?;
                let review = self.review_after(&trigger);
                debug_assert_eq!(
                    review.phase(),
                    phase,
                    "审查子状态载荷与 REVIEW_TRANSITIONS 不一致"
                );
                review
            }
            None => self.review.clone(),
        };

        let applied = Applied {
            state_changed: state != self.state,
            review_changed: review != self.review,
        };
        self.state = state;
        self.review = review;
        Ok(applied)
    }

    /// 被审批调用的全部子动作指纹；没有调用记录的旧审批只有代表动作（T07 方案 6.2）。
    pub fn fingerprints(&self) -> BTreeSet<ActionFingerprint> {
        match &self.call {
            Some(call) => call.fingerprints(),
            None => BTreeSet::from([self.action.fingerprint()]),
        }
    }

    /// 是否计入「待你处理」：待审批且不在 AI 审查中（审查中用户还没被问到）。
    pub fn awaits_user(&self) -> bool {
        self.state == A::Pending && self.review.phase() != R::InProgress
    }

    fn stale_reason(&self, got: ReviewId, ctx: &ApprovalContext) -> Option<StaleReason> {
        let ReviewState::InProgress { review_id: current } = self.review else {
            return Some(StaleReason::NotInProgress);
        };
        if self.state != A::Pending {
            return Some(StaleReason::NotInProgress);
        }
        if current != got {
            return Some(StaleReason::ReviewIdMismatch);
        }
        if ctx.mode_now != ApprovalMode::Auto {
            return Some(StaleReason::ModeNotAuto);
        }
        None
    }

    /// 已通过子状态表检查后，按触发填审查子状态的载荷。
    fn review_after(&self, trigger: &ApprovalTrigger) -> ReviewState {
        match trigger {
            ApprovalTrigger::AiReviewStarted { review_id } => ReviewState::InProgress {
                review_id: *review_id,
            },
            ApprovalTrigger::AiReviewPassed { review_id, reason } => ReviewState::Passed {
                review_id: *review_id,
                reason: reason.clone(),
            },
            ApprovalTrigger::AiReviewFailed { review_id, reason } => ReviewState::Failed {
                review_id: *review_id,
                reason: reason.clone(),
            },
            ApprovalTrigger::AiReviewErrored { review_id } => ReviewState::Errored {
                review_id: *review_id,
            },
            ApprovalTrigger::AiReviewTimedOut { review_id } => ReviewState::TimedOut {
                review_id: *review_id,
            },
            ApprovalTrigger::ModeChangedFromAuto => ReviewState::Abandoned {
                review_id: self
                    .review
                    .review_id()
                    .expect("REVIEW_TRANSITIONS 只允许 InProgress 收到 ModeChangedFromAuto，它一定有 review_id"),
                reason: AbandonReason::ModeChangedFromAuto,
            },
            ApprovalTrigger::AiReviewAbandoned { review_id } => ReviewState::Abandoned {
                review_id: *review_id,
                reason: AbandonReason::ExecutorLost,
            },
            ApprovalTrigger::UserAllow { .. }
            | ApprovalTrigger::UserDeny
            | ApprovalTrigger::ModeSwitchAllows
            | ApprovalTrigger::Void { .. } => {
                unreachable!("{:?} 不改变审查子状态，不应走到这里", trigger.kind())
            }
        }
    }
}

// ---------- 提交时校验（T02 方案 2.6「状态前提」、T07 方案 6.10.7）----------

/// 已落盘的状态接不了这条事件。`message` 点名现在的状态与事件要的状态，由存储层加上实体 id。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PreconditionFailed {
    pub message: String,
}

impl PreconditionFailed {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// 审批事件的状态前提：已落盘的主状态 `state` 与审查子状态 `review` 能不能接 `event`。
/// 只看审批流里会改已有审批的三种事件，别的事件一律通过。读的是 [`APPROVAL_TRANSITIONS`] 与
/// [`REVIEW_TRANSITIONS`]：主状态表的边都从 `Pending` 出发，所以「主状态是 Pending」就是「是表里的边」。
pub fn check_committed(
    state: ApprovalState,
    review: &ReviewState,
    event: &crate::event::DomainEvent,
) -> Result<(), PreconditionFailed> {
    use crate::event::DomainEvent as E;
    let require_pending = |what: &str| {
        if state == ApprovalState::Pending {
            Ok(())
        } else {
            Err(PreconditionFailed::new(format!(
                "{what}要求审批是 Pending，现在是 {state:?}"
            )))
        }
    };
    match event {
        E::ApprovalDecided(e) => match e.state {
            ApprovalState::Approved { .. } | ApprovalState::Denied => require_pending("批准或拒绝"),
            other => Err(PreconditionFailed::new(format!(
                "ApprovalDecided 只记批准与拒绝，不能是 {other:?}"
            ))),
        },
        E::ApprovalVoided(_) => require_pending("作废"),
        E::ApprovalReviewUpdated(e) => {
            require_pending("审查更新")?;
            let (from, to) = (review.phase(), e.review.phase());
            if !REVIEW_TRANSITIONS
                .iter()
                .any(|(a, _, b)| *a == from && *b == to)
            {
                return Err(PreconditionFailed::new(format!(
                    "审查子状态 {from:?} → {to:?} 不是 REVIEW_TRANSITIONS 里的边"
                )));
            }
            if to != ReviewPhase::InProgress && e.review.review_id() != review.review_id() {
                return Err(PreconditionFailed::new(format!(
                    "审查结果的 review_id {:?} 与正在跑的 {:?} 不同",
                    e.review.review_id(),
                    review.review_id()
                )));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// 有没有记录（不是「还在跑」：回合结束了记录仍在）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Presence {
    Present,
    Gone,
}

/// 回合所在的对话：在、删除中、没有记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConversationPresence {
    Present,
    Deleting,
    Gone,
}

/// 一条审批或卡片的归属事实（T07 方案 6.10.7）：判断「它的回合、对话、助手还在不在」要用的三样。
/// 存储一侧读行、引擎折叠一侧查表，各自给出这一份；判断只在 [`owner_alive`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OwnerFacts {
    /// 所属回合有没有记录
    pub turn: Presence,
    /// 回合所在的对话。`turn` 是 `Gone` 时不看
    pub conversation: ConversationPresence,
    /// 回合的助手有没有记录。`turn` 是 `Gone` 时不看
    pub assistant: Presence,
}

/// 缺的是哪一样。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum OwnerGone {
    #[error("回合没有记录")]
    Turn,
    #[error("对话正在删除")]
    ConversationDeleting,
    #[error("对话没有记录")]
    Conversation,
    #[error("助手没有记录")]
    Assistant,
}

/// 归属存续：按回合 → 对话 → 助手的顺序看，报第一样缺的。
pub fn owner_alive(facts: &OwnerFacts) -> Result<(), OwnerGone> {
    if facts.turn == Presence::Gone {
        return Err(OwnerGone::Turn);
    }
    match facts.conversation {
        ConversationPresence::Gone => return Err(OwnerGone::Conversation),
        ConversationPresence::Deleting => return Err(OwnerGone::ConversationDeleting),
        ConversationPresence::Present => {}
    }
    if facts.assistant == Presence::Gone {
        return Err(OwnerGone::Assistant);
    }
    Ok(())
}

/// 归属不在时用什么原因作废：助手没了 → `AssistantDeleted`；别的三样 → `ConversationDeleted`
/// （回合的记录只会随对话一起消失）。
pub fn void_reason_for(gone: OwnerGone) -> VoidReason {
    match gone {
        OwnerGone::Assistant => VoidReason::AssistantDeleted,
        OwnerGone::Turn | OwnerGone::ConversationDeleting | OwnerGone::Conversation => {
            VoidReason::ConversationDeleted
        }
    }
}

/// 这条事件写入前要不要查归属存续（建审批、建卡片、批准、卡片启动）。拒绝、作废、关闭、审查更新不查：
/// 孤儿要能被清掉。存储与引擎共用这一处。
pub fn requires_owner(event: &crate::event::DomainEvent) -> bool {
    use crate::event::DomainEvent as E;
    match event {
        E::ApprovalRequested(_) | E::UserActionProposed(_) => true,
        E::ApprovalDecided(e) => matches!(e.state, ApprovalState::Approved { .. }),
        E::UserActionStateChanged(e) => {
            matches!(e.state, crate::user_action::UserActionState::Starting)
        }
        _ => false,
    }
}
