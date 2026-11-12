//! 对账（T06 方案 4.7）：**一个没标错误的工具结果，它的调用一定在 hook 上得到过「允许」**。不成立 → 结束 CLI、
//! 回 `hook_inactive`。
//!
//! 它是一条断言，不是保证：没过 hook 的调用如果先有了副作用再返回错误，结果标了错误，这里看不见（被拒绝、
//! 输入校验失败的结果同样标错误，从输出里分不开）。所以它不出现在任何安全论证里，也没有哪条路以它为条件放行。
//! 按 `tool_use_id` 对，不看工具名（子代理工具在不同地方叫 `Task` 或 `Agent`，按名字认会对不上）。纯函数。

use std::collections::HashSet;

/// 断言不成立：这个调用没得到过允许，结果却没标错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unapproved {
    pub tool_use_id: String,
}

#[derive(Debug, Default)]
pub struct Reconciler {
    allowed: HashSet<String>,
}

impl Reconciler {
    /// hook 上这个调用拿到了允许（答复已经交给 hook）。
    pub fn on_allowed(&mut self, tool_use_id: &str) {
        self.allowed.insert(tool_use_id.to_owned());
    }

    /// CLI 输出了这个调用的结果。
    pub fn on_result(&self, tool_use_id: &str, failed: bool) -> Result<(), Unapproved> {
        if failed || self.allowed.contains(tool_use_id) {
            return Ok(());
        }
        Err(Unapproved {
            tool_use_id: tool_use_id.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_needs_an_allow_errors_do_not() {
        let mut r = Reconciler::default();
        assert_eq!(
            r.on_result("t1", false),
            Err(Unapproved {
                tool_use_id: "t1".into()
            })
        );
        assert_eq!(r.on_result("t1", true), Ok(()), "标了错误的不看");
        r.on_allowed("t1");
        assert_eq!(r.on_result("t1", false), Ok(()));
        assert!(r.on_result("t2", false).is_err(), "按编号，不串");
    }
}
