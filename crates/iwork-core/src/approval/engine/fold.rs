//! 引擎内存就是已提交事件的折叠（T07 方案 6.10.2）。
//!
//! [`Folded::apply`] 是唯一的写入口：纯函数，不碰端口、不读时钟，按 `seq` 递增一条一条应用。
//! 本地决定只产生要提交的事件，不直接改这里；提交之后由追平折进来（方向 1）。所以提交失败、
//! 回执被取消、谁提交的，都不影响结果——只看日志。
//!
//! 折叠表（加一种事件 = `apply` 里加一个分支；没列的事件什么都不做）见方案 6.10.2。和投影器一样，
//! 审批与卡片的事件在内存里对不上（给不在待审批里的审批记结论）就报错并点名 seq，不兜底：存储在
//! 提交时已按同一组 domain 函数校验过状态前提（6.10.7），对得上的日志不会走到这里。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use iwork_domain::approval::{
    Approval, ApprovalState, ConversationPresence, Decider, OwnerFacts, Presence, ReviewState,
    VoidReason, owner_alive, void_reason_for,
};
use iwork_domain::approval_mode::{ApprovalMode, ProjectDefaultMode};
use iwork_domain::conversation::{ConversationKind, ConversationState};
use iwork_domain::event::{ApprovalModeTarget, DomainEvent, StreamKind};
use iwork_domain::id::{
    ApprovalId, AssistantId, ConversationId, ProjectId, Seq, TurnId, UserActionId,
};
use iwork_domain::message::Author;
use iwork_domain::rule::{RuleEntry, Verdict};
use iwork_domain::turn::TurnState;
use iwork_domain::user_action::{UserActionCard, UserActionState};
use iwork_ports::agent::PermissionDecision;
use iwork_ports::store::StoredEvent;

use super::changes::modes_after;
use super::context::ReviewMessages;
use super::{
    ConversationModes, GrantOnce, MSG_TURN_CANCELLED, MSG_USER_DENIED, card_settled_message,
};
use crate::approval::denial::{DenialEvent, DenialMemory};

/// 引擎关心的流（方案 6.10.4）。Runtime / Settings / System 不读。
pub const ENGINE_STREAMS: [StreamKind; 7] = [
    StreamKind::Project,
    StreamKind::Assistant,
    StreamKind::Conversation,
    StreamKind::Turn,
    StreamKind::Approval,
    StreamKind::Audit,
    StreamKind::Rule,
];

/// 这条事件在不在引擎读的流里。
pub fn in_engine_streams(event: &DomainEvent) -> bool {
    ENGINE_STREAMS.contains(&event.stream().0)
}

/// 折叠不认的事件序列：日志与折叠的前提对不上（缺陷，或加提交时校验之前留下的非法历史）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("审批引擎折叠在 seq {seq} 的 {kind} 上出错：{why}")]
pub struct FoldError {
    pub seq: Seq,
    pub kind: &'static str,
    pub why: String,
}

/// 折叠应用一条事件后要外壳去做的事（方案 6.10.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// 叫醒挂着这条审批的 hook
    Answer {
        approval: ApprovalId,
        decision: PermissionDecision,
    },
    /// 审查不用再跑了
    StopReview { approval: ApprovalId },
    /// 卡片有了结果，把话带给挂着的 hook
    CardSettled { card: UserActionId, message: String },
    /// 回合结束：外壳丢掉这个回合名下剩下的会话绑定
    TurnClosed { turn: TurnId },
    /// 对话整条没了：外壳丢掉这个对话的绑定与令牌
    ConversationGone { conversation: ConversationId },
}

/// 要作废的一项（清孤儿、作废计划用）。原因由 domain 的 `void_reason_for` 定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoidTarget {
    Approval {
        id: ApprovalId,
        reason: VoidReason,
    },
    Card {
        id: UserActionId,
        reason: VoidReason,
    },
}

/// 一个回合属于谁：来自 `TurnQueued`，留到这个回合在投影里的行消失为止（随对话整条删掉）。
/// `TurnFinished` 不删：它名下还没处理完的审批与卡片之后仍要靠它判归属（方案审查 3/F-1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnOwner {
    pub conversation: ConversationId,
    pub assistant: AssistantId,
}

