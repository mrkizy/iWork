//! 界面文案表：会随数据变的文字只在这里写一次（方案 R6）。网页只留固定按钮字样。

use std::time::{Duration, SystemTime};

use iwork_proto::pb;

pub const NEW_CONVERSATION: &str = "新对话";
pub const USER_NAME: &str = "你";
pub const SYSTEM_NAME: &str = "iWork";
pub const QUOTA_MISSING: &str = "—";
pub const QUOTA_MISSING_TIP: &str = "暂时取不到额度";
pub const PATH_MISSING: &str = "找不到路径";
pub const RUNNING_ACTIVITY: &str = "工作中";
pub const PENDING_EMPTY: &str = "没有需要你处理的事";
pub const RULES_EMPTY: &str = "还没有记住的规则";
pub const IMPORT_EMPTY: &str = "没有可导入的项目";
pub const LIST_EMPTY: &str = "还没有对话";
pub const ACTIVITY_EMPTY: &str = "M1 暂不记录助手动态";
pub const MEMORY_EMPTY: &str = "M1 暂不记录助手记忆";
pub const INCOMPLETE: &str = "数据不完整，无法显示这个对话";
pub const RECONNECTING: &str = "正在重新连接";
pub const CONNECTING: &str = "正在连接后台服务";
pub const OVERLOADED: &str = "后台负载过高，稍后重试或重新打开 iWork";
pub const DIAGNOSTICS_TITLE: &str = "连不上后台服务";
pub const NEWER_SCHEMA_HINT: &str = "装回更新的那一版可继续使用";
pub const UNKNOWN_RESULT: &str = "执行结果未知，请自行核对";
pub const UNKNOWN_BLOCKS_DELETE: &str = "有一张卡片的执行结果无法核对，请确认后继续删除";
pub const OUTSIDE_SANDBOX_NOTE: &str = "将在沙箱外运行，命令带起的脚本也不受沙箱约束";
pub const OPAQUE_SCRIPT_NOTE: &str = "审批看不到命令带起的脚本内部";

pub fn list_filter_empty(filter: &str) -> String {
    format!("没有标题包含“{filter}”的对话")
}

pub fn dot_label(dot: pb::DotState) -> &'static str {
    match dot {
        pb::DotState::Waiting => "等你处理",
        pb::DotState::Error => "出错",
        pb::DotState::Running => "运行中",
        pb::DotState::Idle | pb::DotState::Unspecified => "空闲",
    }
}

pub fn approval_mode(mode: pb::ApprovalMode) -> &'static str {
    match mode {
        pb::ApprovalMode::Manual | pb::ApprovalMode::Unspecified => "手动",
        pb::ApprovalMode::Auto => "自动",
        pb::ApprovalMode::FullAccess => "全开",
    }
}

pub fn remember_scope(scope: pb::RememberScope) -> &'static str {
    match scope {
        pb::RememberScope::Once | pb::RememberScope::Unspecified => "仅这一次",
        pb::RememberScope::Conversation => "本对话",
        pb::RememberScope::Project => "本项目",
        pb::RememberScope::Forever => "永久",
    }
}

pub fn void_reason(r: pb::VoidReason) -> &'static str {
    match r {
        pb::VoidReason::TurnCancelled => "回合已取消",
        pb::VoidReason::ConversationDeleted => "对话已删除",
        pb::VoidReason::TurnAbandonedAfterInterrupt => "中断后已放弃",
        pb::VoidReason::AssistantDeleted => "助手已删除",
        pb::VoidReason::TurnFailed => "回合失败",
        pb::VoidReason::Unspecified => "已失效",
    }
}

pub fn review_line(review: pb::ReviewState, reason: Option<&str>) -> String {
    let reason = reason.unwrap_or_default();
    match review {
        pb::ReviewState::InProgress => "AI 审查中…".into(),
        pb::ReviewState::Passed => "AI 审查通过".into(),
        pb::ReviewState::Failed => format!("AI 审查未通过：{reason}"),
        pb::ReviewState::Errored => "AI 审查出错，转交给你".into(),
        pb::ReviewState::TimedOut => "AI 审查超时，转交给你".into(),
        pb::ReviewState::Abandoned => "已切回手动，转交给你".into(),
        pb::ReviewState::NotReviewed | pb::ReviewState::Unspecified => String::new(),
    }
}

