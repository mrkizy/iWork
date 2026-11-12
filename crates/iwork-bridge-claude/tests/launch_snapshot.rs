//! 起 CLI 的计划（参数 + 设置 JSON + 环境变量）的快照与反向断言（T06 方案 15.3、19 节 S4）。
//!
//! 手动、自动、全开三档 × 新开 / 续接，加 AI 审查回合，各一份快照。
//! 更新：`IWORK_UPDATE_SNAPSHOTS=1 cargo test -p iwork-bridge-claude --test launch_snapshot`。
//!
//! 只在 unix 上跑：路径拼接在 Windows 上是反斜杠，M1 不支持 Windows（AGENTS.md）。
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use iwork_bridge_claude::launch::args::NEVER_PASSED;
use iwork_bridge_claude::launch::settings::{HOOK_TIMEOUT_SECS, SELECTED};
use iwork_bridge_claude::launch::{BridgeEnv, Channel, LaunchPlan, Opening, Start, build_launch};
use iwork_bridge_claude::setup::{
    AllowWrite, CommandPattern, Constraints, Model, Network, Setup, Tools,
};
use serde_json::{Value, json};

const DATA: &str = "/Users/u/Library/Application Support/iWork";

/// 三档。沙箱底线三档相同；只写白名单在全开档放开，全开档不提供沙箱外重试（AGENTS.md）。
#[derive(Debug, Clone, Copy)]
enum Tier {
    Manual,
    Auto,
    FullAccess,
}

fn setup(tier: Tier, tools: Tools) -> Setup {
    let (allow_write, retry) = match tier {
        Tier::Manual | Tier::Auto => (AllowWrite::Whitelist(vec!["/work".into()]), true),
        Tier::FullAccess => (AllowWrite::Anywhere, false),
    };
    Setup {
        model: Model {
            id: "claude-haiku-5-5".into(),
            effort: "medium",
        },
        constraints: Constraints {
            deny_read_write: vec!["/Users/u/.ssh".into(), DATA.into()],
            read_only: vec![
                format!("{DATA}/attachments").into(),
                format!("{DATA}/imports").into(),
            ],
            allow_write,
            excluded_commands: vec![
                CommandPattern {
                    program: "git".into(),
                    subcommand: Some("push".into()),
                },
                CommandPattern {
                    program: "gh".into(),
                    subcommand: None,
                },
            ],
            allow_unsandboxed_retry: retry,
            network: Network::Open,
        },
        tools,
        disabled_tools: vec!["CronCreate".into(), "RemoteTrigger".into()],
        plugin_dirs: vec![format!("{DATA}/imports/plugin-a").into()],
        builtin_tool_allowlist: None,
    }
}

/// 表外 CLI 版本的会话：带内置工具白名单（方案第 9 节，P11 的 d）。
fn with_allowlist(mut s: Setup) -> Setup {
    s.builtin_tool_allowlist = Some(vec!["Bash".into(), "Edit".into(), "Read".into()]);
    s
}

fn opening(start: Start) -> Opening {
    Opening {
        cwd: "/work".into(),
        system_prompt: "你是小助手。".into(),
        start,
    }
}

fn new_session() -> Start {
    Start::New {
        session_id: "11111111-2222-4333-8444-555555555555".into(),
    }
}

fn resume() -> Start {
    Start::Resume {
        session_id: "66666666-7777-4888-8999-000000000000".into(),
    }
}

fn plan(setup: &Setup, start: Start) -> LaunchPlan {
    build_launch(
        setup,
        &opening(start),
        &BridgeEnv {
            cli: "/usr/local/bin/claude".into(),
            hook_program: "/Applications/iWork.app/Contents/MacOS/iwork-claude-hook".into(),
        },
        &Channel {
            dir: format!("{DATA}/run/h/0a1b2c3d4e5f").into(),
            token_hex: "ab".repeat(32),
            bridge_pid: 4242,
            hook_deadline: Duration::from_secs(7185),
        },
    )
    .unwrap()
}

