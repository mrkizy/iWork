//! 命令改写通道（T06 方案第 11 节）：把一条 Bash 命令包进沙箱执行器。M1 只有不改写的实现（O2 已定 M1 不做执行器）。
//!
//! 它存在只是因为任务卡要求留接口：桥接在把「允许」交给 hook 之前调它（`acp.rs`），返回 `Some` 时用新命令
//! 作为改写后的输入。M2 接入统一沙箱时，改写该在核心做还是在适配器做要重新看，到时由那张卡定。

use std::path::Path;

pub trait CommandRewriter: Send + Sync {
    /// 返回 `None` 表示不改写。
    fn rewrite(&self, command: &str, workspace: &Path) -> Option<String>;
}

/// M1 的实现：从不改写。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRewrite;

impl CommandRewriter for NoRewrite {
    fn rewrite(&self, _command: &str, _workspace: &Path) -> Option<String> {
        None
    }
}
