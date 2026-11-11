//! ListAnchors 与 GetConversation 的新取页方式（T09 方案 2.9、G10、G11）：对着锚点场景折叠后直接查。

use iwork_fake_daemon::scenario::{self, Step};
use iwork_fake_daemon::{Tables, envelope_time};
use iwork_proto::pb;
use iwork_proto::pb::anchor::Kind as K;
use iwork_proto::pb::query::Kind as Q;
use iwork_proto::pb::query_result::Kind as R;

fn world(name: &str) -> (Tables, u64) {
    let mut t = Tables::default();
    let mut head = 0;
    for s in scenario::get(name).unwrap().steps() {
        match s {
            Step::Append(e) => {
                head += 1;
                t.fold(head, &envelope_time(head), e.event.as_ref().unwrap());
            }
            Step::Mutate(m) => m(&mut t),
            _ => {}
        }
    }
    (t, head)
}

fn anchors(
    t: &Tables,
    head: u64,
    conv: &str,
    around: u64,
    before: u32,
    after: u32,
) -> pb::ListAnchorsResult {
    let q = pb::Query {
        kind: Some(Q::ListAnchors(pb::ListAnchors {
            conversation_id: conv.into(),
            around_seq: around,
            before,
            after,
        })),
    };
    match t.query(head, &q).unwrap().kind.unwrap() {
        R::ListAnchors(a) => a,
        other => panic!("{other:?}"),
    }
}

fn kinds(r: &pb::ListAnchorsResult) -> Vec<&'static str> {
    r.anchors
        .iter()
        .map(|a| match a.kind.as_ref().unwrap() {
            K::UserMessage(_) => "你",
            K::Waiting(_) => "等你",
            K::FailedTurn(_) => "失败",
        })
        .collect()
}

#[test]
fn basic_has_three_kinds_in_seq_order_and_counts_whole_conversation() {
    let (t, head) = world("anchors-basic");
    let all = anchors(&t, head, "c-anchors", 0, 100, 100);
    assert_eq!(all.waiting_count, 1);
    assert_eq!(all.failed_count, 1);
    let k = kinds(&all);
    assert_eq!(
        k.iter().filter(|k| **k == "你").count(),
        18,
        "16 轮 + 失败那一轮 + 等审批那一轮的用户消息"
    );
    assert_eq!(k.last(), Some(&"等你"), "审批最后出现");
    assert!(
        all.anchors
            .windows(2)
            .all(|w| w[0].created_seq < w[1].created_seq),
        "按 seq 升序"
    );
    assert!(!all.has_before && !all.has_after);
    // 失败那一处：输出过内容
    let failed = all.anchors.iter().find_map(|a| match a.kind.as_ref() {
        Some(K::FailedTurn(f)) => Some(f.clone()),
        _ => None,
    });
    assert_eq!(
        failed.map(|f| (f.reason, f.had_output)),
        Some(("模型暂时不可用".into(), true))
    );
}

#[test]
fn window_around_a_position() {
    let (t, head) = world("anchors-basic");
    let all = anchors(&t, head, "c-anchors", 0, 100, 100);
    let mid = all.anchors[8].created_seq;
    let w = anchors(&t, head, "c-anchors", mid, 3, 2);
    assert_eq!(
        w.anchors.iter().map(|a| a.created_seq).collect::<Vec<_>>(),
        all.anchors[6..11]
            .iter()
            .map(|a| a.created_seq)
            .collect::<Vec<_>>(),
        "不大于中心的取 3 条，大于的取 2 条"
    );
    assert!(w.has_before && w.has_after);
    assert_eq!(w.waiting_count, 1, "计数是整个对话的，不随窗口变");
    let latest = anchors(&t, head, "c-anchors", 0, 2, 2);
    assert_eq!(latest.anchors.len(), 2, "0 = 从最新往回取");
    assert!(latest.has_before && !latest.has_after);
}

#[test]
fn decided_approval_stops_being_anchor() {
    let (mut t, mut head) = world("anchors-basic");
    head += 1;
    t.fold(
        head,
        &envelope_time(head),
        iwork_fake_daemon::fixtures::approval_decided(
            "ap-ab-wait",
            pb::ApprovalState::Approved,
            pb::ApprovalDecider::User,
        )
        .event
        .as_ref()
        .unwrap(),
    );
    let all = anchors(&t, head, "c-anchors", 0, 100, 100);
    assert_eq!(all.waiting_count, 0);
    assert!(!kinds(&all).contains(&"等你"));
}

#[test]
fn around_and_after_paging_reach_the_ends() {
    let (t, head) = world("anchors-unloaded");
    let page = |g: pb::GetConversation| match t
        .query(
            head,
            &pb::Query {
                kind: Some(Q::GetConversation(g)),
            },
        )
        .unwrap()
        .kind
        .unwrap()
    {
        R::GetConversation(r) => r,
        other => panic!("{other:?}"),
    };
    let base = || pb::GetConversation {
        conversation_id: "c-anchors-long".into(),
        limit: 50,
        ..Default::default()
    };
    let latest = page(base());
    assert!(
        latest.has_more && !latest.has_newer,
        "最新一页：前面还有，后面没有"
    );
    let first = anchors(&t, head, "c-anchors-long", 1, 0, 1).anchors[0].created_seq;
    let around = page(pb::GetConversation {
        around_seq: Some(first),
        ..base()
    });
    assert!(
        around.messages.iter().any(|m| m.created_seq == first),
        "居中那一页含目标消息"
    );
    assert!(around.has_newer, "居中那一页后面还有");
    // 从居中那一页一直往后翻，最后 has_newer 为假
    let mut last = around.messages.last().unwrap().id.clone();
    let mut pages = 0;
    loop {
        let p = page(pb::GetConversation {
            after_message_id: Some(last.clone()),
            ..base()
        });
        pages += 1;
        if !p.has_newer {
            break;
        }
        last = p.messages.last().unwrap().id.clone();
        assert!(pages < 20, "翻不到头");
    }
}
