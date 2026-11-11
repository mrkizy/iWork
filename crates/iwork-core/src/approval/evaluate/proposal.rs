//! 亲手执行提议（T07 方案 5.4；技术设计 7.2a；AC36）。
//!
//! 只为单个简单命令（整条命令文字）或 Edit（补丁）生成；组合命令、解析不了的、整文件 Write 与
//! NotebookEdit 只拒绝（Q6）。卡片内容与被拒调用一致。

use iwork_domain::action::{ActionTarget, CanonicalPath};
use iwork_domain::common::Sha256;
use iwork_domain::user_action::Proposal;
use sha2::Digest;

use crate::approval::action::{CallShape, NormalizedCall};
use crate::approval::registry::PATCH_TOOL;

/// 调用能不能生成卡片、生成什么。是否命中底线、是否在拒绝记忆里由调用方判。
pub fn propose_user_action(call: &NormalizedCall) -> Option<Proposal> {
    match &call.shape {
        CallShape::SingleSimpleCommand { command_text } => Some(Proposal::Command {
            command: command_text.clone(),
            cwd: call.cwd.clone(),
        }),
        CallShape::Tool if call.tool.0 == PATCH_TOOL => {
            let (_, old, new) = call.edit.as_ref()?;
            let path = call.actions.iter().find_map(|a| match &a.target {
                ActionTarget::Path(p) => Some(p.clone()),
                _ => None,
            })?;
            Some(Proposal::Patch {
                path,
                old: old.clone(),
                new: new.clone(),
            })
        }
        _ => None,
    }
}

/// 提议哈希：`sha256("cmd\0" + command + "\0" + cwd)` 或 `sha256("patch\0" + path + "\0" + old + "\0" + new)`。
/// `ExecuteUserAction.proposal_hash` 与它比对（T10 受理）。
pub fn proposal_hash(p: &Proposal) -> Sha256 {
    let mut h = sha2::Sha256::new();
    let path = |c: &CanonicalPath| c.0.to_string_lossy().into_owned();
    match p {
        Proposal::Command { command, cwd } => {
            h.update(b"cmd\0");
            h.update(command.as_bytes());
            h.update(b"\0");
            h.update(path(cwd).as_bytes());
        }
        Proposal::Patch { path: p, old, new } => {
            h.update(b"patch\0");
            h.update(path(p).as_bytes());
            h.update(b"\0");
            h.update(old.as_bytes());
            h.update(b"\0");
            h.update(new.as_bytes());
        }
    }
    Sha256(h.finalize().into())
}
