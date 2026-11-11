//! 判定结果的严格序（T07 方案 5.2）：拒绝 > 亲手执行 > 沙箱外执行 > 询问 > 放行。

use iwork_domain::rule::Verdict;

/// 越大越严。
pub fn strictness(v: &Verdict) -> u8 {
    match v {
        Verdict::Allow => 0,
        Verdict::Ask(_) => 1,
        Verdict::OutsideSandbox(_) => 2,
        Verdict::UserAction(_) => 3,
        Verdict::Deny(_) => 4,
    }
}
