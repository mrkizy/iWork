//! 视图查询的耗时（T10a 方案第 11 节步 7 验收「行级游标与锚点查询的耗时」；T09 第 3 节要点 ④）。
//!
//! 一个对话里放大量的行：`IWORK_BENCH_VIEW_TURNS` 个回合（默认 200），每个回合一问一答、`IWORK_BENCH_VIEW_TOOLS`
//! 次工具调用（默认 50，即一万次工具调用）、每 10 个回合一次审批；另一个对话放同样多的行，验证取页不扫别的对话。
//! 量（各 `IWORK_BENCH_VIEW_REPS` 次，默认 200，报 P50 / P95，微秒）：
//! - `row_before_mid`：行级游标从对话中间往前取 200 行；
//! - `row_after_mid`：从中间往后取 200 行；
//! - `latest_page`：最新一页（50 条消息，连同范围内的工具调用）；
//! - `around_mid`：以中间为中心取 50 条消息；
//! - `anchors_latest` / `anchors_mid`：锚点窗口前后各 60 条。
//!
//! 每组一行 JSON 到 stdout。新查询，没有「之前」可比，这次的数作基线。
//!
//! 运行：`cargo bench -p iwork-store --bench view_queries`

#[path = "../tests/support/fixtures.rs"]
mod fixtures;

use std::time::Instant;

use fixtures::*;
use iwork_domain::approval::{ApprovalState, Decider};
use iwork_domain::event::{
    ApprovalDecided, ApprovalRequested, AssistantCreated, ConversationCreated, DomainEvent,
    MessageAppended, MessageCompleted, ProjectAdded, ToolCallFinished, ToolCallId, ToolCallStarted,
    TurnFinished, TurnQueued, TurnStateChanged,
};
use iwork_domain::id::Seq;
use iwork_domain::message::{MessageBody, MessageStatus};
use iwork_domain::turn::TurnState;
use iwork_ports::read_model::{AnchorWindow, ConversationPaging};
use iwork_ports::store::StoragePort;

fn env_or(key: &str, default: u128) -> u128 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn change(turn: u128, conv: u128, from: TurnState, to: TurnState) -> DomainEvent {
    TurnStateChanged {
        conversation_id: Some(cid(conv)),
        turn_id: tid(turn),
        from,
        to,
        reason: None,
    }
    .into()
}

/// 一个回合的全部事件：一问、排队、开跑、工具调用、（每 10 个一次审批）、一答、结束。
fn one_turn(conv: u128, n: u128, tools: u128) -> Vec<Vec<DomainEvent>> {
    let t = 1_000_000 * conv + n;
    let mut commits = vec![
        vec![
            MessageAppended {
                message: user_message(t * 10 + 1, conv, &format!("第 {n} 个问题")),
            }
            .into(),
            TurnQueued {
                turn: turn(t, conv),
            }
            .into(),
        ],
        vec![change(t, conv, TurnState::Queued, TurnState::Running)],
    ];
    for k in 0..tools {
        let call = ToolCallId(format!("c{k}"));
        commits.push(vec![
            ToolCallStarted {
                conversation_id: Some(cid(conv)),
                turn_id: tid(t),
                tool_call_id: call.clone(),
                tool_name: "Bash".into(),
                action: approval(1, t, conv).action,
            }
            .into(),
        ]);
        commits.push(vec![
            ToolCallFinished {
                conversation_id: Some(cid(conv)),
                turn_id: tid(t),
                tool_call_id: call,
                ok: true,
                output_artifact: None,
            }
            .into(),
        ]);
    }
    if n % 10 == 0 {
        commits.push(vec![
            ApprovalRequested {
                approval: approval(t, t, conv),
            }
            .into(),
            change(t, conv, TurnState::Running, TurnState::WaitingApproval),
        ]);
        commits.push(vec![
            ApprovalDecided {
                conversation_id: Some(cid(conv)),
                approval_id: apid(t),
                state: ApprovalState::Approved {
                    decider: Decider::User,
                },
                remember: None,
            }
            .into(),
            change(t, conv, TurnState::WaitingApproval, TurnState::Running),
        ]);
    }
    let reply = t * 10 + 2;
    commits.push(vec![
        MessageAppended {
            message: assistant_message(reply, conv, t),
        }
        .into(),
        MessageCompleted {
            conversation_id: cid(conv),
            message_id: mid(reply),
            body: MessageBody::Inline(format!("第 {n} 个回答")),
            status: MessageStatus::Completed,
        }
        .into(),
    ]);
    let end = if n % 7 == 0 {
        TurnState::Failed
    } else {
        TurnState::Completed
    };
    commits.push(vec![
        change(t, conv, TurnState::Running, end),
        TurnFinished {
            conversation_id: Some(cid(conv)),
            turn_id: tid(t),
            state: end,
            usage: Default::default(),
            celebrate: false,
            change_summary: None,
        }
        .into(),
    ]);
    commits
}

