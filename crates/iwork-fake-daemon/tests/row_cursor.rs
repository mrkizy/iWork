//! GetConversation 的行级游标（T09 方案 G11、2.9「行级游标的语义」，组 2b 审查 5/F-3）。对着折叠好的表直接查。

use iwork_fake_daemon::fixtures as f;
use iwork_fake_daemon::{Tables, envelope_time};
use iwork_proto::pb;
use iwork_proto::pb::query::Kind as Q;
use iwork_proto::pb::query_result::Kind as R;
use pb::TurnState as T;

const CONV: &str = "c-long";

/// 一个对话：两问两答；第一个回合里 `tools` 次工具调用，中间夹一个审批；第二个回合正常结束。
fn world(tools: usize) -> (Tables, u64) {
    let mut events = vec![
        f::project("p-demo", "demo", "/tmp/demo"),
        f::project_conversation(CONV, "p-demo"),
        f::user_message("m-q1", CONV, "跑一遍全部测试"),
        f::turn_queued("t1", CONV, f::IWORK),
        f::turn_state("t1", T::Queued, T::Running, None),
    ];
    for i in 0..tools {
        events.push(f::tool_started(
            "t1",
            &format!("tc{i}"),
            "Bash",
            "cargo test",
        ));
        if i == tools / 2 {
            events.push(f::approval(
                "ap1",
                CONV,
                "t1",
                f::command_action("rm -rf build"),
                pb::ApprovalKind::Ask,
                &[],
            ));
        }
    }
    events.push(f::assistant_message("m-a1", CONV, "t1", f::IWORK, "跑完了"));
    events.push(f::turn_finished("t1", T::Completed, false, None));
    events.push(f::user_message("m-q2", CONV, "好"));
    events.push(f::turn_queued("t2", CONV, f::IWORK));
    events.push(f::turn_state("t2", T::Queued, T::Running, None));
    events.push(f::assistant_message("m-a2", CONV, "t2", f::IWORK, "嗯"));
    events.push(f::turn_finished("t2", T::Completed, false, None));
    let mut t = Tables::default();
    let mut head = 0;
    for e in events {
        head += 1;
        t.fold(head, &envelope_time(head), e.event.as_ref().unwrap());
    }
    (t, head)
}

fn query(
    t: &Tables,
    head: u64,
    g: pb::GetConversation,
) -> Result<pb::GetConversationResult, pb::Error> {
    match t
        .query(
            head,
            &pb::Query {
                kind: Some(Q::GetConversation(g)),
            },
        )?
        .kind
        .unwrap()
    {
        R::GetConversation(r) => Ok(r),
        other => panic!("{other:?}"),
    }
}

fn base() -> pb::GetConversation {
    pb::GetConversation {
        conversation_id: CONV.into(),
        limit: 7,
        ..Default::default()
    }
}

/// 一页结果里的「行」（与 2.9 同一个定义），按排序字段从旧到新：(seq, 身份)。
fn rows(r: &pb::GetConversationResult, cursor_side: impl Fn(u64) -> bool) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = Vec::new();
    out.extend(r.messages.iter().map(|m| (m.created_seq, m.id.clone())));
    out.extend(
        r.tool_calls
            .iter()
            .map(|c| (c.created_seq, c.tool_call_id.clone())),
    );
    out.extend(r.approvals.iter().map(|a| (a.created_seq, a.id.clone())));
    out.extend(r.user_actions.iter().map(|c| (c.created_seq, c.id.clone())));
    // 回合可以带来范围外的结束位置：只数游标这一侧、且确实被选中的（客户端照同一个办法挑，见 2.2 第 4 条）
    out.extend(
        r.turns
            .iter()
            .filter(|t| t.ended_seq != 0 && cursor_side(t.ended_seq))
            .map(|t| (t.ended_seq, format!("end:{}", t.id))),
    );
    out.sort();
    out
}

