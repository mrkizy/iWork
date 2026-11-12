//! 运行时登记与不兼容组合（技术设计 6.5、FR17；t0-findings 5.2）。
//!
//! 适配器版本与 CLI 版本可能不一致（实测 codex-acp 内置旧内核），所以登记同时记 CLI 路径、
//! CLI 版本、适配器名与版本、协议版本。已知不兼容的组合写在 [`KNOWN_INCOMPATIBLE`] 一张表里，
//! 加一条 = 加一行；开会话前用 [`check_compatibility`] 判，命中就拒绝运行。

use std::path::PathBuf;

/// iWork 这一侧说的 ACP 协议版本。对端回的版本不同就不运行：协议版本不一致时字段含义没有保证。
pub const SUPPORTED_PROTOCOL_VERSION: u16 = 1;

/// 一个运行时实例的登记信息，来自 ACP 初始化响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRegistration {
    /// 适配器名，取自 `initialize` 响应的 `agentInfo.name`
    pub adapter_name: String,
    /// 适配器版本，取自 `agentInfo.version`
    pub adapter_version: String,
    /// 对端回的 ACP 协议版本
    pub protocol_version: u16,
    /// 适配器驱动的厂商 CLI；原生 ACP 的运行时没有这一层，为 `None`
    pub cli: Option<CliInfo>,
}

/// 适配器底下的厂商 CLI，由适配器在 `initialize` 响应的 `_meta["iwork"]["cli"]` 里报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliInfo {
    pub path: PathBuf,
    pub version: String,
}

/// 版本匹配条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionMatch {
    Any,
    Exact(&'static str),
    /// 以给定前缀开头，如 `"0.16."` 表示 0.16 的所有补丁版本
    Prefix(&'static str),
}

impl VersionMatch {
    fn matches(self, version: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(v) => version == v,
            Self::Prefix(p) => version.starts_with(p),
        }
    }
}

/// 一条已知不兼容的组合。`cli_version` 为 `Any` 时不看 CLI（包括没有 CLI 的情况）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncompatibleCombo {
    pub adapter_name: &'static str,
    pub adapter_version: VersionMatch,
    pub cli_version: VersionMatch,
    /// 给用户看的原因，进错误信息
    pub reason: &'static str,
}

/// 已知不兼容的组合。依据见各行的 `reason`。
pub const KNOWN_INCOMPATIBLE: &[IncompatibleCombo] = &[IncompatibleCombo {
    adapter_name: "codex-acp",
    adapter_version: VersionMatch::Prefix("0.16."),
    cli_version: VersionMatch::Any,
    reason: "适配器内置旧内核，所有模型都不可用（t0-findings 第 3 节实测）",
}];

/// 判断登记信息能不能运行。不能时返回给人看的说明，点名适配器、版本与原因。
pub fn check_compatibility(
    reg: &RuntimeRegistration,
    table: &[IncompatibleCombo],
) -> Result<(), String> {
    if reg.protocol_version != SUPPORTED_PROTOCOL_VERSION {
        return Err(format!(
            "{} {} 使用 ACP 协议版本 {}，iWork 只支持版本 {}",
            reg.adapter_name, reg.adapter_version, reg.protocol_version, SUPPORTED_PROTOCOL_VERSION
        ));
    }
    let cli_version = reg.cli.as_ref().map(|c| c.version.as_str());
    let hit = table.iter().find(|combo| {
        combo.adapter_name == reg.adapter_name
            && combo.adapter_version.matches(&reg.adapter_version)
            && match combo.cli_version {
                VersionMatch::Any => true,
                m => cli_version.is_some_and(|v| m.matches(v)),
            }
    });
    match hit {
        None => Ok(()),
        Some(combo) => Err(match cli_version {
            Some(cli) => format!(
                "{} {}（CLI {}）是已知不兼容的组合：{}",
                reg.adapter_name, reg.adapter_version, cli, combo.reason
            ),
            None => format!(
                "{} {} 是已知不兼容的版本：{}",
                reg.adapter_name, reg.adapter_version, combo.reason
            ),
        }),
    }
}

