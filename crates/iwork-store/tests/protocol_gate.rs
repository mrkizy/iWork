//! 卡验收 5a / 方案 S4：协议主版本不符的库先备份再拒绝启动；`iwork.db`、`-wal`、`-shm` 三个文件的
//! 字节与存在性都不变，不做任何迁移或写入。全部在系统临时目录里建库。

mod support;

use std::path::{Path, PathBuf};

use iwork_ports::store::StorageError;
use iwork_store::{OpenHooks, OpenOptions};
use rusqlite::Connection;
use sha2::Digest;
use support::*;

const PROGRAM: u32 = iwork_proto::PROTOCOL_MAJOR;
/// 假时钟 T0 = 2026-11-10T00:00:00.000Z，备份文件名里的时间（独立算出，不调实现）。
const STAMP: &str = "20261110T000000000Z";
const SCHEMA: &str = include_str!("../src/migrations/0001_initial.sql");

fn sibling(db: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", db.display()))
}

/// 三个文件各自的「存在性 + sha256」。
fn fingerprint(db: &Path) -> Vec<Option<[u8; 32]>> {
    ["", "-wal", "-shm"]
        .iter()
        .map(|s| {
            std::fs::read(sibling(db, s))
                .ok()
                .map(|b| sha2::Sha256::digest(&b).into())
        })
        .collect()
}

/// 建一个 iWork 库（WAL 模式），`meta.protocol_major = major`，带一行标记数据。
fn create_db(db: &Path, major: u32) -> Connection {
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = Connection::open(db).unwrap();
    conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))
        .unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.execute_batch(&format!(
        "INSERT INTO meta VALUES ('schema_version', '1'), ('protocol_major', '{major}'), \
         ('created_at', '0'), ('marker', 'checkpointed');"
    ))
    .unwrap();
    conn
}

fn probes_left(env: &Env) -> Vec<PathBuf> {
    std::fs::read_dir(env.dir.backups())
        .map(|it| {
            it.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".probe-")
                })
                .collect()
        })
        .unwrap_or_default()
}

fn meta(db: &Path, key: &str) -> Option<String> {
    let conn = Connection::open(db).unwrap();
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .ok()
}

/// 不符的库：返回 ProtocolMajorMismatch、备份能打开且与源一致、原文件不变、副本清掉。
fn assert_rejected_and_untouched(env: &Env, before: &[Option<[u8; 32]>]) -> PathBuf {
    let err = env
        .open_with(Env::config(), OpenOptions::default())
        .err()
        .expect("应拒绝启动");
    let StorageError::ProtocolMajorMismatch {
        database,
        program,
        backup,
    } = err
    else {
        panic!("应为 ProtocolMajorMismatch：{err:?}")
    };
    assert_eq!((database, program), (PROGRAM + 1, PROGRAM));
    assert_eq!(
        backup,
        env.dir
            .backups()
            .join(format!("{STAMP}-protocol-v{}.db", PROGRAM + 1))
    );
    assert!(backup.is_file(), "备份存在");
    assert_eq!(
        meta(&backup, "protocol_major"),
        Some((PROGRAM + 1).to_string())
    );
    assert_eq!(
        fingerprint(&env.db()),
        before,
        "三个原文件的字节与存在性都不变"
    );
    assert!(probes_left(env).is_empty(), "探测副本已清掉");
    backup
}

#[test]
fn crash_residue_wal_is_read_from_copy() {
    let env = Env::new();
    // 在另一个目录建库，写入后不关连接，把三个文件当「崩溃快照」复制过来
    let live = env.tmp.path().join("live/iwork.db");
    let conn = create_db(&live, PROGRAM + 1);
    conn.execute_batch(
        "PRAGMA wal_autocheckpoint = 0; INSERT INTO meta VALUES ('wal_only', 'yes');",
    )
    .unwrap();
    std::fs::create_dir_all(env.dir.root()).unwrap();
    for s in ["", "-wal", "-shm"] {
        std::fs::copy(sibling(&live, s), sibling(&env.db(), s)).unwrap();
    }
    drop(conn);
    let before = fingerprint(&env.db());
    assert!(
        before[1].is_some() && before[2].is_some(),
        "对照：快照里确实有 -wal 与 -shm"
    );

    let backup = assert_rejected_and_untouched(&env, &before);
    assert_eq!(
        meta(&backup, "wal_only"),
        Some("yes".into()),
        "备份读到了只在 WAL 里的行"
    );
}

#[test]
fn missing_shm_stays_missing() {
    let env = Env::new();
    let live = env.tmp.path().join("live/iwork.db");
    let conn = create_db(&live, PROGRAM + 1);
    conn.execute_batch(
        "PRAGMA wal_autocheckpoint = 0; INSERT INTO meta VALUES ('wal_only', 'yes');",
    )
    .unwrap();
    std::fs::create_dir_all(env.dir.root()).unwrap();
    for s in ["", "-wal"] {
        std::fs::copy(sibling(&live, s), sibling(&env.db(), s)).unwrap();
    }
    drop(conn);
    let before = fingerprint(&env.db());
    assert!(before[2].is_none(), "对照：没有 -shm");
    assert_rejected_and_untouched(&env, &before);
    assert!(!sibling(&env.db(), "-shm").exists(), "-shm 仍然不存在");
}

