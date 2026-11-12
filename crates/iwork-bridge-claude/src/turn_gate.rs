//! 一条会话里「这一轮」的状态：接不接 prompt、取消做哪几步、SIGTERM 什么时候发、hook 来问时转不转给后台
//! （T06 方案 5.1、4.1、4.6 第 11 行）。纯，不碰时钟与进程：时刻由调用方传进来，要做的事返回给调用方去做。
//!
//! 原来这几样散在会话任务里（正在跑的 prompt、是否在取消、SIGTERM 的期限各一个字段），每个出口都要记得一起清；
//! 收在这一处之后，状态只有下面四种，转换只有这几个方法，属性测试（`tests` 里）对任意交错检查全部规则。
//!
//! | 状态 | prompt 到 | 取消 | 这一轮结束 | SIGTERM 到点 | hook 来问 | 致命 |
//! | --- | --- | --- | --- | --- | --- | --- |
//! | `Idle` | → `Running` | 不做事 | 不做事 | 不做事 | 拒（没有回合） | → `Ended` |
//! | `Running` | 拒（忙） | → `Cancelling`：拒掉在等的 hook、发 interrupt、定 SIGTERM 期限 | → `Idle`，回 prompt | 不做事 | 转给后台 | → `Ended`，回 prompt 错误 |
//! | `Cancelling` | 拒（忙） | 不做事（只做一次） | → `Idle`，回 prompt，期限作废 | 发 SIGTERM（只发一次） | 拒（回合已结束） | → `Ended`，回 prompt 错误 |
//! | `Ended` | 拒（已结束） | 不做事 | 不做事 | 不做事 | 拒（没有回合） | 不做事 |

use std::fmt::Debug;

/// 状态。`D` 是正在跑的那一轮的回复通道（prompt 的响应者），`T` 是时刻。
#[derive(Debug)]
enum Phase<D, T> {
    Idle,
    Running { done: D },
    Cancelling { done: D, term_at: Option<T> },
    Ended,
}

/// prompt 为什么没接。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// 上一轮还没结束
    Busy,
    /// 会话已经结束
    Ended,
}

/// 取消要做的事（5.1 第 1、2 步）：拒掉还在等的 hook、发 `interrupt`、在 `term_at` 给 CLI 发 SIGTERM。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelPlan<T> {
    pub term_at: T,
}

/// hook 来问时怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookVerdict {
    /// 转给后台
    Forward,
    /// 没有在跑的回合
    DenyNoTurn,
    /// 这一轮已经在取消
    DenyTurnGone,
}

#[derive(Debug)]
pub struct TurnGate<D, T> {
    phase: Phase<D, T>,
}

impl<D, T> Default for TurnGate<D, T> {
    fn default() -> Self {
        Self { phase: Phase::Idle }
    }
}

impl<D, T: Copy + Debug> TurnGate<D, T> {
    /// prompt 到了：接下就记住它的回复通道；不接就把通道原样还回去，由调用方回错误。
    pub fn begin(&mut self, done: D) -> Result<(), (D, Refused)> {
        match self.phase {
            Phase::Idle => {
                self.phase = Phase::Running { done };
                Ok(())
            }
            Phase::Running { .. } | Phase::Cancelling { .. } => Err((done, Refused::Busy)),
            Phase::Ended => Err((done, Refused::Ended)),
        }
    }

