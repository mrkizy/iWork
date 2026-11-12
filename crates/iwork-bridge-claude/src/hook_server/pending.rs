//! 等待项只回答一次（T06 方案 4.4）。
//!
//! 每个等待中的 hook 请求一项：`Waiting → Answered`。四种触发谁先到谁生效：后台答复（[`Pending::answer`]）、
//! 桥接兜底到时、hook 连接断开（这两样由连接任务调 [`Pending::settle`]）、回合结束（[`Pending::deny_all`]）。
//! 实现是一把锁里的表：从表里取走发送端的那一方写回答，后到的拿不到，只计数。
//!
//! 后台答复怎么翻译成给 hook 的决定，只在 [`translate`] 这一处（4.1 末条）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use iwork_claude_hook::client::Failure;
use iwork_claude_hook::wire::{Decision, Response};
use serde_json::Value;
use tokio::sync::oneshot;

/// 一次 hook 请求在这个桥接里的编号（只在本进程里有意义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CallId(pub u64);

/// 后台对一次权限请求的答复，已经从 ACP 的形状里取出来（ACP 端在 S6-3 接）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendAnswer {
    /// 选了 `allow_once`；`updated_input` 是 `_meta.iwork.updatedInput` 的 JSON 文本
    Allow { updated_input: Option<String> },
    /// 选了 `reject_once`，带回给模型的说明
    Reject { message: String },
    /// `Cancelled`、错误响应、不认识的选项（4.6 第 12 行）
    Unrecognized,
}

/// 后台答复的翻译（一处）：改写后的输入解析不了或不是 JSON 对象 → 拒绝（4.6 第 9 行）；
/// 认不出的答复 → 拒绝（第 12 行）。
pub fn translate(answer: BackendAnswer) -> Response {
    let unrecognized = || Response::deny(Failure::BadResponse.reason());
    match answer {
        BackendAnswer::Allow {
            updated_input: None,
        } => Response::allow(),
        BackendAnswer::Allow {
            updated_input: Some(text),
        } => match serde_json::from_str::<Value>(&text) {
            Ok(input @ Value::Object(_)) => Response {
                updated_input: Some(input),
                ..Response::allow()
            },
            _ => unrecognized(),
        },
        BackendAnswer::Reject { message } => Response::deny(message),
        BackendAnswer::Unrecognized => unrecognized(),
    }
}

/// [`Pending::answer`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// 这条答复就是交给 hook 的那一个
    Delivered,
    /// 已经被别的触发回答过（hook 断了、兜底到时、回合结束），这条丢弃。`was_allow` 为真时后台已经当作交付了
    /// 一次允许而动作没执行：调用方按兜底处理（4.4 第 2 条）
    Late { was_allow: bool },
}

/// 等待中的 hook 请求。
#[derive(Debug, Default)]
pub struct Pending {
    slots: Mutex<HashMap<CallId, oneshot::Sender<Response>>>,
    next: AtomicU64,
    late: AtomicU64,
}