/// 一条待审批。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub approval: Approval,
    /// 这条审批存在期间，它的回合离开过「等审批」：编排手里它那张票已经清掉，之后给出的结果编排不会采纳，
    /// 也就不会有交付事件。只进不出。批准一条 detached 的审批 = 直接发一次性放行。
    pub detached: bool,
}

/// 一个对话的折叠。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationFold {
    pub modes: ConversationModes,
    /// 私聊属于哪个助手；项目对话为 `None`。助手删除时它的私聊整条消失
    pub dm_assistant: Option<AssistantId>,
    /// 这个对话名下的回合：对话整条删掉时据此清 `Folded::turns`
    pub turns: BTreeSet<TurnId>,
    pub deleting: bool,
    pub denial: DenialMemory,
    pub pending: BTreeMap<ApprovalId, PendingApproval>,
    /// 没到终态的卡片
    pub cards: BTreeMap<UserActionId, UserActionCard>,
    /// 已批准、授权已经交到编排手里、还没见到 `ApprovalDelivered` 的审批（T06 方案 4.2 的 R1）
    pub awaiting_delivery: BTreeMap<ApprovalId, GrantOnce>,
    /// 一次性放行：已批准、确定没交付出去、回合还没结束
    pub grants: BTreeMap<ApprovalId, GrantOnce>,
    pub(super) review_messages: ReviewMessages,
}

/// 引擎的全部判定用状态。只有 [`Folded::apply`] 能改它；`cursor` 只增不减。
/// 不变量：引擎流里 seq ≤ cursor 的事件全部按序应用过，一条不多、一条不少。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Folded {
    cursor: Seq,
    /// 活规则；审计的 `live_rules_seq` 就是求值那一刻的 `cursor`
    live_rules: Arc<Vec<RuleEntry>>,
    project_defaults: BTreeMap<ProjectId, ProjectDefaultMode>,
    /// 最后一条会让会话快照过期的事件（清单、项目、助手增删）
    snapshot_inputs_seq: Seq,
    /// 现存的助手。和投影里 `assistants` 表有没有这一行是同一件事
    assistants: BTreeSet<AssistantId>,
    /// 已落盘状态是 `WaitingApproval` 的回合
    waiting_turns: BTreeSet<TurnId>,
    /// 已落盘状态是 `Interrupted{..}` 的回合
    interrupted_turns: BTreeSet<TurnId>,
    /// 没结束的回合最近一次进入 `Interrupted` 的 seq：判「一次求值之后回合有没有中断过」
    last_interrupt: BTreeMap<TurnId, Seq>,
    /// 「这张表里有没有这个回合」和「`turns` 表里有没有这一行」是同一件事（6.10.7 的归属事实）
    turns: BTreeMap<TurnId, TurnOwner>,
    conversations: BTreeMap<ConversationId, ConversationFold>,
    /// 只含待审批
    approval_index: BTreeMap<ApprovalId, ConversationId>,
    /// 只含没到终态的卡片
    card_index: BTreeMap<UserActionId, ConversationId>,
}

fn deny(message: &str) -> PermissionDecision {
    PermissionDecision::Deny {
        message: message.into(),
    }
}

fn project_default_of(mode: Option<ApprovalMode>) -> ProjectDefaultMode {
    match mode {
        Some(ApprovalMode::Auto) => ProjectDefaultMode::Auto,
        _ => ProjectDefaultMode::Manual,
    }
}

impl Folded {
    /// 从头重放：启动与测试用同一个 [`Folded::apply`]。不在引擎流里的事件跳过（和追平读到的一样）。
    pub fn replay(events: impl IntoIterator<Item = StoredEvent>) -> Result<Self, FoldError> {
        let mut folded = Self::default();
        for e in events {
            if in_engine_streams(&e.event) {
                folded.apply(e.seq, &e.event)?;
            }
        }
        Ok(folded)
    }

    /// 唯一的写入口。`seq` 必须大于游标；出错时什么都不改、游标不动。
    pub fn apply(&mut self, seq: Seq, event: &DomainEvent) -> Result<Vec<Effect>, FoldError> {
        let fail = |why: String| FoldError {
            seq,
            kind: event.kind(),
            why,
        };
        if seq <= self.cursor {
            return Err(fail(format!("序号不大于游标 {}", self.cursor)));
        }
        let effects = self.apply_inner(seq, event).map_err(fail)?;
        self.cursor = seq;
        Ok(effects)
    }

