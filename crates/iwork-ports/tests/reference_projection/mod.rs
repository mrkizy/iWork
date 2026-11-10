//! 审批与卡片的参照投影（T02 方案 2.6「状态前提」「归属存续」；T07 方案 6.10.7、6.10.13）。
//!
//! 只维护提交时校验要用的那几样：回合（→ 对话、助手）、对话（在 / 删除中）、私聊属于哪个助手、
//! 助手、审批（主状态、审查子状态、回合）、卡片（状态、回合）。判断全部调 `iwork-domain` 的同一组函数，
//! 和 `SqliteProjector` 不各写一份规则。别的 crate 的测试经 `#[path]` 引入同一份：存储的对拍测试
//! 拿它当参照，引擎测试的假存储拿它做提交时校验。
//!
//! 一条事件不通过时返回错误、什么都不改；一笔命令要整笔回滚由调用方在副本上应用（见 [`apply_all`]）。

#![allow(dead_code, reason = "各测试各用一部分")]

use std::collections::{BTreeMap, BTreeSet};

use iwork_domain::approval::{
    ApprovalState, ConversationPresence, OwnerFacts, Presence, ReviewState, check_committed,
    owner_alive, requires_owner,
};
use iwork_domain::conversation::{ConversationKind, ConversationState};
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{ApprovalId, AssistantId, ConversationId, TurnId, UserActionId};
use iwork_domain::user_action::{self, UserActionState};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReferenceProjection {
    /// 回合 → (对话, 助手)。`TurnFinished` 不删；随对话、助手私聊的级联删
    pub turns: BTreeMap<TurnId, (ConversationId, AssistantId)>,
    /// 对话 → 是否删除中
    pub conversations: BTreeMap<ConversationId, bool>,
    /// 私聊 → 它的助手
    pub dm_assistant: BTreeMap<ConversationId, AssistantId>,
    pub assistants: BTreeSet<AssistantId>,
    pub approvals: BTreeMap<ApprovalId, (ApprovalState, ReviewState, TurnId, ConversationId)>,
    pub cards: BTreeMap<UserActionId, (UserActionState, TurnId, ConversationId)>,
}

impl ReferenceProjection {
    /// 和存储一侧同一种读法给出的归属事实。
    pub fn owner_facts(&self, turn: TurnId) -> OwnerFacts {
        let Some((conversation, assistant)) = self.turns.get(&turn) else {
            return OwnerFacts {
                turn: Presence::Gone,
                conversation: ConversationPresence::Gone,
                assistant: Presence::Gone,
            };
        };
        OwnerFacts {
            turn: Presence::Present,
            conversation: match self.conversations.get(conversation) {
                None => ConversationPresence::Gone,
                Some(true) => ConversationPresence::Deleting,
                Some(false) => ConversationPresence::Present,
            },
            assistant: if self.assistants.contains(assistant) {
                Presence::Present
            } else {
                Presence::Gone
            },
        }
    }

    fn require_owner(&self, event: &DomainEvent, turn: TurnId) -> Result<(), String> {
        if requires_owner(event) {
            owner_alive(&self.owner_facts(turn))
                .map_err(|g| format!("{} 的归属不在了：{g}", event.kind()))?;
        }
        Ok(())
    }

    fn drop_conversation(&mut self, c: ConversationId) {
        self.conversations.remove(&c);
        self.dm_assistant.remove(&c);
        self.turns.retain(|_, (conv, _)| *conv != c);
        self.approvals.retain(|_, (_, _, _, conv)| *conv != c);
        self.cards.retain(|_, (_, _, conv)| *conv != c);
    }

    /// 应用一条事件；不通过返回错误、什么都不改。
    pub fn apply(&mut self, event: &DomainEvent) -> Result<(), String> {
        use DomainEvent as E;
        match event {
            E::AssistantCreated(e) => {
                self.assistants.insert(e.assistant.id);
            }
            E::AssistantDeleted(e) => {
                self.assistants.remove(&e.assistant_id);
                let dms: Vec<ConversationId> = self
                    .dm_assistant
                    .iter()
                    .filter(|(_, a)| **a == e.assistant_id)
                    .map(|(c, _)| *c)
                    .collect();
                for c in dms {
                    self.drop_conversation(c);
                }
            }
            E::ConversationCreated(e) => {
                let c = &e.conversation;
                self.conversations
                    .insert(c.id, c.state == ConversationState::Deleting);
                if let ConversationKind::AssistantDm { assistant_id, .. } = c.kind {
                    self.dm_assistant.insert(c.id, assistant_id);
                }
            }
            E::ConversationDeleting(e) => {
                let row = self
                    .conversations
                    .get_mut(&e.conversation_id)
                    .ok_or_else(|| format!("没有对话 {}", e.conversation_id))?;
                *row = true;
            }
            E::ConversationDeleted(e) => self.drop_conversation(e.conversation_id),
            E::TurnQueued(e) => {
                self.turns
                    .insert(e.turn.id, (e.turn.conversation_id, e.turn.assistant_id));
            }
            E::ApprovalRequested(e) => {
                let a = &e.approval;
                self.require_owner(event, a.turn_id)?;
                self.approvals.insert(
                    a.id,
                    (a.state, a.review.clone(), a.turn_id, a.conversation_id),
                );
            }
            E::ApprovalReviewUpdated(e) => {
                let (state, review, _, _) = self
                    .approvals
                    .get(&e.approval_id)
                    .ok_or_else(|| format!("没有审批 {}", e.approval_id))?;
                check_committed(*state, review, event).map_err(|e| e.message)?;
                self.approvals.get_mut(&e.approval_id).unwrap().1 = e.review.clone();
            }
            E::ApprovalDecided(e) => {
                let (state, review, turn, _) = self
                    .approvals
                    .get(&e.approval_id)
                    .ok_or_else(|| format!("没有审批 {}", e.approval_id))?;
                check_committed(*state, review, event).map_err(|e| e.message)?;
                self.require_owner(event, *turn)?;
                self.approvals.get_mut(&e.approval_id).unwrap().0 = e.state;
            }
            E::ApprovalVoided(e) => {
                let (state, review, _, _) = self
                    .approvals
                    .get(&e.approval_id)
                    .ok_or_else(|| format!("没有审批 {}", e.approval_id))?;
                check_committed(*state, review, event).map_err(|e| e.message)?;
                self.approvals.get_mut(&e.approval_id).unwrap().0 =
                    ApprovalState::Voided { reason: e.reason };
            }
            E::UserActionProposed(e) => {
                let c = &e.card;
                self.require_owner(event, c.turn_id)?;
                self.cards
                    .insert(c.id, (c.state.clone(), c.turn_id, c.conversation_id));
            }
            E::UserActionStateChanged(e) => {
                let (state, turn, _) = self
                    .cards
                    .get(&e.card_id)
                    .ok_or_else(|| format!("没有卡片 {}", e.card_id))?;
                user_action::check_committed(state, event).map_err(|e| e.message)?;
                self.require_owner(event, *turn)?;
                self.cards.get_mut(&e.card_id).unwrap().0 = e.state.clone();
            }
            _ => {}
        }
        Ok(())
    }
}

/// 一笔命令：在副本上依次应用，全部通过才替换（整笔回滚的语义，T02 2.4 第 5 步）。
pub fn apply_all(p: &mut ReferenceProjection, events: &[DomainEvent]) -> Result<(), String> {
    let mut next = p.clone();
    for e in events {
        next.apply(e)?;
    }
    *p = next;
    Ok(())
}
