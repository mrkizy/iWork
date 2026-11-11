//! 拒绝记忆（需求 AC05；T07 方案 6.3、ADR-T07-c）：每个对话一份，键是动作指纹。
//!
//! 是事件的确定性函数：在线与重放走同一组方法（[`DenialMemory::apply`]），重启时按事件重建。
//!
//! | 事件 | 变化 |
//! | --- | --- |
//! | 用户拒绝 | 被拒调用的**每一个**子动作指纹 → `AutoDeny`（含此前处于 `ForceAskOnce` 的） |
//! | 本对话的用户消息 | 全部 `AutoDeny` → `ForceAskOnce` |
//! | 用户本人允许 | 该调用里处于 `ForceAskOnce` 的条目删除 |
//! | 作废 | 不变：用户没有表态，下次仍强制问 |
//! | 对话删除 | 整份删除（调用方丢掉这一份） |

use std::collections::{BTreeMap, BTreeSet};

use iwork_domain::action::ActionFingerprint;

/// 一个指纹所处的阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DenialPhase {
    /// 自动拒绝，直到用户发出下一条消息
    AutoDeny,
    /// 用户发过新消息：下一次请求强制询问一次（跳过放行规则、三档相同）
    ForceAskOnce,
}

/// 改变拒绝记忆的事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenialEvent {
    UserDenied(BTreeSet<ActionFingerprint>),
    UserMessage,
    UserAllowed(BTreeSet<ActionFingerprint>),
}

/// 一个对话的拒绝记忆。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DenialMemory {
    entries: BTreeMap<ActionFingerprint, DenialPhase>,
}

impl DenialMemory {
    pub fn get(&self, fp: &ActionFingerprint) -> Option<DenialPhase> {
        self.entries.get(fp).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn apply(&mut self, event: &DenialEvent) {
        match event {
            DenialEvent::UserDenied(fps) => {
                for fp in fps {
                    self.entries.insert(fp.clone(), DenialPhase::AutoDeny);
                }
            }
            DenialEvent::UserMessage => {
                for phase in self.entries.values_mut() {
                    *phase = DenialPhase::ForceAskOnce;
                }
            }
            DenialEvent::UserAllowed(fps) => {
                for fp in fps {
                    if self.entries.get(fp) == Some(&DenialPhase::ForceAskOnce) {
                        self.entries.remove(fp);
                    }
                }
            }
        }
    }

    /// 按事件重放：重建与在线走同一个函数。
    pub fn replay<'a>(events: impl IntoIterator<Item = &'a DenialEvent>) -> Self {
        let mut m = Self::default();
        for e in events {
            m.apply(e);
        }
        m
    }
}
