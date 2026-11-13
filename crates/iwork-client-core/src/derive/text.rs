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
pub const QUERY_RETRYING: &str = "部分数据暂时没取到，稍后自动重试";
pub const OVERLOADED: &str = "后台负载过高，稍后重试或重新打开 iWork";
pub const DIAGNOSTICS_TITLE: &str = "连不上后台服务";
/// 版本不一致页的说明（UI 审查 g3 2/F-1；措辞待设计窗口确认）。「重新打开 iWork」只重启客户端，后台不动。
pub const VERSION_MISMATCH_MESSAGE: &str = "这个窗口和后台服务不是同一版，暂时连不上。通常是刚装了新版：重新打开 iWork 就会用上新装的那一版。后台照常运行，进行中的回合不会因此中断。";
/// 不支持的环境整页的标题。
pub const ENVIRONMENT_TITLE: &str = "iWork 暂时不能在这台电脑上运行";
pub const NEWER_SCHEMA_HINT: &str = "装回更新的那一版可继续使用";
/// 诊断页正文：按连不上的原因说一句人话；系统给的原始错误收进「详细信息」（UI 审查 g3 F-5）。
pub const DIAG_NOT_RUNNING: &str =
    "后台服务没在运行，或者还没启动好。iWork 会自动重试，也可以点「重试」。";
pub const DIAG_PEER_REJECTED: &str = "连到的后台服务没通过身份校验，iWork 没有和它通信。";
pub const DIAG_TIMEOUT: &str = "后台服务没有及时回应。iWork 会自动重试。";
pub const DIAG_PROTOCOL: &str = "后台服务的回应不对，可能和客户端不是同一版。";
pub const DIAG_OTHER: &str = "连接后台服务时出了问题。iWork 会自动重试。";
/// 诊断行里放原始错误的那一行。
pub const DIAG_RAW_ERROR: &str = "原始错误";
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

/// 登录方式给用户看的说法（画布 00「已登录 · 本人订阅」）：按 `claude auth status` 的 `subscriptionType` 认；
/// 认不出就不写，不露原值（UI 审查 g3 F-10）。表里的值是 Claude Code 的套餐名，真实取值待 T06 实测核对。
const SUBSCRIPTIONS: &[(&str, &str)] = &[
    ("pro", "本人订阅"),
    ("max", "本人订阅"),
    ("team", "团队订阅"),
    ("enterprise", "企业订阅"),
];

pub fn login_label(subscription_type: &str) -> String {
    let kind = SUBSCRIPTIONS
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(subscription_type.trim()))
        .map(|(_, v)| *v);
    match kind {
        Some(k) => format!("已登录 · {k}"),
        None => "已登录".into(),
    }
}

/// 规则的「记在哪」：全局的规则。
pub const RULE_OWNER_ALL: &str = "所有项目与助手";
/// 规则记在的项目、助手或对话在本机摘要里找不到（已删或还没查到）。
pub const RULE_TARGET_GONE: &str = "（找不到）";