#[test]
fn preexisting_empty_wal_is_left_alone() {
    let env = Env::new();
    drop(create_db(&env.db(), PROGRAM + 1)); // 正常关闭：WAL 已合并删除
    std::fs::write(sibling(&env.db(), "-wal"), b"").unwrap();
    let before = fingerprint(&env.db());
    assert_rejected_and_untouched(&env, &before);
    assert_eq!(
        std::fs::metadata(sibling(&env.db(), "-wal")).unwrap().len(),
        0,
        "空 WAL 还在"
    );
}

#[test]
fn leftover_shm_is_left_alone() {
    let env = Env::new();
    let conn = create_db(&env.db(), PROGRAM + 1);
    let shm = std::fs::read(sibling(&env.db(), "-shm")).unwrap();
    drop(conn);
    std::fs::write(sibling(&env.db(), "-shm"), shm).unwrap();
    let before = fingerprint(&env.db());
    assert!(
        before[1].is_none() && before[2].is_some(),
        "对照：只有残留的 -shm"
    );
    assert_rejected_and_untouched(&env, &before);
}

#[test]
fn backup_failure_still_refuses_without_touching_files() {
    let env = Env::new();
    drop(create_db(&env.db(), PROGRAM + 1));
    env.dir.ensure_layout().unwrap();
    // 把备份的目标路径预先占成一个目录：VACUUM INTO 失败
    let target = env
        .dir
        .backups()
        .join(format!("{STAMP}-protocol-v{}.db", PROGRAM + 1));
    std::fs::create_dir(&target).unwrap();
    let before = fingerprint(&env.db());
    let err = env
        .open_with(Env::config(), OpenOptions::default())
        .err()
        .expect("应拒绝启动");
    let StorageError::Io { message } = err else {
        panic!("备份失败应为 Io：{err:?}")
    };
    assert!(
        message.contains(&PROGRAM.to_string()) && message.contains(&(PROGRAM + 1).to_string()),
        "消息带两个主版本号：{message}"
    );
    assert_eq!(fingerprint(&env.db()), before);
    assert!(probes_left(&env).is_empty());
}

#[test]
fn matching_major_opens_and_cleans_probe() {
    let env = Env::new();
    drop(create_db(&env.db(), PROGRAM));
    let store = env.open();
    assert!(probes_left(&env).is_empty());
    drop(store);
    // 对照：原库确实被打开过（这条路径允许写）
    assert_eq!(meta(&env.db(), "protocol_major"), Some(PROGRAM.to_string()));
}

fn failing_remove(_: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("注入：删不掉"))
}

#[test]
fn probe_cleanup_failure_does_not_mask_the_verdict() {
    let hooks = OpenHooks {
        remove_probe_dir: failing_remove,
    };
    let options = OpenOptions {
        hooks,
        ..OpenOptions::default()
    };
    // 不符：仍是 ProtocolMajorMismatch，不被清理错误遮盖
    let env = Env::new();
    drop(create_db(&env.db(), PROGRAM + 1));
    let before = fingerprint(&env.db());
    let err = env.open_with(Env::config(), options).err().unwrap();
    assert!(
        matches!(err, StorageError::ProtocolMajorMismatch { .. }),
        "{err:?}"
    );
    assert_eq!(fingerprint(&env.db()), before);
    assert_eq!(probes_left(&env).len(), 1, "对照：清理确实失败了");

    // 相符：正常打开
    let env = Env::new();
    drop(create_db(&env.db(), PROGRAM));
    env.open_with(Env::config(), options)
        .expect("清理失败不阻止启动");
}

#[test]
fn stale_probe_from_last_crash_is_cleaned() {
    let env = Env::new();
    drop(create_db(&env.db(), PROGRAM));
    env.dir.ensure_layout().unwrap();
    let stale = env.dir.backups().join(".probe-1-2-3");
    std::fs::create_dir(&stale).unwrap();
    std::fs::copy(env.db(), stale.join("iwork.db")).unwrap();
    let store = env.open();
    assert!(!stale.exists(), "残留被清掉");
    drop(store);

    // 残留删不掉：只记日志，open 结果不受影响
    let env = Env::new();
    drop(create_db(&env.db(), PROGRAM));
    env.dir.ensure_layout().unwrap();
    let stale = env.dir.backups().join(".probe-1-2-3");
    std::fs::create_dir(&stale).unwrap();
    let options = OpenOptions {
        hooks: OpenHooks {
            remove_probe_dir: failing_remove,
        },
        ..OpenOptions::default()
    };
    env.open_with(Env::config(), options).expect("照常打开");
    assert!(stale.exists());
}

#[test]
fn not_an_iwork_database_is_refused() {
    let env = Env::new();
    std::fs::create_dir_all(env.dir.root()).unwrap();
    let conn = Connection::open(env.db()).unwrap();
    conn.execute_batch("CREATE TABLE other(x);").unwrap();
    drop(conn);
    let before = fingerprint(&env.db());
    let err = env
        .open_with(Env::config(), OpenOptions::default())
        .err()
        .unwrap();
    assert!(
        matches!(&err, StorageError::Io { message } if message.contains("不是 iWork 数据库")),
        "{err:?}"
    );
    assert_eq!(fingerprint(&env.db()), before);
}
