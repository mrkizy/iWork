//! 录制样本回放（T06 方案 15.2 验收 5、15.4）：`tests/fixtures/<CLI 版本>/*.jsonl` 逐行喂给 [`StreamMapper`]，
//! 每份样本一个快照（转换出的输出序列与上报），另有手写期望的断言。
//!
//! 回合怎么切：样本里没有记用户消息什么时候发的。回放照产品的次序：开头先开一个回合（产品里用户消息写给 CLI
//! 之前就开），每个回合结束后后面还有 `result` 就再开一个。`interrupt_set_model` 的第一轮照录制时那样请求了中断。
//!
//! 更新快照：`IWORK_UPDATE_SNAPSHOTS=1 cargo test -p iwork-bridge-claude --test replay`。

use std::path::{Path, PathBuf};

use iwork_bridge_claude::cli::errors::TurnEnd;
use iwork_bridge_claude::cli::events::Usage;
use iwork_bridge_claude::cli::facts::{IsolationBroken, SessionExpect};
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

/// 脱敏把 `init.plugins` 收成了 `{"iwork_fixture_count":N}`（路径里有用户名）。录制那台机器上 8 个插件都是
/// `@builtin` 或 `@inline`（S0 只看键名与后缀的统计），这里换成一个自带插件，让回放过会话内容核对；
/// 收起来的原样不换时核对不过（`collapsed_plugins_fail_the_content_check`）。
const COLLAPSED_PLUGINS: &str = "\"plugins\":{\"iwork_fixture_count\":8}";
const BUILTIN_PLUGINS: &str =
    "\"plugins\":[{\"name\":\"fixture\",\"path\":\"fixture\",\"source\":\"fixture@builtin\"}]";

fn fixture(name: &str) -> Vec<String> {
    raw_fixture(name)
        .into_iter()
        .map(|l| l.replace(COLLAPSED_PLUGINS, BUILTIN_PLUGINS))
        .collect()
}

fn raw_fixture(name: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/{VERSION}/{name}.jsonl"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("样本 {} 读不了（{e}）", path.display()))
        .lines()
        .filter(|l| !l.starts_with("{\"_fixture\""))
        .map(str::to_owned)
        .collect()
}

/// 样本里 `memory_paths` 也被收起来了（没有 `auto` 键），按「没有」算过。
fn expect() -> SessionExpect {
    SessionExpect {
        plugin_dirs: vec![],
        memory_dir: "/data/s/abc/m".into(),
    }
}

/// 回放一份样本，返回全部输出与第一条致命错误（有的话就停在那里）。
fn replay(lines: &[String], cancel_first_turn: bool) -> (Vec<Out>, Option<Fatal>) {
    let results_total = lines
        .iter()
        .filter(|l| l.contains("\"type\":\"result\""))
        .count();
    let mut m = StreamMapper::new("low", expect());
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
            resets_at_secs: 1_794_855_600,
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
    let mut m = StreamMapper::new("low", expect());
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
        Some(Fatal::Isolation(IsolationBroken::PermissionMode {
            reported: Some("default".into())
        }))
    );
}

/// 会话内容核对接在每个 init 上（3.5）：脱敏收起来的 `plugins` 不是数组，核对不了，第一个 init 就致命。
#[test]
fn collapsed_plugins_fail_the_content_check() {
    let lines = raw_fixture("text_multiturn");
    assert!(lines.iter().any(|l| l.contains(COLLAPSED_PLUGINS)), "对照");
    let (_, fatal) = replay(&lines, false);
    assert_eq!(
        fatal,
        Some(Fatal::Isolation(IsolationBroken::PluginsUnreadable))
    );
}

/// 真样本里多出一个用户自己的插件（经环境带进来的那种）：isolation_broken，只报名字。
#[test]
fn foreign_plugin_in_real_init_is_detected() {
    let lines: Vec<String> = raw_fixture("text_multiturn")
        .into_iter()
        .map(|l| {
            l.replace(
                COLLAPSED_PLUGINS,
                "\"plugins\":[{\"name\":\"mine\",\"path\":\"/home/u/.claude/plugins/mine\",\"source\":\"mine@inline\"}]",
            )
        })
        .collect();
    let (_, fatal) = replay(&lines, false);
    assert_eq!(
        fatal,
        Some(Fatal::Isolation(IsolationBroken::UnexpectedPlugin {
            name: "mine".into()
        }))
    );
}

