//! 制品存储 `FsArtifactStore`（技术设计 5.4、9.2；方案 2.10、ADR-T02-c）。
//!
//! - 写入：`<区>/tmp/<随机>.part` 边写边算 sha256 → fsync → **先**在写线程上登记或刷新
//!   `artifact_objects.created_at` → 再改名到 `<区>/sha256/<前 2 位>/<其余>`。写到一半崩溃只会在
//!   `tmp/` 留下随机名的半截文件，不会出现以哈希命名的坏文件。
//! - 引用：「有没有引用」由投影表 `artifact_refs` 回答（从事件推导）；`retain/release` 只是显式 pin。
//! - 回收：读侧只做预选；复查（无引用 ∧ 无 pin ∧ 过宽限期）、删行、删文件都在写线程的同一个事务里，
//!   与引用提交、pin、再次 put 串行。运行中不扫目录。
//! - 残留清理（`tmp/` 半截、有行无文件、有文件无行）只在启动恢复 [`FsArtifactStore::recover`] 做。

use std::fs::File;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iwork_domain::common::{ArtifactArea, ArtifactRef, Sha256, Timestamp};
use iwork_ports::artifact::{ArtifactError, ArtifactStore, GcReport};
use iwork_ports::clock::{Clock, IdGenerator};
use iwork_ports::store::{EventCodec, StorageError};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::Digest;

use crate::data_dir::{ALL_AREAS, DataDir};
use crate::reader::ReaderPool;
use crate::reply::{self, Reply};
use crate::sql::{ColumnText, parse_sha, sha_text, sql_err, u64_val};
use crate::store::SqliteStore;
use crate::writer::WriterHandle;

const CHUNK: usize = 64 * 1024;
const IO_THREADS: usize = 2;
const IO_GONE: &str = "制品 I/O 线程已退出";

/// 启动恢复的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoverReport {
    /// 删掉的 `tmp/` 半截文件
    pub tmp_removed: u64,
    /// 删掉的「有行无文件、也没被引用」的登记行
    pub rows_removed: u64,
    /// 删掉的「有文件没登记」的对象文件
    pub files_removed: u64,
    /// 「有行无文件、却还被引用」的对象：内容已丢，只记错误日志（方案已知限制）
    pub missing_referenced: u64,
}