/// 按反引号切开：成对的反引号之间是代码。落单的反引号原样留在文字里。
pub fn code_spans(s: &str) -> Vec<pb::TextSpan> {
    let parts: Vec<&str> = s.split('`').collect();
    // 段数为偶数说明最后一个反引号落单：把它和最后一段并回文字
    let paired = if parts.len() % 2 == 0 {
        parts.len() - 1
    } else {
        parts.len()
    };
    let mut out: Vec<pb::TextSpan> = Vec::new();
    for (i, p) in parts[..paired].iter().enumerate() {
        if !p.is_empty() {
            out.push(pb::TextSpan {
                text: (*p).to_owned(),
                code: i % 2 == 1,
            });
        }
    }
    if paired < parts.len() {
        let rest = format!("`{}", parts[paired]);
        match out.last_mut() {
            Some(last) if !last.code => last.text.push_str(&rest),
            _ => out.push(pb::TextSpan {
                text: rest,
                code: false,
            }),
        }
    }
    out
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
/// 输入框上方挡住发送的一行（运行时）。验证中、未通过两句照 ui-m1.md 2a 第 8 条，带版本号
/// （「Claude Code 2.1.3」）；取不到版本时只写名字。未安装、未登录 2a 没写原文，沿用这里的说法。
pub fn composer_block(
    state: pb::RuntimeState,
    name: &str,
    version: Option<&str>,
) -> Option<String> {
    use pb::RuntimeState as S;
    let named = match version.filter(|v| !v.is_empty()) {
        Some(v) => format!("{name} {v}"),
        None => name.to_owned(),
    };
    match state {
        S::Missing => Some(format!("没有找到 {name}，装好后才能发送")),
        S::NotLoggedIn => Some(format!("{name} 还没登录，登录后才能发送")),
        S::ReadyUnverified => Some(format!("正在检查 {named} 能否安全使用…")),
        S::Incompatible => Some(format!("{named} 没有通过安全检查，暂不能使用")),
        S::ReadyVerified | S::Unspecified => None,
    }
}

pub const COMPOSER_OFFLINE: &str = "正在重新连接，连上后就能发送";
/// ui-m1.md 2a 第 7 条原文。
pub const COMPOSER_PATH_MISSING: &str = "找不到项目文件夹，重新定位后才能继续";
pub const COMPOSER_DELETING: &str = "对话正在删除";
pub const RELOCATE: &str = "重新定位";
/// 拖文件进窗口时整块对话区的虚线框（ui-m1.md 第 2 节第 6 条）。
pub const DROP_TO_ATTACH: &str = "松开以添加";
/// 断线时点了要发命令的东西（附件、选择框）。
pub const OFFLINE_TRY_LATER: &str = "正在重新连接，连上后再试";
/// 首页的回形针：附件挂在对话上，发出第一条消息、对话建好后才能加。
pub const ATTACH_AFTER_FIRST_MESSAGE: &str = "发出第一条消息后可以添加附件";
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

// ---------- 「运行时 · 模型」菜单（ui-m1.md 第 2 节第 14 条） ----------

pub const RUNTIME_LOCKED_TIP: &str = "要换运行时请开新对话";
pub const RUNTIME_NOT_IN_M1: &str = "M2 接入";
/// 添加项目（画布 07 原文；T09 方案 15.2 的表）。「git」这个词只出现在这张文案表里。
pub const ADD_NOT_VERSIONED_TITLE: &str = "这个文件夹还没有版本控制";
pub const ADD_NOT_VERSIONED_BODY: &str = "审查、返工和集成都依赖版本快照，需要先初始化。";
pub const ADD_INIT_LABEL: &str = "初始化为 git 仓库并首次提交";
pub const ADD_SUBDIR_TITLE: &str = "将添加整个仓库";
pub const ADD_CHECK_FAILED_TITLE: &str = "这个文件夹不能添加";
pub const AGENTS_MD_MISSING: &str = "未找到";
pub const AGENTS_MD_PRESENT: &str = "已有";
pub const ADD_CONFIRM: &str = "添加并开始对话";
pub const ADD_OPEN_KNOWN: &str = "打开已有项目";
pub const RUNTIME_GLOBAL_DEFAULT: &str = "（全局默认）";

/// 给人看的路径：家目录写成 `~`（家目录本身是 `~`，之下是 `~/…`）。名字以家目录为前缀的兄弟目录
/// （`/Users/al` 与 `/Users/alice`）不缩写。没有家目录就原样。
pub fn display_path(path: &str, home: Option<&str>) -> String {
    let Some(home) = home
        .map(|h| h.trim_end_matches('/'))
        .filter(|h| !h.is_empty())
    else {
        return path.to_owned();
    };
    if path == home {
        return "~".into();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

/// 连接类整页的出口。
pub const EXIT_RETRY: &str = "重试";
pub const EXIT_RELAUNCH: &str = "重新打开 iWork";

/// 徽标上最多写到的数（ui-m1.md 第 2 节第 4 条，产品负责人 2026-11-11 定）；再多写「99+」。
pub const BADGE_MAX: u32 = 99;

/// 未读徽标；0 不画。
pub fn unread_badge(n: u32) -> Option<pb::CountBadge> {
    count_badge(n, format!("{n} 条未读"))
}

/// 「待你处理」徽标；0 不画。Dock 角标（步 5a）用同一个文字。
pub fn pending_badge(n: u32) -> Option<pb::CountBadge> {
    count_badge(n, format!("{n} 项待你处理"))
}

fn count_badge(n: u32, label: String) -> Option<pb::CountBadge> {
    (n > 0).then(|| pb::CountBadge {
        text: if n > BADGE_MAX {
            format!("{BADGE_MAX}+")
        } else {
            n.to_string()
        },
        label,
    })
}

/// 输入框的占位文字（ui-m1.md 3.4）：「跟阿森说点什么」；名字以拉丁字母开头或结尾时两边留空格（「跟 iWork 说点什么」）。
pub fn composer_placeholder(assistant_name: &str) -> String {
    if assistant_name.is_empty() {
        return "说点什么".into();
    }
    let ascii = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
    let before = if ascii(assistant_name.chars().next()) {
        " "
    } else {
        ""
    };
    let after = if ascii(assistant_name.chars().last()) {
        " "
    } else {
        ""
    };
    format!("跟{before}{assistant_name}{after}说点什么")
}

/// 思考强度的显示文字；运行时给的取值不认识就原样显示。
pub fn effort_label(level: &str) -> String {
    match level {
        "low" => "低".into(),
        "medium" => "中".into(),
        "high" => "高".into(),
        "max" | "xhigh" => "最高".into(),
        other => other.into(),
    }
}

// ---------- 这一轮失败（ui-m1.md 2a 第 10 条） ----------

/// 失败说明行：`started` 为假（助手消息为空）是「没能开始」，否则是「中途失败」。
/// 原因用回合的 state_reason；后台没给就写「运行时没有给出原因」。「详情」是给人核对的技术信息。
pub fn turn_failure(empty: bool, turn: Option<&pb::Turn>) -> pb::FailureLine {
    let reason = turn
        .and_then(|t| t.state_reason.as_deref())
        .filter(|r| !r.is_empty());
    let shown = reason.unwrap_or("运行时没有给出原因");
    let text = if empty {
        format!("这一轮没能开始：{shown}")
    } else {
        format!("这一轮中途失败：{shown}")
    };
    let detail = match turn {
        Some(t) => format!("回合 {}，原因：{}", t.id, reason.unwrap_or("（无）")),
        None => "这条消息不属于任何回合".to_owned(),
    };
    pb::FailureLine { text, detail }
}

/// 重连走了快照、时间线重建时用户正在看旧消息：回到最新，标题栏下提示（Q9，产品负责人 2026-11-11）。
pub const BACK_TO_LATEST: &str = "已重新连接，回到最新";

// ---------- 审批档位菜单（ui-m1.md 第 2 节第 9 条，文案已由产品负责人定稿） ----------

pub fn approval_mode_description(mode: pb::ApprovalMode) -> &'static str {
    match mode {
        pb::ApprovalMode::Manual | pb::ApprovalMode::Unspecified => "写文件、执行命令前始终询问",
        pb::ApprovalMode::Auto => "仅对检测到的风险操作请求批准",
        pb::ApprovalMode::FullAccess => "可不受限地访问和操作一切，只有 git push 仍会询问",
    }
}

/// 全开只在当前对话生效、不可设为默认（需求 FR11）。
pub const FULL_ACCESS_TAG: &str = "仅本对话";

/// 切到全开的确认框（画布 10-ApprovalMode-fullconfirm）。`who` 是受影响的助手名，用顿号连起来。
pub fn full_access_confirm(who: &str) -> pb::ConfirmView {
    pb::ConfirmView {
        title: "本对话切到全开？".into(),
        body: format!(
            "{who}将不再询问你，删除也直接执行；只有 git push 仍会询问。凭据和 iWork 数据仍受保护，只有需凭据的命令（如 git pull）会在沙箱外运行。"
        ),
        footnote: "只在本对话有效，新对话回到默认".into(),
        confirm_label: "切到全开".into(),
        for_value: 0,
    }
}

/// 首页「新建对话并发出首条消息」没成（草稿还在输入框里）。
pub fn start_error(e: &crate::store::home::StartError) -> String {
    use crate::store::home::StartError as E;
    match e {
        E::Create(why) => format!("没能新建对话：{why}"),
        E::Send(why) => format!("新对话建好了，但消息没发出去：{why}。可以在侧栏打开它再发"),
        E::Lost => "重新连接后认不出刚建的对话，消息没发出去。可以在侧栏打开它再发".into(),
    }
}

#[cfg(test)]
mod placeholder_tests {
    use super::composer_placeholder;

    #[test]
    fn spaces_only_around_latin_names() {
        assert_eq!(composer_placeholder("阿森"), "跟阿森说点什么");
        assert_eq!(composer_placeholder("iWork"), "跟 iWork 说点什么");
        assert_eq!(composer_placeholder("小 k"), "跟小 k 说点什么");
        assert_eq!(composer_placeholder(""), "说点什么");
    }
}

#[cfg(test)]
mod badge_tests {
    use super::{pending_badge, unread_badge};

    #[test]
    fn over_99_reads_99_plus_and_zero_draws_nothing() {
        let t = |b: Option<iwork_proto::pb::CountBadge>| b.map(|b| (b.text, b.label));
        assert_eq!(t(unread_badge(0)), None);
        assert_eq!(t(unread_badge(7)), Some(("7".into(), "7 条未读".into())));
        assert_eq!(t(unread_badge(99)), Some(("99".into(), "99 条未读".into())));
        assert_eq!(
            t(unread_badge(100)),
            Some(("99+".into(), "100 条未读".into()))
        );
        assert_eq!(
            t(pending_badge(250)),
            Some(("99+".into(), "250 项待你处理".into()))
        );
    }
}

#[cfg(test)]
mod composer_block_tests {
    use super::composer_block;
    use iwork_proto::pb::RuntimeState as S;

    /// ui-m1.md 2a 第 8 条的原文，带版本号；取不到版本只写名字。
    #[test]
    fn verifying_and_incompatible_follow_the_spec_wording() {
        assert_eq!(
            composer_block(S::ReadyUnverified, "Claude Code", Some("2.1.3")).as_deref(),
            Some("正在检查 Claude Code 2.1.3 能否安全使用…")
        );
        assert_eq!(
            composer_block(S::Incompatible, "Claude Code", Some("2.1.3")).as_deref(),
            Some("Claude Code 2.1.3 没有通过安全检查，暂不能使用")
        );
        assert_eq!(
            composer_block(S::Incompatible, "Claude Code", None).as_deref(),
            Some("Claude Code 没有通过安全检查，暂不能使用")
        );
        assert_eq!(
            composer_block(S::ReadyVerified, "Claude Code", Some("2.1.3")),
            None
        );
    }
}

// ---------- 消息锚点（方案 2.9） ----------

/// 跳到还没加载的一处、正在取那一段时的提示（Q18：原地不动加提示）。
pub const ANCHOR_JUMP_LOADING: &str = "正在加载那一处…";

pub fn anchor_kind(k: pb::AnchorKind) -> &'static str {
    match k {
        pb::AnchorKind::User => "你",
        pb::AnchorKind::Waiting => "等你处理",
        pb::AnchorKind::Failed => "失败",
        pb::AnchorKind::Unspecified => "",
    }
}

/// 失败回合的预览：与时间线里那一行同两种说法（2a 第 10 条）。
pub fn anchor_failure(had_output: bool, reason: &str) -> String {
    let shown = if reason.is_empty() {
        "运行时没有给出原因"
    } else {
        reason
    };
    if had_output {
        format!("这一轮中途失败：{shown}")
    } else {
        format!("这一轮没能开始：{shown}")
    }
}

/// 一个时刻怎么写（锚点、全部对话的最近活动，ui-m1 2a 第 5 条）：今天写时刻（14:05），今年更早写日期（11月3日），
/// 往年带年份（2025年11月3日）。按 `tz` 算日期。
pub fn clock_or_date(now: SystemTime, at: SystemTime, tz: &jiff::tz::TimeZone) -> String {
    let (Ok(now), Ok(at)) = (
        jiff::Timestamp::try_from(now),
        jiff::Timestamp::try_from(at),
    ) else {
        return String::new();
    };
    let (now, at) = (now.to_zoned(tz.clone()), at.to_zoned(tz.clone()));
    if now.date() == at.date() {
        format!("{:02}:{:02}", at.hour(), at.minute())
    } else if now.year() == at.year() {
        format!("{}月{}日", at.month(), at.day())
    } else {
        format!("{}年{}月{}日", at.year(), at.month(), at.day())
    }
}

#[cfg(test)]
mod anchor_text_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn time_label_today_this_year_and_older() {
        let tz = jiff::tz::TimeZone::fixed(jiff::tz::offset(8));
        // 2026-11-12 10:00 +08:00
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_448_800);
        let at = |back: u64| now - Duration::from_secs(back);
        assert_eq!(clock_or_date(now, at(3600 + 15 * 60), &tz), "08:45");
        assert_eq!(
            clock_or_date(now, at(11 * 3600), &tz),
            "11月11日",
            "前一天（本地时区）"
        );
        assert_eq!(clock_or_date(now, at(400 * 86_400), &tz), "2025年10月8日");
    }

    #[test]
    fn code_spans_split_on_backticks() {
        let spans = |s: &str| {
            code_spans(s)
                .into_iter()
                .map(|x| (x.text, x.code))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            spans("执行 `npm test …` 不再问"),
            vec![
                ("执行 ".to_owned(), false),
                ("npm test …".to_owned(), true),
                (" 不再问".to_owned(), false),
            ]
        );
        assert_eq!(
            spans("写 `src/`"),
            vec![("写 ".to_owned(), false), ("src/".to_owned(), true)]
        );
        assert_eq!(spans("没有代码"), vec![("没有代码".to_owned(), false)]);
        assert_eq!(
            spans("落单 `反引号"),
            vec![("落单 `反引号".to_owned(), false)],
            "落单的反引号原样留着"
        );
    }

    #[test]
    fn login_label_is_readable() {
        assert_eq!(login_label("max"), "已登录 · 本人订阅");
        assert_eq!(login_label("Pro"), "已登录 · 本人订阅");
        assert_eq!(login_label("team"), "已登录 · 团队订阅");
        assert_eq!(login_label("x-unknown"), "已登录", "认不出：不露原值");
        assert_eq!(login_label(""), "已登录");
    }

    #[test]
    fn failure_preview_has_both_wordings() {
        assert_eq!(
            anchor_failure(false, "启动失败"),
            "这一轮没能开始：启动失败"
        );
        assert_eq!(
            anchor_failure(true, ""),
            "这一轮中途失败：运行时没有给出原因"
        );
    }
}