    /// 各分支先查后改：查不到就在改任何东西之前返回错误。
    fn apply_inner(&mut self, seq: Seq, event: &DomainEvent) -> Result<Vec<Effect>, String> {
        use DomainEvent as E;
        let mut effects = Vec::new();
        match event {
            E::ProjectAdded(e) => {
                self.project_defaults
                    .insert(e.project.id, e.project.settings.default_approval_mode);
                self.snapshot_inputs_seq = seq;
            }
            E::ProjectSettingsUpdated(e) => {
                self.project_defaults
                    .insert(e.project_id, e.settings.default_approval_mode);
            }
            E::ProjectRemoved(_)
            | E::SecurityListEntryAdded(_)
            | E::SecurityListEntryRemoved(_) => {
                self.snapshot_inputs_seq = seq;
            }
            E::AssistantCreated(e) => {
                self.assistants.insert(e.assistant.id);
                self.snapshot_inputs_seq = seq;
            }
            E::AssistantDeleted(e) => {
                self.assistants.remove(&e.assistant_id);
                self.snapshot_inputs_seq = seq;
                // 投影器在这条事件上级联删它的私聊，折叠跟着删；项目对话里的待审批等作废事件
                let dms: Vec<ConversationId> = self
                    .conversations
                    .iter()
                    .filter(|(_, c)| c.dm_assistant == Some(e.assistant_id))
                    .map(|(id, _)| *id)
                    .collect();
                for c in dms {
                    effects.extend(self.drop_conversation(c));
                }
            }
            E::ConversationCreated(e) => {
                let c = &e.conversation;
                let (modes, dm_assistant) = match c.kind {
                    ConversationKind::Project { project_id } => (
                        ConversationModes::Project {
                            project_default: self.project_default(project_id),
                            conversation_setting: c.approval_mode,
                            member_overrides: BTreeMap::new(),
                        },
                        None,
                    ),
                    ConversationKind::AssistantDm {
                        assistant_id,
                        context_project,
                    } => (
                        ConversationModes::Dm {
                            own_setting: c.approval_mode,
                            selected_project_default: context_project
                                .map(|p| self.project_default(p)),
                        },
                        Some(assistant_id),
                    ),
                };
                self.conversations.insert(
                    c.id,
                    ConversationFold {
                        modes,
                        dm_assistant,
                        turns: BTreeSet::new(),
                        deleting: c.state == ConversationState::Deleting,
                        denial: DenialMemory::default(),
                        pending: BTreeMap::new(),
                        cards: BTreeMap::new(),
                        awaiting_delivery: BTreeMap::new(),
                        grants: BTreeMap::new(),
                        review_messages: ReviewMessages::default(),
                    },
                );
            }
            E::ApprovalModeChanged(e) => match e.target {
                ApprovalModeTarget::Project(p) => {
                    self.project_defaults.insert(p, project_default_of(e.mode));
                }
                ApprovalModeTarget::Conversation(c)
                | ApprovalModeTarget::Member {
                    conversation_id: c, ..
                } => {
                    if let Some(conv) = self.conversations.get_mut(&c)
                        && let Some(modes) = modes_after(&conv.modes, e.target, e.mode)
                    {
                        conv.modes = modes;
                    }
                }
            },
            E::DmProjectSwitched(e) => {
                let default = e.project_id.map(|p| self.project_default(p));
                if let Some(conv) = self.conversations.get_mut(&e.conversation_id) {
                    conv.modes = ConversationModes::Dm {
                        own_setting: None,
                        selected_project_default: default,
                    };
                }
            }
            E::ConversationDeleting(e) => {
                if let Some(conv) = self.conversations.get_mut(&e.conversation_id) {
                    conv.deleting = true;
                }
            }
            E::ConversationDeleted(e) => effects.extend(self.drop_conversation(e.conversation_id)),
            E::MessageAppended(e) => {
                if let Some(conv) = self.conversations.get_mut(&e.message.conversation_id) {
                    conv.review_messages.observe(event);
                    if e.message.author == Author::User {
                        conv.denial.apply(&DenialEvent::UserMessage);
                    }
                }
            }
            E::MessageCompleted(e) => {
                if let Some(conv) = self.conversations.get_mut(&e.conversation_id) {
                    conv.review_messages.observe(event);
                }
            }
            E::TurnQueued(e) => {
                let t = &e.turn;
                self.turns.insert(
                    t.id,
                    TurnOwner {
                        conversation: t.conversation_id,
                        assistant: t.assistant_id,
                    },
                );
                if let Some(conv) = self.conversations.get_mut(&t.conversation_id) {
                    conv.turns.insert(t.id);
                }
            }
            E::TurnStateChanged(e) => self.turn_state_changed(seq, e.turn_id, e.to),
            E::TurnFinished(e) => {
                self.waiting_turns.remove(&e.turn_id);
                self.interrupted_turns.remove(&e.turn_id);
                self.last_interrupt.remove(&e.turn_id);
                self.drop_turn_grants(e.turn_id);
                effects.push(Effect::TurnClosed { turn: e.turn_id });
            }
            E::ApprovalDelivered(e) => {
                // 被拒绝的审批交付时也有这条事件：不在等交付里就什么都不做
                if let Some(conv) = self.turn_conversation_mut(e.turn_id) {
                    conv.awaiting_delivery.remove(&e.approval_id);
                }
            }
            E::ApprovalRequested(e) => {
                let a = &e.approval;
                let detached = !self.waiting_turns.contains(&a.turn_id);
                let conv = self
                    .conversations
                    .get_mut(&a.conversation_id)
                    .ok_or_else(|| {
                        format!("审批 {} 的对话 {} 不在折叠里", a.id, a.conversation_id)
                    })?;
                conv.pending.insert(
                    a.id,
                    PendingApproval {
                        approval: a.clone(),
                        detached,
                    },
                );
                self.approval_index.insert(a.id, a.conversation_id);
            }
            E::ApprovalReviewUpdated(e) => {
                let p = self.pending_mut(e.approval_id)?;
                p.approval.review = e.review.clone();
                if !matches!(e.review, ReviewState::InProgress { .. }) {
                    effects.push(Effect::StopReview {
                        approval: e.approval_id,
                    });
                }
            }
            E::ApprovalDecided(e) => {
                if !matches!(
                    e.state,
                    ApprovalState::Approved { .. } | ApprovalState::Denied
                ) {
                    return Err(format!("审批 {} 的决定不是批准或拒绝", e.approval_id));
                }
                let conversation = self.pending_conversation(e.approval_id)?;
                let conv = self
                    .conversations
                    .get_mut(&conversation)
                    .expect("索引里的对话在折叠里");
                let p = conv
                    .pending
                    .remove(&e.approval_id)
                    .expect("索引里的审批在待审批里");
                self.approval_index.remove(&e.approval_id);
                let fingerprints = p.approval.fingerprints();
                match e.state {
                    ApprovalState::Denied => {
                        conv.denial.apply(&DenialEvent::UserDenied(fingerprints));
                        effects.push(Effect::Answer {
                            approval: e.approval_id,
                            decision: deny(MSG_USER_DENIED),
                        });
                    }
                    ApprovalState::Approved { decider } => {
                        if decider == Decider::User {
                            conv.denial.apply(&DenialEvent::UserAllowed(fingerprints));
                        }
                        if let Some(record) = p.approval.call.as_deref().cloned() {
                            let grant = GrantOnce {
                                approval_id: e.approval_id,
                                turn: p.approval.turn_id,
                                record,
                            };
                            if p.detached {
                                conv.grants.insert(e.approval_id, grant);
                            } else {
                                conv.awaiting_delivery.insert(e.approval_id, grant);
                            }
                        }
                        // detached：正常情况下已经没人在等；万一还有，也只留一次性放行这一条授权
                        effects.push(Effect::Answer {
                            approval: e.approval_id,
                            decision: if p.detached {
                                deny(MSG_TURN_CANCELLED)
                            } else {
                                PermissionDecision::Allow
                            },
                        });
                    }
                    ApprovalState::Pending | ApprovalState::Voided { .. } => {
                        unreachable!("上面已经拒掉")
                    }
                }
            }
            E::ApprovalVoided(e) => {
                let conversation = self.pending_conversation(e.approval_id)?;
                self.conversations
                    .get_mut(&conversation)
                    .expect("索引里的对话在折叠里")
                    .pending
                    .remove(&e.approval_id);
                self.approval_index.remove(&e.approval_id);
                effects.push(Effect::Answer {
                    approval: e.approval_id,
                    decision: deny(MSG_TURN_CANCELLED),
                });
                effects.push(Effect::StopReview {
                    approval: e.approval_id,
                });
            }
            E::ApprovalEvaluated(e) => {
                if let Some(id) = e.grant_approval_id {
                    let interrupted_after_eval = self
                        .last_interrupt
                        .get(&e.turn_id)
                        .is_some_and(|b| *b > e.live_rules_seq);
                    if let Some(conv) = self.turn_conversation_mut(e.turn_id)
                        && let Some(grant) = conv.grants.get(&id).cloned()
                    {
                        if e.verdict != Verdict::Allow {
                            // 命中但资格不在了：作废
                            conv.grants.remove(&id);
                        } else if !interrupted_after_eval {
                            conv.grants.remove(&id);
                            conv.awaiting_delivery.insert(id, grant);
                        }
                        // 求值之后、审计落盘之前回合中断过：过期的消费，放行留在 grants（方案审查 1/F-1）
                    }
                }
            }
            E::UserActionProposed(e) => {
                let c = &e.card;
                let conv = self
                    .conversations
                    .get_mut(&c.conversation_id)
                    .ok_or_else(|| {
                        format!("卡片 {} 的对话 {} 不在折叠里", c.id, c.conversation_id)
                    })?;
                conv.cards.insert(c.id, c.clone());
                self.card_index.insert(c.id, c.conversation_id);
            }
            E::UserActionStateChanged(e) => {
                let conversation = *self
                    .card_index
                    .get(&e.card_id)
                    .ok_or_else(|| format!("卡片 {} 不在没到终态的卡片里", e.card_id))?;
                let conv = self
                    .conversations
                    .get_mut(&conversation)
                    .expect("索引里的对话在折叠里");
                let card = conv
                    .cards
                    .get_mut(&e.card_id)
                    .expect("索引里的卡片在对话里");
                // `Unknown` 先有了话、之后的 `Ended` 不再重复带话
                let newly_settled = card_settled_message(&card.state).is_none();
                card.state = e.state.clone();
                if e.output_artifact.is_some() {
                    card.output_artifact = e.output_artifact;
                }
                if newly_settled && let Some(message) = card_settled_message(&e.state) {
                    effects.push(Effect::CardSettled {
                        card: e.card_id,
                        message,
                    });
                }
                if e.state.is_terminal() {
                    conv.cards.remove(&e.card_id);
                    self.card_index.remove(&e.card_id);
                }
            }
            E::RuleRemembered(e) => {
                let mut rules: Vec<RuleEntry> = self
                    .live_rules
                    .iter()
                    .filter(|r| r.id != e.rule.id)
                    .cloned()
                    .collect();
                rules.push(e.rule.clone());
                self.live_rules = Arc::new(rules);
            }
            E::RuleRevoked(e) => {
                self.live_rules = Arc::new(
                    self.live_rules
                        .iter()
                        .filter(|r| r.id != e.rule_id)
                        .cloned()
                        .collect(),
                );
            }
            E::RulesDeleted(e) => {
                self.live_rules = Arc::new(
                    self.live_rules
                        .iter()
                        .filter(|r| !e.rule_ids.contains(&r.id))
                        .cloned()
                        .collect(),
                );
            }
            _ => {}
        }
        Ok(effects)
    }

