//! 续接说明（T06 方案 5.3）：回合中断后续接时，发给模型的第一段话。纯函数。
//!
//! 说明里不放原始工具输入：判定方不存原始输入全文，模型自己的上文里有它上次发的调用。模型没有原样重发时
//! 一次性授权不命中，照常再问一次（T07 方案 6.2），只会多问。

/// 为什么续接。`summary` 取 `Approval.action.raw_summary`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeReason<'a> {
    /// 中断后，用户批准了那次调用
    Approved { tool: &'a str, summary: &'a str },
    /// 中断后用户拒绝，随后点了「继续」
    Denied { tool: &'a str, summary: &'a str },
    /// 崩溃恢复后用户点了「继续」
    Interrupted,
}

/// 续接说明的文字。
pub fn resume_note(reason: &ResumeReason<'_>) -> String {
    match reason {
        ResumeReason::Approved { tool, summary } => format!(
            "[iWork] 上一轮因为等用户审批暂停了。用户现在已批准你当时发起的那次调用（{tool}：{summary}）。\
             请原样重新发起它，所有参数与上次逐字相同，不要改写、不要合并别的操作。这次批准只对这一次调用有效。"
        ),
        ResumeReason::Denied { tool, summary } => format!(
            "[iWork] 上一轮因为等用户审批暂停了。用户拒绝了你当时发起的那次调用（{tool}：{summary}）。\
             不要换别的方式做同一件事。请继续剩下的工作；如果没有它就完不成，说明原因后停下。"
        ),
        ResumeReason::Interrupted => {
            "[iWork] 上一轮被中断了。请先检查做到了哪一步，再从中断的地方继续，不要重复已经完成的操作。"
                .to_owned()
        }
    }
}
