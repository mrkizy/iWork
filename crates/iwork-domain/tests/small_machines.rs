//! 消息状态与项目路径状态的转换表（方案 4.3）。

mod support;

use iwork_domain::DomainError;
use iwork_domain::common::Timestamp;
use iwork_domain::id::AssistantId;
use iwork_domain::id::{ProjectId, RawUlid};
use iwork_domain::machine::Finite;
use iwork_domain::message::{
    self, Author, MESSAGE_MACHINE, MESSAGE_TRANSITIONS, MessageStatus, MessageTrigger,
};
use iwork_domain::project::{
    self, PATH_MACHINE, PATH_TRANSITIONS, PathStatus, Project, ProjectSettings, VcsKind,
};
use support::verify_machine;

#[test]
fn message_table_verified() {
    assert_eq!(MESSAGE_TRANSITIONS.len(), 3, "对照：表行数");
    assert_eq!(
        verify_machine(MESSAGE_MACHINE, message::next, MESSAGE_TRANSITIONS),
        Ok(())
    );
}

#[test]
fn message_scenarios() {
    let assistant = Author::Assistant {
        id: AssistantId::from_raw(RawUlid::from_u128(1)),
        name_snapshot: "iWork".into(),
    };
    assert_eq!(
        MessageStatus::initial_for(&assistant),
        MessageStatus::Streaming
    );
    assert_eq!(
        MessageStatus::initial_for(&Author::User),
        MessageStatus::Sent
    );
    assert_eq!(
        MessageStatus::initial_for(&Author::System),
        MessageStatus::Sent
    );
    // 用户消息创建即终态
    for trigger in MessageTrigger::all() {
        assert!(message::next(MessageStatus::Sent, trigger).is_err());
    }
    assert_eq!(
        message::next(MessageStatus::Streaming, MessageTrigger::Interrupted),
        Ok(MessageStatus::Interrupted)
    );
}

#[test]
fn path_table_verified() {
    assert_eq!(PATH_TRANSITIONS.len(), 3, "对照：表行数");
    assert_eq!(
        verify_machine(PATH_MACHINE, project::next, PATH_TRANSITIONS),
        Ok(())
    );
}

fn missing_project(id: ProjectId) -> Project {
    Project {
        id,
        root_path: "/old".into(),
        vcs_kind: VcsKind("v".into()),
        name: "p".into(),
        pinned: false,
        last_active_at: Some(Timestamp(1)),
        path_status: PathStatus::Missing,
        settings: ProjectSettings::default(),
    }
}

#[test]
fn relocate_requires_same_project_id() {
    let id = ProjectId::from_raw(RawUlid::from_u128(1));
    let other = ProjectId::from_raw(RawUlid::from_u128(2));

    let mut p = missing_project(id);
    assert_eq!(
        p.relocate("/new".into(), other),
        Err(DomainError::ProjectIdMismatch {
            expected: id,
            found: other
        })
    );
    assert_eq!(p, missing_project(id), "ID 不一致时什么都不改");

    p.relocate("/new".into(), id).unwrap();
    assert_eq!(
        (p.path_status, p.root_path.to_str()),
        (PathStatus::Ok, Some("/new"))
    );
}
