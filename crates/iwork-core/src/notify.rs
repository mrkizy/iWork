//! 系统通知发不发、发什么（T10a 方案 2.10、第 0a.3 节 N1；任务卡范围 4）。纯函数：不读时钟、不做 I/O。
//!
//! - 发不发：有一个桌面客户端报了窗口在前台，并且正看着这一项所在的对话或「待你处理」页 → 不发；别的情况都发。
//!   同一项只定一次、已处理或作废的不发，不归这里：前者靠 `notify` 任务那一笔提交的 `command_id`，后者靠调用方只
//!   传还在待处理的项、投递时再查一次（N2）。
//! - 发什么：只用「待你处理」页上这一项已有的文字——标题是种类，正文是这一项的标题（和 client-core
//!   `derive::pending::item_title` 同一套规则，两边由 `iwork-daemon` 的对拍测试守着）。不出现「主管」。

use iwork_domain::turn::InterruptReason;
use iwork_domain::user_action::Proposal;
use iwork_ports::notify::{OpenTarget, Presence, SystemNotification, Viewing};
use iwork_ports::read_model::PendingItem;

/// 待处理项 `item` 新出现时，要不要发系统通知。`presences` 是此刻各个桌面客户端报的在看哪（没有连接就是空的）。
pub fn should_notify<'a>(
    item: &PendingItem,
    presences: impl IntoIterator<Item = &'a Presence>,
) -> bool {
    let conversation = item.conversation_id();
    !presences.into_iter().any(|p| {
        p.window_focused
            && match p.viewing {
                Viewing::PendingList => true,
                Viewing::Conversation(c) => Some(c) == conversation,
                Viewing::Elsewhere => false,
            }
    })
}

/// 这一项的系统通知。`project_name`：AGENTS.md 草稿所属项目的名字（别的种类用不到；查不到时传 `None`，正文为空）。
pub fn notification(item: &PendingItem, project_name: Option<&str>) -> SystemNotification {
    SystemNotification {
        title: kind_label(item).to_owned(),
        body: item_title(item, project_name),
        open: Some(OpenTarget::of(item)),
    }
}

/// 「待你处理」页上这一项的种类文字。
pub fn kind_label(item: &PendingItem) -> &'static str {
    match item {
        PendingItem::Approval(_) => "审批",
        PendingItem::UserAction(_) => "需要你亲手执行",
        PendingItem::AgentsMdDraft(_) => "AGENTS.md 草稿",
        PendingItem::Resume(_) => "继续或放弃",
    }
}

/// 「待你处理」页上这一项的标题：审批写动作摘要，亲手执行写命令或补丁目标，草稿写项目名，继续卡片写中断原因。
pub fn item_title(item: &PendingItem, project_name: Option<&str>) -> String {
    match item {
        PendingItem::Approval(a) => format!("审批：{}", a.action.raw_summary),
        PendingItem::UserAction(c) => match &c.proposal {
            Proposal::Command { command, .. } => command.clone(),
            Proposal::Patch { path, .. } => format!("修改 {}", path.0.display()),
        },
        PendingItem::AgentsMdDraft(_) => project_name.unwrap_or_default().to_owned(),
        PendingItem::Resume(r) => interrupt_reason(r.reason).to_owned(),
    }
}

fn interrupt_reason(r: InterruptReason) -> &'static str {
    match r {
        InterruptReason::Crash => "后台服务意外中断，回合停在这里",
        InterruptReason::ApprovalWaitLimit => "等审批太久，回合先暂停了",
    }
}