/// 每个快照对应的 (名字, 计划)。
fn cases() -> Vec<(&'static str, LaunchPlan)> {
    vec![
        (
            "manual_new",
            plan(&setup(Tier::Manual, Tools::Default), new_session()),
        ),
        (
            "manual_resume",
            plan(&setup(Tier::Manual, Tools::Default), resume()),
        ),
        (
            "auto_new",
            plan(&setup(Tier::Auto, Tools::Default), new_session()),
        ),
        (
            "auto_resume",
            plan(&setup(Tier::Auto, Tools::Default), resume()),
        ),
        (
            "full_access_new",
            plan(&setup(Tier::FullAccess, Tools::Default), new_session()),
        ),
        (
            "full_access_resume",
            plan(&setup(Tier::FullAccess, Tools::Default), resume()),
        ),
        (
            "ai_review_new",
            plan(&setup(Tier::Manual, Tools::None), new_session()),
        ),
        (
            "unverified_cli_allowlist_new",
            plan(
                &with_allowlist(setup(Tier::Manual, Tools::Default)),
                new_session(),
            ),
        ),
    ]
}

/// 键按字典序重排：serde_json 开没开 `preserve_order`（全量构建时被别的 crate 打开）都得到同一份文字。
fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted(&m[k])))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

fn render(p: &LaunchPlan) -> String {
    let v = json!({
        "program": p.program,
        "args": p.args,
        "cwd": p.cwd,
        "env": p.env.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
        "settings": p.settings,
        "mcp": p.mcp,
    });
    format!("{}\n", serde_json::to_string_pretty(&sorted(&v)).unwrap())
}

fn snapshot_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/snapshots/launch/{name}.json"))
}

#[test]
fn launch_plans_match_snapshots() {
    let update = std::env::var_os("IWORK_UPDATE_SNAPSHOTS").is_some();
    for (name, p) in cases() {
        let text = render(&p);
        let path = snapshot_path(name);
        if update {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &text).unwrap();
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("快照 {} 读不了（{e}）", path.display()));
        assert_eq!(text, expected, "{name}：启动计划变了，确认后更新快照");
    }
}

fn hooks(p: &LaunchPlan, event: &str) -> Vec<Value> {
    p.settings["hooks"][event]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["hooks"].as_array().unwrap().clone())
        .collect()
}

/// 3.1「不传的参数」一个都不出现。
#[test]
fn never_passed_args_are_absent() {
    for (name, p) in cases() {
        for bad in NEVER_PASSED {
            assert!(!p.args.iter().any(|a| a == bad), "{name}：{bad}");
        }
    }
}

