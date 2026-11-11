//! 会牵动审批或卡片的外部命令经引擎提交（T07 方案 6.10.6 第 5、6、9 条，6.10.8）。
//!
//! 引擎拿令牌、追平、算附带事件（排在命令自己的事件前面）、一笔提交、被拒重算、放令牌。
//! **令牌不出引擎**：调用方拿不到句柄，不会拿着令牌去等别的东西。提交之后、放掉令牌之后再清孤儿。
use super::fold::{Folded, VoidTarget};
use super::sync::TokenGuard;
use super::*;

/// 这笔命令牵动的范围。加一种 = 加一个变体和它的计划分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 附带这个对话全部待审批与 Pending 卡片的作废
    ConversationDeleting(ConversationId),
    /// 附带这个助手名下（各个项目对话里、它的私聊里）全部待审批与 Pending 卡片的作废；
    /// 漏在令牌集合之外的由清孤儿收
    AssistantDeleted(AssistantId),
    /// 附带离开自动档要放弃的审查
    DmProjectSwitched {
        conversation: ConversationId,
        project: Option<ProjectId>,
    },
    /// 卡片从 Pending 出发的状态变化：没有附带，只在令牌上和作废计划排队
    Card(ConversationId),
    /// 没有编排在跑的回合补作废（6.10.8「谁来补」）
    VoidTurn {
        conversation: ConversationId,
        turn: TurnId,
        reason: VoidReason,
    },
}

/// `commit_with` 的结果：回执与引擎附带进去的事件。没有句柄。
#[derive(Debug, Clone, PartialEq)]
pub struct Committed {
    pub receipt: CommitReceipt,
    pub attached: Vec<DomainEvent>,
}

impl Scope {
    /// 要拿令牌的对话，按 id 从小到大（跨多个对话时依次拿，不会死锁）。
    fn conversations(&self, folded: &Folded) -> Vec<ConversationId> {
        match *self {
            Self::ConversationDeleting(c)
            | Self::DmProjectSwitched {
                conversation: c, ..
            }
            | Self::Card(c)
            | Self::VoidTurn {
                conversation: c, ..
            } => vec![c],
            Self::AssistantDeleted(a) => folded
                .conversations()
                .filter(|(_, conv)| {
                    conv.dm_assistant == Some(a) || !assistant_targets(folded, conv, a).is_empty()
                })
                .map(|(id, _)| *id)
                .collect(),
        }
    }

    /// 附带事件（纯函数，在追平后的折叠上算）。
    fn plan(
        &self,
        folded: &Folded,
        conversations: &[ConversationId],
    ) -> Result<Vec<DomainEvent>, ApprovalApplyError> {
        let mut events = Vec::new();
        for c in conversations {
            let Some(conv) = folded.conversation(*c) else {
                continue;
            };
            match *self {
                Self::ConversationDeleting(_) => {
                    let targets =
                        changes::targets_where(conv, VoidReason::ConversationDeleted, |_| true);
                    events.extend(changes::plan_voids(folded, conv, &targets));
                }
                Self::AssistantDeleted(a) => {
                    let targets = if conv.dm_assistant == Some(a) {
                        changes::targets_where(conv, VoidReason::AssistantDeleted, |_| true)
                    } else {
                        assistant_targets(folded, conv, a)
                    };
                    events.extend(changes::plan_voids(folded, conv, &targets));
                }
                Self::DmProjectSwitched { project, .. } => {
                    let modes = ConversationModes::Dm {
                        own_setting: None,
                        selected_project_default: project.map(|p| folded.project_default(p)),
                    };
                    events.extend(changes::plan_reviews_leaving_auto(
                        folded,
                        conv,
                        &modes,
                        &[],
                    )?);
                }
                Self::Card(_) => {}
                Self::VoidTurn { turn, reason, .. } => {
                    let targets = changes::targets_where(conv, reason, |t| t == turn);
                    events.extend(changes::plan_voids(folded, conv, &targets));
                }
            }
        }
        Ok(events)
    }
}

/// 项目对话里属于助手 `a` 的回合的待审批与 Pending 卡片。
fn assistant_targets(
    folded: &Folded,
    conv: &fold::ConversationFold,
    a: AssistantId,
) -> Vec<VoidTarget> {
    changes::targets_where(conv, VoidReason::AssistantDeleted, |t| {
        folded.turn_owner(t).is_some_and(|o| o.assistant == a)
    })
}

impl<C, G, S, A, T, R> ApprovalEngine<C, G, S, A, T, R>
where
    C: Clock + 'static,
    G: IdGenerator + 'static,
    S: StoragePort + 'static,
    A: AgentPort + 'static,
    T: Timer + 'static,
    R: PathResolver + 'static,
{
    /// 经引擎提交一笔会牵动审批或卡片的命令。被投影器拒了在里面重算，最多 3 次；
    /// 第 3 次还被拒返回 `Conflict`。附带事件为空、命令自己被拒（比如卡片已不在 Pending）也算被拒，
    /// 原样交回 `Storage(Projection)`。
    pub async fn commit_with(
        &self,
        scope: Scope,
        req: CommitRequest,
    ) -> Result<Committed, EngineError> {
        for _ in 0..REPLAN_ATTEMPTS {
            self.catch_up().await?;
            let conversations = scope.conversations(&lock_std(&self.inner).folded);
            let mut tokens: Vec<TokenGuard> = Vec::new();
            for c in &conversations {
                tokens.push(self.acquire(*c).await?);
            }
            self.catch_up().await?;
            let attached = {
                let inner = lock_std(&self.inner);
                scope.plan(&inner.folded, &conversations).map_err(|e| {
                    EngineError::Fold(fold::FoldError {
                        seq: inner.folded.cursor(),
                        kind: "commit_with",
                        why: e.to_string(),
                    })
                })?
            };
            let mut events = attached.clone();
            events.extend(req.events.iter().cloned());
            let mut refs: Vec<&mut TokenGuard> = tokens.iter_mut().collect();
            let result = self
                .commit_request(
                    &mut refs,
                    CommitRequest {
                        command_id: req.command_id,
                        actor: req.actor.clone(),
                        events,
                        outbox: req.outbox.clone(),
                    },
                )
                .await;
            match result {
                Ok(receipt) => {
                    self.catch_up_after_commit("commit_with").await;
                    // 顺序写死：提交 → 追平 → 放掉这次拿的全部令牌 → 再清孤儿（不会重入同一把令牌）
                    drop(refs);
                    drop(tokens);
                    if let Err(error) = self.sweep_orphans().await {
                        tracing::error!(%error, "commit_with 之后清孤儿失败，留给 sync 或重启");
                    }
                    return Ok(Committed { receipt, attached });
                }
                // 没有附带事件时，被拒的是命令自己（比如卡片已不在 Pending）：重算也一样，原样交回
                Err(rejected @ StorageError::Projection { .. }) if attached.is_empty() => {
                    return Err(rejected.into());
                }
                Err(StorageError::Projection { .. }) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(EngineError::Conflict)
    }
}
