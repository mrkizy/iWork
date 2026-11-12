//! Dock 角标与跳动（T09 方案 2.8）：只算，壳执行。
//!
//! - 角标文字 = 侧栏「待你处理」徽标的同一个函数（`derive::text::pending_badge`）算出的文字，不另数一遍；
//!   为 0 时清掉。待处理表每次整份换过（摘要重查、快照）之后更新；断线期间表不变，角标保留最后的数字。
//! - 跳动：新的一份表里出现了上一份里没有的条目 id → 请求一次。上一份表在进程里一直留着（跨重连），
//!   同一项不会跳第二次；一项刚处理掉、同时又来一项，数字没变也会跳。第一份表（启动时）不跳。
//!   系统只在应用不在前台时才让图标跳，「用户正在看」不用这里判断。
//! - 不看网页：窗口关着、没有网页在取帧时照常算、照常发。

use std::collections::BTreeSet;

use crate::derive;
use crate::intent::Effect;
use crate::store::Store;

#[derive(Debug, Default)]
pub struct Attention {
    /// 上一份待处理表里的条目 id；还没见过任何一份时为 `None`。
    known: Option<BTreeSet<String>>,
    /// 上一次交给壳的角标文字（`None` = 没有角标）；还没交过时为外层 `None`。
    badge: Option<Option<String>>,
}

impl Attention {
    /// 待处理表整份换过之后调：返回要交给壳的效果（角标变了、出现了新条目）。
    pub fn on_pending_replaced(&mut self, store: &Store) -> Vec<Effect> {
        let mut out = Vec::new();
        let ids: BTreeSet<String> = store
            .summary
            .pending
            .iter()
            .filter_map(|p| derive::pending::item_id(p).map(str::to_owned))
            .collect();
        let badge = derive::text::pending_badge(u32::try_from(ids.len()).unwrap_or(u32::MAX))
            .map(|b| b.text);
        if self.badge.as_ref() != Some(&badge) {
            self.badge = Some(badge.clone());
            out.push(Effect::SetBadge(badge));
        }
        if let Some(known) = &self.known
            && ids.iter().any(|id| !known.contains(id))
        {
            out.push(Effect::RequestAttention);
        }
        self.known = Some(ids);
        out
    }
}
