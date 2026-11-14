//! 系统通知发不发、发什么（T10a 方案 2.10、N1；任务卡验收 1 的通知五格里归纯函数的几格）。
//!
//! 「同一项重连、重启后不重复」「已决定的不发」不归这个函数（`command_id` 去重、只传还在待处理的项），在 daemon
//! 的接线测试里（步 8b）。

mod support;

use std::path::PathBuf;

use iwork_core::notify::{item_title, kind_label, notification, should_notify};
use iwork_domain::action::CanonicalPath;
use iwork_domain::common::{ArtifactArea, ArtifactRef, Sha256, Timestamp};
use iwork_domain::id::{JobId, ProjectId, RawUlid};
use iwork_domain::turn::InterruptReason;
use iwork_domain::user_action::Proposal;
use iwork_ports::notify::{OpenTarget, PendingItemRef, Presence, Viewing};
use iwork_ports::read_model::{AgentsMdDraftCard, PendingItem, ResumeCard};
use proptest::prelude::*;
use support::fold_fx::{approval, card, cid, tid};

const CONV: u128 = 10;
const OTHER: u128 = 11;

fn draft() -> PendingItem {
    PendingItem::AgentsMdDraft(AgentsMdDraftCard {
        card_id: JobId::from_raw(RawUlid::from_u128(40)),
        project_id: ProjectId::from_raw(RawUlid::from_u128(1)),
        content: ArtifactRef {
            sha256: Sha256([3; 32]),
            area: ArtifactArea::Artifacts,
            size: 10,
        },
        proposed_at: Timestamp(0),
    })
}

/// 四种待处理项各一个，除草稿外都在对话 `CONV` 里。
fn items() -> Vec<PendingItem> {
    vec![
        PendingItem::Approval(approval(1, 20, CONV, "git push")),
        PendingItem::UserAction(card(2, 20, CONV)),
        PendingItem::Resume(ResumeCard {
            turn_id: tid(20),
            conversation_id: cid(CONV),
            reason: InterruptReason::Crash,
        }),
        draft(),
    ]
}

fn focused(viewing: Viewing) -> Presence {
    Presence {
        window_focused: true,
        viewing,
    }
}

fn background(viewing: Viewing) -> Presence {
    Presence {
        window_focused: false,
        viewing,
    }
}

/// 验收 1 的几格：没有桌面客户端连着 → 发；前台正看着这个对话或「待你处理」页 → 不发；看着别的对话 → 发。
#[test]
fn notify_rules_table() {
    let in_conv = PendingItem::Approval(approval(1, 20, CONV, "git push"));
    let cases: [(&str, Vec<Presence>, bool); 7] = [
        ("没有桌面客户端连着", vec![], true),
        (
            "前台正看着这个对话",
            vec![focused(Viewing::Conversation(cid(CONV)))],
            false,
        ),
        (
            "前台正看着「待你处理」页",
            vec![focused(Viewing::PendingList)],
            false,
        ),
        (
            "前台看着别的对话",
            vec![focused(Viewing::Conversation(cid(OTHER)))],
            true,
        ),
        ("前台在别的页面", vec![focused(Viewing::Elsewhere)], true),
        (
            "窗口在后台，停在这个对话",
            vec![background(Viewing::Conversation(cid(CONV)))],
            true,
        ),
        (
            "两个客户端，一个在后台看着、一个在前台看着",
            vec![
                background(Viewing::Elsewhere),
                focused(Viewing::Conversation(cid(CONV))),
            ],
            false,
        ),
    ];
    for (name, presences, want) in cases {
        assert_eq!(should_notify(&in_conv, &presences), want, "{name}");
    }
}

/// 草稿没有对话归属：只有前台看着「待你处理」页时不发，看着任何对话都发。
#[test]
fn draft_is_held_back_only_by_pending_list() {
    let d = draft();
    assert!(should_notify(
        &d,
        &[focused(Viewing::Conversation(cid(CONV)))]
    ));
    assert!(!should_notify(&d, &[focused(Viewing::PendingList)]));
}

/// 文字只用「待你处理」页上已有的：标题是种类，正文是这一项的标题；点开的目标是这一项与它的对话。
#[test]
fn notification_text_comes_from_pending_summary() {
    let mut patch = card(3, 20, CONV);
    patch.proposal = Proposal::Patch {
        path: CanonicalPath(PathBuf::from("/work/p1/.git/config")),
        old: String::new(),
        new: "x".into(),
    };
    let wait_limit = PendingItem::Resume(ResumeCard {
        turn_id: tid(21),
        conversation_id: cid(CONV),
        reason: InterruptReason::ApprovalWaitLimit,
    });
    let cases = [
        (items()[0].clone(), "审批", "审批：git push"),
        (items()[1].clone(), "需要你亲手执行", "rm -rf .git/hooks/x"),
        (
            PendingItem::UserAction(patch),
            "需要你亲手执行",
            "修改 /work/p1/.git/config",
        ),
        (
            items()[2].clone(),
            "继续或放弃",
            "后台服务意外中断，回合停在这里",
        ),
        (wait_limit, "继续或放弃", "等审批太久，回合先暂停了"),
        (draft(), "AGENTS.md 草稿", "登录服务"),
    ];
    for (item, title, body) in cases {
        let n = notification(&item, Some("登录服务"));
        assert_eq!((n.title.as_str(), n.body.as_str()), (title, body));
        assert!(!n.title.contains("主管") && !n.body.contains("主管"));
        assert_eq!(n.open, Some(OpenTarget::of(&item)));
    }
    // 对照：草稿查不到项目名时正文为空，别的种类不看项目名
    assert_eq!(item_title(&draft(), None), "");
    assert_eq!(item_title(&items()[0], None), "审批：git push");
    let t = OpenTarget::of(&draft());
    assert!(matches!(t.item, PendingItemRef::AgentsMdDraft(_)) && t.conversation_id.is_none());
    assert_eq!(kind_label(&draft()), "AGENTS.md 草稿");
}

fn arb_viewing() -> impl Strategy<Value = Viewing> {
    prop_oneof![
        Just(Viewing::Elsewhere),
        Just(Viewing::PendingList),
        prop_oneof![Just(CONV), Just(OTHER)].prop_map(|c| Viewing::Conversation(cid(c))),
    ]
}

fn arb_presence() -> impl Strategy<Value = Presence> {
    (any::<bool>(), arb_viewing()).prop_map(|(window_focused, viewing)| Presence {
        window_focused,
        viewing,
    })
}

proptest! {
    /// 没有客户端在看时一定发；窗口不在前台的客户端挡不住；多一个客户端只可能把「发」变成「不发」。
    #[test]
    fn presence_only_ever_holds_back(
        which in 0usize..4,
        xs in proptest::collection::vec(arb_presence(), 0..4),
        extra in arb_presence(),
    ) {
        let item = &items()[which];
        prop_assert!(should_notify(item, &[]));
        let base = should_notify(item, &xs);
        let mut more = xs.clone();
        more.push(extra);
        let with = should_notify(item, &more);
        if !extra.window_focused {
            prop_assert_eq!(with, base, "后台的客户端不改结果");
        }
        prop_assert!(!with || base, "多一个客户端不会把「不发」变成「发」");
        if extra.window_focused && extra.viewing == Viewing::PendingList {
            prop_assert!(!with, "前台看着「待你处理」页，哪一项都不发");
        }
    }
}