fn pct(v: &mut [u128], p: f64) -> u128 {
    v.sort_unstable();
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn main() {
    let turns = env_or("IWORK_BENCH_VIEW_TURNS", 200);
    let tools = env_or("IWORK_BENCH_VIEW_TOOLS", 50);
    let reps = env_or("IWORK_BENCH_VIEW_REPS", 200) as usize;
    let env = Env::new();
    let store = env.open();
    let mut n = 0u128;
    let mut commit = |events: Vec<DomainEvent>| {
        n += 1;
        block(store.commit(cmd(n, events))).expect("提交");
    };
    commit(vec![
        AssistantCreated {
            assistant: assistant(1, false),
        }
        .into(),
        ProjectAdded {
            project: project(1),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(10, 1),
        }
        .into(),
        ConversationCreated {
            conversation: project_conversation(11, 1),
        }
        .into(),
    ]);
    // 两个对话交替写，对话 10 的行在全局 seq 上是稀疏的
    for k in 1..=turns {
        for conv in [10, 11] {
            for events in one_turn(conv, k, tools) {
                commit(events);
            }
        }
    }
    let head = block(store.head_seq()).unwrap().0;
    let mid_seq = Seq(head / 2);
    let read = read_model(&store);
    let c = cid(10);
    let cases: Vec<(&str, Box<dyn Fn() -> usize>)> = vec![
        (
            "row_before_mid",
            Box::new(|| {
                block(read.view(move |tx| {
                    let p = tx
                        .conversation_page(
                            c,
                            ConversationPaging::BeforeRow {
                                seq: mid_seq,
                                limit: 200,
                            },
                        )?
                        .expect("对话在");
                    Ok(p.tool_calls.len() + p.messages.len())
                }))
                .unwrap()
            }),
        ),
        (
            "row_after_mid",
            Box::new(|| {
                block(read.view(move |tx| {
                    let p = tx
                        .conversation_page(
                            c,
                            ConversationPaging::AfterRow {
                                seq: mid_seq,
                                limit: 200,
                            },
                        )?
                        .expect("对话在");
                    Ok(p.tool_calls.len() + p.messages.len())
                }))
                .unwrap()
            }),
        ),
        (
            "latest_page",
            Box::new(|| {
                block(read.view(move |tx| {
                    let p = tx
                        .conversation_page(c, ConversationPaging::Latest { limit: 50 })?
                        .expect("对话在");
                    Ok(p.tool_calls.len() + p.messages.len())
                }))
                .unwrap()
            }),
        ),
        (
            "around_mid",
            Box::new(|| {
                block(read.view(move |tx| {
                    let p = tx
                        .conversation_page(
                            c,
                            ConversationPaging::Around {
                                seq: mid_seq,
                                limit: 50,
                            },
                        )?
                        .expect("对话在");
                    Ok(p.tool_calls.len() + p.messages.len())
                }))
                .unwrap()
            }),
        ),
        (
            "anchors_latest",
            Box::new(|| {
                block(read.view(move |tx| {
                    let a = tx
                        .anchors(
                            c,
                            AnchorWindow {
                                around: None,
                                before: 60,
                                after: 60,
                            },
                        )?
                        .expect("对话在");
                    Ok(a.anchors.len())
                }))
                .unwrap()
            }),
        ),
        (
            "anchors_mid",
            Box::new(|| {
                block(read.view(move |tx| {
                    let a = tx
                        .anchors(
                            c,
                            AnchorWindow {
                                around: Some(mid_seq),
                                before: 60,
                                after: 60,
                            },
                        )?
                        .expect("对话在");
                    Ok(a.anchors.len())
                }))
                .unwrap()
            }),
        ),
    ];
    for (name, f) in cases {
        let rows = f();
        let mut us: Vec<u128> = (0..reps)
            .map(|_| {
                let t = Instant::now();
                std::hint::black_box(f());
                t.elapsed().as_micros()
            })
            .collect();
        println!(
            "{{\"bench\":\"view_queries\",\"case\":\"{name}\",\"turns\":{turns},\"tools_per_turn\":{tools},\"head\":{head},\"rows\":{rows},\"reps\":{reps},\"p50_us\":{},\"p95_us\":{}}}",
            pct(&mut us, 0.5),
            pct(&mut us, 0.95)
        );
    }
}
