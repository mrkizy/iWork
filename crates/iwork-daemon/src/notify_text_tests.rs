//! 系统通知的文字和「待你处理」页同一份（T10a 方案 2.10：只用卡片上已有的摘要）。
//!
//! 通知在后台出（`iwork-core::notify`，读领域类型），页面在客户端出（client-core `derive::pending`，读协议类型），
//! 两个 crate 互不依赖，抄成了两份：这里把同一批待处理项转成协议交给客户端那一份，逐项比。改了任何一边的写法，
//! 这里红。

use iwork_app::views::pending_entry_to_proto;
use iwork_client_core::derive::pending::item_title;
use iwork_client_core::store::Store;
use iwork_core::notify::notification;
use iwork_domain::action::CanonicalPath;
use iwork_domain::id::Seq;
use iwork_domain::turn::InterruptReason;
use iwork_domain::user_action::Proposal;
use iwork_ports::read_model::{AgentsMdDraftCard, PendingEntry, PendingItem, ResumeCard};
use iwork_proto::pb;

use crate::fixtures::*;

const PROJECT_NAME: &str = "登录服务";

fn items() -> Vec<PendingItem> {
    let mut patch = card(31, 20, 10);
    patch.proposal = Proposal::Patch {
        path: CanonicalPath("/work/p1/.git/config".into()),
        old: String::new(),
        new: "x".into(),
    };
    let mut items = vec![
        PendingItem::Approval(approval(30, 20, 10)),
        PendingItem::UserAction(card(32, 20, 10)),
        PendingItem::UserAction(patch),
        PendingItem::AgentsMdDraft(AgentsMdDraftCard {
            card_id: jid(40),
            project_id: pid(1),
            content: artifact(3, iwork_domain::common::ArtifactArea::Artifacts),
            proposed_at: iwork_domain::common::Timestamp(T0),
        }),
    ];
    for reason in [InterruptReason::Crash, InterruptReason::ApprovalWaitLimit] {
        items.push(PendingItem::Resume(ResumeCard {
            turn_id: tid(21),
            conversation_id: cid(10),
            reason,
        }));
    }
    items
}

#[test]
fn notification_body_equals_pending_page_title() {
    let mut store = Store::new();
    store.summary.projects.push(pb::Project {
        id: pid(1).to_string(),
        name: PROJECT_NAME.into(),
        ..Default::default()
    });
    let items = items();
    assert_eq!(items.len(), 6, "对照：四种都在，补丁与两种中断原因各一项");
    for item in items {
        let page = pending_entry_to_proto(&PendingEntry {
            item: item.clone(),
            created_seq: Seq(1),
        })
        .unwrap();
        let n = notification(&item, Some(PROJECT_NAME));
        assert_eq!(n.body, item_title(&store, &page), "{item:?}");
        assert!(!n.body.is_empty(), "对照：比的不是两个空串（{item:?}）");
    }
}