/// 测试钩子：在两个关键时刻插一段代码，证明写线程上的复查挡得住竞态。
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct ArtifactHooks {
    /// `put` 的登记已提交、改名之前
    pub after_touch: Option<Arc<dyn Fn() + Send + Sync>>,
    /// `collect_garbage` 读侧预选之后、写线程复查之前
    pub after_select: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// 制品存储。
pub struct FsArtifactStore<G: IdGenerator, K: Clock> {
    inner: Arc<Inner<G, K>>,
}

struct Inner<G, K> {
    dir: DataDir,
    writer: WriterHandle,
    readers: Arc<ReaderPool>,
    io: IoPool,
    ids: Arc<G>,
    clock: Arc<K>,
    grace: Duration,
    max_batch: usize,
    delivery_started: Arc<AtomicBool>,
    hooks: Mutex<ArtifactHooks>,
}

impl<G: IdGenerator + 'static, K: Clock + 'static> FsArtifactStore<G, K> {
    /// 与 `store` 共用写线程与读线程池；宽限期取 `store` 的配置。
    pub fn new<C: EventCodec + 'static>(
        store: &SqliteStore<C>,
        ids: Arc<G>,
        clock: Arc<K>,
    ) -> Result<Self, ArtifactError> {
        let io = IoPool::start(IO_THREADS).map_err(|e| ArtifactError::Io {
            path: store.dir.root().to_path_buf(),
            message: format!("起制品 I/O 线程失败：{e}"),
        })?;
        Ok(Self {
            inner: Arc::new(Inner {
                dir: store.dir.clone(),
                writer: store.writer.clone(),
                readers: store.readers.clone(),
                io,
                ids,
                clock,
                grace: store.config.gc_grace,
                max_batch: store.config.max_batch.max(1),
                delivery_started: store.delivery_started.clone(),
                hooks: Mutex::new(ArtifactHooks::default()),
            }),
        })
    }

    #[doc(hidden)]
    pub fn set_hooks(&self, hooks: ArtifactHooks) {
        *lock(&self.inner.hooks) = hooks;
    }

    /// 从任意读源写入（`put_bytes` / `put_file` 共用）。测试用它注入写到一半出错或崩溃的读源。
    #[doc(hidden)]
    pub fn put_reader<R: Read + Send + 'static>(
        &self,
        area: ArtifactArea,
        source: R,
    ) -> impl Future<Output = Result<ArtifactRef, ArtifactError>> + Send + use<G, K, R> {
        let inner = self.inner.clone();
        let root = inner.dir.root().to_path_buf();
        let reply = self.inner.io.run(move || inner.write_stream(area, source));
        async move { flatten(reply.await, &root) }
    }

    /// 启动恢复（方案 2.10）：清 `tmp/` 半截、有行无文件、有文件无行三类残留。
    /// 只能在 `mark_delivery_started()` 之前、任何 put 之前调；之后调用返回错误且不动任何文件。
    pub fn recover(
        &self,
    ) -> impl Future<Output = Result<RecoverReport, ArtifactError>> + Send + use<G, K> {
        let inner = self.inner.clone();
        let root = inner.dir.root().to_path_buf();
        let started = inner.delivery_started.load(Ordering::SeqCst);
        let reply = if started {
            Reply::ready(Ok(Err(ArtifactError::Io {
                path: root.clone(),
                message: "投递已开始，制品残留清理只能在启动阶段执行".into(),
            })))
        } else {
            self.inner.io.run(move || inner.recover_blocking())
        };
        async move { flatten(reply.await, &root) }
    }
}

/// I/O 线程上的结果是 `Result<Result<T, ArtifactError>, StorageError>`：外层是线程没了。
fn flatten<T>(
    r: Result<Result<T, ArtifactError>, StorageError>,
    root: &Path,
) -> Result<T, ArtifactError> {
    r.map_err(|e| storage_err(root, e))?
}

