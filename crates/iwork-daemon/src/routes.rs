//! outbox 的真实去向（T10a 方案 2.9、2.10；义务行 N2、X6）。去向登记表在 `daemon.rs` 的 `outbox_routes()`，这里每个
//! 去向一个投递实现，外加生成与认回去向字符串的函数（同一处写、同一处读）。
//!
//! - `notify:<待处理项的键>`（N2）：发一条系统通知。结果不确定标失败、不重投（通知丢了不影响业务，重复了打扰用户）；
//!   单次 5 秒、只投 1 次。投之前查这一项还在不在待处理：已处理、已作废的不发。拿不到通知权限只记一行日志，算投完。

use std::sync::Arc;
use std::time::Duration;

use iwork_core::notify::notification;
use iwork_ports::notify::{Notifier, PendingItemRef};
use iwork_ports::read_model::{PendingItem, ReadModelPort};
use iwork_ports::store::{OutboxItem, OutboxTarget};

use crate::tasks::outbox::{
    DispatchError, DispatchFuture, OnUncertain, OutboxDispatch, OutboxRoute,
};

/// 系统通知去向的前缀。
pub const NOTIFY_PREFIX: &str = "notify:";

/// 给待处理项 `item` 发系统通知的 outbox 去向（`notify` 任务写条目时用）。
pub fn notify_target(item: PendingItemRef) -> OutboxTarget {
    OutboxTarget(format!("{NOTIFY_PREFIX}{}", item.key()))
}

/// [`notify_target`] 的逆。
fn notify_item(target: &OutboxTarget) -> Result<PendingItemRef, String> {
    let key = target
        .0
        .strip_prefix(NOTIFY_PREFIX)
        .ok_or_else(|| format!("不是系统通知的去向：{:?}", target.0))?;
    PendingItemRef::parse_key(key)
}

/// 去向登记表里系统通知那一行。
pub fn notify_route<R, N>(read: Arc<R>, notifier: Arc<N>) -> OutboxRoute
where
    R: ReadModelPort + Send + Sync + 'static,
    N: Notifier + 'static,
{
    let mut route = OutboxRoute::new(
        NOTIFY_PREFIX,
        OnUncertain::Fail,
        Box::new(NotifyDispatch { read, notifier }),
    );
    route.timeout = Duration::from_secs(5);
    route.max_attempts = 1;
    route
}

/// 系统通知的投递。
pub struct NotifyDispatch<R, N> {
    read: Arc<R>,
    notifier: Arc<N>,
}