pub fn approval_state(a: &pb::Approval) -> String {
    match pb::ApprovalState::try_from(a.state) {
        Ok(pb::ApprovalState::Approved) => match a
            .decider
            .and_then(|d| pb::ApprovalDecider::try_from(d).ok())
        {
            Some(pb::ApprovalDecider::ModeSwitch) => "切换档位后已放行".into(),
            Some(pb::ApprovalDecider::AiReview) => "AI 审查已放行".into(),
            _ => "已允许".into(),
        },
        Ok(pb::ApprovalState::Denied) => "已拒绝".into(),
        Ok(pb::ApprovalState::Voided) => format!(
            "已作废：{}",
            void_reason(
                a.void_reason
                    .and_then(|r| pb::VoidReason::try_from(r).ok())
                    .unwrap_or_default()
            )
        ),
        _ => String::new(),
    }
}

pub fn user_action_state(c: &pb::UserActionCard) -> String {
    use pb::UserActionState as S;
    match S::try_from(c.state).unwrap_or_default() {
        S::Pending | S::Unspecified => String::new(),
        S::Starting => "正在启动".into(),
        S::Executing => "执行中".into(),
        S::Stopping => "正在停止".into(),
        S::Done => "你已执行".into(),
        S::Failed => format!("执行失败：{}", c.error.as_deref().unwrap_or_default()),
        S::Unknown => "执行结果未知".into(),
        S::Ended => "你已核对".into(),
        S::Dismissed => "已忽略".into(),
        S::Voided => format!(
            "已作废：{}",
            void_reason(
                c.void_reason
                    .and_then(|r| pb::VoidReason::try_from(r).ok())
                    .unwrap_or_default()
            )
        ),
    }
}

pub fn exit_label(exit: &pb::ExitStatus) -> Option<String> {
    match exit.kind? {
        pb::exit_status::Kind::Code(c) => Some(format!("退出码 {c}")),
        pb::exit_status::Kind::Signal(s) => Some(format!("被信号 {s} 结束")),
    }
}

pub fn interrupt_reason(r: pb::InterruptReason) -> &'static str {
    match r {
        pb::InterruptReason::Crash => "后台服务意外中断，回合停在这里",
        pb::InterruptReason::ApprovalWaitLimit => "等审批太久，回合先暂停了",
        pb::InterruptReason::Unspecified => "回合中断了",
    }
}

pub fn change_summary(c: &pb::ChangeSummary) -> String {
    format!(
        "改了 {} 个文件 +{} −{}",
        c.files_changed, c.insertions, c.deletions
    )
}

pub fn turn_outcome(t: &pb::Turn) -> String {
    match pb::TurnState::try_from(t.state).unwrap_or_default() {
        pb::TurnState::Failed => match t.state_reason.as_deref() {
            Some(r) if !r.is_empty() => format!("失败：{r}"),
            _ => "失败".into(),
        },
        pb::TurnState::Cancelled => "已取消".into(),
        _ => String::new(),
    }
}

pub fn message_status(status: pb::MessageStatus) -> &'static str {
    match status {
        pb::MessageStatus::Interrupted => "已中断",
        pb::MessageStatus::Failed => "失败",
        _ => "",
    }
}

pub fn model_label(m: &pb::ModelSelection) -> String {
    match m.effort.as_deref() {
        Some(e) if !e.is_empty() => format!("{} · {e}", m.model_id),
        _ => m.model_id.clone(),
    }
}

pub fn switched_model(m: &pb::ModelSelection) -> String {
    format!("已切换为 {}", model_label(m))
}

pub fn switched_project(name: Option<&str>) -> String {
    match name {
        Some(n) => format!("切换到 {n}"),
        None => "切换到私聊".into(),
    }
}

pub fn switched_mode(mode: Option<pb::ApprovalMode>) -> String {
    match mode {
        Some(m) => format!("审批方式已切换为{}", approval_mode(m)),
        None => "审批方式已恢复跟随".into(),
    }
}

pub fn mode_scope_title(dm_assistant: Option<&str>) -> String {
    match dm_assistant {
        Some(name) => format!("仅对 {name} 生效"),
        None => "对本对话全体助手生效".into(),
    }
}

pub fn tokens(n: u64) -> String {
    if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

pub fn usage_text(u: &pb::Usage) -> String {
    format!(
        "本对话用量 {} tokens",
        tokens(u.input_tokens + u.output_tokens)
    )
}

pub fn usage_tooltip(u: &pb::Usage) -> String {
    let mut s = format!(
        "输入 {} · 输出 {} · 缓存读 {} · 缓存写 {}",
        u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens
    );
    if let Some(c) = u.cost_usd {
        s.push_str(&format!(" · 约 ${c:.2}"));
    }
    s
}

pub fn runtime_state(state: pb::RuntimeState) -> &'static str {
    use pb::RuntimeState as S;
    match state {
        S::Missing => "未安装",
        S::NotLoggedIn => "未登录",
        S::ReadyVerified | S::ReadyUnverified => "可用",
        S::Incompatible => "不兼容",
        S::Unspecified => "未检测",
    }
}

