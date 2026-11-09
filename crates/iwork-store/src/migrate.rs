//! 打开前的探测与迁移（技术设计 5.5，方案 2.2）。
//!
//! - **协议主版本门**：在 `iwork.db`、`-wal`、`-shm` 的**副本**上读 `meta`，原文件不被任何 SQLite
//!   连接打开（只读连接也会删掉预先存在的空 WAL、建 `-shm`，验收 5a 要三个文件都不变）。
//! - **迁移**：编译进二进制、只向前；执行前 `VACUUM INTO` 备份；本次全部待执行迁移与
//!   `schema_version` 更新放在同一个事务，失败整体退回升级前。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use iwork_domain::common::Timestamp;
use iwork_ports::store::StorageError;
use rusqlite::{Connection, OptionalExtension, params};

use crate::data_dir::DataDir;
use crate::sql::sql_err;

/// 一个迁移：编号从 1 连续递增，`sql` 编进二进制。
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

/// 迁移登记表。加一个迁移 = `migrations/` 加一个文件 + 这里一行；只能往后加，不能改已发布的。
pub(crate) const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "initial",
    sql: include_str!("migrations/0001_initial.sql"),
}];

/// `meta` 表的三个键。
const KEY_SCHEMA_VERSION: &str = "schema_version";
const KEY_PROTOCOL_MAJOR: &str = "protocol_major";
const KEY_CREATED_AT: &str = "created_at";

/// 探测用副本目录名的前缀。主库固定是数据目录根下的 `iwork.db`，这里的东西永远不会被当成主库。
const PROBE_PREFIX: &str = ".probe-";

/// 打开流程里能替换的文件操作（测试注入失败用）。生产用 [`OpenHooks::default`]。
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct OpenHooks {
    /// 删除探测副本目录（本次的与上次崩溃残留的）。
    pub remove_probe_dir: fn(&Path) -> std::io::Result<()>,
}

impl Default for OpenHooks {
    fn default() -> Self {
        Self {
            remove_probe_dir: |p| std::fs::remove_dir_all(p),
        }
    }
}

/// 探测结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Probe {
    /// 库文件不存在或为 0 字节
    NewDatabase,
    /// 主版本相符、schema 不比程序新
    Existing { schema_version: u32 },
}

/// 协议主版本门与 schema 版本探测（方案 2.2 第 2 步）。原文件自始至终不被打开。
pub(crate) fn probe(
    dir: &DataDir,
    program_major: u32,
    now: Timestamp,
    migrations: &[Migration],
    hooks: &OpenHooks,
) -> Result<Probe, StorageError> {
    clean_stale_probes(dir, hooks);
    let db = dir.db_path();
    let len = match std::fs::metadata(&db) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(io_err(&db, e)),
    };
    if len == 0 {
        return Ok(Probe::NewDatabase);
    }

    let probe_dir = dir.backups().join(probe_dir_name(now));
    let result = copy_db_files(&db, &probe_dir)
        .and_then(|copy| inspect_copy(dir, &copy, program_major, now, migrations));
    // 副本连接已在 inspect_copy 里关掉；清理失败不改变探测结论（方案 2.2 第 2 步最后一条）
    if probe_dir.exists()
        && let Err(e) = (hooks.remove_probe_dir)(&probe_dir)
    {
        tracing::warn!(path = %probe_dir.display(), error = %e, "删除探测副本失败，下次启动再试");
    }
    result
}

/// 迁移或建新库（方案 2.2 第 4、5 步）。`conn` 是已设好 PRAGMA 的读写连接。
pub(crate) fn migrate(
    conn: &Connection,
    dir: &DataDir,
    probe: Probe,
    program_major: u32,
    now: Timestamp,
    migrations: &[Migration],
) -> Result<(), StorageError> {
    check_registry(migrations)?;
    let target = migrations.len() as u32;
    match probe {
        Probe::NewDatabase => {
            run_in_tx(conn, |conn| {
                for m in migrations {
                    conn.execute_batch(m.sql)
                        .map_err(|e| migration_err(m, &e, None))?;
                }
                let mut put = conn
                    .prepare("INSERT INTO meta(key, value) VALUES (?1, ?2)")
                    .map_err(sql_err)?;
                for (k, v) in [
                    (KEY_SCHEMA_VERSION, target.to_string()),
                    (KEY_PROTOCOL_MAJOR, program_major.to_string()),
                    (KEY_CREATED_AT, now.0.to_string()),
                ] {
                    put.execute(params![k, v]).map_err(sql_err)?;
                }
                Ok(())
            })?;
            tracing::info!(
                schema_version = target,
                protocol_major = program_major,
                "新建数据库"
            );
            Ok(())
        }
        Probe::Existing { schema_version } if schema_version == target => Ok(()),
        Probe::Existing { schema_version } => {
            let backup = dir
                .backups()
                .join(format!("{}-schema-v{schema_version}.db", utc_stamp(now)));
            vacuum_into(conn, &backup).map_err(|message| StorageError::Io {
                message: format!("迁移前备份到 {} 失败，未迁移：{message}", backup.display()),
            })?;
            let pending = &migrations[schema_version as usize..];
            run_in_tx(conn, |conn| {
                for m in pending {
                    conn.execute_batch(m.sql)
                        .map_err(|e| migration_err(m, &e, Some(&backup)))?;
                }
                conn.execute(
                    "UPDATE meta SET value = ?1 WHERE key = ?2",
                    params![target.to_string(), KEY_SCHEMA_VERSION],
                )
                .map_err(sql_err)?;
                Ok(())
            })?;
            tracing::info!(
                from = schema_version,
                to = target,
                backup = %backup.display(),
                "数据库迁移完成"
            );
            Ok(())
        }
    }
}

