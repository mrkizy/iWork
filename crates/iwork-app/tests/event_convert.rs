//! 领域事件 ↔ proto 的对拍（方案 3.6）。

mod support;

use std::path::PathBuf;

use iwork_app::convert::{ConvertError, event_from_proto, event_to_proto};
use iwork_domain::event::DomainEvent;
use iwork_proto::pb;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use prost::Message;
use support::arb::event_strategies;
use support::oneof_fields;

/// 任意 domain 事件 → proto → domain，等于原值；再经二进制编码一次也一样。
/// 逐个变体跑，保证 53 个变体每个都被测到（混在一个 oneof 里随机抽可能漏）。
#[test]
fn domain_event_proto_roundtrip() {
    let strategies = event_strategies();
    assert_eq!(
        strategies.len(),
        DomainEvent::KINDS.len(),
        "对照：策略数等于事件数"
    );
    for (kind, strategy) in strategies {
        let mut runner = TestRunner::new(Config {
            cases: 64,
            ..Config::default()
        });
        runner
            .run(&strategy, |event| {
                let proto = event_to_proto(&event)
                    .map_err(|e| TestCaseError::fail(format!("编码失败：{e}")))?;
                let bytes = proto.encode_to_vec();
                let decoded = pb::DomainEvent::decode(bytes.as_slice()).expect("刚编出的字节能解");
                let back = event_from_proto(decoded)
                    .map_err(|e| TestCaseError::fail(format!("解码失败：{e}")))?;
                if back != event {
                    return Err(TestCaseError::fail(format!(
                        "往返不等：\n{event:?}\n{back:?}"
                    )));
                }
                Ok(())
            })
            .unwrap_or_else(|e| panic!("事件 {kind}：{e}"));
    }
}

/// 策略表的种类名与生成出的事件一致，且恰好覆盖 KINDS。
#[test]
fn event_strategies_cover_every_kind() {
    let mut runner = TestRunner::deterministic();
    let mut names = Vec::new();
    for (kind, strategy) in event_strategies() {
        let event = proptest::strategy::ValueTree::current(
            &proptest::strategy::Strategy::new_tree(&strategy, &mut runner).unwrap(),
        );
        assert_eq!(event.kind(), kind);
        names.push(kind);
    }
    assert_eq!(names, DomainEvent::KINDS);
}

/// proto `DomainEvent` 的 oneof 字段名与 `DomainEvent::KINDS` 逐一对应，顺序也相同。
#[test]
fn domain_event_kinds_match_proto_oneof() {
    let proto: Vec<String> = oneof_fields("DomainEvent")
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    assert_eq!(proto.len(), 53, "对照：proto 事件数");
    assert_eq!(proto, DomainEvent::KINDS);
}

/// 真实样本（`proto/compat/v1/samples/DomainEvent.*.binpb`）全部能解成 domain 事件，且覆盖每个种类。
#[test]
fn compat_samples_decode_into_domain() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../proto/compat/v1/samples");
    let mut kinds = Vec::new();
    let mut failures = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("样本目录存在") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !(name.starts_with("DomainEvent.") && name.ends_with(".binpb")) {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        let proto =
            pb::DomainEvent::decode(bytes.as_slice()).unwrap_or_else(|e| panic!("{name}：{e}"));
        match event_from_proto(proto) {
            Ok(event) => kinds.push(event.kind()),
            Err(e) => failures.push(format!("{name}：{e}")),
        }
    }
    assert!(
        failures.is_empty(),
        "解不成 domain 事件的样本：\n{}",
        failures.join("\n")
    );
    kinds.sort_unstable();
    kinds.dedup();
    let mut expected = DomainEvent::KINDS.to_vec();
    expected.sort_unstable();
    assert_eq!(kinds, expected, "样本应覆盖全部事件种类");
}

fn project_removed(id: &str) -> pb::DomainEvent {
    pb::DomainEvent {
        event: Some(pb::domain_event::Event::ProjectRemoved(
            pb::ProjectRemoved {
                project_id: id.into(),
            },
        )),
    }
}

#[test]
fn malformed_proto_is_rejected_with_names() {
    // 对照：合法 ID 能解
    assert!(event_from_proto(project_removed("01ARZ3NDEKTSV4RRFFQ69G5FAV")).is_ok());

    let err = event_from_proto(project_removed("not-an-id")).unwrap_err();
    assert!(
        matches!(
            err,
            ConvertError::InvalidValue {
                kind: "ProjectRemoved",
                field: "project_id",
                ..
            }
        ),
        "{err:?}"
    );

    assert_eq!(
        event_from_proto(pb::DomainEvent { event: None }),
        Err(ConvertError::MissingField {
            kind: "DomainEvent",
            field: "event"
        })
    );

    // UNSPECIFIED 与未知枚举值
    for value in [0, 99] {
        let e = pb::DomainEvent {
            event: Some(pb::domain_event::Event::ProjectPathStatusChanged(
                pb::ProjectPathStatusChanged {
                    project_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                    path_status: value,
                },
            )),
        };
        assert_eq!(
            event_from_proto(e),
            Err(ConvertError::UnsupportedEnum {
                kind: "ProjectPathStatusChanged",
                field: "path_status",
                value
            })
        );
    }

    // 哈希：大写、长度不对都拒
    for sha in ["AB".repeat(32), "ab".repeat(31)] {
        let e = pb::DomainEvent {
            event: Some(pb::domain_event::Event::AgentsMdDraftProposed(
                pb::AgentsMdDraftProposed {
                    card_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                    project_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                    content_artifact: Some(pb::ArtifactRef {
                        sha256: sha.clone(),
                        area: pb::ArtifactArea::Artifacts as i32,
                        size: 1,
                    }),
                },
            )),
        };
        let err = event_from_proto(e).unwrap_err();
        assert!(
            matches!(
                err,
                ConvertError::InvalidValue {
                    kind: "ArtifactRef",
                    field: "sha256",
                    ..
                }
            ),
            "{sha}: {err:?}"
        );
    }

    // 项目默认档位不能是全开
    let mut settings = pb::ProjectSettings {
        default_approval_mode: pb::ApprovalMode::FullAccess as i32,
        default_model: None,
        ai_review: Some(pb::AiReviewSettings::default()),
    };
    let wrap = |s: pb::ProjectSettings| pb::DomainEvent {
        event: Some(pb::domain_event::Event::ProjectSettingsUpdated(
            pb::ProjectSettingsUpdated {
                project_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                settings: Some(s),
            },
        )),
    };
    assert!(matches!(
        event_from_proto(wrap(settings.clone())),
        Err(ConvertError::UnsupportedEnum {
            kind: "ProjectSettings",
            field: "default_approval_mode",
            ..
        })
    ));
    settings.default_approval_mode = pb::ApprovalMode::Auto as i32;
    assert!(event_from_proto(wrap(settings)).is_ok());

    // 错误映射到 INTERNAL 并点名
    let error = err_to_error();
    assert_eq!(error.code, pb::ErrorCode::Internal as i32);
    assert_eq!(
        error
            .details
            .get(iwork_app::details::KIND)
            .map(String::as_str),
        Some("ProjectRemoved")
    );
    assert_eq!(
        error
            .details
            .get(iwork_app::details::FIELD)
            .map(String::as_str),
        Some("project_id")
    );
}

fn err_to_error() -> pb::Error {
    event_from_proto(project_removed("x"))
        .unwrap_err()
        .to_error()
}