    /// 项目这一刻的默认档位；没登记过的项目按手动。
    pub fn project_default(&self, project: ProjectId) -> ProjectDefaultMode {
        self.project_defaults
            .get(&project)
            .copied()
            .unwrap_or_default()
    }

    fn turn_conversation_mut(&mut self, turn: TurnId) -> Option<&mut ConversationFold> {
        let owner = self.turns.get(&turn)?;
        self.conversations.get_mut(&owner.conversation)
    }

    fn pending_conversation(&self, id: ApprovalId) -> Result<ConversationId, String> {
        self.approval_index
            .get(&id)
            .copied()
            .ok_or_else(|| format!("审批 {id} 不在待审批里"))
    }

    fn pending_mut(&mut self, id: ApprovalId) -> Result<&mut PendingApproval, String> {
        let conversation = self.pending_conversation(id)?;
        Ok(self
            .conversations
            .get_mut(&conversation)
            .and_then(|c| c.pending.get_mut(&id))
            .expect("索引里的审批在待审批里"))
    }

    fn turn_state_changed(&mut self, seq: Seq, turn: TurnId, to: TurnState) {
        if to == TurnState::WaitingApproval {
            self.waiting_turns.insert(turn);
        } else {
            self.waiting_turns.remove(&turn);
        }
        let interrupted = matches!(to, TurnState::Interrupted { .. });
        if interrupted {
            self.interrupted_turns.insert(turn);
            self.last_interrupt.insert(turn, seq);
        } else {
            self.interrupted_turns.remove(&turn);
        }
        let Some(conv) = self.turn_conversation_mut(turn) else {
            return;
        };
        if to != TurnState::WaitingApproval {
            for p in conv.pending.values_mut() {
                if p.approval.turn_id == turn {
                    p.detached = true;
                }
            }
        }
        if interrupted {
            // R1 第二项：批准了、还没交付的授权，回合中断时变成一次性放行
            let moved: Vec<ApprovalId> = conv
                .awaiting_delivery
                .iter()
                .filter(|(_, g)| g.turn == turn)
                .map(|(id, _)| *id)
                .collect();
            for id in moved {
                let grant = conv.awaiting_delivery.remove(&id).expect("刚列出的条目");
                conv.grants.insert(id, grant);
            }
        }
        if to.is_terminal() {
            self.drop_turn_grants(turn);
        }
    }

