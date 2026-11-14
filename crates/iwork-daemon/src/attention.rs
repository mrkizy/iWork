//! 用户在看哪、点了哪条通知（T10a 方案 2.10；义务行 A7、Q2、S5、N1）。只在内存里，不提交、不产生事件。
//!
//! - 在看哪：每条桌面客户端连接报一次记一次（`ReportPresence`，后报的覆盖先报的），连接断开就删。后台进程刚起来
//!   时没有任何连接报过，按「没人在看」算。`notify` 任务决定发不发系统通知时读它（N1）。
//! - 点了哪条通知：后台进程里只存一份，带一个递增的 `token`，新的一次点击覆盖旧的；连接断开**不**清——用户点
//!   通知时客户端往往还没开，被叫起来的是一条新连接，它要问得到（supervisor 2026-11-14 定，方案 S5 行随之改）。
//!   桌面客户端被叫到前台后用 `GetOpenTarget` 来问。`token` 只在这一次运行里递增，后台重启后从 1 重新数，所以
//!   答案里另带这一次运行的 `boot_id`，客户端按 (boot_id, token) 认新旧（审查 2026-11-14-m1-t10a-8a 1/F-1）。
//!
//! 这两份状态只由这里改：在看哪经 [`Attention::report`] 与 [`Attention::connection_closed`]，目标经
//! [`Attention::record_open`]（系统通知的点击回调调它）。

use std::collections::HashMap;
use std::sync::Mutex;

use iwork_domain::common::Timestamp;
use iwork_ports::notify::{OpenRecord, OpenTarget, Presence};
use iwork_proto::pb;

#[derive(Debug)]
pub struct Attention {
    /// 这一次运行的标识，启动时生成，进程里不变
    boot_id: String,
    presences: Mutex<HashMap<u64, Presence>>,
    open: Mutex<Option<OpenRecord>>,
}

impl Attention {
    /// `boot_id`：这一次运行的标识，每次启动都要不同。
    pub fn new(boot_id: String) -> Self {
        Self {
            boot_id,
            presences: Mutex::default(),
            open: Mutex::default(),
        }
    }

    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    /// 记下连接 `connection_id`（桌面客户端）此刻在看哪。只收桌面客户端：别的种类返回 `false`，什么都不记。
    pub fn report(&self, connection_id: u64, kind: pb::ClientKind, presence: Presence) -> bool {
        if kind != pb::ClientKind::Desktop {
            return false;
        }
        lock(&self.presences).insert(connection_id, presence);
        true
    }

    /// 连接断开：删掉它报的在看哪。点通知记下的目标不动。
    pub fn connection_closed(&self, connection_id: u64) {
        lock(&self.presences).remove(&connection_id);
    }

    /// 此刻各个桌面客户端报的在看哪。
    pub fn presences(&self) -> Vec<Presence> {
        lock(&self.presences).values().copied().collect()
    }

    /// 用户点了一条通知：记下要打开的那一项，覆盖上一次的，`token` 加一。
    pub fn record_open(&self, target: OpenTarget, at: Timestamp) -> OpenRecord {
        let mut open = lock(&self.open);
        let token = open.map_or(1, |r| r.token + 1);
        let record = OpenRecord { token, target, at };
        *open = Some(record);
        record
    }

    /// 最近一次点击记下的目标；没点过为 `None`。
    pub fn open_target(&self) -> Option<OpenRecord> {
        *lock(&self.open)
    }
}

/// 锁里只做一次插入、删除或读取，不会在持锁时 panic；万一中毒也照样用里面的值。
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwork_domain::id::{ApprovalId, ConversationId, RawUlid};
    use iwork_ports::notify::{PendingItemRef, Viewing};

    fn presence(focused: bool) -> Presence {
        Presence {
            window_focused: focused,
            viewing: Viewing::PendingList,
        }
    }

    fn target(n: u128) -> OpenTarget {
        OpenTarget {
            item: PendingItemRef::Approval(ApprovalId::from_raw(RawUlid::from_u128(n))),
            conversation_id: Some(ConversationId::from_raw(RawUlid::from_u128(10))),
        }
    }

    /// 在看哪按连接记、后报的覆盖先报的；连接断开只删它自己的；只收桌面客户端。
    #[test]
    fn presence_is_per_desktop_connection_and_cleared_on_close() {
        let a = Attention::new("boot-1".into());
        assert!(a.presences().is_empty(), "刚起来没人报过");
        assert!(a.report(1, pb::ClientKind::Desktop, presence(true)));
        assert!(a.report(2, pb::ClientKind::Desktop, presence(true)));
        assert!(a.report(1, pb::ClientKind::Desktop, presence(false)));
        let mut got = a.presences();
        got.sort_by_key(|p| p.window_focused);
        assert_eq!(
            got,
            [presence(false), presence(true)],
            "连接 1 后报的覆盖了先报的"
        );
        assert!(
            !a.report(3, pb::ClientKind::DevTool, presence(true)),
            "不是桌面客户端"
        );
        assert_eq!(a.presences().len(), 2, "对照：开发工具的连接没记进去");
        a.connection_closed(1);
        assert_eq!(a.presences(), [presence(true)]);
        a.connection_closed(1);
        assert_eq!(a.presences().len(), 1, "重复断开无害");
    }

    /// 点通知记下的目标：进程里一份，新点击覆盖、`token` 递增；连接断开不清。
    #[test]
    fn open_target_is_process_wide_and_survives_disconnect() {
        let a = Attention::new("boot-1".into());
        assert_eq!(a.open_target(), None);
        let first = a.record_open(target(1), Timestamp(5));
        assert_eq!(first.token, 1);
        a.report(7, pb::ClientKind::Desktop, presence(true));
        a.connection_closed(7);
        assert_eq!(a.open_target(), Some(first), "连接断开不清目标");
        let second = a.record_open(target(2), Timestamp(6));
        assert_eq!((second.token, second.target), (2, target(2)));
        assert_eq!(a.open_target(), Some(second), "新点击覆盖旧的");
    }
}