fn storage_err(path: &Path, e: StorageError) -> ArtifactError {
    ArtifactError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

fn io_err(path: &Path, e: std::io::Error) -> ArtifactError {
    ArtifactError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `release` 的结果：配对错误是数据，不是 SQLite 错误。
enum PinOutcome {
    Done,
    NotRegistered,
    AlreadyZero,
}

impl<G: IdGenerator, K: Clock> Inner<G, K> {
    fn object_path(&self, area: ArtifactArea, sha: &Sha256) -> PathBuf {
        let hex = sha_text(sha);
        self.dir.area_objects(area).join(&hex[..2]).join(&hex[2..])
    }

    fn write_stream<R: Read>(
        &self,
        area: ArtifactArea,
        mut source: R,
    ) -> Result<ArtifactRef, ArtifactError> {
        let tmp = self
            .dir
            .area_tmp(area)
            .join(format!("{:032x}.part", self.ids.next().to_u128()));
        let mut file = create_private_file(&tmp)?;
        let written = (|| {
            let mut hasher = sha2::Sha256::new();
            let mut size = 0u64;
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = source.read(&mut buf).map_err(|e| io_err(&tmp, e))?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n]).map_err(|e| io_err(&tmp, e))?;
                hasher.update(&buf[..n]);
                size += n as u64;
            }
            file.sync_all().map_err(|e| io_err(&tmp, e))?;
            Ok(ArtifactRef {
                sha256: Sha256(hasher.finalize().into()),
                area,
                size,
            })
        })();
        drop(file);
        let r = match written {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };
        // 先登记（或刷新 created_at，旧对象交给新调用者时重新获得一整个宽限期）再改名：
        // GC 的复查排在这次登记之后，看到新鲜的 created_at 就跳过
        if let Err(e) = self.touch(r).wait() {
            let _ = std::fs::remove_file(&tmp);
            return Err(storage_err(&tmp, e));
        }
        if let Some(hook) = lock(&self.hooks).after_touch.clone() {
            hook();
        }
        let target = self.object_path(area, &r.sha256);
        let parent = target.parent().expect("对象路径有上级目录");
        create_private_dirs(parent)?;
        // 目标已存在也覆盖：内容按哈希相同，覆盖无害
        if let Err(e) = std::fs::rename(&tmp, &target) {
            // Windows 上目标正被别的句柄读着时改名会失败：目标在且大小相同就算成功
            let same = std::fs::metadata(&target).is_ok_and(|m| m.len() == r.size);
            let _ = std::fs::remove_file(&tmp);
            if !same {
                return Err(io_err(&target, e));
            }
        }
        sync_dir(parent)?;
        tracing::debug!(sha256 = %r.sha256, size = r.size, "写入制品");
        Ok(r)
    }

    fn touch(&self, r: ArtifactRef) -> Reply<()> {
        self.writer.sql(move |conn, now: Timestamp| {
            conn.execute(
                "INSERT INTO artifact_objects(area, sha256, size, pins, created_at) VALUES (?1, ?2, ?3, 0, ?4) \
                 ON CONFLICT(area, sha256) DO UPDATE SET created_at = excluded.created_at",
                params![r.area.to_text(), sha_text(&r.sha256), u64_val("制品大小", r.size)?, now.0],
            )
            .map_err(sql_err)?;
            Ok(())
        })
    }

    fn recover_blocking(&self) -> Result<RecoverReport, ArtifactError> {
        let root = self.dir.root().to_path_buf();
        let mut report = RecoverReport::default();
        for area in ALL_AREAS {
            // a. tmp/ 下的半截文件
            let tmp = self.dir.area_tmp(area);
            for entry in read_dir(&tmp)? {
                std::fs::remove_file(&entry).map_err(|e| io_err(&entry, e))?;
                report.tmp_removed += 1;
            }

            // b. 有行没文件：无引用删行；有引用只记错误（内容已丢）
            let area_text = area.to_text();
            let registered: Vec<(String, bool)> = self
                .readers
                .run(move |conn| {
                    let mut stmt = conn
                        .prepare(
                            "SELECT o.sha256, EXISTS (SELECT 1 FROM artifact_refs r \
                             WHERE r.area = o.area AND r.sha256 = o.sha256) \
                             FROM artifact_objects o WHERE o.area = ?1",
                        )
                        .map_err(sql_err)?;
                    let rows = stmt
                        .query_map([area_text], |r| Ok((r.get(0)?, r.get(1)?)))
                        .map_err(sql_err)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(sql_err)?;
                    Ok(rows)
                })
                .wait()
                .map_err(|e| storage_err(&root, e))?;
            let mut orphan_rows = Vec::new();
            for (hex, referenced) in &registered {
                let Some(sha) = parse_sha(hex) else {
                    return Err(ArtifactError::Io {
                        path: root.clone(),
                        message: format!("artifact_objects.sha256 的值 {hex:?} 不合法"),
                    });
                };
                if !self.object_path(area, &sha).exists() {
                    if *referenced {
                        tracing::error!(sha256 = %hex, area = area_text, "被引用的制品文件不见了，内容已丢失");
                        report.missing_referenced += 1;
                    } else {
                        orphan_rows.push(hex.clone());
                    }
                }
            }
            if !orphan_rows.is_empty() {
                let n = orphan_rows.len() as u64;
                self.writer
                    .sql(move |conn, _| {
                        for hex in &orphan_rows {
                            conn.execute(
                                "DELETE FROM artifact_objects WHERE area = ?1 AND sha256 = ?2",
                                params![area_text, hex],
                            )
                            .map_err(sql_err)?;
                        }
                        Ok(())
                    })
                    .wait()
                    .map_err(|e| storage_err(&root, e))?;
                report.rows_removed += n;
            }

            // c. 有文件没登记
            let known: std::collections::HashSet<String> =
                registered.into_iter().map(|(h, _)| h).collect();
            for prefix in read_dir(&self.dir.area_objects(area))? {
                for file in read_dir(&prefix)? {
                    let hex = format!(
                        "{}{}",
                        prefix.file_name().unwrap_or_default().to_string_lossy(),
                        file.file_name().unwrap_or_default().to_string_lossy()
                    );
                    if !known.contains(&hex) {
                        std::fs::remove_file(&file).map_err(|e| io_err(&file, e))?;
                        report.files_removed += 1;
                    }
                }
            }
        }
        tracing::info!(?report, "制品启动恢复完成");
        Ok(report)
    }
}

