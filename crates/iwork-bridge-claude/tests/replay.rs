//! 录制样本回放（T06 方案 15.2 验收 5、15.4）：`tests/fixtures/<CLI 版本>/*.jsonl` 逐行喂给 [`StreamMapper`]，
//! 每份样本一个快照（转换出的输出序列与上报），另有手写期望的断言。
//!
//! 回合怎么切：样本里没有记用户消息什么时候发的。回放照产品的次序：开头先开一个回合（产品里用户消息写给 CLI
//! 之前就开），每个回合结束后后面还有 `result` 就再开一个。`interrupt_set_model` 的第一轮照录制时那样请求了中断。
//!
//! 更新快照：`IWORK_UPDATE_SNAPSHOTS=1 cargo test -p iwork-bridge-claude --test replay`。

use std::path::{Path, PathBuf};

use iwork_bridge_claude::cli::errors::TurnEnd;
use iwork_bridge_claude::cli::facts::IsolationBroken;
use iwork_bridge_claude::cli::map::{Fatal, Out, QuotaWindow, Report, StreamMapper};
use serde_json::Value;

const VERSION: &str = "2.1.322";
const SAMPLES: [&str; 6] = [
    "text_multiturn",
    "tool_allowed",
    "tool_denied",
    "resume",
    "bad_model",
    "interrupt_set_model",
];

fn fixture(name: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/{VERSION}/{name}.jsonl"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("样本 {} 读不了（{e}）", path.display()))
        .lines()
        .filter(|l| !l.starts_with("{\"_fixture\""))
        .map(str::to_owned)
        .collect()
}

/// 回放一份样本，返回全部输出与第一条致命错误（有的话就停在那里）。
fn replay(lines: &[String], cancel_first_turn: bool) -> (Vec<Out>, Option<Fatal>) {
    let results_total = lines
        .iter()
        .filter(|l| l.contains("\"type\":\"result\""))
        .count();
    let mut m = StreamMapper::new("low");
    let mut out = m.begin_turn();
    if cancel_first_turn {
        m.request_cancel();
    }
    let mut ended = 0;
    for line in lines {
        match m.on_line(line) {
            Ok(o) => {
                let ends = o.iter().filter(|x| matches!(x, Out::End(_))).count();
                out.extend(o);
                ended += ends;
                if ends > 0 && ended < results_total {
                    out.extend(m.begin_turn());
                }
            }
            Err(f) => return (out, Some(f)),
        }
    }
    (out, None)
}

fn run(name: &str) -> Vec<Out> {
    let (out, fatal) = replay(&fixture(name), name == "interrupt_set_model");
    assert_eq!(fatal, None, "{name}");
    out
}

/// 工具输入的 JSON 键按字典序重排：serde_json 开没开 `preserve_order` 都得到同一份快照文字。
fn canonical(o: &Out) -> Out {
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
    match o {
        Out::ToolCall {
            id,
            name,
            raw_input,
        } => Out::ToolCall {
            id: id.clone(),
            name: name.clone(),
            raw_input: sorted(raw_input),
        },
        other => other.clone(),
    }
}

fn snapshot_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/snapshots/replay/{name}.txt"))
}

#[test]
fn replays_match_snapshots() {
    let update = std::env::var_os("IWORK_UPDATE_SNAPSHOTS").is_some();
    for name in SAMPLES {
        let text: String = run(name)
            .iter()
            .map(|o| format!("{:?}\n", canonical(o)))
            .collect();
        let path = snapshot_path(name);
        if update {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &text).unwrap();
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("快照 {} 读不了（{e}）", path.display()))
            .replace("\r\n", "\n");
        assert_eq!(text, expected, "{name}：回放结果变了，确认后更新快照");
    }
}