    /// `session/cancel`：在跑的回合第一次取消时返回要做的事；别的时候什么都不做。
    pub fn cancel(&mut self, term_at: T) -> Option<CancelPlan<T>> {
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Running { done } => {
                self.phase = Phase::Cancelling {
                    done,
                    term_at: Some(term_at),
                };
                Some(CancelPlan { term_at })
            }
            other => {
                self.phase = other;
                None
            }
        }
    }

    /// 这一轮结束了（`result` 到了、或映射给出了结局）：交回回复通道，SIGTERM 的期限随之作废。
    pub fn finish(&mut self) -> Option<D> {
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Running { done } | Phase::Cancelling { done, .. } => Some(done),
            other => {
                self.phase = other;
                None
            }
        }
    }

    /// SIGTERM 的期限（会话循环拿它定计时器）；没有就是 `None`。
    pub fn term_at(&self) -> Option<T> {
        match &self.phase {
            Phase::Cancelling { term_at, .. } => *term_at,
            _ => None,
        }
    }

    /// 期限到了：取消中、这一轮还没结束时返回真（发 SIGTERM），只返回一次。
    pub fn term_due(&mut self) -> bool {
        match &mut self.phase {
            Phase::Cancelling { term_at, .. } => term_at.take().is_some(),
            _ => false,
        }
    }

    /// hook 来问。
    pub fn hook(&self) -> HookVerdict {
        match self.phase {
            Phase::Running { .. } => HookVerdict::Forward,
            Phase::Cancelling { .. } => HookVerdict::DenyTurnGone,
            Phase::Idle | Phase::Ended => HookVerdict::DenyNoTurn,
        }
    }

    /// 有没有在跑（含取消中）的回合。
    pub fn in_turn(&self) -> bool {
        matches!(self.phase, Phase::Running { .. } | Phase::Cancelling { .. })
    }

    /// 致命错误或会话结束：之后一切都拒；正在跑的那一轮的回复通道交回，由调用方回错误。
    pub fn end(&mut self) -> Option<D> {
        match std::mem::replace(&mut self.phase, Phase::Ended) {
            Phase::Running { done } | Phase::Cancelling { done, .. } => Some(done),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// 会发生的事：任意混合。
    #[derive(Debug, Clone, Copy)]
    enum Op {
        Prompt,
        Cancel,
        Finish,
        TermDue,
        Hook,
        End,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            Just(Op::Prompt),
            Just(Op::Cancel),
            Just(Op::Finish),
            Just(Op::TermDue),
            Just(Op::Hook),
            Just(Op::End),
        ]
    }

    /// 从已经发生的事推出来的「应该是什么」：和被测的实现分开写，只看历史。
    #[derive(Default)]
    struct Oracle {
        /// 接下的、还没回的 prompt
        open: Option<u32>,
        /// 这一轮取消过
        cancelled: bool,
        /// 这一轮发过 SIGTERM
        termed: bool,
        ended: bool,
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        /// 任意交错下的规则（5.1、4.1、4.6 第 11 行）：
        /// R1 每个 prompt 恰好回一次（接下的由结束或致命回，不接的当场回）；同时最多一个在跑。
        /// R2 hook 转给后台，当且仅当有在跑、没取消过的回合、会话没结束。取消后拒的原因是「回合已结束」。
        /// R3 取消只在这一轮第一次时做事（拒 hook、发 interrupt、定期限各一次）。
        /// R4 SIGTERM 只在取消过、这一轮还没结束时发，每次取消最多一次；这一轮结束后期限作废。
        /// R5 会话结束之后：prompt 一律以「已结束」拒，hook 一律拒，不再发 SIGTERM。
        #[test]
        fn any_interleaving_keeps_the_rules(ops in prop::collection::vec(op(), 0..60)) {
            let mut gate: TurnGate<u32, u32> = TurnGate::default();
            let mut o = Oracle::default();
            let mut answered = std::collections::BTreeMap::<u32, u32>::new();
            let mut next = 0u32;
            for (step, op) in ops.into_iter().enumerate() {
                let step = step as u32;
                match op {
                    Op::Prompt => {
                        let id = next;
                        next += 1;
                        match gate.begin(id) {
                            Ok(()) => {
                                prop_assert!(o.open.is_none() && !o.ended, "R1 接下时不能已有在跑的");
                                o.open = Some(id);
                                o.cancelled = false;
                                o.termed = false;
                            }
                            Err((back, why)) => {
                                prop_assert_eq!(back, id);
                                let want = if o.ended { Refused::Ended } else { Refused::Busy };
                                prop_assert!(o.ended || o.open.is_some(), "R1 空闲时不该拒");
                                prop_assert_eq!(why, want, "R5");
                                *answered.entry(id).or_default() += 1;
                            }
                        }
                    }
                    Op::Cancel => {
                        let plan = gate.cancel(step);
                        let first = o.open.is_some() && !o.cancelled && !o.ended;
                        prop_assert_eq!(plan.is_some(), first, "R3");
                        if first {
                            prop_assert_eq!(plan.unwrap().term_at, step);
                            o.cancelled = true;
                        }
                    }
                    Op::Finish => {
                        let done = gate.finish();
                        prop_assert_eq!(done, o.open.filter(|_| !o.ended), "R1");
                        if let Some(id) = done {
                            *answered.entry(id).or_default() += 1;
                            o.open = None;
                        }
                        prop_assert_eq!(gate.term_at(), None, "R4 结束后期限作废");
                    }
                    Op::TermDue => {
                        let fire = gate.term_due();
                        let want = o.open.is_some() && o.cancelled && !o.termed && !o.ended;
                        prop_assert_eq!(fire, want, "R4");
                        if fire {
                            o.termed = true;
                        }
                    }
                    Op::Hook => {
                        let want = if o.ended || o.open.is_none() {
                            HookVerdict::DenyNoTurn
                        } else if o.cancelled {
                            HookVerdict::DenyTurnGone
                        } else {
                            HookVerdict::Forward
                        };
                        prop_assert_eq!(gate.hook(), want, "R2");
                    }
                    Op::End => {
                        let done = gate.end();
                        prop_assert_eq!(done, o.open.filter(|_| !o.ended), "R1");
                        if let Some(id) = done {
                            *answered.entry(id).or_default() += 1;
                        }
                        o.open = None;
                        o.ended = true;
                    }
                }
                // 每一步之后：期限只在「取消过、还没发 SIGTERM、这一轮没结束」时存在
                let armed = o.open.is_some() && o.cancelled && !o.termed && !o.ended;
                prop_assert_eq!(gate.term_at().is_some(), armed, "R4 期限与状态一致");
                prop_assert_eq!(gate.in_turn(), o.open.is_some() && !o.ended);
            }
            // R1：回过的都恰好一次；还在跑的那一个（如果有）一次都没回
            for (id, n) in &answered {
                prop_assert_eq!(*n, 1, "prompt {} 回了 {} 次", id, n);
            }
            if let Some(id) = o.open.filter(|_| !o.ended) {
                prop_assert!(!answered.contains_key(&id));
            }
        }
    }
}
