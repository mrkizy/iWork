//! 卡验收 5 / 方案 S5：迁移编进二进制、只向前；执行前 `VACUUM INTO` 备份；本次全部待执行迁移一个
//! 事务，失败整体退回升级前、拒绝启动；schema 比程序新的库拒绝打开。

mod support;

use std::path::Path;

use iwork_domain::event::ProjectAdded;
use iwork_ports::read_model::ReadModelPort;
use iwork_ports::store::{StorageError, StoragePort};
use iwork_store::{Migration, OpenOptions};
use sha2::Digest;
use support::*;

const V1: Migration = Migration {
    version: 1,
    name: "initial",
    sql: include_str!("../src/migrations/0001_initial.sql"),
};
const V2: Migration = Migration {
    version: 2,
    name: "add_extra",
    sql: "CREATE TABLE extra (x INTEGER) STRICT;",
};
const V3_BAD: Migration = Migration {
    version: 3,
    name: "broken",
    sql: "CREATE TABLE extra3 (x INTEGER) STRICT; THIS IS NOT SQL;",
};
static ONLY_V1: [Migration; 1] = [V1];
static V1_V2: [Migration; 2] = [V1, V2];
static V1_V2_BAD: [Migration; 3] = [V1, V2, V3_BAD];

fn with(migrations: &'static [Migration]) -> OpenOptions {
    OpenOptions {
        migrations,
        ..OpenOptions::default()
    }
}

fn schema_version(db: &Path) -> String {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn has_table(db: &Path, name: &str) -> bool {
    scalar(
        db,
        &format!("SELECT COUNT(*) FROM sqlite_schema WHERE name = '{name}'"),
    ) == 1
}

fn backups(env: &Env) -> Vec<String> {
    std::fs::read_dir(env.dir.backups())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with(".probe-"))
        .collect()
}

/// 用 v1 建库并写一个项目。
fn v1_with_data(env: &Env) {
    let store = env.open_with(Env::config(), with(&ONLY_V1)).unwrap();
    block(store.commit(cmd(
        1,
        vec![
                ProjectAdded {
                    project: project(1),
                }
                .into(),
            ],
    )))
    .unwrap();
    store.close().unwrap();
}

fn project_names(env: &Env, migrations: &'static [Migration]) -> Vec<String> {
    let store = env.open_with(Env::config(), with(migrations)).unwrap();
    let names = block(read_model(&store).projects())
        .unwrap()
        .into_iter()
        .map(|p| p.project.name)
        .collect();
    store.close().unwrap();
    names
}

#[test]
fn failed_migration_keeps_old_version_and_backup() {
    let env = Env::new();
    v1_with_data(&env);
    let err = env
        .open_with(Env::config(), with(&V1_V2_BAD))
        .err()
        .expect("应拒绝启动");
    let StorageError::Io { message } = &err else {
        panic!("{err:?}")
    };
    assert!(message.contains("0003_broken"), "点名失败的迁移：{message}");
    assert!(message.contains("备份"), "带备份路径：{message}");

    assert_eq!(schema_version(&env.db()), "1", "仍是升级前的版本");
    assert!(!has_table(&env.db(), "extra"), "v2 成功了也一起退回");
    assert!(!has_table(&env.db(), "extra3"));
    let files = backups(&env);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].ends_with("-schema-v1.db"));
    // 装回旧程序（只认识 v1）能打开，数据在
    assert_eq!(project_names(&env, &ONLY_V1), vec!["p1"]);
}

#[test]
fn successful_migration_keeps_data() {
    let env = Env::new();
    v1_with_data(&env);
    assert_eq!(
        project_names(&env, &V1_V2),
        vec!["p1"],
        "迁移后旧数据查询结果不变"
    );
    assert_eq!(schema_version(&env.db()), "2");
    assert!(has_table(&env.db(), "extra"));
    assert_eq!(backups(&env).len(), 1, "迁移前备份了");
    // 再打开一次：已是最新，不再迁移、不再备份
    project_names(&env, &V1_V2);
    assert_eq!(backups(&env).len(), 1);
}

#[test]
fn newer_schema_is_refused_without_writing() {
    let env = Env::new();
    v1_with_data(&env);
    project_names(&env, &V1_V2); // 升到 v2
    let hash = |p: &Path| {
        std::fs::read(p)
            .ok()
            .map(|b| sha2::Sha256::digest(&b).to_vec())
    };
    let before = hash(&env.db());
    let err = env
        .open_with(Env::config(), with(&ONLY_V1))
        .err()
        .expect("旧程序打不开新库");
    assert!(
        matches!(&err, StorageError::Io { message } if message.contains("schema 版本 2")),
        "{err:?}"
    );
    assert_eq!(hash(&env.db()), before, "库没被写");
}

#[test]
fn new_database_records_versions() {
    let env = Env::new();
    drop(env.open());
    assert_eq!(schema_version(&env.db()), "1");
    let conn = rusqlite::Connection::open(env.db()).unwrap();
    let major: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'protocol_major'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(major, iwork_proto::PROTOCOL_MAJOR.to_string());
    assert!(backups(&env).is_empty(), "新库不需要备份");
}
