//! 审批档位菜单（ui-m1.md 第 2 节第 9 条）：每一档的说明、全开的「仅本对话」标签、切到全开的确认框，
//! 文字都由 Rust 给；已经是全开时不再给确认框。

mod support;

use iwork_client_core::derive::conversation::composer;
use iwork_client_core::store::Store;
use iwork_proto::pb;
use pb::domain_event::Event as E;
use support::*;

fn mode_view(s: &Store) -> pb::ApprovalModeView {
    composer(s, s.open.as_ref().unwrap()).approval_mode.unwrap()
}

#[test]
fn menu_has_descriptions_tag_and_confirm() {
    let s = store_with_open();
    let m = mode_view(&s);
    let rows: Vec<_> = m
        .options
        .iter()
        .map(|o| (o.label.as_str(), o.description.as_str(), o.tag.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("手动", "写文件、执行命令前始终询问", ""),
            ("自动", "仅对检测到的风险操作请求批准", ""),
            (
                "全开",
                "可不受限地访问和操作一切，只有 git push 仍会询问",
                "仅本对话"
            ),
        ]
    );
    let c = m.full_access_confirm.expect("不是全开时带确认框");
    assert_eq!(c.title, "本对话切到全开？");
    assert_eq!(c.confirm_label, "切到全开");
    assert!(c.body.contains("只有 git push 仍会询问"));
    assert_eq!(c.for_value, m.options[2].value, "选全开那一项才弹");
    assert!(!m.shield_checked, "手动时盾牌不带勾");
}

#[test]
fn no_confirm_when_already_full_access() {
    let mut s = store_with_open();
    // 时间线里先有对话实体（打开对话时的快照），档位事件才有地方落
    s.apply(snapshot(CONV, 3, vec![page(3, vec![], false)]));
    assert!(!mode_view(&s).full_access, "对照：切之前不是全开");
    s.apply(persisted(
        5,
        &[CONV],
        E::ApprovalModeChanged(pb::ApprovalModeChanged {
            target: Some(pb::ApprovalModeTarget {
                kind: Some(pb::approval_mode_target::Kind::ConversationId(CID.into())),
            }),
            mode: Some(pb::ApprovalMode::FullAccess as i32),
            ..Default::default()
        }),
    ));
    let m = mode_view(&s);
    assert!(m.full_access);
    assert!(m.shield_checked);
    assert_eq!(m.full_access_confirm, None);
}