fn texts(out: &[Out]) -> Vec<&str> {
    out.iter()
        .filter_map(|o| match o {
            Out::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

fn ends(out: &[Out]) -> Vec<&TurnEnd> {
    out.iter()
        .filter_map(|o| match o {
            Out::End(e) => Some(e),
            _ => None,
        })
        .collect()
}

fn models(out: &[Out]) -> Vec<&str> {
    out.iter()
        .filter_map(|o| match o {
            Out::Report(Report::Model { id, .. }) => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

/// 两轮问答：正文各发一次（增量发了，整条消息不重复补）；两轮都成功；每一轮报一次模型、一次用量。
#[test]
fn text_multiturn_two_turns() {
    let out = run("text_multiturn");
    assert_eq!(texts(&out), ["ONE", "TWO"]);
    assert_eq!(ends(&out), [&TurnEnd::Completed, &TurnEnd::Completed]);
    // 第一轮靠 init，第二轮靠缓存在开头报（方案 6.2）
    assert_eq!(models(&out), ["claude-haiku-5-5", "claude-haiku-5-5"]);
    let usages = out
        .iter()
        .filter(|o| matches!(o, Out::Report(Report::Usage(_))))
        .count();
    assert_eq!(usages, 2);
    assert!(
        !out.iter().any(|o| matches!(o, Out::Diagnostic(_))),
        "没有诊断：{out:?}"
    );
}

/// 额度：窗口类型、使用率、重置时间三个值（期望值手写，样本里是 seven_day、0.74、1794855600）。
#[test]
fn rate_limit_event_maps_to_quota() {
    let out = run("text_multiturn");
    let quotas: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Out::Report(Report::Quota(w)) => Some(w.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        quotas,
        [vec![QuotaWindow {
            window: "seven_day".into(),
            used_pct: 74,
            resets_at_secs: Some(1_794_855_600),
        }]]
    );
}

/// 工具调用获准：tool_use → 工具调用（输入原样），结果 → 成功的更新。
#[test]
fn allowed_tool_call_maps_to_call_and_completed_update() {
    let out = run("tool_allowed");
    let call = out
        .iter()
        .find_map(|o| match o {
            Out::ToolCall {
                id,
                name,
                raw_input,
            } => Some((id.clone(), name.clone(), raw_input.clone())),
            _ => None,
        })
        .expect("有工具调用");
    assert_eq!(call.1, "Bash");
    assert_eq!(call.2["command"], "echo iwork-sample-ok");
    assert!(out.contains(&Out::ToolCallUpdate {
        id: call.0,
        failed: false,
        content: "iwork-sample-ok".into(),
    }));
    assert_eq!(ends(&out), [&TurnEnd::Completed]);
}

/// 工具调用被拒：结果是失败的更新，原因带 hook 的拒绝；回合照常结束（模型自己收尾）。
#[test]
fn denied_tool_call_maps_to_failed_update() {
    let out = run("tool_denied");
    let update = out
        .iter()
        .find_map(|o| match o {
            Out::ToolCallUpdate {
                failed, content, ..
            } => Some((*failed, content.clone())),
            _ => None,
        })
        .expect("有工具结果");
    assert!(update.0);
    assert!(
        update.1.contains("PreToolUse:Bash hook error"),
        "{}",
        update.1
    );
    assert_eq!(ends(&out), [&TurnEnd::Completed]);
}

/// 不存在的模型（N5）：subtype 是 success 但带错误标记，按登记表结束为 model_unavailable（验收 7b）。
#[test]
fn unknown_model_fails_with_reason() {
    let out = run("bad_model");
    assert_eq!(
        ends(&out),
        [&TurnEnd::Error {
            code: "model_unavailable",
            reason: Some("retired")
        }]
    );
}

/// 中断：第一轮按取消收尾；之后同一进程接得上下一轮；控制请求的响应交给薄壳配对，initialize 里读得到权限模式。
#[test]
fn interrupt_then_next_turn() {
    let out = run("interrupt_set_model");
    assert_eq!(ends(&out), [&TurnEnd::Cancelled, &TurnEnd::Completed]);
    let controls: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Out::Control(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(controls.len(), 3);
    assert_eq!(controls[0].request_id.as_deref(), Some("init-1"));
    assert_eq!(
        controls[0].current_permission_mode.as_deref(),
        Some("dontAsk")
    );
    assert!(controls.iter().all(|c| c.success));
}

/// 续接：照常结束；会话事实里记下了 init 报的会话 id。
#[test]
fn resume_completes_and_records_session() {
    let lines = fixture("resume");
    let mut m = StreamMapper::new("low");
    m.begin_turn();
    let mut all = Vec::new();
    for l in &lines {
        all.extend(m.on_line(l).unwrap());
    }
    assert_eq!(ends(&all), [&TurnEnd::Completed]);
    assert!(m.facts().session_id.is_some());
    assert_eq!(m.facts().permission_mode.as_deref(), Some("dontAsk"));
}

/// 设置文件被忽略时 CLI 报 default：每个 init 都核对，对不上就致命（isolation_broken）。用真样本改一个字段。
#[test]
fn settings_ignored_is_detected() {
    let lines: Vec<String> = fixture("text_multiturn")
        .into_iter()
        .map(|l| {
            l.replace(
                "\"permissionMode\":\"dontAsk\"",
                "\"permissionMode\":\"default\"",
            )
        })
        .collect();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("\"permissionMode\":\"default\"")),
        "对照：确实改到了"
    );
    let (_, fatal) = replay(&lines, false);
    assert_eq!(
        fatal,
        Some(Fatal::Isolation(IsolationBroken {
            reported: Some("default".into())
        }))
    );
}