/// 已验证 CLI 版本里实测到的内置工具，按会话形状分开记（T06 方案第 9 节，P11 的 d，产品负责人 2026-11-12 定）。
///
/// 表外的 CLI 版本开会话时只给这里见过的工具（`SessionSetup.builtin_tool_allowlist` → `--tools`），新出现的内置工具
/// 不进会话。挑哪一个形状、取并集归版本策略（S8）；这一张表是那一步与 d1 退路（第 9 节：不用 `--tools`、把多出来的
/// 工具追加进 `disabled_tools`）共用的数据。代码里没有工具名，名字只在这张表里。
///
/// 一个版本进已验证表（S8 的 `VERIFIED_CLI`）时同时在这里登记它的清单，数据来自那个版本的 G1a 与 G8c。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedTools {
    pub adapter_name: &'static str,
    pub cli_version: &'static str,
    pub shapes: &'static [ToolShape],
}

/// 一种会话形状：这些工具被禁用时，`system/init.tools` 是什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolShape {
    pub disabled: &'static [&'static str],
    pub tools: &'static [&'static str],
}

/// 每个已验证版本至少有「什么都不禁」这一种形状。
pub const VERIFIED_TOOLS: &[VerifiedTools] = &[VerifiedTools {
    adapter_name: "iwork-bridge-claude",
    cli_version: "2.1.322",
    // t0-findings 2.4：G1a（默认 25 个）、G8c（禁掉 Bash 后多出 Glob、Grep）
    shapes: &[
        ToolShape {
            disabled: &[],
            tools: &[
                "Task",
                "Bash",
                "CronCreate",
                "CronDelete",
                "CronList",
                "DesignSync",
                "Edit",
                "EnterWorktree",
                "ExitWorktree",
                "ListAgents",
                "Monitor",
                "NotebookEdit",
                "PushNotification",
                "Read",
                "RemoteTrigger",
                "ReportFindings",
                "ScheduleWakeup",
                "SendMessage",
                "Skill",
                "TaskStop",
                "ToolSearch",
                "WebFetch",
                "WebSearch",
                "Workflow",
                "Write",
            ],
        },
        ToolShape {
            disabled: &["Bash"],
            tools: &[
                "Task",
                "CronCreate",
                "CronDelete",
                "CronList",
                "DesignSync",
                "Edit",
                "EnterWorktree",
                "ExitWorktree",
                "Glob",
                "Grep",
                "ListAgents",
                "Monitor",
                "NotebookEdit",
                "PushNotification",
                "Read",
                "RemoteTrigger",
                "ReportFindings",
                "ScheduleWakeup",
                "SendMessage",
                "Skill",
                "TaskStop",
                "ToolSearch",
                "WebFetch",
                "WebSearch",
                "Workflow",
                "Write",
            ],
        },
    ],
}];

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(name: &str, version: &str, cli: Option<&str>) -> RuntimeRegistration {
        RuntimeRegistration {
            adapter_name: name.into(),
            adapter_version: version.into(),
            protocol_version: SUPPORTED_PROTOCOL_VERSION,
            cli: cli.map(|v| CliInfo {
                path: PathBuf::from("/opt/cli"),
                version: v.into(),
            }),
        }
    }

    const TABLE: &[IncompatibleCombo] = &[IncompatibleCombo {
        adapter_name: "bridge",
        adapter_version: VersionMatch::Exact("1.0.0"),
        cli_version: VersionMatch::Prefix("2."),
        reason: "测试用原因",
    }];

    #[test]
    fn listed_combination_is_blocked_with_names_and_reason() {
        let err = check_compatibility(&reg("bridge", "1.0.0", Some("2.3.4")), TABLE).unwrap_err();
        assert!(err.contains("bridge 1.0.0"), "{err}");
        assert!(err.contains("CLI 2.3.4"), "{err}");
        assert!(err.contains("测试用原因"), "{err}");
    }

    #[test]
    fn neighbours_of_a_listed_combination_still_run() {
        assert_eq!(
            check_compatibility(&reg("bridge", "1.0.1", Some("2.3.4")), TABLE),
            Ok(())
        );
        assert_eq!(
            check_compatibility(&reg("bridge", "1.0.0", Some("3.0.0")), TABLE),
            Ok(())
        );
        // 条件要求 CLI 版本，没报 CLI 的不算命中
        assert_eq!(
            check_compatibility(&reg("bridge", "1.0.0", None), TABLE),
            Ok(())
        );
        assert_eq!(
            check_compatibility(&reg("other", "1.0.0", Some("2.0")), TABLE),
            Ok(())
        );
    }

    #[test]
    fn other_protocol_version_is_blocked() {
        let mut r = reg("bridge", "9.9.9", None);
        r.protocol_version = 2;
        let err = check_compatibility(&r, &[]).unwrap_err();
        assert!(err.contains("协议版本 2"), "{err}");
    }

    #[test]
    fn known_table_blocks_old_codex_acp() {
        let err =
            check_compatibility(&reg("codex-acp", "0.16.3", None), KNOWN_INCOMPATIBLE).unwrap_err();
        assert!(err.contains("codex-acp 0.16.3"), "{err}");
        assert_eq!(
            check_compatibility(&reg("codex-acp", "2.5.0", None), KNOWN_INCOMPATIBLE),
            Ok(())
        );
    }

    /// 已验证工具清单的形状（方案第 9 节）：每个版本都有清单、至少有「什么都不禁」这一种；每种形状的清单不空、
    /// 不重名、不含它自己禁掉的工具；名字过得了桥接对工具名的校验（不空、不带逗号与空白，会拼进 `--tools`）。
    #[test]
    fn verified_tools_table_is_well_formed() {
        assert!(!VERIFIED_TOOLS.is_empty());
        for v in VERIFIED_TOOLS {
            let who = format!("{} {}", v.adapter_name, v.cli_version);
            assert!(
                v.shapes.iter().any(|s| s.disabled.is_empty()),
                "{who}：缺「什么都不禁」的形状"
            );
            for shape in v.shapes {
                assert!(!shape.tools.is_empty(), "{who} {:?}", shape.disabled);
                let mut seen = std::collections::BTreeSet::new();
                for t in shape.tools {
                    assert!(seen.insert(*t), "{who}：{t} 重复");
                    assert!(!shape.disabled.contains(t), "{who}：{t} 被禁用却在清单里");
                    assert!(
                        !t.is_empty() && !t.contains(',') && !t.chars().any(char::is_whitespace),
                        "{who}：工具名 {t:?}"
                    );
                }
            }
        }
    }

    /// 2.1.322 的两种形状（期望值手写，t0-findings 2.4 G1a、G8c）：默认 25 个、没有 Grep 与 Glob；禁掉 Bash 时
    /// 少了 Bash、多出 Grep 与 Glob，别的一样。
    #[test]
    fn claude_2_1_322_shapes() {
        let v = VERIFIED_TOOLS
            .iter()
            .find(|v| v.adapter_name == "iwork-bridge-claude" && v.cli_version == "2.1.322")
            .expect("基准版本已登记");
        let shape = |disabled: &[&str]| -> std::collections::BTreeSet<&str> {
            v.shapes
                .iter()
                .find(|s| s.disabled == disabled)
                .unwrap_or_else(|| panic!("缺形状 {disabled:?}"))
                .tools
                .iter()
                .copied()
                .collect()
        };
        let default = shape(&[]);
        assert_eq!(default.len(), 25);
        assert!(default.contains("Bash") && !default.contains("Grep") && !default.contains("Glob"));
        let mut expected = default.clone();
        expected.remove("Bash");
        expected.extend(["Grep", "Glob"]);
        assert_eq!(shape(&["Bash"]), expected);
    }
}