/// 从最新往前按 before_seq 一页页取到头，再从最旧往后按 after_seq 取到头：两次拼出来的行序列相同、没有重复，
/// 且等于整个对话的全部行（对照：消息游标一次全取）。
#[test]
fn row_cursor_pages_tile_the_conversation() {
    let (t, head) = world(30);
    let limit = base().limit as usize;

    // 往前：从最大的 seq 之后开始
    let mut backward: Vec<(u64, String)> = Vec::new();
    let mut cursor = head + 1;
    loop {
        let r = query(
            &t,
            head,
            pb::GetConversation {
                before_seq: Some(cursor),
                ..base()
            },
        )
        .unwrap();
        let mut page: Vec<_> = rows(&r, |s| s < cursor);
        // 后台选中的正好是紧挨游标的 limit 行：客户端照同一个办法从候选里挑
        if page.len() > limit {
            page.drain(..page.len() - limit);
        }
        assert!(page.len() <= limit);
        let Some(&(oldest, _)) = page.first() else {
            break;
        };
        assert!(page.iter().all(|(s, _)| *s < cursor), "都在游标这一侧");
        page.extend(backward);
        backward = page;
        cursor = oldest;
        if !r.has_more {
            break;
        }
    }
    // 往后：从 0 之后开始（游标 0 不合法，从最旧一行的前一个位置起：1）
    let mut forward: Vec<(u64, String)> = Vec::new();
    let first = backward.first().unwrap().0;
    let mut r0 = query(
        &t,
        head,
        pb::GetConversation {
            before_seq: Some(first + 1),
            limit: 1,
            ..base()
        },
    )
    .unwrap();
    assert!(!r0.has_more, "对照：最旧一行之前没有了");
    forward.extend(rows(&r0, |s| s <= first));
    let mut cursor = first;
    loop {
        r0 = query(
            &t,
            head,
            pb::GetConversation {
                after_seq: Some(cursor),
                ..base()
            },
        )
        .unwrap();
        let mut page = rows(&r0, |s| s > cursor);
        page.truncate(limit);
        let Some(&(newest, _)) = page.last() else {
            break;
        };
        assert!(page.iter().all(|(s, _)| *s > cursor));
        forward.extend(page);
        cursor = newest;
        if !r0.has_newer {
            break;
        }
    }
    assert_eq!(forward, backward, "两个方向拼出来的一样");
    let mut ids: Vec<&String> = backward.iter().map(|(_, id)| id).collect();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "没有重复");
    // 对照：整个对话的行 = 4 条消息 + 30 个工具 + 1 个审批 + 2 个回合结束
    assert_eq!(n, 4 + 30 + 1 + 2, "{backward:?}");
}

/// 五个取法字段任取两个都回 INVALID_ARGUMENT；行级游标的 limit 为 0、游标为 0 也回；limit 大于 1000 按 1000。
#[test]
fn conversation_query_modes_are_exclusive() {
    let (t, head) = world(3);
    let set: [fn(&mut pb::GetConversation); 5] = [
        |g| g.before_message_id = Some("m-q2".into()),
        |g| g.after_message_id = Some("m-q1".into()),
        |g| g.around_seq = Some(5),
        |g| g.before_seq = Some(10),
        |g| g.after_seq = Some(2),
    ];
    let invalid = i32::from(pb::ErrorCode::InvalidArgument);
    for i in 0..set.len() {
        let mut one = base();
        set[i](&mut one);
        assert!(query(&t, head, one).is_ok(), "对照：只设第 {i} 个可以");
        for j in (i + 1)..set.len() {
            let mut g = base();
            set[i](&mut g);
            set[j](&mut g);
            assert_eq!(
                query(&t, head, g).unwrap_err().code,
                invalid,
                "第 {i}、{j} 个同时设"
            );
        }
    }
    let zero_limit = pb::GetConversation {
        before_seq: Some(10),
        limit: 0,
        ..base()
    };
    assert_eq!(
        query(&t, head, zero_limit).unwrap_err().code,
        invalid,
        "行级游标 limit 为 0"
    );
    let zero_cursor = pb::GetConversation {
        after_seq: Some(0),
        ..base()
    };
    assert_eq!(
        query(&t, head, zero_cursor).unwrap_err().code,
        invalid,
        "游标为 0"
    );

    let (big, big_head) = world(1100);
    let r = query(
        &big,
        big_head,
        pb::GetConversation {
            after_seq: Some(1),
            limit: 5000,
            ..base()
        },
    )
    .unwrap();
    let n = r.messages.len() + r.tool_calls.len() + r.approvals.len() + r.user_actions.len();
    assert!(
        (999..=1000).contains(&n),
        "limit 按 1000 封顶：选中 {n} 行（回合结束行在 turns 里另计）"
    );
    assert!(r.has_newer);
}
