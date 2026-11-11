//! 会话绑定项守卫（T06 方案 2.6 第 8 条）：一条会话线绑定一份会话快照。下一回合的 `SessionSpec` 与打开
//! 当前会话时用的比：模型、系统提示变了就关掉会话、带 `resume` 重开（「下一轮生效」）；别的绑定项变了是调用方
//! 的错（快照变了应当新建会话线与新的判定方），回合失败。目的：审批用的清单和运行时实际加载的清单永远是同一份。

use iwork_ports::agent::SessionSpec;

/// 下一回合的 spec 和当前会话的绑定项比出来的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rebind {
    Same,
    /// 模型或系统提示变了：重开会话（续接同一条运行时会话）
    Reopen,
    /// 这个绑定项变了：回合失败，不开会话
    Mismatch(&'static str),
}

/// `bound` 是打开当前会话时用的 spec。逐字段解构 `next`：`SessionSpec` 加字段时这里编译不过，
/// 逼着加的人决定它属于哪一类。
pub(crate) fn rebind(bound: &SessionSpec, next: &SessionSpec) -> Rebind {
    let SessionSpec {
        cwd,
        model,
        system_prompt,
        mcp,
        constraints,
        tool_set,
        disabled_tools,
        plugin_dirs,
        // 续接 id 由会话线自己填，不是绑定项
        resume: _,
        kind,
    } = next;
    let fixed = [
        ("constraints", *constraints == bound.constraints),
        ("tool_set", *tool_set == bound.tool_set),
        ("disabled_tools", *disabled_tools == bound.disabled_tools),
        ("plugin_dirs", *plugin_dirs == bound.plugin_dirs),
        ("mcp", *mcp == bound.mcp),
        ("cwd", *cwd == bound.cwd),
        ("kind", *kind == bound.kind),
    ];
    if let Some((field, _)) = fixed.iter().find(|(_, same)| !same) {
        return Rebind::Mismatch(field);
    }
    if *model != bound.model || *system_prompt != bound.system_prompt {
        Rebind::Reopen
    } else {
        Rebind::Same
    }
}

/// 回合因绑定项不一致失败时的原因。
pub(crate) fn mismatch_reason(field: &str) -> String {
    format!("会话约束与快照不一致：{field}")
}
