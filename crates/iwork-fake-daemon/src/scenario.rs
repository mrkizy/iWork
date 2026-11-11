//! 场景登记表：名字 → 步骤列表（T09 方案 2.6）。加一个场景 = [`REGISTRY`] 加一行 + 一个构造函数。
//!
//! 方案第 8 节的状态清单每行一个场景；这里先是支撑 Rust 测试与基准的核心集，网页各页（步 7）按需往表里加。
//! 每个场景都从 [`base`] 的同一个小世界开始（运行时、额度、设置、内置助手与它的私聊、一个项目）。

use std::time::Duration;

use iwork_proto::pb;
use iwork_proto::pb::ApprovalState as AS;
use iwork_proto::pb::TurnState as T;
use iwork_proto::pb::UserActionState as U;

use crate::fixtures::{self as f, IWORK};
use crate::tables::Tables;

/// 场景的一步。
pub enum Step {
    /// 装箱：持久事件比其他步骤大得多，场景动辄十万步。
    Append(Box<pb::DomainEvent>),
    Transient(pb::TransientEvent),
    Sleep(Duration),
    /// 关掉全部连接（模拟后台重启），日志与表保留。
    DisconnectAll,
    /// 关掉全部连接，之后重连一律给快照、不重放（模拟断得太久：落后超过重放上限）。
    DisconnectForSnapshot,
    /// 停止监听：客户端连不上，一直在重连（「断线」状态）。之后的 DisconnectAll 恢复监听。
    StopListening,
    /// 只在录像里有意义：客户端在这里打开 focus 对话（并做场景的 `client` 动作），之后的步骤是增量。
    /// 没有这个标记时切点由 [`Scenario::split_at_focus`] 推算。跑场景时跳过。
    OpenHere,
    /// 只给测试场景用：直接改折叠表，不产生事件。
    Mutate(Box<dyn FnOnce(&mut Tables) + Send>),
}

/// 打开对话之后最多实时跑多少步（见 [`Scenario::split_at_focus`]）。
pub const LIVE_TAIL: usize = 200;

pub struct Scenario {
    pub name: &'static str,
    pub about: &'static str,
    /// 录像与预览页打开的对话；没有就停在首页（录像只记侧栏、待处理、设置）。
    pub focus: Option<&'static str>,
    /// 只给录像与预览：助手 id → 形象 id（client-core `Options::characters`）。
    /// M1 没有选形象的入口，正式运行时内置 iWork 画标志、其余一律圆团子；阿森、林岚、老周只在这里出现。
    pub characters: &'static [(&'static str, &'static str)],
    /// 只给录像与预览：打开对话之后、跑剩下的事件之前，客户端要做的事。有些状态要用户先动一下才出现
    /// （往上翻、停在旧消息、拖入超大文件），假后台的事件造不出来。
    pub client: &'static [ClientAct],
    /// 只给录像与预览：客户端时间线行数上限（调小才翻得出「回到最新（N）」）；None 用默认。
    pub timeline_cap: Option<usize>,
    build: fn() -> Vec<Step>,
}

/// 录像时客户端的动作（见 [`Scenario::client`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAct {
    /// 往上翻一页（LoadOlder）。
    LoadOlder,
    /// 可见窗口停在最老的几行上（在看旧消息）。
    ReadOldest,
    /// 拖入一个超过 20MB 的文件（输入框上方出现原因）。
    DropOversized,
}

impl Scenario {
    pub fn steps(&self) -> Vec<Step> {
        (self.build)()
    }