/// 迁移编号必须从 1 连续递增：登记表写错是代码缺陷，在动库之前就拒绝。
fn check_registry(migrations: &[Migration]) -> Result<(), StorageError> {
    for (i, m) in migrations.iter().enumerate() {
        if m.version as usize != i + 1 {
            return Err(StorageError::Io {
                message: format!(
                    "迁移登记表第 {} 项的编号是 {}（应为 {}）",
                    i + 1,
                    m.version,
                    i + 1
                ),
            });
        }
    }
    Ok(())
}

fn migration_err(m: &Migration, e: &rusqlite::Error, backup: Option<&Path>) -> StorageError {
    let backup = backup.map_or(String::new(), |b| {
        format!("；升级前的备份在 {}", b.display())
    });
    StorageError::Io {
        message: format!(
            "迁移 {:04}_{} 失败，数据库保持升级前的版本：{e}{backup}",
            m.version, m.name
        ),
    }
}

/// `BEGIN IMMEDIATE … COMMIT`，失败 `ROLLBACK`。SQLite 的 DDL 是事务性的，失败后一张新表都不留。
fn run_in_tx(
    conn: &Connection,
    body: impl FnOnce(&Connection) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    conn.execute_batch("BEGIN IMMEDIATE").map_err(sql_err)?;
    match body(conn).and_then(|()| conn.execute_batch("COMMIT").map_err(sql_err)) {
        Ok(()) => Ok(()),
        Err(e) => {
            if !conn.is_autocommit()
                && let Err(rb) = conn.execute_batch("ROLLBACK")
            {
                tracing::error!(error = %rb, "迁移事务回滚失败");
            }
            Err(e)
        }
    }
}

/// 副本里的三个文件。
struct ProbeCopy {
    db: PathBuf,
}

/// 原样复制 `iwork.db` 与存在的 `-wal`、`-shm`。此时单实例锁已拿到（T04），没有别的进程在写。
fn copy_db_files(db: &Path, probe_dir: &Path) -> Result<ProbeCopy, StorageError> {
    std::fs::create_dir_all(probe_dir).map_err(|e| io_err(probe_dir, e))?;
    let copy_db = probe_dir.join(db.file_name().expect("库路径有文件名"));
    for suffix in ["", "-wal", "-shm"] {
        let src = sibling(db, suffix);
        let dst = sibling(&copy_db, suffix);
        match std::fs::copy(&src, &dst) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !suffix.is_empty() => {}
            Err(e) => return Err(io_err(&src, e)),
        }
    }
    Ok(ProbeCopy { db: copy_db })
}

