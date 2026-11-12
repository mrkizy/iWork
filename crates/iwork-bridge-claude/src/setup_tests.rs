//! 会话配置的解码与校验：合法的原样转过来；缺字段、值不认识、路径不是绝对路径，各点名一个字段拒绝。

use serde_json::json;

use super::*;

/// 这个平台上的绝对路径：unix 原样，Windows 前面补盘符（M1 不支持 Windows，AGENTS.md：只在测试里统一写法，
/// 不为它改产品代码）。
fn abs(p: &str) -> String {
    if cfg!(windows) {
        format!("C:{p}")
    } else {
        p.to_owned()
    }
}

pub(crate) fn valid() -> pb::SessionSetup {
    pb::SessionSetup {
        model: Some(pb::ModelSelection {
            model_id: "claude-haiku-5-5".into(),
            effort: Some("high".into()),
        }),
        constraints: Some(pb::SessionConstraints {
            deny_read_write: vec![abs("/home/u/.ssh")],
            read_only: vec![abs("/data/attachments")],
            allow_write: Some(pb::AllowWrite {
                kind: Some(pb::allow_write::Kind::Whitelist(pb::WritablePaths {
                    paths: vec![abs("/work")],
                })),
            }),
            excluded_commands: vec![
                pb::CommandPattern {
                    program: "git".into(),
                    subcommand: Some("push".into()),
                },
                pb::CommandPattern {
                    program: "gh".into(),
                    subcommand: None,
                },
            ],
            allow_unsandboxed_retry: true,
            network: pb::NetworkPolicy::Open.into(),
        }),
        tool_set: Some(pb::SessionToolSet {
            enabled_groups: vec![pb::ToolGroup::ReadAndSearch.into()],
            include_always_on: true,
        }),
        disabled_tools: vec!["CronCreate".into()],
        plugin_dirs: vec![abs("/data/imports/p1")],
    }
}

fn meta_of(setup: &pb::SessionSetup) -> Value {
    json!({ META_KEY: { SESSION_SETUP_KEY: BASE64.encode(setup.encode_to_vec()) } })
}

#[test]
fn decodes_valid_setup() {
    let got = from_meta(Some(&meta_of(&valid()))).unwrap();
    assert_eq!(
        got,
        Setup {
            model: Model {
                id: "claude-haiku-5-5".into(),
                effort: "high",
            },
            constraints: Constraints {
                deny_read_write: vec![abs("/home/u/.ssh").into()],
                read_only: vec![abs("/data/attachments").into()],
                allow_write: AllowWrite::Whitelist(vec![abs("/work").into()]),
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
                allow_unsandboxed_retry: true,
                network: Network::Open,
            },
            tools: Tools::Default,
            disabled_tools: vec!["CronCreate".into()],
            plugin_dirs: vec![abs("/data/imports/p1").into()],
        }
    );
}

#[test]
fn ai_review_turn_gets_no_tools() {
    let mut s = valid();
    s.tool_set = Some(pb::SessionToolSet {
        enabled_groups: vec![],
        include_always_on: false,
    });
    assert_eq!(validate(s).unwrap().tools, Tools::None);
    // 对照：没有分组但给自身无副作用的工具，不算「一个都不给」
    let mut s = valid();
    s.tool_set = Some(pb::SessionToolSet {
        enabled_groups: vec![],
        include_always_on: true,
    });
    assert_eq!(validate(s).unwrap().tools, Tools::Default);
}

#[test]
fn meta_must_carry_decodable_setup() {
    for meta in [
        None,
        Some(json!({})),
        Some(json!({ META_KEY: {} })),
        Some(json!({ META_KEY: { SESSION_SETUP_KEY: 7 } })),
        Some(json!({ META_KEY: { SESSION_SETUP_KEY: "不是 base64" } })),
        Some(json!({ META_KEY: { SESSION_SETUP_KEY: BASE64.encode([0xff, 0xff, 0xff]) } })),
    ] {
        assert_eq!(
            from_meta(meta.as_ref()),
            Err(invalid("session_setup")),
            "{meta:?}"
        );
    }
}

/// 每一种不合法各改一处，期望点名对应的字段（期望值手写）。
#[test]
fn each_invalid_field_is_named() {
    type Edit = fn(&mut pb::SessionSetup);
    let cases: [(&str, Edit); 17] = [
        ("model", |s| s.model = None),
        ("model.model_id", |s| {
            s.model.as_mut().unwrap().model_id = " ".into()
        }),
        ("model.effort", |s| s.model.as_mut().unwrap().effort = None),
        ("model.effort", |s| {
            s.model.as_mut().unwrap().effort = Some("turbo".into())
        }),
        ("constraints", |s| s.constraints = None),
        ("constraints.allow_write", |s| {
            s.constraints.as_mut().unwrap().allow_write = None
        }),
        ("constraints.allow_write", |s| {
            s.constraints.as_mut().unwrap().allow_write = Some(pb::AllowWrite {
                kind: Some(pb::allow_write::Kind::Whitelist(pb::WritablePaths {
                    paths: vec!["work".into()],
                })),
            })
        }),
        ("constraints.network", |s| {
            s.constraints.as_mut().unwrap().network = pb::NetworkPolicy::Unspecified.into()
        }),
        ("constraints.network", |s| {
            s.constraints.as_mut().unwrap().network = 99
        }),
        ("constraints.deny_read_write", |s| {
            s.constraints.as_mut().unwrap().deny_read_write = vec!["~/.ssh".into()]
        }),
        ("constraints.read_only", |s| {
            s.constraints.as_mut().unwrap().read_only = vec!["".into()]
        }),
        ("constraints.excluded_commands", |s| {
            s.constraints.as_mut().unwrap().excluded_commands[0].program = "git push".into()
        }),
        ("constraints.excluded_commands", |s| {
            s.constraints.as_mut().unwrap().excluded_commands[0].subcommand = Some("".into())
        }),
        ("tool_set", |s| s.tool_set = None),
        ("tool_set.enabled_groups", |s| {
            s.tool_set.as_mut().unwrap().enabled_groups = vec![0]
        }),
        ("disabled_tools", |s| {
            s.disabled_tools = vec!["Bash,Read".into()]
        }),
        ("plugin_dirs", |s| s.plugin_dirs = vec!["imports/p1".into()]),
    ];
    for (field, edit) in cases {
        let mut s = valid();
        edit(&mut s);
        assert_eq!(validate(s), Err(invalid(field)), "{field}");
        // 字段路径的形状要过 bridge_error 的 enum_like 检查
        assert!(
            field.len() <= 40
                && field
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_' || b == b'.'),
            "{field}"
        );
    }
}

#[test]
fn every_effort_level_is_accepted() {
    for level in EFFORT_LEVELS {
        let mut s = valid();
        s.model.as_mut().unwrap().effort = Some(level.into());
        assert_eq!(validate(s).unwrap().model.effort, level);
    }
}