impl Pending {
    /// 登记一项，返回编号与收答复的一端。
    pub fn register(&self) -> (CallId, oneshot::Receiver<Response>) {
        let id = CallId(self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.slots.lock().expect("锁没有中毒").insert(id, tx);
        (id, rx)
    }

    /// 取走发送端：拿到的一方负责回答。拿不到说明已经有人答过（或从没登记过），计一次迟到。
    fn take(&self, id: CallId) -> Option<oneshot::Sender<Response>> {
        let taken = self.slots.lock().expect("锁没有中毒").remove(&id);
        if taken.is_none() {
            self.late.fetch_add(1, Ordering::Relaxed);
        }
        taken
    }

    /// 后台答复到了。
    pub fn answer(&self, id: CallId, answer: BackendAnswer) -> Delivery {
        let response = translate(answer);
        let was_allow = response.decision == Decision::Allow;
        match self.take(id) {
            // 收的一端已经没了（连接任务刚好退出）：hook 拿不到这条答复，和迟到一样处理
            Some(tx) => match tx.send(response) {
                Ok(()) => Delivery::Delivered,
                Err(_) => Delivery::Late { was_allow },
            },
            None => Delivery::Late { was_allow },
        }
    }

    /// 桥接自己的触发（兜底到时、hook 断开）：抢到了就用 `response` 回答，返回是否抢到。
    /// hook 断开时传什么都送不到，传拒绝即可，抢到这件事本身让后到的后台答复变成迟到。
    pub fn settle(&self, id: CallId, response: Response) -> bool {
        match self.take(id) {
            Some(tx) => {
                let _ = tx.send(response);
                true
            }
            None => false,
        }
    }

    /// 回合结束（取消、CLI 退出）：还在等的全部答拒绝，返回答了几个。
    pub fn deny_all(&self, message: &str) -> usize {
        let drained: Vec<_> = self
            .slots
            .lock()
            .expect("锁没有中毒")
            .drain()
            .map(|(_, tx)| tx)
            .collect();
        let n = drained.len();
        for tx in drained {
            let _ = tx.send(Response::deny(message));
        }
        n
    }

    /// 还在等的有几项。
    pub fn waiting(&self) -> usize {
        self.slots.lock().expect("锁没有中毒").len()
    }

    /// 被丢弃的后到触发一共几次（诊断）。
    pub fn late_count(&self) -> u64 {
        self.late.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    fn translation_table() {
        assert_eq!(
            translate(BackendAnswer::Allow {
                updated_input: None
            }),
            Response::allow()
        );
        let rewritten = translate(BackendAnswer::Allow {
            updated_input: Some(r#"{"command":"true"}"#.into()),
        });
        assert_eq!(rewritten.decision, Decision::Allow);
        assert_eq!(
            rewritten.updated_input,
            Some(serde_json::json!({"command":"true"}))
        );
        let bad = Response::deny(Failure::BadResponse.reason());
        for text in ["not json", "[1]", "\"s\"", "null", ""] {
            assert_eq!(
                translate(BackendAnswer::Allow {
                    updated_input: Some(text.into())
                }),
                bad,
                "{text:?}"
            );
        }
        assert_eq!(
            translate(BackendAnswer::Reject {
                message: "不行".into()
            }),
            Response::deny("不行")
        );
        assert_eq!(translate(BackendAnswer::Unrecognized), bad);
    }

    #[test]
    fn answer_then_late_triggers_are_dropped() {
        let p = Pending::default();
        let (id, mut rx) = p.register();
        assert_eq!(
            p.answer(
                id,
                BackendAnswer::Allow {
                    updated_input: None
                }
            ),
            Delivery::Delivered
        );
        assert_eq!(rx.try_recv().unwrap(), Response::allow());
        assert!(!p.settle(id, Response::deny("兜底")));
        assert_eq!(p.deny_all("回合结束"), 0);
        assert_eq!(p.late_count(), 1);
        assert_eq!(p.waiting(), 0);
    }

    /// hook 先断了（或兜底先到），后台的允许才来：迟到，而且报出它是一条允许（调用方按兜底处理）。
    #[test]
    fn allow_after_settle_is_reported_late() {
        let p = Pending::default();
        let (id, _rx) = p.register();
        assert!(p.settle(id, Response::deny("hook 断了")));
        assert_eq!(
            p.answer(
                id,
                BackendAnswer::Allow {
                    updated_input: None
                }
            ),
            Delivery::Late { was_allow: true }
        );
        assert_eq!(
            p.answer(id, BackendAnswer::Unrecognized),
            Delivery::Late { was_allow: false }
        );
    }

    /// 收的一端已经没了：送不到，按迟到报。
    #[test]
    fn answer_to_dropped_receiver_is_late() {
        let p = Pending::default();
        let (id, rx) = p.register();
        drop(rx);
        assert_eq!(
            p.answer(
                id,
                BackendAnswer::Allow {
                    updated_input: None
                }
            ),
            Delivery::Late { was_allow: true }
        );
    }

    #[derive(Debug, Clone, Copy)]
    enum Trigger {
        Answer,
        Backstop,
        HookGone,
        EndTurn,
    }

    fn fire(p: &Pending, id: CallId, t: Trigger) -> bool {
        match t {
            Trigger::Answer => {
                p.answer(
                    id,
                    BackendAnswer::Allow {
                        updated_input: None,
                    },
                ) == Delivery::Delivered
            }
            Trigger::Backstop => p.settle(id, Response::deny("兜底")),
            Trigger::HookGone => p.settle(id, Response::deny("hook 断了")),
            Trigger::EndTurn => p.deny_all("回合结束") == 1,
        }
    }

    /// 4.4：四种触发两两并发，每一轮恰好一方生效，收的一端恰好收到一条，输家都计成迟到。
    #[test]
    fn concurrent_triggers_answer_exactly_once() {
        use Trigger::*;
        let all = [Answer, Backstop, HookGone, EndTurn];
        for a in all {
            for b in all {
                for _ in 0..1000 {
                    let p = Arc::new(Pending::default());
                    let (id, mut rx) = p.register();
                    let gate = Arc::new(Barrier::new(2));
                    let racer = |t: Trigger| {
                        let (p, gate) = (Arc::clone(&p), Arc::clone(&gate));
                        std::thread::spawn(move || {
                            gate.wait();
                            fire(&p, id, t)
                        })
                    };
                    let (ha, hb) = (racer(a), racer(b));
                    let won = [ha.join().unwrap(), hb.join().unwrap()];
                    assert_eq!(won.iter().filter(|w| **w).count(), 1, "{a:?} × {b:?}");
                    assert!(rx.try_recv().is_ok(), "{a:?} × {b:?}：收到一条");
                    assert_eq!(p.waiting(), 0);
                    // EndTurn 输了时 deny_all 什么都没取到，不算迟到（它不针对某一项）
                    let losers_counted = [a, b]
                        .iter()
                        .zip(won)
                        .filter(|(t, w)| !w && !matches!(t, EndTurn))
                        .count() as u64;
                    assert_eq!(p.late_count(), losers_counted, "{a:?} × {b:?}");
                }
            }
        }
    }
}