/// 3.4a 第 2 条：权限模式是 dontAsk。
#[test]
fn default_mode_is_dont_ask() {
    assert_eq!(SELECTED.default_mode, "dontAsk");
    for (name, p) in cases() {
        assert_eq!(
            p.settings["permissions"]["defaultMode"], "dontAsk",
            "{name}"
        );
        assert_eq!(
            p.settings["permissions"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["defaultMode", "disableBypassPermissionsMode"],
            "{name}：选定配置不带别的权限键"
        );
    }
}

/// 3.4a 第 1 条：每个 PreToolUse hook 都带 onFailure: block，且确实有 hook（对照）。
#[test]
fn every_pre_tool_use_hook_blocks_on_failure() {
    for (name, p) in cases() {
        let pre = hooks(&p, "PreToolUse");
        assert!(!pre.is_empty(), "{name}：对照，确实有 PreToolUse hook");
        for h in pre {
            assert_eq!(h["onFailure"], "block", "{name}：{h}");
            assert_eq!(h["timeout"], HOOK_TIMEOUT_SECS, "{name}");
            assert!(
                h["command"].as_str().unwrap().ends_with(" pre-tool-use"),
                "{name}：{h}"
            );
        }
    }
}

/// 这两个键会让工作区外该放行的读写在 hook 允许后仍被拒（S0 外层保护预检）；验证专用的探针 hook
/// 永远不进会话的设置（10.1）；hook 不被整体关掉。
#[test]
fn settings_have_no_forbidden_keys() {
    for (name, p) in cases() {
        let text = p.settings.to_string();
        for bad in [
            "blockReadsOutsideWorkingDirectories",
            "blockWritesOutsideWorkingDirectories",
            "verify-hook",
            "allowUnixSockets",
        ] {
            assert!(!text.contains(bad), "{name}：{bad}");
        }
        assert_eq!(p.settings["disableAllHooks"], false, "{name}");
        assert_eq!(p.settings["sandbox"]["enabled"], true, "{name}");
        assert_eq!(
            p.settings["sandbox"]["autoAllowBashIfSandboxed"], false,
            "{name}"
        );
    }
}

/// 沙箱底线三档相同（技术设计 7.0）：denyRead / denyWrite / credentials 一样；不同的只有可写范围与沙箱外重试。
#[test]
fn baseline_is_identical_across_tiers() {
    let base = |p: &LaunchPlan| {
        let sb = &p.settings["sandbox"];
        (
            sb["filesystem"]["denyRead"].clone(),
            sb["filesystem"]["denyWrite"].clone(),
            sb["credentials"].clone(),
        )
    };
    let manual = plan(&setup(Tier::Manual, Tools::Default), new_session());
    let full = plan(&setup(Tier::FullAccess, Tools::Default), new_session());
    assert_eq!(base(&manual), base(&full));
    // 对照：两档确实不同在可写范围与重试上
    assert_ne!(
        manual.settings["sandbox"]["filesystem"]["allowWrite"],
        full.settings["sandbox"]["filesystem"]["allowWrite"]
    );
    assert_eq!(manual.settings["sandbox"]["allowUnsandboxedCommands"], true);
    assert_eq!(full.settings["sandbox"]["allowUnsandboxedCommands"], false);
}

/// 新开传 --session-id、续接传 --resume，不会两个都有；AI 审查回合才有 --tools ""。
#[test]
fn start_and_tools_args() {
    let pos = |args: &[String], flag: &str| args.iter().position(|a| a == flag);
    let n = plan(&setup(Tier::Manual, Tools::Default), new_session());
    assert!(pos(&n.args, "--session-id").is_some() && pos(&n.args, "--resume").is_none());
    let r = plan(&setup(Tier::Manual, Tools::Default), resume());
    assert!(pos(&r.args, "--resume").is_some() && pos(&r.args, "--session-id").is_none());
    assert!(pos(&n.args, "--tools").is_none());
    let review = plan(&setup(Tier::Manual, Tools::None), new_session());
    let at = pos(&review.args, "--tools").expect("AI 审查回合传 --tools");
    assert_eq!(review.args[at + 1], "");
}

/// 令牌只在环境变量里，不进参数、不进设置文件；设置文件屏蔽它进沙箱内命令（G9b）。
#[test]
fn token_stays_in_env_only() {
    let p = plan(&setup(Tier::Manual, Tools::Default), new_session());
    let token = "ab".repeat(32);
    assert!(
        p.env
            .iter()
            .any(|(k, v)| *k == "IWORK_HOOK_TOKEN" && *v == token)
    );
    assert!(!p.args.iter().any(|a| a.contains(&token)));
    assert!(!p.settings.to_string().contains(&token));
    assert_eq!(
        p.settings["sandbox"]["credentials"]["envVars"],
        json!([{ "name": "IWORK_HOOK_TOKEN", "mode": "deny" }])
    );
}

/// cwd 不是绝对路径：拒绝，点名 cwd。
#[test]
fn relative_cwd_is_refused() {
    let mut o = opening(new_session());
    o.cwd = "work".into();
    let err = build_launch(
        &setup(Tier::Manual, Tools::Default),
        &o,
        &BridgeEnv {
            cli: "/c".into(),
            hook_program: "/h".into(),
        },
        &Channel {
            dir: "/d".into(),
            token_hex: String::new(),
            bridge_pid: 1,
            hook_deadline: Duration::from_secs(1),
        },
    )
    .unwrap_err();
    assert_eq!(err.field, "cwd");
}

/// `--tools` 出现的次数与跟在后面的值。
fn tools_flags(args: &[String]) -> Vec<&str> {
    args.windows(2)
        .filter(|w| w[0] == "--tools")
        .map(|w| w[1].as_str())
        .collect()
}

/// 有白名单 → `--tools a,b,c`，`--disallowedTools` 照旧（两者同用时禁用优先，G8d）。
#[test]
fn allowlist_becomes_tools_flag() {
    let p = plan(
        &with_allowlist(setup(Tier::Manual, Tools::Default)),
        new_session(),
    );
    assert_eq!(tools_flags(&p.args), ["Bash,Edit,Read"]);
    let at = p
        .args
        .iter()
        .position(|a| a == "--disallowedTools")
        .expect("禁用照旧");
    assert_eq!(p.args[at + 1], "CronCreate,RemoteTrigger");
}

/// 没有白名单（表内版本）→ 不传 `--tools`。
#[test]
fn no_allowlist_no_tools_flag() {
    let p = plan(&setup(Tier::Manual, Tools::Default), new_session());
    assert!(tools_flags(&p.args).is_empty(), "{:?}", p.args);
}

/// AI 审查回合仍是 `--tools ""`：白名单管不到它，两者都有时以它为准，而且只出现一次。
#[test]
fn ai_review_turn_keeps_empty_tools() {
    let p = plan(
        &with_allowlist(setup(Tier::Manual, Tools::None)),
        new_session(),
    );
    assert_eq!(tools_flags(&p.args), [""]);
}