    /// R2：回合到终态，删它的一次性放行与等交付的授权。
    fn drop_turn_grants(&mut self, turn: TurnId) {
        if let Some(conv) = self.turn_conversation_mut(turn) {
            conv.grants.retain(|_, g| g.turn != turn);
            conv.awaiting_delivery.retain(|_, g| g.turn != turn);
        }
    }

    /// 对话整条删掉（对话删除，或助手删除连带它的私聊）：连同索引与它名下回合的归属一起删。
    fn drop_conversation(&mut self, id: ConversationId) -> Vec<Effect> {
        let Some(conv) = self.conversations.remove(&id) else {
            return Vec::new();
        };
        let mut effects = Vec::new();
        for approval in conv.pending.keys() {
            self.approval_index.remove(approval);
            effects.push(Effect::Answer {
                approval: *approval,
                decision: deny(MSG_TURN_CANCELLED),
            });
        }
        for card in conv.cards.keys() {
            self.card_index.remove(card);
            effects.push(Effect::CardSettled {
                card: *card,
                message: MSG_TURN_CANCELLED.into(),
            });
        }
        for turn in &conv.turns {
            self.turns.remove(turn);
            self.waiting_turns.remove(turn);
            self.interrupted_turns.remove(turn);
            self.last_interrupt.remove(turn);
        }
        effects.push(Effect::ConversationGone { conversation: id });
        effects
    }

