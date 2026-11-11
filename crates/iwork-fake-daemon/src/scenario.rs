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
    build: fn() -> Vec<Step>,
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
    },
    Scenario {
        name: "conversation-basic",
        about: "一问一答、工具活动、回合结束印章",
        build: conversation_basic,
        focus: Some("c-basic"),
    },
    Scenario {
        name: "streaming",
        about: "一条助手消息分 20 段流式到达",
        build: streaming,
        focus: Some("c-stream"),
    },
    Scenario {
        name: "approval-states",
        about: "审批：待定、已允许、已拒绝、已作废、切档放行、必问、沙箱外",
        build: approval_states,
        focus: Some("c-approval"),
    },
    Scenario {
        name: "user-action-states",
        about: "亲手执行：待定、执行中、已执行、已忽略、失败、已作废、结果未知",
        build: user_action_states,
        focus: Some("c-user-action"),
    },
    Scenario {
        name: "user-action-unknown-deleting",
        about: "结果未知的卡片挡住删除，对话停在删除中",
        build: user_action_unknown_deleting,
        focus: Some("c-ua-deleting"),
    },
    Scenario {
        name: "turn-end",
        about: "回合结束：搞定印章、失败、取消",
        build: turn_end,
        focus: Some("c-turn-end"),
    },
    Scenario {
        name: "turn-failed",
        about: "回合失败：一开始就失败（空的失败消息）、输出一部分后失败（ui-m1.md 2a 第 10 条）",
        focus: Some("c-turn-failed"),
        build: turn_failed,
    },
    Scenario {
        name: "resume-card",
        about: "崩溃中断的回合：继续 / 放弃卡片",
        build: resume_card,
        focus: Some("c-resume"),
    },
    Scenario {
        name: "sidebar-dots",
        about: "侧栏圆点四种状态与未读",
        build: sidebar_dots,
        focus: None,
    },
    Scenario {
        name: "assistants-20",
        about: "20 个助手，部分有最近动态",
        build: assistants_20,
        focus: None,
    },
    Scenario {
        name: "quota-missing",
        about: "额度取不到",
        build: quota_missing,
        focus: None,
    },
    Scenario {
        name: "stream-16x100",
        about: "16 个对话各 100 条流式消息（基准用）",
        build: stream_16x100,
        focus: Some("c-s00"),
    },
    Scenario {
        name: "history-100k",
        about: "一个对话十万条消息",
        build: history_100k,
        focus: Some("c-history"),
    },
    Scenario {
        name: "xss-corpus",
        about: "正文、标题、文件名、工具输出里的脚本与链接",
        build: xss_corpus,
        focus: Some("c-xss"),
    },
    Scenario {
        name: "reconnect-basic",
        about: "中途断开全部连接后继续",
        build: reconnect_basic,
        focus: Some("c-reconnect"),
    },
    Scenario {
        name: "dm-project-switch",
        about: "助手私聊换项目：「已切换为」说明、项目分隔线；打开后两个项目都改名",
        build: dm_project_switch,
        focus: Some(IWORK_DM),
    },
    Scenario {
        name: "rename-via-summary-refresh-until-t10",
        about: "测试专用：直接改摘要里的标题，再发一条落在侧栏订阅上的已有事件触发摘要失效。T10 补上 ConversationRenamed 后换成真事件",
        build: rename_via_summary_refresh_until_t10,
        focus: Some("c-rename"),
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
