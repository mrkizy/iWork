//! 状态机的公共部件：可穷举枚举、表外转换错误、按表查下一状态。
//!
//! 每个状态机写成「const 转换表 + 纯函数 `next()`」：加一条转换 = 表里加一行，
//! 表外的 `(状态, 触发)` 一律返回 [`IllegalTransition`] 并点名，不在 `match` 里散落规则。

use core::fmt;

/// 取值有限、能全部列出来的类型。状态机测试靠它做「状态 × 触发」穷举。
pub trait Finite: Sized {
    /// 全部取值，按声明顺序；带载荷的变体按载荷的全部取值展开。
    fn all() -> Vec<Self>;
}

/// 定义一个可穷举的枚举，并从**同一份**变体列表生成 [`Finite::all`]。
///
/// 为什么用宏而不手写 `ALL` 常量：手写的清单和枚举定义是两份，新增变体忘了加进清单时
/// 穷举测试会悄悄少测一块。宏让两者出自同一组记号，加变体就自动进 `all()`。
///
/// 变体只能是无字段，或者只有一个字段的结构变体，且字段类型也实现 [`Finite`]
/// （例如 `Interrupted { reason: InterruptReason }` 会按 `InterruptReason::all()` 展开）。
macro_rules! finite_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $variant:ident $({ $field:ident : $fty:ty })?
            ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis enum $name {
            $(
                $(#[$vmeta])*
                $variant $({ $field: $fty })?
            ),*
        }

        impl $crate::machine::Finite for $name {
            fn all() -> Vec<Self> {
                let mut all = Vec::new();
                $( finite_enum!(@push all, $name, $variant $(, $field, $fty)?); )*
                all
            }
        }
    };
    (@push $all:ident, $name:ident, $variant:ident) => {
        $all.push($name::$variant);
    };
    (@push $all:ident, $name:ident, $variant:ident, $field:ident, $fty:ty) => {
        for value in <$fty as $crate::machine::Finite>::all() {
            $all.push($name::$variant { $field: value });
        }
    };
}
pub(crate) use finite_enum;

/// 转换表里的一行：`(当前状态, 触发, 下一状态)`。
pub type Transition<S, T> = (S, T, S);

/// 表外转换：当前状态不接受这个触发。这是程序缺陷，不是预料之中的乱序结果。
///
/// `machine` 点名是哪个状态机，`state`、`trigger` 是出事时的输入，日志里一眼能看出该补哪一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IllegalTransition<S, T> {
    pub machine: &'static str,
    pub state: S,
    pub trigger: T,
}

impl<S: fmt::Debug, T: fmt::Debug> fmt::Display for IllegalTransition<S, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} 状态机：状态 {:?} 不接受触发 {:?}",
            self.machine, self.state, self.trigger
        )
    }
}

impl<S: fmt::Debug, T: fmt::Debug> std::error::Error for IllegalTransition<S, T> {}

/// 在转换表里查 `(state, trigger)`；查不到返回点名的 [`IllegalTransition`]。
///
/// 各状态机的 `next()` 都只调它，保证「表就是唯一真值」。
pub fn lookup<S: Copy + PartialEq, T: Copy + PartialEq>(
    machine: &'static str,
    table: &[Transition<S, T>],
    state: S,
    trigger: T,
) -> Result<S, IllegalTransition<S, T>> {
    table
        .iter()
        .find(|(from, on, _)| *from == state && *on == trigger)
        .map(|&(_, _, to)| to)
        .ok_or(IllegalTransition {
            machine,
            state,
            trigger,
        })
}