    /// 切成「打开对话之前」与「打开之后」两段：前一段跑完、客户端打开对话拿到快照，再跑后一段，
    /// 后一段的事件才会作为增量进时间线（对拍要比的是增量，不是两份快照）。没有 focus 时前一段为空。
    /// 切点在建出 focus 对话之后；后一段最多留 [`LIVE_TAIL`] 步，更早的在打开前跑完
    /// （history-100k 若十万条都在打开后实时推，对拍要跑好几分钟，增量也不需要那么多）。
    pub fn split_at_focus(&self) -> (Vec<Step>, Vec<Step>) {
        let mut steps = self.steps();
        if let Some(at) = steps.iter().position(|s| matches!(s, Step::OpenHere)) {
            let mut rest = steps.split_off(at);
            rest.remove(0);
            return (steps, rest);
        }
        let Some(focus) = self.focus else {
            return (Vec::new(), steps);
        };
        let at = steps.iter().position(|s| match s {
            Step::Append(e) => matches!(
                &e.event,
                Some(pb::domain_event::Event::ConversationCreated(c))
                    if c.conversation.as_ref().is_some_and(|c| c.id == focus)
            ),
            _ => false,
        });
        let at = at
            .unwrap_or_else(|| panic!("场景 {} 里没有建出 focus 对话 {focus} 的那一步", self.name));
        let cut = (at + 1).max(steps.len().saturating_sub(LIVE_TAIL));
        let rest = steps.split_off(cut);
        (steps, rest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("没有这个场景：{0}（可用：iwork-fake-daemon --list）")]
pub struct UnknownScenario(pub String);

pub const REGISTRY: &[Scenario] = &[
    Scenario {
        name: "empty",
        about: "只有基础世界",
        build: empty,
        focus: None,
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "conversation-basic",
        about: "一问一答、工具活动、回合结束印章",
        build: conversation_basic,
        focus: Some("c-basic"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "streaming",
        about: "一条助手消息分 20 段流式到达",
        build: streaming,
        focus: Some("c-stream"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "approval-states",
        about: "审批：待定、已允许、已拒绝、已作废、切档放行、必问、沙箱外",
        build: approval_states,
        focus: Some("c-approval"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "user-action-states",
        about: "亲手执行：待定、执行中、已执行、已忽略、失败、已作废、结果未知",
        build: user_action_states,
        focus: Some("c-user-action"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "user-action-unknown-deleting",
        about: "结果未知的卡片挡住删除，对话停在删除中",
        build: user_action_unknown_deleting,
        focus: Some("c-ua-deleting"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "turn-end",
        about: "回合结束：搞定印章、失败、取消",
        build: turn_end,
        focus: Some("c-turn-end"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "turn-failed",
        about: "回合失败：一开始就失败（空的失败消息）、输出一部分后失败（ui-m1.md 2a 第 10 条）",
        focus: Some("c-turn-failed"),
        build: turn_failed,
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "resume-card",
        about: "崩溃中断的回合：继续 / 放弃卡片",
        build: resume_card,
        focus: Some("c-resume"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "sidebar-dots",
        about: "侧栏圆点四种状态与未读",
        build: sidebar_dots,
        focus: None,
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "assistants-20",
        about: "20 个助手，部分有最近动态",
        build: assistants_20,
        focus: None,
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "quota-missing",
        about: "额度取不到",
        build: quota_missing,
        focus: None,
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "stream-16x100",
        about: "16 个对话各 100 条流式消息（基准用）",
        build: stream_16x100,
        focus: Some("c-s00"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "history-100k",
        about: "一个对话十万条消息",
        build: history_100k,
        focus: Some("c-history"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "xss-corpus",
        about: "正文、标题、文件名、工具输出里的脚本与链接",
        build: xss_corpus,
        focus: Some("c-xss"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "reconnect-basic",
        about: "中途断开全部连接后继续",
        build: reconnect_basic,
        focus: Some("c-reconnect"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "dm-project-switch",
        about: "助手私聊换项目：「已切换为」说明、项目分隔线；打开后两个项目都改名",
        build: dm_project_switch,
        focus: Some(IWORK_DM),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "characters-preview-only",
        about: "测试专用：阿森、林岚、老周与圆团子的形象和表情（工作中、等你、出错、空闲）。M1 没有选形象的入口，形象由场景按助手 id 映射",
        build: characters_preview_only,
        focus: Some("c-dm-laozhou"),
        characters: &[
            ("as-asen", "asen"),
            ("as-linlan", "linlan"),
            ("as-laozhou", "laozhou"),
        ],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "characters-sorry-preview-only",
        about: "测试专用：阿森、林岚、老周最近一轮都失败了，三个都显示出错表情（sorry）。M1 没有选形象的入口，形象由场景按助手 id 映射",
        build: characters_sorry_preview_only,
        focus: Some("c-dm-asen"),
        characters: &[
            ("as-asen", "asen"),
            ("as-linlan", "linlan"),
            ("as-laozhou", "laozhou"),
        ],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "back-to-latest-notice",
        about: "Q9：停在旧消息上时断线、重连给的是快照：回到最新并提示「已重新连接，回到最新」",
        build: back_to_latest_notice,
        focus: Some("c-q9"),
        characters: &[],
        client: &[ClientAct::ReadOldest],
        timeline_cap: None,
    },
    Scenario {
        name: "new-below-count",
        about: "往上翻到超出时间线上限后来了新消息：「回到最新（N）」",
        build: new_below_count,
        focus: Some("c-scrolled"),
        characters: &[],
        client: &[ClientAct::LoadOlder],
        timeline_cap: Some(40),
    },
    Scenario {
        name: "composer-path-missing",
        about: "输入框上方：项目找不到路径（警示色 +「重新定位」）",
        build: composer_path_missing,
        focus: Some("c-block"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "composer-verifying",
        about: "输入框上方：运行时验证中（转圈，发送禁用）",
        build: composer_verifying,
        focus: Some("c-block"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "composer-incompatible",
        about: "输入框上方：运行时验证未通过（警示色 +「查看详情」）",
        build: composer_incompatible,
        focus: Some("c-block"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "composer-not-installed",
        about: "输入框上方：运行时未安装（「查看详情」）",
        build: composer_not_installed,
        focus: Some("c-block"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "composer-not-logged-in",
        about: "输入框上方：运行时未登录（「查看详情」）",
        build: composer_not_logged_in,
        focus: Some("c-block"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "composer-offline",
        about: "输入框上方：后台连不上（断线，发送禁用）",
        build: composer_offline,
        focus: Some("c-block"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "attachments-tray",
        about: "附件托盘两个文件；拖入超过 20MB 的文件，输入框上方说明原因",
        build: attachments_tray,
        focus: Some("c-attach"),
        characters: &[],
        client: &[ClientAct::DropOversized],
        timeline_cap: None,
    },
    Scenario {
        name: "projects-sidebar",
        about: "侧栏项目：置顶、找不到路径、未读超过 99（「99+」）；右键项目看菜单",
        build: projects_sidebar,
        focus: None,
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "tool-output",
        about: "工具行：输出分段到达，点开看输出（当文字显示）",
        build: tool_output,
        focus: Some("c-tool"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
    Scenario {
        name: "rename-via-summary-refresh-until-t10",
        about: "测试专用：直接改摘要里的标题，再发一条落在侧栏订阅上的已有事件触发摘要失效。T10 补上 ConversationRenamed 后换成真事件",
        build: rename_via_summary_refresh_until_t10,
        focus: Some("c-rename"),
        characters: &[],
        client: &[],
        timeline_cap: None,
    },
];

pub fn names() -> Vec<&'static str> {
    REGISTRY.iter().map(|s| s.name).collect()
}

pub fn get(name: &str) -> Option<&'static Scenario> {
    REGISTRY.iter().find(|s| s.name == name)
}

// ---------- 基础世界 ----------

pub const PROJECT: &str = "p-demo";
pub const IWORK_DM: &str = "c-dm-iwork";
/// 第二个项目：只有需要两个项目的场景才建。
pub const OTHER_PROJECT: &str = "p-web";

fn a(e: pb::DomainEvent) -> Step {
    Step::Append(Box::new(e))
}

fn world(with_quota: bool) -> Vec<Step> {
    let mut s = vec![
        a(f::runtime(pb::RuntimeState::ReadyVerified)),
        a(f::settings(true)),
        a(f::assistant(IWORK, "iWork", true)),
        a(f::dm(IWORK_DM, IWORK)),
        a(f::project(PROJECT, "demo", "~/work/demo")),
    ];
    if with_quota {
        s.insert(1, a(f::quota(37, 12)));
    }
    s
}

pub fn base() -> Vec<Step> {
    world(true)
}

fn empty() -> Vec<Step> {
    base()
}

/// 一问一答的回合：用户消息、排队、运行、助手消息、完成。返回步骤，回合结束由调用方决定。
fn exchange(conv: &str, n: &str, question: &str, answer: &str) -> Vec<Step> {
    let turn = format!("t-{n}");
    let user = format!("m-{n}-u");
    let reply = format!("m-{n}-a");
    vec![
        a(f::user_message(&user, conv, question)),
        a(f::turn_queued(&turn, conv, IWORK)),
        a(f::turn_state(&turn, T::Queued, T::Running, None)),
        a(f::assistant_message(&reply, conv, &turn, IWORK, "iWork")),
        a(f::message_completed(conv, &reply, answer)),
    ]
}

fn conversation_basic() -> Vec<Step> {
    let c = "c-basic";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.extend(exchange(
        c,
        "basic",
        "帮我把登录页改成深色",
        "好的，我先看一下样式文件。\n改完了，跑过测试。",
    ));
    s.push(a(f::tool_started("t-basic", "tc-1", "Bash", "npm test")));
    s.push(a(f::tool_finished("t-basic", "tc-1", true)));
    s.push(a(f::turn_finished(
        "t-basic",
        T::Completed,
        true,
        Some(f::changes(&["src/login.css", "src/theme.ts"], 24, 6)),
    )));
    s
}

fn streaming() -> Vec<Step> {
    let c = "c-stream";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.push(a(f::user_message("m-stream-u", c, "解释一下这个模块")));
    s.push(a(f::turn_queued("t-stream", c, IWORK)));
    s.push(a(f::turn_state("t-stream", T::Queued, T::Running, None)));
    s.push(a(f::assistant_message(
        "m-stream-a",
        c,
        "t-stream",
        IWORK,
        "iWork",
    )));
    let mut body = String::new();
    for i in 0..20 {
        let piece = format!("第 {i} 段。");
        s.push(Step::Transient(f::delta(
            "m-stream-a",
            body.len() as u64,
            &piece,
        )));
        s.push(Step::Sleep(Duration::from_millis(16)));
        body.push_str(&piece);
    }
    s.push(a(f::message_completed(c, "m-stream-a", &body)));
    s.push(a(f::turn_finished("t-stream", T::Completed, false, None)));
    s
}

fn approval_states() -> Vec<Step> {
    use pb::RememberScope as R;
    let c = "c-approval";
    let t = "t-approval";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.push(a(f::user_message("m-approval-u", c, "跑一下测试并发布")));
    s.push(a(f::turn_queued(t, c, IWORK)));
    s.push(a(f::turn_state(t, T::Queued, T::Running, None)));
    let ask = pb::ApprovalKind::Ask;
    let remember = [R::Once, R::Conversation, R::Project, R::Forever];
    let approve =
        |id: &str, cmd: &str| f::approval(id, c, t, f::command_action(cmd), ask, &remember);
    s.push(a(approve("ap-approved", "npm install")));
    s.push(a(f::approval_decided(
        "ap-approved",
        AS::Approved,
        pb::ApprovalDecider::User,
    )));
    s.push(a(approve("ap-denied", "rm -rf build")));
    s.push(a(f::approval_decided(
        "ap-denied",
        AS::Denied,
        pb::ApprovalDecider::User,
    )));
    s.push(a(approve("ap-voided", "npm run lint")));
    s.push(a(f::approval_voided(
        "ap-voided",
        pb::VoidReason::TurnCancelled,
    )));
    s.push(a(approve("ap-mode-switch", "npm run build")));
    s.push(a(f::approval_decided(
        "ap-mode-switch",
        AS::Approved,
        pb::ApprovalDecider::ModeSwitch,
    )));
    s.push(a(f::approval(
        "ap-must-ask",
        c,
        t,
        pb::ActionSummary {
            kind: pb::ActionKind::Push.into(),
            ..f::command_action("git push origin main")
        },
        ask,
        &[],
    )));
    s.push(a(f::approval(
        "ap-outside",
        c,
        t,
        pb::ActionSummary {
            kind: pb::ActionKind::OutsideSandbox.into(),
            out_of_bounds: true,
            bypass_sandbox_requested: true,
            opaque_script: true,
            ..f::command_action("./scripts/release.sh")
        },
        pb::ApprovalKind::OutsideSandbox,
        &[R::Once],
    )));
    s.push(a(approve("ap-pending", "npm test")));
    s.push(a(f::turn_state(t, T::Running, T::WaitingApproval, None)));
    s
}

fn user_action_states() -> Vec<Step> {
    let c = "c-user-action";
    let t = "t-user-action";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.push(a(f::user_message("m-ua-u", c, "看看我加载了哪些密钥")));
    s.push(a(f::turn_queued(t, c, IWORK)));
    s.push(a(f::turn_state(t, T::Queued, T::Running, None)));
    // 终态、退出码、错误、作废原因。
    type End = (U, Option<i32>, Option<&'static str>, Option<pb::VoidReason>);
    let cards: [(&str, Option<End>); 7] = [
        ("ua-pending", None),
        ("ua-executing", Some((U::Executing, None, None, None))),
        ("ua-done", Some((U::Done, Some(0), None, None))),
        ("ua-dismissed", Some((U::Dismissed, None, None, None))),
        (
            "ua-failed",
            Some((U::Failed, None, Some("命令没找到"), None)),
        ),
        (
            "ua-voided",
            Some((U::Voided, None, None, Some(pb::VoidReason::TurnCancelled))),
        ),
        (
            "ua-unknown",
            Some((U::Unknown, None, Some("后台服务中断"), None)),
        ),
    ];
    for (id, state) in cards {
        s.push(a(f::user_action(id, c, t, "ssh-add -l")));
        if let Some((st, exit, err, void)) = state {
            if matches!(st, U::Done | U::Unknown | U::Failed) {
                s.push(a(f::user_action_state(id, U::Executing, None, None, None)));
            }
            s.push(a(f::user_action_state(id, st, exit, err, void)));
        }
    }
    s
}

fn user_action_unknown_deleting() -> Vec<Step> {
    let c = "c-ua-deleting";
    let t = "t-ua-deleting";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.push(a(f::user_message("m-uad-u", c, "清一下钥匙串缓存")));
    s.push(a(f::turn_queued(t, c, IWORK)));
    s.push(a(f::user_action(
        "ua-unknown-del",
        c,
        t,
        "security delete-generic-password -s demo",
    )));
    s.push(a(f::user_action_state(
        "ua-unknown-del",
        U::Executing,
        None,
        None,
        None,
    )));
    s.push(a(f::user_action_state(
        "ua-unknown-del",
        U::Unknown,
        None,
        Some("后台服务中断"),
        None,
    )));
    s.push(a(f::ev(pb::domain_event::Event::ConversationDeleting(
        pb::ConversationDeleting {
            conversation_id: c.into(),
        },
    ))));
    s
}

fn turn_end() -> Vec<Step> {
    let c = "c-turn-end";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.extend(exchange(c, "ok", "改一下按钮颜色", "改好了。"));
    s.push(a(f::turn_finished(
        "t-ok",
        T::Completed,
        true,
        Some(f::changes(&["src/button.css"], 3, 1)),
    )));
    s.extend(exchange(c, "fail", "再跑一遍", "开始跑……"));
    s.push(a(f::turn_state(
        "t-fail",
        T::Running,
        T::Failed,
        Some("模型暂时不可用：额度已用完"),
    )));
    s.push(a(f::turn_finished("t-fail", T::Failed, false, None)));
    s.extend(exchange(c, "cancel", "顺便重构一下", "好，我先……"));
    s.push(a(f::turn_state(
        "t-cancel",
        T::Running,
        T::Cancelling,
        None,
    )));
    s.push(a(f::turn_finished("t-cancel", T::Cancelled, false, None)));
    s
}

fn turn_failed() -> Vec<Step> {
    let c = "c-turn-failed";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    // 一开始就失败：prompt 没发出去，助手消息是空的、状态为失败（T05 补丁带来的形状）。
    s.push(a(f::user_message("m-start-u", c, "帮我看看构建为什么挂了")));
    s.push(a(f::turn_queued("t-start", c, IWORK)));
    s.push(a(f::turn_state("t-start", T::Queued, T::Running, None)));
    s.push(a(f::assistant_message(
        "m-start-a",
        c,
        "t-start",
        IWORK,
        "iWork",
    )));
    s.push(a(f::turn_state(
        "t-start",
        T::Running,
        T::Failed,
        Some("Claude Code 启动后立刻退出了"),
    )));
    s.push(a(f::message_ended(
        c,
        "m-start-a",
        "",
        pb::MessageStatus::Failed,
    )));
    s.push(a(f::turn_finished("t-start", T::Failed, false, None)));
    // 输出一部分后失败。
    s.push(a(f::user_message("m-mid-u", c, "再试一次")));
    s.push(a(f::turn_queued("t-mid", c, IWORK)));
    s.push(a(f::turn_state("t-mid", T::Queued, T::Running, None)));
    s.push(a(f::assistant_message(
        "m-mid-a", c, "t-mid", IWORK, "iWork",
    )));
    s.push(a(f::turn_state(
        "t-mid",
        T::Running,
        T::Failed,
        Some("模型暂时不可用"),
    )));
    s.push(a(f::message_ended(
        c,
        "m-mid-a",
        "我先看一下 CI 的日志，第一处报错在",
        pb::MessageStatus::Failed,
    )));
    s.push(a(f::turn_finished("t-mid", T::Failed, false, None)));
    s
}

fn resume_card() -> Vec<Step> {
    let c = "c-resume";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.push(a(f::user_message("m-resume-u", c, "把依赖都升级一遍")));
    s.push(a(f::turn_queued("t-resume", c, IWORK)));
    s.push(a(f::turn_state("t-resume", T::Queued, T::Running, None)));
    s.push(a(f::assistant_message(
        "m-resume-a",
        c,
        "t-resume",
        IWORK,
        "iWork",
    )));
    s.push(a(f::message_ended(
        c,
        "m-resume-a",
        "先升级 tokio……",
        pb::MessageStatus::Interrupted,
    )));
    s.push(a(f::turn_state(
        "t-resume",
        T::Running,
        T::Interrupted,
        None,
    )));
    s
}

fn sidebar_dots() -> Vec<Step> {
    let mut s = base();
    for c in ["c-idle", "c-running", "c-waiting", "c-error"] {
        s.push(a(f::project_conversation(c, PROJECT)));
    }
    s.extend(exchange(
        "c-idle",
        "idle",
        "看下 README",
        "看过了，没问题。",
    ));
    s.push(a(f::turn_finished("t-idle", T::Completed, false, None)));
    s.extend(exchange("c-running", "running", "跑全量测试", "正在跑……"));
    s.extend(exchange("c-waiting", "waiting", "发布一下", "需要你批准。"));
    s.push(a(f::approval(
        "ap-dot",
        "c-waiting",
        "t-waiting",
        f::command_action("npm publish"),
        pb::ApprovalKind::Ask,
        &[pb::RememberScope::Once],
    )));
    s.extend(exchange("c-error", "error", "部署", "部署失败。"));
    s.push(a(f::turn_finished("t-error", T::Failed, false, None)));
    s
}

fn assistants_20() -> Vec<Step> {
    let mut s = base();
    for i in 1..=20 {
        let id = format!("as-{i:02}");
        let dm = format!("c-dm-{i:02}");
        s.push(a(f::assistant(&id, &format!("助手{i:02}"), false)));
        s.push(a(f::dm(&dm, &id)));
        if i % 3 == 0 {
            let t = format!("t-as-{i:02}");
            let m = format!("m-as-{i:02}");
            s.push(a(f::user_message(&format!("{m}-u"), &dm, "在吗")));
            s.push(a(f::turn_queued(&t, &dm, &id)));
            s.push(a(f::turn_state(&t, T::Queued, T::Running, None)));
            s.push(a(f::assistant_message(
                &m,
                &dm,
                &t,
                &id,
                &format!("助手{i:02}"),
            )));
            s.push(a(f::message_completed(
                &dm,
                &m,
                "在的，有什么要做？\n第二行",
            )));
            if i % 5 != 0 {
                s.push(a(f::turn_finished(&t, T::Completed, false, None)));
            }
        }
    }
    s
}

fn quota_missing() -> Vec<Step> {
    world(false)
}

fn stream_16x100() -> Vec<Step> {
    let mut s = base();
    let convs: Vec<String> = (0..16).map(|i| format!("c-s{i:02}")).collect();
    for (i, c) in convs.iter().enumerate() {
        s.push(a(f::project_conversation(c, PROJECT)));
        s.push(a(f::user_message(&format!("m-s{i:02}-u"), c, "持续输出")));
        s.push(a(f::turn_queued(&format!("t-s{i:02}"), c, IWORK)));
        s.push(a(f::turn_state(
            &format!("t-s{i:02}"),
            T::Queued,
            T::Running,
            None,
        )));
    }
    for round in 0..100 {
        for (i, c) in convs.iter().enumerate() {
            let m = format!("m-s{i:02}-{round:03}");
            s.push(a(f::assistant_message(
                &m,
                c,
                &format!("t-s{i:02}"),
                IWORK,
                "iWork",
            )));
            let mut body = String::new();
            for k in 0..4 {
                let piece = format!("第 {round} 条第 {k} 段。");
                s.push(Step::Transient(f::delta(&m, body.len() as u64, &piece)));
                body.push_str(&piece);
            }
            s.push(a(f::message_completed(c, &m, &body)));
        }
        s.push(Step::Sleep(Duration::from_millis(10)));
    }
    s
}

pub const HISTORY_LEN: usize = 100_000;

fn history_100k() -> Vec<Step> {
    let c = "c-history";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    for i in 0..HISTORY_LEN / 2 {
        s.push(a(f::user_message(
            &format!("m-h{i:06}-u"),
            c,
            &format!("第 {i} 个问题"),
        )));
        let m = format!("m-h{i:06}-a");
        // 历史消息不挂回合：只为量滚动与切换，不需要回合行。
        let mut e = f::assistant_message(&m, c, "", IWORK, "iWork");
        if let Some(pb::domain_event::Event::MessageAppended(x)) = &mut e.event
            && let Some(msg) = &mut x.message
        {
            msg.turn_id = None;
        }
        s.push(a(e));
        s.push(a(f::message_completed(c, &m, &format!("第 {i} 个回答"))));
    }
    s
}

fn xss_corpus() -> Vec<Step> {
    let c = "c-xss";
    let t = "t-xss";
    let mut s = base();
    s.push(a(f::project(
        "p-xss",
        "<b onmouseover=alert(1)>proj</b>",
        "~/work/<script>",
    )));
    s.push(a(f::project_conversation(c, "p-xss")));
    s.push(a(f::user_message(
        "m-xss-u",
        c,
        "<img src=x onerror=alert(1)> 标题",
    )));
    s.push(a(f::turn_queued(t, c, IWORK)));
    s.push(a(f::turn_state(t, T::Queued, T::Running, None)));
    s.push(a(f::assistant_message(
        "m-xss-a",
        c,
        t,
        IWORK,
        "<script>alert(1)</script>",
    )));
    s.push(a(f::message_completed(
        c,
        "m-xss-a",
        "<script>window.__probe=1</script>\n[点我](javascript:alert(1))\n<iframe src=\"https://example.com\"></iframe>\n![远程图](https://example.com/x.png)\n<a href=\"#\" onclick=\"alert(1)\">x</a>",
    )));
    s.push(a(f::tool_started(
        t,
        "tc-xss",
        "<svg onload=alert(1)>",
        "echo '<script>alert(1)</script>'",
    )));
    s.push(a(f::tool_finished(t, "tc-xss", true)));
    s.push(a(f::ev(pb::domain_event::Event::AttachmentStored(
        pb::AttachmentStored {
            attachment: Some(pb::Attachment {
                id: "att-xss".into(),
                conversation_id: c.into(),
                sha256: "00".repeat(32),
                size: 10,
                mime: "image/png".into(),
                name: "<svg onload=alert(1)>.png".into(),
                origin: None,
                needs_confirmation: false,
                created_at: None,
            }),
        },
    ))));
    s.push(a(f::turn_finished(
        t,
        T::Completed,
        true,
        Some(f::changes(&["<script>.js"], 1, 0)),
    )));
    s
}

fn reconnect_basic() -> Vec<Step> {
    let c = "c-reconnect";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.extend(exchange(c, "r1", "第一问", "第一答"));
    s.push(a(f::turn_finished("t-r1", T::Completed, false, None)));
    s.push(Step::DisconnectAll);
    s.extend(exchange(c, "r2", "第二问", "第二答"));
    s.push(a(f::turn_finished("t-r2", T::Completed, false, None)));
    s
}

/// 私聊里的当次说明与分隔线都按项目 id 现查项目名（derive::row::project_name）：
/// 先切到 demo（出「已切换为 demo」），聊一轮；再来一轮上下文是 web 的消息，中间没有切换说明，出分隔线；
/// 最后两个项目都改名，说明与分隔线要跟着变（项目表变化要标脏这两种行）。
fn dm_project_switch() -> Vec<Step> {
    let c = IWORK_DM;
    let mut s = base();
    s.push(a(f::project(OTHER_PROJECT, "web", "~/work/web")));
    s.push(a(f::dm_project_switched(c, Some(PROJECT))));
    for (n, project, q, ans) in [
        ("dm1", PROJECT, "demo 的测试怎么跑", "pnpm test"),
        ("dm2", OTHER_PROJECT, "web 呢", "也是 pnpm test"),
    ] {
        let turn = format!("t-{n}");
        let reply = format!("m-{n}-a");
        s.push(a(f::in_project(
            f::user_message(&format!("m-{n}-u"), c, q),
            project,
        )));
        s.push(a(f::turn_queued(&turn, c, IWORK)));
        s.push(a(f::turn_state(&turn, T::Queued, T::Running, None)));
        s.push(a(f::in_project(
            f::assistant_message(&reply, c, &turn, IWORK, "iWork"),
            project,
        )));
        s.push(a(f::message_completed(c, &reply, ans)));
        s.push(a(f::turn_finished(&turn, T::Completed, false, None)));
    }
    // 隔开几帧再改名：说明与分隔线先按旧名字推出去，改名才是对这两行的增量
    s.push(Step::Sleep(Duration::from_millis(100)));
    s.push(a(f::project_renamed(PROJECT, "demo-改名")));
    s.push(a(f::project_renamed(OTHER_PROJECT, "web-改名")));
    s
}

/// 四个助手各停在一种状态：阿森工作中（浮动）、林岚等你（有待审批）、老周最近一轮失败（sorry）、
/// 圆团子（没换形象、配置里的颜色）空闲。打开老周的私聊，消息头像也是 sorry。
fn characters_preview_only() -> Vec<Step> {
    let mut s = base();
    for (id, name) in [
        ("as-asen", "阿森"),
        ("as-linlan", "林岚"),
        ("as-laozhou", "老周"),
        ("as-blob", "小团"),
    ] {
        s.push(a(f::assistant(id, name, false)));
        s.push(a(f::dm(&format!("c-dm-{}", &id[3..]), id)));
    }
    // 阿森：回合在跑
    s.push(a(f::user_message("m-asen-u", "c-dm-asen", "把测试补上")));
    s.push(a(f::turn_queued("t-asen", "c-dm-asen", "as-asen")));
    s.push(a(f::turn_state("t-asen", T::Queued, T::Running, None)));
    // 林岚：等你批一条命令
    s.push(a(f::user_message(
        "m-linlan-u",
        "c-dm-linlan",
        "清一下构建目录",
    )));
    s.push(a(f::turn_queued("t-linlan", "c-dm-linlan", "as-linlan")));
    s.push(a(f::turn_state("t-linlan", T::Queued, T::Running, None)));
    s.push(a(f::approval(
        "ap-linlan",
        "c-dm-linlan",
        "t-linlan",
        f::command_action("rm -rf target"),
        pb::ApprovalKind::Ask,
        &[],
    )));
    s.push(a(f::turn_state(
        "t-linlan",
        T::Running,
        T::WaitingApproval,
        None,
    )));
    // 老周：最近一轮失败
    s.push(a(f::user_message(
        "m-laozhou-u",
        "c-dm-laozhou",
        "看看为什么挂了",
    )));
    s.push(a(f::turn_queued("t-laozhou", "c-dm-laozhou", "as-laozhou")));
    s.push(a(f::turn_state("t-laozhou", T::Queued, T::Running, None)));
    s.push(a(f::assistant_message(
        "m-laozhou-a",
        "c-dm-laozhou",
        "t-laozhou",
        "as-laozhou",
        "老周",
    )));
    s.push(a(f::turn_state(
        "t-laozhou",
        T::Running,
        T::Failed,
        Some("模型暂时不可用"),
    )));
    s.push(a(f::message_ended(
        "c-dm-laozhou",
        "m-laozhou-a",
        "我先看一下日志",
        pb::MessageStatus::Failed,
    )));
    s.push(a(f::turn_finished("t-laozhou", T::Failed, false, None)));
    s
}

/// 三个形象各自的私聊里最近一轮都失败：侧栏三个头像、阿森私聊里的消息头像都是 sorry。
/// 失败放在打开对话之后，录像里有「工作中 → 出错」的增量。
fn characters_sorry_preview_only() -> Vec<Step> {
    let mut s = base();
    let who = [
        ("as-asen", "阿森", "c-dm-asen"),
        ("as-linlan", "林岚", "c-dm-linlan"),
        ("as-laozhou", "老周", "c-dm-laozhou"),
    ];
    for (id, name, dm) in who {
        s.push(a(f::assistant(id, name, false)));
        s.push(a(f::dm(dm, id)));
        let t = format!("t-{id}");
        let m = format!("m-{id}");
        s.push(a(f::user_message(&format!("{m}-u"), dm, "看看为什么挂了")));
        s.push(a(f::turn_queued(&t, dm, id)));
        s.push(a(f::turn_state(&t, T::Queued, T::Running, None)));
        s.push(a(f::assistant_message(&m, dm, &t, id, name)));
    }
    s.push(Step::OpenHere);
    for (id, _, dm) in who {
        let t = format!("t-{id}");
        let m = format!("m-{id}");
        s.push(a(f::turn_state(
            &t,
            T::Running,
            T::Failed,
            Some("模型暂时不可用"),
        )));
        s.push(a(f::message_ended(
            dm,
            &m,
            "我先看一下日志",
            pb::MessageStatus::Failed,
        )));
        s.push(a(f::turn_finished(&t, T::Failed, false, None)));
    }
    s
}

/// 一段对话历史：n 轮一问一答（都已结束）。
fn history(c: &str, tag: &str, n: usize) -> Vec<Step> {
    let mut s = Vec::new();
    for i in 0..n {
        let id = format!("{tag}{i}");
        s.extend(exchange(
            c,
            &id,
            &format!("第 {i} 问"),
            &format!("第 {i} 答"),
        ));
        s.push(a(f::turn_finished(
            &format!("t-{id}"),
            T::Completed,
            false,
            None,
        )));
    }
    s
}

fn back_to_latest_notice() -> Vec<Step> {
    let c = "c-q9";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.extend(history(c, "q9-", 30));
    s.push(Step::OpenHere);
    // 客户端停在最老的几行上（场景的 client 动作），这时断线、重连拿到的是快照
    s.push(Step::DisconnectForSnapshot);
    s.extend(history(c, "q9-new-", 1));
    s
}

fn new_below_count() -> Vec<Step> {
    let c = "c-scrolled";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.extend(history(c, "nb-", 40));
    s.push(Step::OpenHere);
    // 客户端往上翻了一页、超出上限，不再接着最新：新消息只计数
    s.extend(history(c, "nb-new-", 2));
    s
}

/// 输入框被挡住的几种情形：先有一轮对话，打开后再变。
fn composer_block(change: Step) -> Vec<Step> {
    let c = "c-block";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.extend(history(c, "blk-", 1));
    s.push(Step::OpenHere);
    s.push(change);
    s
}

fn composer_path_missing() -> Vec<Step> {
    composer_block(a(f::project_path_status(PROJECT, pb::PathStatus::Missing)))
}

fn composer_verifying() -> Vec<Step> {
    composer_block(a(f::runtime(pb::RuntimeState::ReadyUnverified)))
}

fn composer_incompatible() -> Vec<Step> {
    composer_block(a(f::runtime(pb::RuntimeState::Incompatible)))
}

fn composer_not_installed() -> Vec<Step> {
    composer_block(a(f::runtime(pb::RuntimeState::Missing)))
}

fn composer_not_logged_in() -> Vec<Step> {
    composer_block(a(f::runtime(pb::RuntimeState::NotLoggedIn)))
}

fn composer_offline() -> Vec<Step> {
    composer_block(Step::StopListening)
}

fn attachments_tray() -> Vec<Step> {
    let c = "c-attach";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.extend(history(c, "att-", 1));
    s.push(Step::OpenHere);
    s.push(a(f::attachment("att-1", c, "设计稿.png", 345_678)));
    s.push(a(f::attachment("att-2", c, "接口说明.pdf", 2_345_678)));
    s
}

/// 侧栏项目三种样子：置顶、找不到路径、未读多（超过 99 写「99+」）。停在首页。
fn projects_sidebar() -> Vec<Step> {
    let mut s = base();
    s.push(a(f::project("p-legacy", "legacy-app", "~/work/legacy")));
    s.push(a(f::project("p-busy", "busy-service", "~/work/busy")));
    s.push(a(f::project_pinned(PROJECT, true)));
    s.push(a(f::project_path_status(
        "p-legacy",
        pb::PathStatus::Missing,
    )));
    let c = "c-busy";
    s.push(a(f::project_conversation(c, "p-busy")));
    s.push(a(f::turn_queued("t-busy", c, IWORK)));
    s.push(a(f::turn_state("t-busy", T::Queued, T::Running, None)));
    for i in 0..120 {
        let m = format!("m-busy-{i}");
        s.push(a(f::assistant_message(&m, c, "t-busy", IWORK, "iWork")));
        s.push(a(f::message_completed(c, &m, &format!("进度 {i}"))));
    }
    s.push(a(f::turn_finished("t-busy", T::Completed, false, None)));
    s
}

fn tool_output() -> Vec<Step> {
    let c = "c-tool";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.extend(exchange(c, "tool", "跑一下测试", "好，我来跑。"));
    s.push(a(f::tool_started("t-tool", "tc-out", "Bash", "cargo test")));
    s.push(Step::OpenHere);
    let lines = [
        "   Compiling iwork v0.1.0\n",
        "    Finished test profile in 3.2s\n",
        "running 12 tests\n",
        "test result: ok. 12 passed; 0 failed\n",
    ];
    let mut offset = 0;
    for l in lines {
        s.push(Step::Transient(f::tool_output("tc-out", offset, l)));
        offset += l.len() as u64;
    }
    s.push(a(f::tool_finished("t-tool", "tc-out", true)));
    s.push(a(f::turn_finished("t-tool", T::Completed, false, None)));
    s
}

/// 改名后侧栏要显示新标题，靠的是通用的那条路：摘要失效 → 重查 → 显示新值，和事件种类无关（supervisor 2026-11-10）。
/// 这里先直接改掉表里的标题（等同后台受理了改名），再追加一条已有事件让侧栏订阅命中：同一对话的
/// `ConversationRead`，`last_read_seq` 不变（对话从没读过，仍是 0），所以除了触发失效不改任何东西。
/// T10 补上 ConversationRenamed 后换成真事件。
fn rename_via_summary_refresh_until_t10() -> Vec<Step> {
    let c = "c-rename";
    let mut s = base();
    s.push(a(f::project_conversation(c, PROJECT)));
    s.push(a(f::user_message("m-rename-u", c, "这是自动标题的来源")));
    s.push(Step::Mutate(Box::new(|t: &mut Tables| {
        if let Some(row) = t.conversations.get_mut("c-rename") {
            row.conversation.title = RENAMED_TITLE.into();
            row.conversation.title_source = pb::TitleSource::User.into();
        }
    })));
    s.push(a(f::conversation_read(c, 0)));
    s
}

/// `rename-via-summary-refresh-until-t10` 改成的标题。
pub const RENAMED_TITLE: &str = "改过的标题";
