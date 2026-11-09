//! 状态机测试的公共检查（方案 4.3「测试怎么写」第 1、2、5 类）。
//!
//! 每个集成测试文件各自 `mod support;`，用不到的函数在那个编译单元里是死代码，所以整体放开 dead_code。
#![allow(dead_code)]

use std::collections::{HashSet, VecDeque};
use std::fmt::Debug;
use std::hash::Hash;

use iwork_domain::machine::{Finite, IllegalTransition, Transition};

/// 状态机函数的形状：`next(状态, 触发)`。
pub type NextFn<S, T> = fn(S, T) -> Result<S, IllegalTransition<S, T>>;

/// 一处不符：输入、期望、实际。
#[derive(Debug, PartialEq)]
pub struct Mismatch<S, T> {
    pub state: S,
    pub trigger: T,
    pub expected: Result<S, IllegalTransition<S, T>>,
    pub actual: Result<S, IllegalTransition<S, T>>,
}

/// 第 1 类：表里每一行 `(s, t, s')` 都满足 `next(s, t) == Ok(s')`。
pub fn check_forward<S, T>(next: NextFn<S, T>, table: &[Transition<S, T>]) -> Vec<Mismatch<S, T>>
where
    S: Copy + PartialEq,
    T: Copy + PartialEq,
{
    table
        .iter()
        .filter_map(|&(state, trigger, to)| {
            let actual = next(state, trigger);
            (actual != Ok(to)).then_some(Mismatch {
                state,
                trigger,
                expected: Ok(to),
                actual,
            })
        })
        .collect()
}

/// 第 2 类：`状态 × 触发` 全部组合里，不在表里的都返回点名的 `IllegalTransition`。
pub fn check_reverse<S, T>(
    machine: &'static str,
    next: NextFn<S, T>,
    table: &[Transition<S, T>],
) -> Vec<Mismatch<S, T>>
where
    S: Finite + Copy + PartialEq,
    T: Finite + Copy + PartialEq,
{
    let mut mismatches = Vec::new();
    for state in S::all() {
        for trigger in T::all() {
            if table.iter().any(|&(s, t, _)| s == state && t == trigger) {
                continue;
            }
            let expected = Err(IllegalTransition {
                machine,
                state,
                trigger,
            });
            let actual = next(state, trigger);
            if actual != expected {
                mismatches.push(Mismatch {
                    state,
                    trigger,
                    expected,
                    actual,
                });
            }
        }
    }
    mismatches
}

/// 表里同一个 `(状态, 触发)` 出现两次时返回这些键：确定性被破坏，`next` 只会用到第一行。
pub fn duplicate_keys<S, T>(table: &[Transition<S, T>]) -> Vec<(S, T)>
where
    S: Copy + Eq + Hash,
    T: Copy + Eq + Hash,
{
    let mut seen = HashSet::new();
    table
        .iter()
        .filter(|&&(s, t, _)| !seen.insert((s, t)))
        .map(|&(s, t, _)| (s, t))
        .collect()
}

/// 第 5 类的被测对象：把正向、反向与表的确定性包成一个判断。
///
/// 两个桩（`always_illegal`、`always_identity`）都必须让它返回 `Err`，否则这个函数本身失效。
pub fn verify_machine<S, T>(
    machine: &'static str,
    next: NextFn<S, T>,
    table: &[Transition<S, T>],
) -> Result<(), Vec<Mismatch<S, T>>>
where
    S: Finite + Copy + Eq + Hash + Debug,
    T: Finite + Copy + Eq + Hash + Debug,
{
    let duplicates = duplicate_keys(table);
    assert!(
        duplicates.is_empty(),
        "{machine} 表有重复的键：{duplicates:?}"
    );
    let mut mismatches = check_forward(next, table);
    mismatches.extend(check_reverse(machine, next, table));
    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(mismatches)
    }
}

/// 从 `start` 沿表能走到的全部状态（含自身）。
pub fn reachable<S, T>(table: &[Transition<S, T>], start: S) -> Vec<S>
where
    S: Copy + PartialEq,
{
    let mut seen = vec![start];
    let mut queue = VecDeque::from([start]);
    while let Some(state) = queue.pop_front() {
        for &(from, _, to) in table {
            if from == state && !seen.contains(&to) {
                seen.push(to);
                queue.push_back(to);
            }
        }
    }
    seen
}