/// 目录下的全部条目路径；目录不存在当作空。
fn read_dir(dir: &Path) -> Result<Vec<PathBuf>, ArtifactError> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.map(|e| e.path()).map_err(|e| io_err(dir, e)))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(io_err(dir, e)),
    }
}

/// 新文件只有本人可读写（`private/` 区存凭据附件副本，方案 2.10；其他区同样处理，不放宽）。
fn create_private_file(path: &Path) -> Result<File, ArtifactError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).map_err(|e| io_err(path, e))
}

fn create_private_dirs(dir: &Path) -> Result<(), ArtifactError> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir).map_err(|e| io_err(dir, e))
}

/// 改名落盘：Unix 上 fsync 目录；Windows 没有目录 fsync。
fn sync_dir(dir: &Path) -> Result<(), ArtifactError> {
    #[cfg(unix)]
    {
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| io_err(dir, e))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

impl<G: IdGenerator + 'static, K: Clock + 'static> ArtifactStore for FsArtifactStore<G, K> {
    fn put_bytes(
        &self,
        area: ArtifactArea,
        data: Vec<u8>,
    ) -> impl Future<Output = Result<ArtifactRef, ArtifactError>> + Send {
        self.put_reader(area, std::io::Cursor::new(data))
    }

    /// 只读源文件复制，不移动、不改源文件。
    fn put_file(
        &self,
        area: ArtifactArea,
        path: &Path,
    ) -> impl Future<Output = Result<ArtifactRef, ArtifactError>> + Send {
        let inner = self.inner.clone();
        let root = inner.dir.root().to_path_buf();
        let path = path.to_path_buf();
        let reply = self.inner.io.run(move || {
            let file = File::open(&path).map_err(|e| io_err(&path, e))?;
            inner.write_stream(area, file)
        });
        async move { flatten(reply.await, &root) }
    }

    fn read_range(
        &self,
        r: &ArtifactRef,
        range: Range<u64>,
    ) -> impl Future<Output = Result<Vec<u8>, ArtifactError>> + Send {
        let inner = self.inner.clone();
        let r = *r;
        let root = inner.dir.root().to_path_buf();
        async move {
            let (area, sha) = (r.area.to_text(), sha_text(&r.sha256));
            let size: Option<i64> = inner
                .readers
                .run(move |conn| {
                    conn.query_row(
                        "SELECT size FROM artifact_objects WHERE area = ?1 AND sha256 = ?2",
                        params![area, sha],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql_err)
                })
                .await
                .map_err(|e| storage_err(&root, e))?;
            let size = size.ok_or(ArtifactError::NotFound { sha256: r.sha256 })? as u64;
            if range.start > range.end || range.end > size {
                return Err(ArtifactError::RangeInvalid {
                    sha256: r.sha256,
                    start: range.start,
                    end: range.end,
                    size,
                });
            }
            let path = inner.object_path(r.area, &r.sha256);
            let reply = inner.io.run(move || {
                let mut f = File::open(&path).map_err(|e| io_err(&path, e))?;
                f.seek(SeekFrom::Start(range.start))
                    .map_err(|e| io_err(&path, e))?;
                let mut buf = vec![0u8; (range.end - range.start) as usize];
                f.read_exact(&mut buf).map_err(|e| io_err(&path, e))?;
                Ok(buf)
            });
            flatten(reply.await, &root)
        }
    }

    fn retain(&self, r: &ArtifactRef) -> impl Future<Output = Result<(), ArtifactError>> + Send {
        self.pin(r, true)
    }

    fn release(&self, r: &ArtifactRef) -> impl Future<Output = Result<(), ArtifactError>> + Send {
        self.pin(r, false)
    }

    fn collect_garbage(&self) -> impl Future<Output = Result<GcReport, ArtifactError>> + Send {
        let inner = self.inner.clone();
        let root = inner.dir.root().to_path_buf();
        async move {
            // 1. 读侧预选（只为不占写线程做全表扫描）；结果只是候选
            let grace_ms = i64::try_from(inner.grace.as_millis()).unwrap_or(i64::MAX);
            let cutoff = inner.clock.now().0.saturating_sub(grace_ms);
            let limit = i64::try_from(inner.max_batch).unwrap_or(i64::MAX);
            let candidates: Vec<(String, String)> = inner
                .readers
                .run(move |conn| {
                    let mut stmt = conn
                        .prepare_cached(
                            "SELECT area, sha256 FROM artifact_objects o WHERE pins = 0 AND created_at < ?1 \
                             AND NOT EXISTS (SELECT 1 FROM artifact_refs r WHERE r.area = o.area \
                             AND r.sha256 = o.sha256) LIMIT ?2",
                        )
                        .map_err(sql_err)?;
                    let rows = stmt
                        .query_map(params![cutoff, limit], |r| Ok((r.get(0)?, r.get(1)?)))
                        .map_err(sql_err)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(sql_err)?;
                    Ok(rows)
                })
                .await
                .map_err(|e| storage_err(&root, e))?;
            if candidates.is_empty() {
                return Ok(GcReport::default());
            }
            if let Some(hook) = lock(&inner.hooks).after_select.clone() {
                hook();
            }
            // 2. 写线程上：复查、删行、删文件，同一个事务
            let objects = inner.dir.clone();
            let grace = grace_ms;
            inner
                .writer
                .exclusive(move |conn, now| {
                    gc_in_tx(conn, &objects, &candidates, now.0.saturating_sub(grace))
                })
                .await
                .map_err(|e| storage_err(&root, e))
        }
    }
}

impl<G: IdGenerator + 'static, K: Clock + 'static> FsArtifactStore<G, K> {
    fn pin(
        &self,
        r: &ArtifactRef,
        up: bool,
    ) -> impl Future<Output = Result<(), ArtifactError>> + Send + use<G, K> {
        let r = *r;
        let path = self.inner.object_path(r.area, &r.sha256);
        let reply = self.inner.writer.sql(move |conn, _| {
            let (area, sha) = (r.area.to_text(), sha_text(&r.sha256));
            let pins: Option<i64> = conn
                .query_row(
                    "SELECT pins FROM artifact_objects WHERE area = ?1 AND sha256 = ?2",
                    params![area, sha],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            let outcome = match pins {
                None => PinOutcome::NotRegistered,
                Some(0) if !up => PinOutcome::AlreadyZero,
                Some(_) => {
                    conn.execute(
                        "UPDATE artifact_objects SET pins = pins + ?1 WHERE area = ?2 AND sha256 = ?3",
                        params![if up { 1 } else { -1 }, area, sha],
                    )
                    .map_err(sql_err)?;
                    PinOutcome::Done
                }
            };
            Ok(outcome)
        });
        async move {
            match reply.await.map_err(|e| storage_err(&path, e))? {
                PinOutcome::Done => Ok(()),
                PinOutcome::NotRegistered => Err(ArtifactError::NotFound { sha256: r.sha256 }),
                PinOutcome::AlreadyZero => Err(ArtifactError::Io {
                    path,
                    message: "pins 已为 0".into(),
                }),
            }
        }
    }
}

/// GC 的写线程部分（方案 2.10 第 2 步）。删文件放在事务里、删行之后：先 COMMIT 再删文件会和
/// 「登记 → 改名」形成竞态。
fn gc_in_tx(
    conn: &Connection,
    dir: &DataDir,
    candidates: &[(String, String)],
    cutoff: i64,
) -> Result<GcReport, StorageError> {
    let exec = |sql: &str| conn.execute_batch(sql).map_err(sql_err);
    exec("BEGIN IMMEDIATE")?;
    let body = || -> Result<GcReport, StorageError> {
        let mut report = GcReport::default();
        for (area_text, hex) in candidates {
            exec("SAVEPOINT gc")?;
            let still: Option<i64> = conn
                .query_row(
                    "SELECT size FROM artifact_objects o WHERE area = ?1 AND sha256 = ?2 AND pins = 0 \
                     AND created_at < ?3 AND NOT EXISTS (SELECT 1 FROM artifact_refs r \
                     WHERE r.area = o.area AND r.sha256 = o.sha256)",
                    params![area_text, hex, cutoff],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            let Some(size) = still else {
                // 选出之后有人引用、pin、或重新 put 刷新了 created_at
                exec("RELEASE gc")?;
                continue;
            };
            conn.execute(
                "DELETE FROM artifact_objects WHERE area = ?1 AND sha256 = ?2",
                params![area_text, hex],
            )
            .map_err(sql_err)?;
            let (Some(area), Some(_)) = (ArtifactArea::from_text(area_text), parse_sha(hex)) else {
                return Err(StorageError::Io {
                    message: format!("artifact_objects 里的 ({area_text}, {hex}) 不合法"),
                });
            };
            let path = dir.area_objects(area).join(&hex[..2]).join(&hex[2..]);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "回收制品时删文件失败，下次再试");
                    exec("ROLLBACK TO gc")?;
                    exec("RELEASE gc")?;
                    continue;
                }
            }
            exec("RELEASE gc")?;
            report.removed += 1;
            report.freed_bytes += size as u64;
        }
        Ok(report)
    };
    match body().and_then(|r| exec("COMMIT").map(|()| r)) {
        Ok(r) => {
            tracing::info!(
                removed = r.removed,
                freed_bytes = r.freed_bytes,
                "制品回收完成"
            );
            Ok(r)
        }
        Err(e) => {
            if !conn.is_autocommit() {
                let _ = exec("ROLLBACK");
            }
            Err(e)
        }
    }
}

