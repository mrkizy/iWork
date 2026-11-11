//! 参照判定方（T06 方案 15.3）：只吃已提交的事件、按 R1–R3 推进。审批的请求与用户的决定都写进同一个
//! 存储，授权由「把日志从头折一遍」得出，不看回答通道还在不在。屏障测试先用它开发，合入前换成真实引擎
//! 重跑同一组断言（`judges.rs`）。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use iwork_core::turn::{Decided, PermissionDecider, VoidPlanError};
use iwork_domain::approval::{ApprovalState, Decider, VoidReason};
use iwork_domain::common::Actor;
use iwork_domain::event::{ApprovalDecided, ApprovalVoided, DomainEvent};
use iwork_domain::id::{ApprovalId, CommandId, ConversationId, RawUlid, TurnId};
use iwork_domain::turn::TurnState;
use iwork_ports::agent::{PermissionDecision, PermissionRequest};
use iwork_ports::store::{CommitRequest, StoragePort};
use tokio::sync::oneshot;

use super::engine_fakes::MemStore;
use super::fold_fx::{approval, e_requested};

/// 参照判定方拒绝时给的话。
pub const REFERENCE_DENY: &str = "参照判定方：用户拒绝";

pub struct OrderedDecider {
    store: MemStore,
    conversation: ConversationId,
    next: AtomicU64,
    waiting: Mutex<BTreeMap<ApprovalId, oneshot::Sender<PermissionDecision>>>,
}

/// 日志折出来的审批事实。
#[derive(Default)]
struct Folded {
    turn_of: BTreeMap<ApprovalId, TurnId>,
    pending: BTreeSet<ApprovalId>,
    /// 已批准、还没有 `ApprovalDelivered`
    awaiting_delivery: BTreeSet<ApprovalId>,
    grants: BTreeSet<ApprovalId>,
    interrupted: BTreeSet<TurnId>,
}

impl Folded {
    fn of(events: &[DomainEvent]) -> Self {
        let mut f = Self::default();
        for e in events {
            match e {
                DomainEvent::ApprovalRequested(r) => {
                    f.turn_of.insert(r.approval.id, r.approval.turn_id);
                    f.pending.insert(r.approval.id);
                }
                DomainEvent::ApprovalDecided(d) => {
                    f.pending.remove(&d.approval_id);
                    if matches!(d.state, ApprovalState::Approved { .. }) {
                        // R1 第一项之后批准：回合已中断就直接是一次性授权
                        if f.interrupted.contains(&f.turn_of[&d.approval_id]) {
                            f.grants.insert(d.approval_id);
                        } else {
                            f.awaiting_delivery.insert(d.approval_id);
                        }
                    }
                }
                DomainEvent::ApprovalVoided(v) => {
                    f.pending.remove(&v.approval_id);
                }
                // R1 第三项：交付过就用掉了
                DomainEvent::ApprovalDelivered(d) => {
                    f.awaiting_delivery.remove(&d.approval_id);
                }
                DomainEvent::TurnStateChanged(c) => match c.to {
                    // R1 第二项：没交付的批准留作一次性授权
                    TurnState::Interrupted { .. } => {
                        f.interrupted.insert(c.turn_id);
                        let moved: Vec<_> = f
                            .awaiting_delivery
                            .iter()
                            .filter(|a| f.turn_of[*a] == c.turn_id)
                            .copied()
                            .collect();
                        for a in moved {
                            f.awaiting_delivery.remove(&a);
                            f.grants.insert(a);
                        }
                    }
                    TurnState::Running => {
                        f.interrupted.remove(&c.turn_id);
                    }
                    // R2：终态不留授权
                    to if to.is_terminal() => {
                        f.interrupted.remove(&c.turn_id);
                        let turn_of = &f.turn_of;
                        f.grants.retain(|a| turn_of[a] != c.turn_id);
                        f.awaiting_delivery.retain(|a| turn_of[a] != c.turn_id);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        f
    }
}

impl OrderedDecider {
    pub fn new(store: MemStore, conversation: ConversationId) -> Self {
        Self {
            store,
            conversation,
            next: AtomicU64::new(0),
            waiting: Mutex::new(BTreeMap::new()),
        }
    }

    async fn commit(&self, events: Vec<DomainEvent>) {
        self.store
            .commit(CommitRequest {
                command_id: CommandId::from_raw(RawUlid::from_u128(0)),
                actor: Actor("user".into()),
                events,
                outbox: Vec::new(),
            })
            .await
            .expect("参照判定方的提交应当被接受");
    }

    fn folded(&self) -> Folded {
        Folded::of(&self.store.all())
    }

    /// 用户决定：先提交 `ApprovalDecided`，回合没中断就把答复交给还在等的请求。
    pub async fn decide_user(&self, id: ApprovalId, allow: bool) {
        let state = if allow {
            ApprovalState::Approved {
                decider: Decider::User,
            }
        } else {
            ApprovalState::Denied
        };
        self.commit(vec![
            ApprovalDecided {
                approval_id: id,
                state,
                remember: None,
            }
            .into(),
        ])
        .await;
        let f = self.folded();
        if f.interrupted.contains(&f.turn_of[&id]) {
            return;
        }
        if let Some(tx) = self.waiting.lock().unwrap().remove(&id) {
            let _ = tx.send(if allow {
                PermissionDecision::Allow
            } else {
                PermissionDecision::Deny {
                    message: REFERENCE_DENY.into(),
                }
            });
        }
    }

    pub fn has_grant(&self, id: ApprovalId) -> bool {
        self.folded().grants.contains(&id)
    }
}

impl PermissionDecider for OrderedDecider {
    type VoidHold = ();

    async fn decide(&self, turn_id: TurnId, req: &PermissionRequest) -> Decided {
        let n = u128::from(self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let mut a = approval(n, 0, 0, "x");
        a.turn_id = turn_id;
        a.conversation_id = self.conversation;
        a.action.raw_summary = req.raw_input.0.clone();
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().unwrap().insert(a.id, tx);
        let id = a.id;
        self.commit(vec![e_requested(a)]).await;
        let decision = rx.await.unwrap_or(PermissionDecision::Deny {
            message: REFERENCE_DENY.into(),
        });
        Decided {
            decision,
            approval: Some(id),
        }
    }

    async fn void_pending(
        &self,
        turn_id: TurnId,
        reason: VoidReason,
    ) -> Result<(Vec<DomainEvent>, ()), VoidPlanError> {
        let f = self.folded();
        let events = f
            .pending
            .iter()
            .filter(|a| f.turn_of[*a] == turn_id)
            .map(|a| {
                ApprovalVoided {
                    approval_id: *a,
                    reason,
                }
                .into()
            })
            .collect();
        Ok((events, ()))
    }
}