impl<R, N> OutboxDispatch for NotifyDispatch<R, N>
where
    R: ReadModelPort + Send + Sync + 'static,
    N: Notifier + 'static,
{
    fn dispatch<'a>(&'a self, item: &'a OutboxItem) -> DispatchFuture<'a> {
        Box::pin(async move {
            let wanted = notify_item(&item.target).map_err(|message| DispatchError {
                retryable: false,
                message,
            })?;
            let storage = |e: iwork_ports::store::StorageError| DispatchError {
                retryable: e.retryable(),
                message: format!("读待处理项失败：{e}"),
            };
            let pending = self.read.pending_items().await.map_err(storage)?;
            let Some(found) = pending.into_iter().find(|p| p.item_ref() == wanted) else {
                tracing::debug!(item = %wanted.key(), "这一项已不在待处理，不发通知");
                return Ok(());
            };
            let project_name = match &found {
                PendingItem::AgentsMdDraft(d) => self
                    .read
                    .projects()
                    .await
                    .map_err(storage)?
                    .into_iter()
                    .find(|p| p.project.id == d.project_id)
                    .map(|p| p.project.name),
                _ => None,
            };
            if let Err(e) = self
                .notifier
                .notify(notification(&found, project_name.as_deref()))
                .await
            {
                // 拿不到权限（没有 Developer ID 签名时的常态）：只记日志，算投完，不重投
                tracing::info!(item = %wanted.key(), error = %e, "系统通知没发出去");
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use iwork_domain::approval::{ApprovalState, Decider};
    use iwork_domain::event::{
        ApprovalDecided, ApprovalRequested, AssistantCreated, ConversationCreated, ProjectAdded,
        TurnQueued,
    };
    use iwork_ports::notify::{NotifyError, SystemNotification};
    use iwork_ports::store::{DeliveryState, OutboxId, StoragePort};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeNotifier {
        sent: Mutex<Vec<SystemNotification>>,
        refuse: bool,
    }

    impl Notifier for FakeNotifier {
        async fn notify(&self, n: SystemNotification) -> Result<(), NotifyError> {
            if self.refuse {
                return Err(NotifyError::Unavailable {
                    message: "没有通知权限".into(),
                });
            }
            self.sent.lock().unwrap().push(n);
            Ok(())
        }
    }

    fn item(target: OutboxTarget) -> OutboxItem {
        OutboxItem {
            id: OutboxId(1),
            target,
            payload: Vec::new(),
            state: DeliveryState::Dispatched,
            attempts: 1,
        }
    }

    /// 布景：两个待审批，对话 10 里回合 20 的审批 30、回合 21 的审批 31。
    fn world(env: &Env) -> Store {
        let store = env.open();
        let mut t = turn(20, 10);
        t.state = iwork_domain::turn::TurnState::Queued;
        block(store.commit(cmd(
            1,
            vec![
                AssistantCreated {
                    assistant: assistant(1, false),
                }
                .into(),
                ProjectAdded {
                    project: project(1),
                }
                .into(),
                ConversationCreated {
                    conversation: project_conversation(10, 1),
                }
                .into(),
                TurnQueued { turn: t }.into(),
                TurnQueued {
                    turn: turn(21, 10),
                }
                .into(),
                ApprovalRequested {
                    approval: approval(30, 20, 10),
                }
                .into(),
                ApprovalRequested {
                    approval: approval(31, 21, 10),
                }
                .into(),
            ],
        )))
        .unwrap();
        store
    }

    /// 还在待处理的发一条（文字来自待处理项）；处理掉之后同一条去向什么都不发，照样算投完。
    #[test]
    fn notify_dispatch_sends_only_while_pending() {
        let env = Env::new();
        let store = world(&env);
        let notifier = Arc::new(FakeNotifier::default());
        let route = notify_route(Arc::new(read_model(&store)), notifier.clone());
        assert_eq!(
            (route.prefix, route.on_uncertain, route.max_attempts),
            (NOTIFY_PREFIX, OnUncertain::Fail, 1)
        );
        assert_eq!(route.timeout, Duration::from_secs(5));
        let target = notify_target(PendingItemRef::Approval(apid(30)));
        block(route.dispatch.dispatch(&item(target.clone()))).unwrap();
        let sent = notifier.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].title, "审批");
        assert!(sent[0].body.starts_with("审批："), "{:?}", sent[0]);
        assert_eq!(
            sent[0].open.map(|o| (o.item, o.conversation_id)),
            Some((PendingItemRef::Approval(apid(30)), Some(cid(10))))
        );

        block(store.commit(cmd(
            2,
            vec![
                ApprovalDecided {
                    conversation_id: Some(cid(10)),
                    approval_id: apid(30),
                    state: ApprovalState::Approved {
                        decider: Decider::User,
                    },
                    remember: None,
                }
                .into(),
            ],
        )))
        .unwrap();
        block(route.dispatch.dispatch(&item(target))).unwrap();
        assert_eq!(
            notifier.sent.lock().unwrap().len(),
            1,
            "已决定的不发，也不拿还在待处理的另一项顶上"
        );
        // 对照：另一项还在待处理，给它的照常发、发的就是它
        block(
            route
                .dispatch
                .dispatch(&item(notify_target(PendingItemRef::Approval(apid(31))))),
        )
        .unwrap();
        let sent = notifier.sent.lock().unwrap().clone();
        assert_eq!(
            sent.last().and_then(|n| n.open).map(|o| o.item),
            Some(PendingItemRef::Approval(apid(31)))
        );
    }

    /// 拿不到权限只记日志，算投完；去向写坏了是不可重试的错误，点名原文。
    #[test]
    fn notify_dispatch_refused_and_bad_target() {
        let env = Env::new();
        let store = world(&env);
        let notifier = Arc::new(FakeNotifier {
            refuse: true,
            ..Default::default()
        });
        let route = notify_route(Arc::new(read_model(&store)), notifier.clone());
        let target = notify_target(PendingItemRef::Approval(apid(30)));
        assert_eq!(block(route.dispatch.dispatch(&item(target))), Ok(()));
        let bad = block(
            route
                .dispatch
                .dispatch(&item(OutboxTarget("notify:approval:x".into()))),
        )
        .unwrap_err();
        assert!(
            !bad.retryable && bad.message.contains("approval:x"),
            "{bad:?}"
        );
    }

    #[test]
    fn notify_target_roundtrips() {
        let r = PendingItemRef::Resume(tid(20));
        assert_eq!(notify_item(&notify_target(r)), Ok(r));
        assert!(notify_item(&OutboxTarget("dev-fail:x".into())).is_err());
    }
}
