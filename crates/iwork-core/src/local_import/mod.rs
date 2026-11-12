//! 本机导入的开关与判定（T10f 方案 A3.2）。
//!
//! 纯函数：不读文件，不认识任何运行时的文件名与键名。M1 里什么都导不进来：两个导入开关默认关着，
//! 开着也只把原因从「还没启用」换成「还没实现」；[`ImportPlan`] 没有变体，[`plan_import`] 只可能返回拒绝。
//! 真正放开导入随 B（MCP）、C（技能）的方案走：它们给 [`ImportPlan`] 加变体。

use iwork_domain::runtime::LocalConfigKind;
use iwork_ports::local_import::{DiscoveredName, ItemId};

/// MCP 导入。等产品负责人答 PD-1；改成 `true` 也没有导入的代码可走（原因变成 [`Refusal::NotImplemented`]）。
pub const MCP_IMPORT_ENABLED: bool = false;
/// 技能（含插件）导入。等 PD-9、PD-5 与 K3 实测；同上。
pub const SKILL_IMPORT_ENABLED: bool = false;

/// 两个导入开关。生产代码只经 [`Gates::current`] 从上面的常量构造；测试可以给任意值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gates {
    pub mcp: bool,
    pub skill: bool,
}

impl Gates {
    /// 这一版的开关（来自 [`MCP_IMPORT_ENABLED`]、[`SKILL_IMPORT_ENABLED`]）。
    pub const fn current() -> Self {
        Gates {
            mcp: MCP_IMPORT_ENABLED,
            skill: SKILL_IMPORT_ENABLED,
        }
    }

    /// 管这一种条目的开关：MCP 看 `mcp`；技能与插件看 `skill`（插件导进来的是它带的技能）。
    pub const fn covers(self, kind: LocalConfigKind) -> bool {
        match kind {
            LocalConfigKind::Mcp => self.mcp,
            LocalConfigKind::Skill | LocalConfigKind::Plugin => self.skill,
        }
    }
}

/// 一项为什么导不了。给人看的文字由应用服务照它拼（能力与界面分离：原因由后台定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 管这一种的开关关着：「MCP 导入还没启用」/「技能导入还没启用」。
    NotEnabled(LocalConfigKind),
    /// 开关开着，可这一版没有导入的代码：「这一类导入还没实现」。
    NotImplemented,
    /// 勾选的条目不在这次发现的结果里（条目 id 是序号，用户改了配置之后可能对不上，方案 LA4）。
    UnknownItem,
}

/// 这一种条目为什么导不了。A 里每一种都有原因：开关关着是还没启用，开着是还没实现。
pub const fn refusal(kind: LocalConfigKind, gates: Gates) -> Refusal {
    if gates.covers(kind) {
        Refusal::NotImplemented
    } else {
        Refusal::NotEnabled(kind)
    }
}

/// 导入计划。A 里没有「接受」：这个枚举没有变体，所以 [`plan_import`] 只可能返回 `Err`。B、C 给它加变体。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportPlan {}

/// 判定勾选的条目能不能导。`found` 是这次发现的结果，`selected` 是用户勾的 id。
///
/// A 里一律拒绝：返回 `selected` 里每一项（按勾选的次序，重复的照样各一条）的 id 与原因；不在 `found` 里的是
/// [`Refusal::UnknownItem`]。`selected` 为空时返回空的拒绝列表，调用方在调用之前就该按参数错误回绝空的勾选。
pub fn plan_import(
    found: &[DiscoveredName],
    selected: &[ItemId],
    gates: Gates,
) -> Result<ImportPlan, Vec<(ItemId, Refusal)>> {
    let reasons = selected
        .iter()
        .map(|id| {
            let why = match found.iter().find(|f| f.id == *id) {
                Some(f) => refusal(f.kind, gates),
                None => Refusal::UnknownItem,
            };
            (*id, why)
        })
        .collect();
    Err(reasons)
}
