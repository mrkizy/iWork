//! 本机导入的判定（T10f 方案 A3.2、RA3）：M1 里什么都导不进来，每一项都有原因。

use iwork_core::local_import::{Gates, ImportPlan, Refusal, plan_import, refusal};
use iwork_domain::runtime::LocalConfigKind::{Mcp, Plugin, Skill};
use iwork_ports::local_import::{DiscoveredName, ItemId, Name, Owner};
use proptest::prelude::*;

const CLOSED: Gates = Gates {
    mcp: false,
    skill: false,
};
const OPEN: Gates = Gates {
    mcp: true,
    skill: true,
};

fn item(id: ItemId) -> DiscoveredName {
    DiscoveredName {
        id,
        kind: id.kind(),
        name: Name::from_source("x"),
        owner: Owner::User,
        transport: None,
        value_keys: vec![],
        skill_dirs: 0,
    }
}

fn found() -> Vec<DiscoveredName> {
    [
        ItemId::UserMcp(0),
        ItemId::UserSkill(0),
        ItemId::Plugin(0),
        ItemId::PluginMcp(0, 0),
        ItemId::PluginSkill(0, 1),
    ]
    .into_iter()
    .map(item)
    .collect()
}

#[test]
fn plan_import_reasons() {
    let all = [
        ItemId::UserMcp(0),
        ItemId::UserSkill(0),
        ItemId::Plugin(0),
        ItemId::PluginMcp(0, 0),
        ItemId::PluginSkill(0, 1),
    ];
    // 开关关：MCP、技能、插件各得到自己的「还没启用」（插件带的 MCP、技能按各自的种类）
    assert_eq!(
        plan_import(&found(), &all, CLOSED).unwrap_err(),
        vec![
            (ItemId::UserMcp(0), Refusal::NotEnabled(Mcp)),
            (ItemId::UserSkill(0), Refusal::NotEnabled(Skill)),
            (ItemId::Plugin(0), Refusal::NotEnabled(Plugin)),
            (ItemId::PluginMcp(0, 0), Refusal::NotEnabled(Mcp)),
            (ItemId::PluginSkill(0, 1), Refusal::NotEnabled(Skill)),
        ]
    );
    // 开关开：照样导不了，原因是「还没实现」
    let open = plan_import(&found(), &all, OPEN).unwrap_err();
    assert_eq!(
        open.iter().map(|(_, r)| *r).collect::<Vec<_>>(),
        vec![Refusal::NotImplemented; 5]
    );
    // 不在这次发现结果里的 id
    assert_eq!(
        plan_import(&found(), &[ItemId::UserMcp(7)], OPEN).unwrap_err(),
        vec![(ItemId::UserMcp(7), Refusal::UnknownItem)]
    );
}

#[test]
fn each_gate_covers_its_own_kinds() {
    // 只开 MCP：MCP 是「还没实现」，技能与插件仍是「还没启用」；只开技能反过来，插件跟着技能走
    let mcp_only = Gates {
        mcp: true,
        skill: false,
    };
    assert_eq!(refusal(Mcp, mcp_only), Refusal::NotImplemented);
    assert_eq!(refusal(Skill, mcp_only), Refusal::NotEnabled(Skill));
    assert_eq!(refusal(Plugin, mcp_only), Refusal::NotEnabled(Plugin));
    let skill_only = Gates {
        mcp: false,
        skill: true,
    };
    assert_eq!(refusal(Mcp, skill_only), Refusal::NotEnabled(Mcp));
    assert_eq!(refusal(Skill, skill_only), Refusal::NotImplemented);
    assert_eq!(refusal(Plugin, skill_only), Refusal::NotImplemented);
}

#[test]
fn this_build_has_both_gates_closed() {
    assert_eq!(
        Gates::current(),
        CLOSED,
        "放开导入要随 B、C 的方案走，不在 A 里改"
    );
}

#[test]
fn import_plan_has_no_variants() {
    // ImportPlan 没有变体：拿到一个值本身就不可能。穷尽 match 固定这一点，B、C 给它加变体时这一条要跟着改
    fn accepted(plan: ImportPlan) -> ! {
        match plan {}
    }
    let _ = accepted;
}

#[test]
fn empty_selection_gives_no_reasons() {
    assert_eq!(plan_import(&found(), &[], CLOSED).unwrap_err(), vec![]);
}

fn item_id() -> impl Strategy<Value = ItemId> {
    prop_oneof![
        (0u32..3).prop_map(ItemId::UserMcp),
        (0u32..3).prop_map(ItemId::UserSkill),
        (0u32..3).prop_map(ItemId::Plugin),
        (0u32..2, 0u32..2).prop_map(|(n, m)| ItemId::PluginMcp(n, m)),
        (0u32..2, 0u32..2).prop_map(|(n, m)| ItemId::PluginSkill(n, m)),
    ]
}

proptest! {
    /// 任何开关、任何发现结果、任何勾选：都拒绝；每个勾选项按次序各一条原因，原因照规则（这里独立写一遍）算。
    #[test]
    fn every_selection_is_refused_with_its_reason(
        found in prop::collection::vec(item_id(), 0..8),
        selected in prop::collection::vec(item_id(), 0..8),
        mcp in any::<bool>(),
        skill in any::<bool>(),
    ) {
        let found: Vec<DiscoveredName> = found.into_iter().map(item).collect();
        let reasons = plan_import(&found, &selected, Gates { mcp, skill }).unwrap_err();
        prop_assert_eq!(reasons.len(), selected.len());
        for ((id, why), want_id) in reasons.iter().zip(&selected) {
            prop_assert_eq!(id, want_id);
            let expected = match found.iter().find(|f| f.id == *id).map(|f| f.kind) {
                None => Refusal::UnknownItem,
                Some(Mcp) if mcp => Refusal::NotImplemented,
                Some(Skill | Plugin) if skill => Refusal::NotImplemented,
                Some(k) => Refusal::NotEnabled(k),
            };
            prop_assert_eq!(*why, expected);
        }
    }
}