    // ---------- 读 ----------

    pub fn cursor(&self) -> Seq {
        self.cursor
    }

    /// 活规则（`Arc` 整体替换，求值拿到的那份不会中途变）。
    pub fn live_rules(&self) -> Arc<Vec<RuleEntry>> {
        self.live_rules.clone()
    }

    /// 最后一条会让会话快照过期的事件；T10 拿它和生成快照之前读到的 head 比。
    pub fn snapshot_inputs_seq(&self) -> Seq {
        self.snapshot_inputs_seq
    }

    pub fn conversation(&self, id: ConversationId) -> Option<&ConversationFold> {
        self.conversations.get(&id)
    }

    pub fn conversations(&self) -> impl Iterator<Item = (&ConversationId, &ConversationFold)> {
        self.conversations.iter()
    }

    /// 待审批所在的对话。
    pub fn approval_conversation(&self, id: ApprovalId) -> Option<ConversationId> {
        self.approval_index.get(&id).copied()
    }

    /// 没到终态的卡片所在的对话。
    pub fn card_conversation(&self, id: UserActionId) -> Option<ConversationId> {
        self.card_index.get(&id).copied()
    }

    pub fn pending_approval(&self, id: ApprovalId) -> Option<&PendingApproval> {
        let conversation = self.approval_index.get(&id)?;
        self.conversations.get(conversation)?.pending.get(&id)
    }