/// 制品 I/O 线程池：读写文件不占调用方（异步任务）的线程，也不占写线程。
struct IoPool {
    tx: SyncSender<Box<dyn FnOnce() + Send>>,
}

impl IoPool {
    fn start(threads: usize) -> std::io::Result<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Box<dyn FnOnce() + Send>>(256);
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..threads {
            let rx: Arc<Mutex<Receiver<Box<dyn FnOnce() + Send>>>> = rx.clone();
            std::thread::Builder::new()
                .name(format!("iwork-artifact-io-{i}"))
                .spawn(move || {
                    loop {
                        let job = lock(&rx).recv();
                        let Ok(job) = job else { return };
                        // 任务 panic 不带走线程：调用方因发送端被丢弃而得到错误
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                })?;
        }
        Ok(Self { tx })
    }

    fn run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Reply<T> {
        let (reply_tx, reply_rx) = reply::channel(IO_GONE);
        match self.tx.try_send(Box::new(move || reply_tx.send(Ok(f())))) {
            Ok(()) => reply_rx,
            Err(TrySendError::Full(_)) => Reply::ready(Err(StorageError::Busy)),
            Err(TrySendError::Disconnected(_)) => Reply::ready(Err(StorageError::Io {
                message: IO_GONE.into(),
            })),
        }
    }
}
