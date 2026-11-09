//! 审批测试共用的环境：固定形状的路径、同构的 `FakeFs`、ID 与范围。

#![allow(dead_code, reason = "各测试各用一部分")]

#[path = "../../../iwork-ports/tests/fake_fs/mod.rs"]
pub mod fake_fs;

use std::path::PathBuf;

use fake_fs::FakeFs;
use iwork_core::approval::env::{EnvPaths, Platform, ScopeKeys};
use iwork_domain::action::CanonicalPath;
use iwork_domain::id::{AssistantId, ConversationId, ProjectId, RawUlid};

pub const HOME: &str = "/home/u";
pub const DATA: &str = "/home/u/Library/Application Support/iWork";
pub const IWORK_ROOT: &str = "/home/u/iWork";
pub const WS: &str = "/home/u/iWork/projects/proj";
pub const TMP: &str = "/tmp";

pub fn cp(p: &str) -> CanonicalPath {
    CanonicalPath(PathBuf::from(p))
}

/// macOS 形状的环境（路径用 `/home/u` 前缀，与真实用户无关）。
pub fn env() -> EnvPaths {
    env_at(WS)
}

pub fn env_at(workspace: &str) -> EnvPaths {
    EnvPaths {
        platform: Platform::MacOs,
        home: cp(HOME),
        data_dir: cp(DATA),
        iwork_root: cp(IWORK_ROOT),
        workspace_root: cp(workspace),
        tmp_dirs: vec![cp(TMP)],
        dm_private_dir: None,
        other_project_roots: vec![cp("/home/u/iWork/projects/other")],
        other_dm_dirs: vec![cp("/home/u/iWork/assistants/bob")],
    }
}

/// 与 `env()` 同构的文件系统：工作区、`.git`、凭据目录与几条链接。大小写敏感（Linux 形状）。
pub fn fs() -> FakeFs {
    fs_ci(false)
}

pub fn fs_ci(case_insensitive: bool) -> FakeFs {
    let mut f = FakeFs::new(HOME, case_insensitive);
    for p in [
        "/home/u/iWork/projects/proj/src/a.rs",
        "/home/u/iWork/projects/proj/.git/objects",
        "/home/u/iWork/projects/proj/.git/config",
        "/home/u/iWork/projects/proj/.git/hooks",
        "/home/u/.ssh/id_rsa",
        "/home/u/.ssh/config",
        "/home/u/.aws/credentials",
        "/home/u/Documents/notes/x.txt",
        "/home/u/Library/Application Support/iWork/attachments/a.png",
        "/home/u/Library/Application Support/iWork/imports/skill.md",
        "/home/u/Library/Application Support/iWork/artifacts/log.txt",
        "/home/u/Library/Application Support/iWork/iwork.db",
        "/usr/include/stdio.h",
        "/tmp",
    ] {
        f.add(p);
    }
    // 工作区里指向 .git/objects、指向凭据目录、指向工作区外的链接
    f.link("/home/u/iWork/projects/proj/objs", ".git/objects");
    f.link("/home/u/iWork/projects/proj/keys", "/home/u/.ssh");
    f.link("/home/u/iWork/projects/proj/out", "/home/u/outside");
    f
}

pub fn project() -> ProjectId {
    ProjectId::from_raw(RawUlid::from_u128(0x11))
}

pub fn other_project() -> ProjectId {
    ProjectId::from_raw(RawUlid::from_u128(0x12))
}

pub fn assistant() -> AssistantId {
    AssistantId::from_raw(RawUlid::from_u128(0x21))
}

pub fn conversation() -> ConversationId {
    ConversationId::from_raw(RawUlid::from_u128(0x31))
}

pub fn scope() -> ScopeKeys {
    ScopeKeys {
        project: Some(project()),
        assistant: assistant(),
        conversation: conversation(),
    }
}
