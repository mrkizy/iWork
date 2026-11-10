//! 等待登记与会话绑定（T07 方案 6.10.5）。两样都不在折叠里、不由事件推进：
//! 等待登记是进程里的通道，会话绑定记每条审批用哪份 `SessionKeys`。
//!
//! 登记的所有权在发请求的 future 手里：守卫析构时把自己那一项拿掉。提交失败、等回执时被取消、
//! 等用户时被编排丢弃，都是守卫析构、登记消失，没有别的清理动作。
use super::fold::{Effect, Folded};
use super::*;

/// 挂着的 hook。
#[derive(Default)]
pub(super) struct Waiting {
    asks: BTreeMap<ApprovalId, oneshot::Sender<PermissionDecision>>,
    reviews: BTreeMap<ApprovalId, oneshot::Sender<()>>,
    cards: BTreeMap<UserActionId, oneshot::Sender<String>>,
}

/// 一条审批绑定发起它的那份会话键（同一回合续接后会换快照，各用各的）。
pub(super) struct Binding {
    pub turn: TurnId,
    pub conversation: ConversationId,
    pub keys: Arc<SessionKeys>,
}

/// 引擎里由 std 锁保护的全部内容。锁只在读游标、应用一批、取视图时持有，从不跨 `await`。
#[derive(Default)]
pub(super) struct Inner {
    pub folded: Folded,
    pub waiting: Waiting,
    /// 不参与「内存等于折叠」：查绑定总是从折叠里的待审批 id 出发
    pub bindings: BTreeMap<ApprovalId, Binding>,
}

impl Inner {
    /// 执行折叠给出的效果：往等待方的通道里发，非阻塞；找不到等待方就算了。
    pub fn run(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Answer { approval, decision } => {
                    self.bindings.remove(&approval);
                    if let Some(tx) = self.waiting.asks.remove(&approval) {
                        let _ = tx.send(decision);
                    }
                }
                Effect::StopReview { approval } => {
                    if let Some(tx) = self.waiting.reviews.remove(&approval) {
                        let _ = tx.send(());
                    }
                }
                Effect::CardSettled { card, message } => {
                    if let Some(tx) = self.waiting.cards.remove(&card) {
                        let _ = tx.send(message);
                    }
                }
                Effect::TurnClosed { turn } => self.bindings.retain(|_, b| b.turn != turn),
                Effect::ConversationGone { conversation } => {
                    self.bindings.retain(|_, b| b.conversation != conversation);
                }
            }
        }
    }
}

/// 一条询问的等待登记；析构即撤。
pub(super) struct AskGuard<'a> {
    inner: &'a Mutex<Inner>,
    approval: ApprovalId,
}

impl Drop for AskGuard<'_> {
    fn drop(&mut self) {
        let mut inner = lock_std(self.inner);
        inner.waiting.asks.remove(&self.approval);
        inner.waiting.reviews.remove(&self.approval);
    }
}

/// 一张卡片的等待登记；析构即撤。
pub(super) struct CardGuard<'a> {
    inner: &'a Mutex<Inner>,
    card: UserActionId,
}

impl Drop for CardGuard<'_> {
    fn drop(&mut self) {
        lock_std(self.inner).waiting.cards.remove(&self.card);
    }
}

/// 一条询问登记下来的东西：守卫、答复的接收端、审查的停止信号。
pub(super) struct AskRegistration<'a> {
    pub guard: AskGuard<'a>,
    pub reply: oneshot::Receiver<PermissionDecision>,
    pub stopped: oneshot::Receiver<()>,
}

/// 提交之前登记等待方与会话绑定（先登记再提交：`ApprovalRequested` 落盘之前不可能有它的终态事件）。
pub(super) fn register_ask<'a>(
    inner: &'a Mutex<Inner>,
    approval: ApprovalId,
    turn: TurnId,
    keys: &Arc<SessionKeys>,
) -> AskRegistration<'a> {
    let (tx, reply) = oneshot::channel();
    let (stop, stopped) = oneshot::channel();
    let mut guard_inner = lock_std(inner);
    guard_inner.waiting.asks.insert(approval, tx);
    guard_inner.waiting.reviews.insert(approval, stop);
    guard_inner.bindings.insert(
        approval,
        Binding {
            turn,
            conversation: keys.conversation,
            keys: keys.clone(),
        },
    );
    drop(guard_inner);
    AskRegistration {
        guard: AskGuard { inner, approval },
        reply,
        stopped,
    }
}

pub(super) fn register_card(
    inner: &Mutex<Inner>,
    card: UserActionId,
) -> (CardGuard<'_>, oneshot::Receiver<String>) {
    let (tx, rx) = oneshot::channel();
    lock_std(inner).waiting.cards.insert(card, tx);
    (CardGuard { inner, card }, rx)
}