    pub fn turn_owner(&self, turn: TurnId) -> Option<TurnOwner> {
        self.turns.get(&turn).copied()
    }

    pub fn is_waiting(&self, turn: TurnId) -> bool {
        self.waiting_turns.contains(&turn)
    }

    pub fn is_interrupted(&self, turn: TurnId) -> bool {
        self.interrupted_turns.contains(&turn)
    }

    pub fn has_assistant(&self, id: AssistantId) -> bool {
        self.assistants.contains(&id)
    }

    /// 回合表的条数（基准报内存占用用）。
    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    /// 一次性放行出自哪条审批（任何对话）。
    pub fn grant(&self, id: ApprovalId) -> Option<&GrantOnce> {
        self.conversations.values().find_map(|c| c.grants.get(&id))
    }

    /// 等交付的授权（任何对话）。
    pub fn awaiting_delivery(&self, id: ApprovalId) -> Option<&GrantOnce> {
        self.conversations
            .values()
            .find_map(|c| c.awaiting_delivery.get(&id))
    }

    /// R1 的「要求续接」：有一次性放行、回合在 `Interrupted` 的回合。
    pub fn turns_to_resume(&self) -> Vec<TurnId> {
        let mut out: BTreeSet<TurnId> = BTreeSet::new();
        for conv in self.conversations.values() {
            for g in conv.grants.values() {
                if self.interrupted_turns.contains(&g.turn) {
                    out.insert(g.turn);
                }
            }
        }
        out.into_iter().collect()
    }

    /// 折叠这一侧的归属事实（6.10.7）：对同一段日志前缀和存储那一侧给出同样的值。
    pub fn owner_facts(&self, turn: TurnId) -> OwnerFacts {
        let Some(owner) = self.turns.get(&turn) else {
            return OwnerFacts {
                turn: Presence::Gone,
                conversation: ConversationPresence::Gone,
                assistant: Presence::Gone,
            };
        };
        OwnerFacts {
            turn: Presence::Present,
            conversation: match self.conversations.get(&owner.conversation) {
                None => ConversationPresence::Gone,
                Some(c) if c.deleting => ConversationPresence::Deleting,
                Some(_) => ConversationPresence::Present,
            },
            assistant: if self.assistants.contains(&owner.assistant) {
                Presence::Present
            } else {
                Presence::Gone
            },
        }
    }

    /// 孤儿：归属不在了（`owner_alive` 不通过）的待审批与 Pending 卡片，按对话分组。纯函数。
    pub fn orphans(&self) -> BTreeMap<ConversationId, Vec<VoidTarget>> {
        let mut out: BTreeMap<ConversationId, Vec<VoidTarget>> = BTreeMap::new();
        for (cid, conv) in &self.conversations {
            let mut targets = Vec::new();
            for (id, p) in &conv.pending {
                if let Err(gone) = owner_alive(&self.owner_facts(p.approval.turn_id)) {
                    targets.push(VoidTarget::Approval {
                        id: *id,
                        reason: void_reason_for(gone),
                    });
                }
            }
            for (id, card) in &conv.cards {
                if card.state == UserActionState::Pending
                    && let Err(gone) = owner_alive(&self.owner_facts(card.turn_id))
                {
                    targets.push(VoidTarget::Card {
                        id: *id,
                        reason: void_reason_for(gone),
                    });
                }
            }
            if !targets.is_empty() {
                out.insert(*cid, targets);
            }
        }
        out
    }
}

impl ConversationFold {
    /// 某个助手在这个对话里的有效档位。
    pub fn effective_mode(&self, assistant: AssistantId) -> ApprovalMode {
        self.modes.effective(assistant)
    }
}
