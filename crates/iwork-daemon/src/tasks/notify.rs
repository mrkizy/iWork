//! `notify`（可重启；T10a 方案 2.10、义务行 N1）：待处理新增一项时定发不发系统通知，每一项只定一次。
//!
//! 被叫醒（提交观察者看到 [`NOTIFY_WAKE_KINDS`] 里的事件）→ 读待处理项 → 对每一项按此刻各桌面客户端报的在看哪调
//! [`should_notify`] → 一笔提交：`command_id` 由这一项算出（[`command_id_of`]），要发就带一条 outbox 条目（去向
//! [`notify_target`]，投递见 `routes.rs`），不发就是空的一笔。同一项永远是同一个 `command_id`：重复提交只得到
//! `replayed`、什么都不写，所以重连、重启、任务重起都不会再定一次。
//!
//! 内存里记着这次运行里定过、而且还在待处理的项，只为省掉重复提交；去重靠 `command_id`。提交经带封口闸的存储
//! （E4）；出错（封口后的 `Busy`、读写失败、到时）只记日志，这一轮停下，等下一次叫醒再对账。可重启：没有别的状态。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use iwork_core::notify::should_notify;
use iwork_domain::common::Actor;
use iwork_domain::id::CommandId;
use iwork_ports::notify::PendingItemRef;
use iwork_ports::read_model::PendingEntry;
use iwork_ports::store::{CommitRequest, EventCodec, NewOutboxItem, StoragePort};
use iwork_store::SqliteReadModel;
use tokio::sync::Notify;

use crate::attention::Attention;
use crate::routes::notify_target;

/// 落盘就叫醒本任务的事件种类：待处理表可能多出一项的那几种。
pub const NOTIFY_WAKE_KINDS: &[&str] = &[
    "approval_requested",
    "approval_review_updated",
    "user_action_proposed",
    "turn_state_changed",
    "agents_md_draft_proposed",
];

/// 定夺那一笔提交的执行者。
pub const NOTIFY_ACTOR: &str = "system:notify";

/// 一次读、一笔提交最多等这么久，到时这一轮作废、等下一次叫醒。
pub const NOTIFY_IO_LIMIT: Duration = Duration::from_secs(30);

/// 给待处理项 `entry` 定夺的那一笔提交的 `command_id`。
///
/// 继续卡片带上进入中断的 seq：它的编号是回合 id，同一个回合中断、续接、再中断会再出一张卡片，编号不变；不带 seq
/// 的话第二次中断得到 `replayed`、不再提醒。审批、亲手执行、草稿的编号一次一个，不用带；审批在 AI 审查更新后还是
/// 同一项，也不该再提醒一次。
pub fn command_id_of(entry: &PendingEntry) -> CommandId {
    match entry.item.item_ref() {
        PendingItemRef::Approval(id) => CommandId::derived("notify:approval", id.to_raw()),
        PendingItemRef::UserAction(id) => CommandId::derived("notify:user_action", id.to_raw()),
        PendingItemRef::AgentsMdDraft(id) => {
            CommandId::derived("notify:agents_md_draft", id.to_raw())
        }
        PendingItemRef::Resume(id) => CommandId::derived(
            &format!("notify:resume:{}", entry.created_seq.0),
            id.to_raw(),
        ),
    }
}

/// 任务体：先对一次账，之后每次被叫醒再对一次。
pub async fn run<St, C>(
    store: Arc<St>,
    read: Arc<SqliteReadModel<C>>,
    attention: Arc<Attention>,
    wake: Arc<Notify>,
) where
    St: StoragePort,
    C: EventCodec + 'static,
{
    let mut decided: HashSet<CommandId> = HashSet::new();
    loop {
        if let Err(why) = round(store.as_ref(), &read, &attention, &mut decided).await {
            tracing::warn!(why, "系统通知的定夺这一轮没做完，等下一次叫醒");
        }
        wake.notified().await;
    }
}

/// 对一次账：还在待处理、这次运行里没定过的每一项各提交一笔。
async fn round<St, C>(
    store: &St,
    read: &SqliteReadModel<C>,
    attention: &Attention,
    decided: &mut HashSet<CommandId>,
) -> Result<(), String>
where
    St: StoragePort,
    C: EventCodec + 'static,
{
    let pending = tokio::time::timeout(NOTIFY_IO_LIMIT, read.view(|tx| tx.pending()))
        .await
        .map_err(|_| "读待处理项超时".to_owned())?
        .map_err(|e| format!("读待处理项失败：{e}"))?;
    // 已经不在待处理的不用再记（它们不会回来：回来的继续卡片带新的 seq）
    let ids: Vec<CommandId> = pending.iter().map(command_id_of).collect();
    decided.retain(|id| ids.contains(id));
    let presences = attention.presences();
    for (entry, id) in pending.iter().zip(ids) {
        if decided.contains(&id) {
            continue;
        }
        let outbox = if should_notify(&entry.item, &presences) {
            vec![NewOutboxItem {
                target: notify_target(entry.item.item_ref()),
                payload: Vec::new(),
            }]
        } else {
            Vec::new()
        };
        let request = CommitRequest {
            command_id: id,
            actor: Actor(NOTIFY_ACTOR.into()),
            events: Vec::new(),
            outbox,
        };
        tokio::time::timeout(NOTIFY_IO_LIMIT, store.commit(request))
            .await
            .map_err(|_| format!("定夺 {} 的提交超时", entry.item.item_ref().key()))?
            .map_err(|e| format!("定夺 {} 的提交失败：{e}", entry.item.item_ref().key()))?;
        decided.insert(id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use iwork_domain::id::Seq;
    use iwork_domain::turn::InterruptReason;
    use iwork_ports::read_model::{PendingItem, ResumeCard};

    fn resume(turn: u128, seq: u64) -> PendingEntry {
        PendingEntry {
            item: PendingItem::Resume(ResumeCard {
                turn_id: tid(turn),
                conversation_id: cid(10),
                reason: InterruptReason::Crash,
            }),
            created_seq: Seq(seq),
        }
    }

    fn approval_entry(n: u128, seq: u64) -> PendingEntry {
        PendingEntry {
            item: PendingItem::Approval(approval(n, 20, 10)),
            created_seq: Seq(seq),
        }
    }

    /// 同一项永远同一个；继续卡片换一次中断就换一个；审批只看编号（审查更新挪了 seq 也一样）；不同种类不撞。
    #[test]
    fn command_id_per_pending_item() {
        assert_eq!(command_id_of(&resume(20, 7)), command_id_of(&resume(20, 7)));
        assert_ne!(
            command_id_of(&resume(20, 7)),
            command_id_of(&resume(20, 9)),
            "同一回合再中断一次是新的一项"
        );
        assert_eq!(
            command_id_of(&approval_entry(30, 7)),
            command_id_of(&approval_entry(30, 12))
        );
        assert_ne!(
            command_id_of(&approval_entry(30, 7)),
            command_id_of(&approval_entry(31, 7))
        );
        // 同一个 ULID 当审批与继续卡片的编号：名字空间不同，不撞
        let mut same_raw = resume(30, 7);
        if let PendingItem::Resume(r) = &mut same_raw.item {
            r.turn_id = iwork_domain::id::TurnId::from_raw(apid(30).to_raw());
        }
        assert_ne!(
            command_id_of(&same_raw),
            command_id_of(&approval_entry(30, 7))
        );
    }
}