/// 版本验证状态：只给设置 › 运行时、引导页与诊断页（方案 R10）。
pub fn verification(state: pb::RuntimeState) -> Option<&'static str> {
    use pb::RuntimeState as S;
    match state {
        S::ReadyVerified => Some("已验证"),
        S::ReadyUnverified => Some("验证中"),
        S::Incompatible => Some("验证未通过"),
        S::Missing | S::NotLoggedIn | S::Unspecified => None,
    }
}

pub fn runtime_guidance(state: pb::RuntimeState, name: &str) -> Option<String> {
    use pb::RuntimeState as S;
    match state {
        S::Missing => Some(format!("没有找到 {name}。装好后点「重新检测」。")),
        S::NotLoggedIn => Some(format!("{name} 还没登录。在终端里登录后点「重新检测」。")),
        _ => None,
    }
}

/// 输入框被挡住时的说明（方案 2.7 首页一行；R10：验证状态在这里只说「为什么发不了」）。
pub fn composer_block(state: pb::RuntimeState, name: &str) -> Option<String> {
    use pb::RuntimeState as S;
    match state {
        S::Missing => Some(format!("没有找到 {name}，装好后才能发送")),
        S::NotLoggedIn => Some(format!("{name} 还没登录，登录后才能发送")),
        S::ReadyUnverified => Some(format!("正在验证这个版本的 {name}，验证通过后就能发送")),
        S::Incompatible => Some(format!("这个版本的 {name} 没通过验证，暂时不能发送")),
        S::ReadyVerified | S::Unspecified => None,
    }
}

pub const COMPOSER_OFFLINE: &str = "正在重新连接，连上后就能发送";
pub const COMPOSER_PATH_MISSING: &str = "找不到项目文件夹，重新定位后才能发送";
pub const COMPOSER_DELETING: &str = "对话正在删除";
pub const RELOCATE: &str = "重新定位";
pub const DETAILS: &str = "查看详情";

pub fn tool_group(g: pb::ToolGroup) -> &'static str {
    match g {
        pb::ToolGroup::ReadAndSearch => "读取与搜索",
        pb::ToolGroup::EditFiles => "编辑文件",
        pb::ToolGroup::RunCommands => "运行命令",
        pb::ToolGroup::Web => "联网",
        pb::ToolGroup::Unspecified => "",
    }
}

pub fn pending_kind(kind: pb::PendingKind) -> &'static str {
    match kind {
        pb::PendingKind::Approval => "审批",
        pb::PendingKind::UserAction => "需要你亲手执行",
        pb::PendingKind::AgentsMdDraft => "AGENTS.md 草稿",
        pb::PendingKind::Resume => "继续或放弃",
        pb::PendingKind::Unspecified => "",
    }
}

pub fn size_label(bytes: u64) -> String {
    const K: u64 = 1024;
    if bytes >= K * K {
        format!("{:.1} MB", bytes as f64 / (K * K) as f64)
    } else if bytes >= K {
        format!("{:.0} KB", bytes as f64 / K as f64)
    } else {
        format!("{bytes} B")
    }
}

/// 相对时间：「刚刚」「N 分钟前」……
pub fn relative_past(now: SystemTime, at: SystemTime) -> String {
    let d = now.duration_since(at).unwrap_or(Duration::ZERO).as_secs();
    match d {
        0..60 => "刚刚".into(),
        60..3600 => format!("{} 分钟前", d / 60),
        3600..86_400 => format!("{} 小时前", d / 3600),
        _ => format!("{} 天前", d / 86_400),
    }
}

pub fn relative_future(now: SystemTime, at: SystemTime) -> String {
    let d = at.duration_since(now).unwrap_or(Duration::ZERO).as_secs();
    match d {
        0..60 => "马上重置".into(),
        60..3600 => format!("约 {} 分钟后重置", d / 60),
        3600..86_400 => format!("约 {} 小时后重置", d / 3600),
        _ => format!("约 {} 天后重置", d / 86_400),
    }
}

pub fn quota_tooltip(window: &str, used: u32, reset: Option<String>) -> String {
    match reset {
        Some(r) => format!("{window}：已用 {used}% · {r}"),
        None => format!("{window}：已用 {used}%"),
    }
}