/// 一个 `result` 转出的用量的手写期望：(输入, 输出, 缓存读, 缓存写, 费用)。
type Expect = (u64, u64, u64, u64, f64);

fn usages(out: &[Out]) -> Vec<Usage> {
    out.iter()
        .filter_map(|o| match o {
            Out::Report(Report::Usage(u)) => Some(u.clone()),
            _ => None,
        })
        .collect()
}

/// 方案 6.3：每个 `result` 报的是这一个 `result` 新增的用量，不是累计值（S5 审查 1/F-1）。三件事：
/// ① 转出的值等于手写期望（含 text_multiturn 第 2 轮 0.00140933、resume 0.00023148）；
/// ② 同一进程相邻两个 `result`：后一个的 total_cost_usd 减前一个的，等于后一个转出的费用；modelUsage 的 token 同理；
/// ③ 续接那一轮转出的费用小于它的 total_cost_usd（历史没被算进来）。
#[test]
fn usage_is_per_result_delta() {
    let expected: [(&str, &[Expect]); 6] = [
        (
            "text_multiturn",
            &[
                (2, 5, 11_888, 9_413, 0.002_004_18),
                (2, 5, 15_063, 6_280, 0.001_409_33),
            ],
        ),
        ("tool_allowed", &[(4, 90, 35_935, 6_876, 0.001_779_95)]),
        ("tool_denied", &[(4, 308, 35_931, 6_910, 0.001_895_71)]),
        ("resume", &[(2, 5, 21_318, 78, 0.000_231_48)]),
        ("bad_model", &[(0, 0, 0, 0, 0.0)]),
        (
            "interrupt_set_model",
            &[
                (2, 87, 14_591, 6_726, 0.001_534_81),
                (2, 6, 21_317, 287, 0.000_273_77),
            ],
        ),
    ];
    for (name, want) in expected {
        let got = usages(&run(name));
        assert_eq!(got.len(), want.len(), "{name}：每个 result 报一条");
        for (i, (u, w)) in got.iter().zip(want).enumerate() {
            assert_eq!(
                (
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_read_tokens,
                    u.cache_write_tokens
                ),
                (w.0, w.1, w.2, w.3),
                "{name} 第 {} 个 result 的 token",
                i + 1
            );
            let cost = u.cost_usd.unwrap_or_else(|| panic!("{name}：费用应当有"));
            assert!(
                (cost - w.4).abs() < 1e-12,
                "{name} 第 {} 个：{cost} ≠ {}",
                i + 1,
                w.4
            );
        }

        // ② 用样本里的累计值核对「增量」这个语义本身
        let results: Vec<Value> = fixture(name)
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["type"] == "result")
            .collect();
        for i in 1..results.len() {
            let delta = results[i]["total_cost_usd"].as_f64().unwrap()
                - results[i - 1]["total_cost_usd"].as_f64().unwrap();
            assert!(
                (delta - got[i].cost_usd.unwrap()).abs() < 1e-9,
                "{name}：累计差 {delta} 与第 {} 个的增量不符",
                i + 1
            );
            let tokens = |v: &Value, key: &str| -> u64 {
                v["modelUsage"]
                    .as_object()
                    .unwrap()
                    .values()
                    .map(|m| m[key].as_u64().unwrap())
                    .sum()
            };
            assert_eq!(
                tokens(&results[i], "outputTokens") - tokens(&results[i - 1], "outputTokens"),
                got[i].output_tokens,
                "{name}：modelUsage 的累计差"
            );
        }
    }
    // ③ 续接：历史在累计值里，不在转出的增量里
    let resume_total = fixture("resume")
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["type"] == "result")
        .unwrap()["total_cost_usd"]
        .as_f64()
        .unwrap();
    let resume_cost = usages(&run("resume"))[0].cost_usd.unwrap();
    assert!(
        resume_cost < resume_total,
        "{resume_cost} 应小于 {resume_total}"
    );
}