/// 在副本上读 `meta`。函数返回前副本连接已关闭（`conn` 在这里 drop）。
fn inspect_copy(
    dir: &DataDir,
    copy: &ProbeCopy,
    program_major: u32,
    now: Timestamp,
    migrations: &[Migration],
) -> Result<Probe, StorageError> {
    let db = dir.db_path();
    let conn = Connection::open(&copy.db).map_err(sql_err)?;
    let has_meta: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'meta')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| StorageError::Io {
            message: format!("{} 不是 iWork 数据库（读不出表结构：{e}）", db.display()),
        })?;
    if !has_meta {
        return Err(StorageError::Io {
            message: format!("{} 不是 iWork 数据库（无 meta 表）", db.display()),
        });
    }
    let read_u32 = |key: &str| -> Result<u32, StorageError> {
        let value: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .map_err(sql_err)?;
        let value = value.ok_or_else(|| StorageError::Io {
            message: format!("{} 的 meta 表缺 {key}", db.display()),
        })?;
        value.parse().map_err(|_| StorageError::Io {
            message: format!("{} 的 meta.{key} 不是数字：{value:?}", db.display()),
        })
    };

    let database_major = read_u32(KEY_PROTOCOL_MAJOR)?;
    if database_major != program_major {
        let backup = dir
            .backups()
            .join(format!("{}-protocol-v{database_major}.db", utc_stamp(now)));
        return match vacuum_into(&conn, &backup) {
            Ok(()) => {
                tracing::warn!(
                    database = database_major,
                    program = program_major,
                    backup = %backup.display(),
                    "协议主版本不符，已备份并拒绝启动"
                );
                Err(StorageError::ProtocolMajorMismatch {
                    database: database_major,
                    program: program_major,
                    backup,
                })
            }
            Err(message) => Err(StorageError::Io {
                message: format!(
                    "数据库协议主版本 {database_major} 与程序 {program_major} 不符，\
                     备份到 {} 失败，拒绝启动：{message}",
                    backup.display()
                ),
            }),
        };
    }

    let schema_version = read_u32(KEY_SCHEMA_VERSION)?;
    let known = migrations.len() as u32;
    if schema_version > known {
        return Err(StorageError::Io {
            message: format!(
                "{} 的 schema 版本 {schema_version} 比程序认识的 {known} 新（被更新的程序写过），拒绝打开",
                db.display()
            ),
        });
    }
    Ok(Probe::Existing { schema_version })
}

/// `VACUUM INTO`：WAL 里已提交的帧一起合进备份，得到一个独立的、不带 WAL 的库文件。
fn vacuum_into(conn: &Connection, target: &Path) -> Result<(), String> {
    let target = target
        .to_str()
        .ok_or_else(|| format!("路径不是 UTF-8：{}", target.display()))?;
    conn.execute("VACUUM INTO ?1", [target])
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 删掉上次崩溃留下的 `.probe-*`；失败只记日志，不阻止启动。
fn clean_stale_probes(dir: &DataDir, hooks: &OpenHooks) {
    let Ok(entries) = std::fs::read_dir(dir.backups()) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(PROBE_PREFIX)
        {
            let path = entry.path();
            if let Err(e) = (hooks.remove_probe_dir)(&path) {
                tracing::warn!(path = %path.display(), error = %e, "删除上次残留的探测副本失败");
            }
        }
    }
}

/// `.probe-<进程号>-<时钟毫秒>-<进程内计数>`：同一进程内不重名（计数），跨进程靠进程号；
/// 不用 ULID 是因为 `open` 不带 ID 生成器（supervisor 2026-11-10 同意）。
fn probe_dir_name(now: Timestamp) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{PROBE_PREFIX}{}-{}-{n}", std::process::id(), now.0)
}

fn sibling(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.file_name().expect("库路径有文件名").to_os_string();
    name.push(suffix);
    db.with_file_name(name)
}

fn io_err(path: &Path, e: std::io::Error) -> StorageError {
    StorageError::Io {
        message: format!("{}：{e}", path.display()),
    }
}

/// UTC 时间戳 → `YYYYMMDDTHHMMSSmmmZ`（备份文件名用，按字典序即按时间序）。
pub(crate) fn utc_stamp(at: Timestamp) -> String {
    let ms = at.0.rem_euclid(1000);
    let secs = at.0.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}{ms:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

/// 1970-01-01 起的天数 → 公历年月日（Howard Hinnant 的 civil_from_days）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// 单测用：在一条连接上跑完全部迁移（不写 meta 之外的东西）。
#[cfg(test)]
pub(crate) fn apply_all_for_test(conn: &Connection) {
    for m in MIGRATIONS {
        conn.execute_batch(m.sql).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_stamp_formats_known_instants() {
        assert_eq!(utc_stamp(Timestamp(0)), "19700101T000000000Z");
        // 2026-11-10T03:04:05.678Z
        assert_eq!(
            utc_stamp(Timestamp(1_794_279_845_678)),
            "20261110T030405678Z"
        );
        // 闰年 2月29日
        assert_eq!(utc_stamp(Timestamp(951_782_400_000)), "20000229T000000000Z");
    }

    #[test]
    fn probe_dir_names_never_repeat_in_one_process() {
        let a = probe_dir_name(Timestamp(5));
        let b = probe_dir_name(Timestamp(5));
        assert_ne!(a, b);
        assert!(a.starts_with(PROBE_PREFIX));
    }

    #[test]
    fn registry_numbers_are_contiguous() {
        check_registry(MIGRATIONS).unwrap();
        let bad = [Migration {
            version: 2,
            name: "x",
            sql: "",
        }];
        assert!(check_registry(&bad).is_err(), "对照：跳号被拒");
    }
}
