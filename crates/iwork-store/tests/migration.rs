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

// ---------- 首次初始化中断（第 1 轮审查 1/F-6，方案 2.2「首次初始化没提交」） ----------

const BROKEN_INITIAL: Migration = Migration {
    version: 1,
    name: "broken_initial",
    sql: "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT; THIS IS NOT SQL;",
};
static ONLY_BROKEN_INITIAL: [Migration; 1] = [BROKEN_INITIAL];

#[test]
fn failed_initial_migration_can_be_retried() {
    let env = Env::new();
    let err = env
        .open_with(Env::config(), with(&ONLY_BROKEN_INITIAL))
        .err();
    assert!(
        matches!(&err, Some(StorageError::Io { message }) if message.contains("broken_initial")),
        "{err:?}"
    );
    let len = std::fs::metadata(env.db()).unwrap().len();
    assert!(len > 0, "对照：留下的是非 0 字节的空库（{len}）");

    // 换正常程序重开：按新库初始化
    let names = project_names_after_seed(&env);
    assert_eq!(names, vec!["p1"]);
    assert_eq!(schema_version(&env.db()), "1");
}

fn project_names_after_seed(env: &Env) -> Vec<String> {
    let store = env.open();
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
    let names = block(read_model(&store).projects())
        .unwrap()
        .into_iter()
        .map(|p| p.project.name)
        .collect();
    store.close().unwrap();
    names
}

/// 一个很慢的初始迁移：正常建表之后跑一个耗时几十秒的查询，给父进程在事务里 kill 的窗口。
const SLOW_INITIAL: Migration = Migration {
    version: 1,
    name: "slow_initial",
    sql: concat!(
        include_str!("../src/migrations/0001_initial.sql"),
        "\nCREATE TABLE slow AS WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c \
         WHERE x < 2000000000) SELECT sum(x) AS s FROM c;"
    ),
};
static ONLY_SLOW_INITIAL: [Migration; 1] = [SLOW_INITIAL];
const INIT_CHILD_ENV: &str = "IWORK_STORE_INIT_CHILD";

/// 子进程入口：带慢迁移打开，打开完成（不该发生）就打印 OPENED。
#[test]
fn init_child() {
    let Some(root) = std::env::var_os(INIT_CHILD_ENV) else {
        return;
    };
    let env = Env::new();
    let env = Env {
        dir: iwork_store::DataDir::from_root(root.into()),
        ..env
    };
    if env
        .open_with(Env::config(), with(&ONLY_SLOW_INITIAL))
        .is_ok()
    {
        println!("OPENED");
    }
}

#[test]
fn killed_during_initial_migration_can_be_retried() {
    if std::env::var_os(INIT_CHILD_ENV).is_some() {
        return;
    }
    let env = Env::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "init_child", "--nocapture", "--test-threads=1"])
        .env(INIT_CHILD_ENV, env.dir.root())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // 等子进程切好 WAL、进入初始迁移事务
    let started = std::time::Instant::now();
    while std::fs::metadata(env.db()).map(|m| m.len()).unwrap_or(0) == 0 {
        assert!(started.elapsed().as_secs() < 30, "子进程没建出库文件");
        assert!(child.try_wait().unwrap().is_none(), "子进程提前退出");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        child.try_wait().unwrap().is_none(),
        "子进程应仍在迁移事务里"
    );
    child.kill().unwrap();
    child.wait().unwrap();
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut out).unwrap();
    assert!(!out.contains("OPENED"), "对照：初始化确实没完成");

    assert_eq!(
        project_names_after_seed(&env),
        vec!["p1"],
        "换正常程序重开成功"
    );
    assert!(!has_table(&env.db(), "slow"), "没提交的迁移一点不剩");
}
