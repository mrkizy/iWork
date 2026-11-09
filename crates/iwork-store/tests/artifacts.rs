//! 卡验收 4 / 方案第 5 步、S7、S8：制品存储。

mod support;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iwork_domain::attachment::AttachmentOrigin;
use iwork_domain::common::{ArtifactArea, ArtifactRef};
use iwork_domain::event::*;
use iwork_domain::message::{MessageBody, MessageStatus};
use iwork_ports::artifact::{ArtifactError, ArtifactStore, GcReport};
use iwork_ports::read_model::ReadModelPort;
use iwork_ports::store::StoragePort;
use iwork_store::ArtifactHooks;
use support::*;

const DAY: i64 = 24 * 3600 * 1000;

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_under(&p));
        } else {
            out.push(p);
        }
    }
    out.sort();
    out
}

fn objects(env: &Env, area: ArtifactArea) -> Vec<PathBuf> {
    files_under(&env.dir.area_objects(area))
}

fn tmp_files(env: &Env, area: ArtifactArea) -> Vec<PathBuf> {
    files_under(&env.dir.area_tmp(area))
}

fn rows(env: &Env) -> i64 {
    scalar(&env.db(), "SELECT COUNT(*) FROM artifact_objects")
}

struct World {
    env: Env,
    store: Arc<Store>,
    art: Artifacts,
}

fn world() -> World {
    let env = Env::new();
    let store = Arc::new(env.open());
    let art = env.artifacts(&store);
    block(store.commit(cmd(
        1,
        vec![
            ProjectAdded { project: project(1) }.into(),
            ConversationCreated { conversation: project_conversation(10, 1) }.into(),
            TurnQueued { turn: turn(20, 10) }.into(),
            MessageAppended { message: assistant_message(30, 10, 20) }.into(),
        ],
    )))
    .unwrap();
    World { env, store, art }
}

/// 用户发一条正文在制品里的长消息（Sent + Artifact）。
fn sent_with_body(n: u128, r: ArtifactRef) -> DomainEvent {
    let mut m = user_message(n, 10, "");
    m.body = MessageBody::Artifact(r);
    MessageAppended { message: m }.into()
}

fn complete_with(r: ArtifactRef) -> DomainEvent {
    MessageCompleted {
        conversation_id: cid(10),
        message_id: mid(30),
        body: MessageBody::Artifact(r),
        status: MessageStatus::Completed,
    }
    .into()
}

#[test]
fn same_content_stored_once() {
    let w = world();
    let a = block(w.art.put_bytes(ArtifactArea::Artifacts, b"hello".to_vec())).unwrap();
    assert_eq!(a.size, 5);
    w.env.clock.advance(1000);
    let b = block(w.art.put_bytes(ArtifactArea::Artifacts, b"hello".to_vec())).unwrap();
    assert_eq!(a, b);
    assert_eq!(
        objects(&w.env, ArtifactArea::Artifacts).len(),
        1,
        "相同内容只存一份"
    );
    assert_eq!(
        scalar(&w.env.db(), "SELECT created_at FROM artifact_objects"),
        T0 + 1000,
        "再次 put 刷新 created_at"
    );
    // 两线程并发写同内容
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let art = w.art.clone();
            std::thread::spawn(move || {
                block(art.put_bytes(ArtifactArea::Artifacts, vec![7u8; 200_000])).unwrap()
            })
        })
        .collect();
    let refs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(refs[0], refs[1]);
    assert_eq!(objects(&w.env, ArtifactArea::Artifacts).len(), 2);
    assert_eq!(rows(&w.env), 2);
    assert_eq!(
        block(w.art.read_range(&refs[0], 0..200_000)).unwrap(),
        vec![7u8; 200_000]
    );
    assert!(tmp_files(&w.env, ArtifactArea::Artifacts).is_empty());
}

#[test]
fn put_file_copies_without_touching_source() {
    let w = world();
    let src = w.env.tmp.path().join("src.txt");
    std::fs::write(&src, b"file body").unwrap();
    let r = block(w.art.put_file(ArtifactArea::Attachments, &src)).unwrap();
    assert_eq!(block(w.art.read_range(&r, 0..9)).unwrap(), b"file body");
    assert_eq!(std::fs::read(&src).unwrap(), b"file body", "源文件不动");
}

/// 读到 `fail_at` 字节后出错（`panic = true` 时直接 panic，模拟进程在写到一半时没了）。
struct Faulty {
    sent: usize,
    fail_at: usize,
    panic: bool,
}

impl Read for Faulty {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.sent >= self.fail_at {
            if self.panic {
                panic!("注入：写到一半崩溃");
            }
            return Err(std::io::Error::other("注入：读源出错"));
        }
        let n = buf.len().min(self.fail_at - self.sent).min(1000);
        buf[..n].fill(b'x');
        self.sent += n;
        Ok(n)
    }
}

#[test]
fn crash_mid_write_leaves_no_hash_named_file() {
    let w = world();
    // 读源出错：返回错误、临时文件清掉
    let err = block(w.art.put_reader(
        ArtifactArea::Artifacts,
        Faulty {
            sent: 0,
            fail_at: 5000,
            panic: false,
        },
    ));
    assert!(matches!(err, Err(ArtifactError::Io { .. })), "{err:?}");
    assert!(tmp_files(&w.env, ArtifactArea::Artifacts).is_empty());

    // 写到一半崩溃：半截只在 tmp/，名字不是哈希
    let crashed = block(w.art.put_reader(
        ArtifactArea::Artifacts,
        Faulty {
            sent: 0,
            fail_at: 5000,
            panic: true,
        },
    ));
    assert!(crashed.is_err());
    assert!(
        objects(&w.env, ArtifactArea::Artifacts).is_empty(),
        "sha256/ 下没有新文件"
    );
    let parts = tmp_files(&w.env, ArtifactArea::Artifacts);
    assert_eq!(parts.len(), 1);
    assert!(parts[0].extension().is_some_and(|e| e == "part"));
    assert_eq!(rows(&w.env), 0, "没登记");

    // 运行中 GC 不扫目录：半截还在
    w.env.clock.advance(2 * DAY);
    block(w.art.collect_garbage()).unwrap();
    assert_eq!(tmp_files(&w.env, ArtifactArea::Artifacts).len(), 1);

    // 下次启动、任何 put 之前：recover 清掉
    let report = block(w.art.recover()).unwrap();
    assert_eq!(report.tmp_removed, 1);
    assert!(tmp_files(&w.env, ArtifactArea::Artifacts).is_empty());
    // I/O 线程没被 panic 带走
    block(w.art.put_bytes(ArtifactArea::Artifacts, b"after".to_vec())).unwrap();
}

#[test]
fn read_range_bounds() {
    let w = world();
    let r = block(
        w.art
            .put_bytes(ArtifactArea::Artifacts, b"0123456789".to_vec()),
    )
    .unwrap();
    assert_eq!(block(w.art.read_range(&r, 2..5)).unwrap(), b"234");
    assert_eq!(block(w.art.read_range(&r, 10..10)).unwrap(), b"");
    assert!(matches!(
        block(w.art.read_range(&r, 5..11)),
        Err(ArtifactError::RangeInvalid {
            start: 5,
            end: 11,
            size: 10,
            ..
        })
    ));
    #[allow(clippy::reversed_empty_ranges)]
    let backwards = 6..5;
    assert!(matches!(
        block(w.art.read_range(&r, backwards)),
        Err(ArtifactError::RangeInvalid { .. })
    ));
    let missing = artifact(1, ArtifactArea::Artifacts);
    assert!(matches!(
        block(w.art.read_range(&missing, 0..1)),
        Err(ArtifactError::NotFound { .. })
    ));
    // 文件被用户手删：Io 点名路径
    std::fs::remove_file(&objects(&w.env, ArtifactArea::Artifacts)[0]).unwrap();
    assert!(matches!(
        block(w.art.read_range(&r, 0..1)),
        Err(ArtifactError::Io { .. })
    ));
}

#[test]
fn retain_release_pairing() {
    let w = world();
    let r = block(w.art.put_bytes(ArtifactArea::Artifacts, b"p".to_vec())).unwrap();
    assert!(matches!(
        block(w.art.release(&r)),
        Err(ArtifactError::Io { message, .. }) if message.contains("pins 已为 0")
    ));
    block(w.art.retain(&r)).unwrap();
    block(w.art.release(&r)).unwrap();
    let missing = artifact(1, ArtifactArea::Artifacts);
    assert!(matches!(
        block(w.art.retain(&missing)),
        Err(ArtifactError::NotFound { .. })
    ));
}

#[test]
fn gc_keeps_referenced_objects() {
    let w = world();
    let put = |b: &[u8]| block(w.art.put_bytes(ArtifactArea::Artifacts, b.to_vec())).unwrap();
    let (a, a2, b, c) = (put(b"A"), put(b"A'"), put(b"B"), put(b"C"));
    block(
        w.store
            .commit(cmd(2, vec![complete_with(a), sent_with_body(31, a2)])),
    )
    .unwrap();
    block(w.art.retain(&b)).unwrap();
    w.env.clock.advance(DAY + 1);
    let report = block(w.art.collect_garbage()).unwrap();
    assert_eq!(
        report,
        GcReport {
            removed: 1,
            freed_bytes: 1
        }
    );
    for kept in [a, a2, b] {
        assert!(
            block(w.art.read_range(&kept, 0..kept.size)).is_ok(),
            "{kept:?} 保留"
        );
    }
    assert!(matches!(
        block(w.art.read_range(&c, 0..1)),
        Err(ArtifactError::NotFound { .. })
    ));
    assert_eq!(objects(&w.env, ArtifactArea::Artifacts).len(), 3);
    // 对照：release B 后再回收，B 没了
    block(w.art.release(&b)).unwrap();
    assert_eq!(block(w.art.collect_garbage()).unwrap().removed, 1);
    assert_eq!(objects(&w.env, ArtifactArea::Artifacts).len(), 2);
}

#[test]
fn gc_respects_grace_period() {
    let w = world();
    let r = block(w.art.put_bytes(ArtifactArea::Artifacts, b"young".to_vec())).unwrap();
    w.env.clock.advance(DAY - 1);
    assert_eq!(
        block(w.art.collect_garbage()).unwrap().removed,
        0,
        "宽限期内不删"
    );
    w.env.clock.advance(2);
    assert_eq!(block(w.art.collect_garbage()).unwrap().removed, 1);
    assert!(block(w.art.read_range(&r, 0..1)).is_err());
}

/// 在预选之后、写线程复查之前插一段操作。
fn after_select(w: &World, f: impl Fn() + Send + Sync + 'static) {
    w.art.set_hooks(ArtifactHooks {
        after_select: Some(Arc::new(f)),
        ..ArtifactHooks::default()
    });
}

fn old_object(w: &World, body: &[u8]) -> ArtifactRef {
    let r = block(w.art.put_bytes(ArtifactArea::Artifacts, body.to_vec())).unwrap();
    w.env.clock.advance(DAY + 1);
    r
}

#[test]
fn gc_barrier_reference_after_select() {
    let w = world();
    let r = old_object(&w, b"ref");
    let store = w.store.clone();
    after_select(&w, move || {
        block(store.commit(cmd(2, vec![sent_with_body(31, r)]))).unwrap();
    });
    assert_eq!(block(w.art.collect_garbage()).unwrap().removed, 0);
    assert!(block(w.art.read_range(&r, 0..3)).is_ok());
    w.art.set_hooks(ArtifactHooks::default());
}

#[test]
fn gc_barrier_retain_after_select() {
    let w = world();
    let r = old_object(&w, b"pin");
    let art = w.art.clone();
    after_select(&w, move || block(art.retain(&r)).unwrap());
    assert_eq!(block(w.art.collect_garbage()).unwrap().removed, 0);
    assert!(block(w.art.read_range(&r, 0..3)).is_ok());
    w.art.set_hooks(ArtifactHooks::default());
}

#[test]
fn gc_barrier_reput_after_select() {
    let w = world();
    let r = old_object(&w, b"again");
    let art = w.art.clone();
    after_select(&w, move || {
        let again = block(art.put_bytes(ArtifactArea::Artifacts, b"again".to_vec())).unwrap();
        assert_eq!(again, r);
    });
    assert_eq!(block(w.art.collect_garbage()).unwrap().removed, 0);
    assert!(
        block(w.art.read_range(&r, 0..5)).is_ok(),
        "再次 put 返回的引用可读"
    );
    w.art.set_hooks(ArtifactHooks::default());
}

#[test]
fn gc_barrier_control_without_interference_deletes() {
    let w = world();
    let r = old_object(&w, b"gone");
    after_select(&w, || {});
    assert_eq!(
        block(w.art.collect_garbage()).unwrap().removed,
        1,
        "对照：什么都不做就删"
    );
    assert!(block(w.art.read_range(&r, 0..1)).is_err());
}

#[test]
fn gc_between_touch_and_rename_keeps_registration() {
    let w = world();
    let r = old_object(&w, b"touch");
    let art = w.art.clone();
    let reports = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = reports.clone();
    w.art.set_hooks(ArtifactHooks {
        after_touch: Some(Arc::new(move || {
            seen.lock()
                .unwrap()
                .push(block(art.collect_garbage()).unwrap())
        })),
        ..ArtifactHooks::default()
    });
    let again = block(w.art.put_bytes(ArtifactArea::Artifacts, b"touch".to_vec())).unwrap();
    w.art.set_hooks(ArtifactHooks::default());
    assert_eq!(
        reports.lock().unwrap().as_slice(),
        &[GcReport::default()],
        "登记已刷新，GC 跳过"
    );
    block(w.store.commit(cmd(2, vec![sent_with_body(31, again)]))).unwrap();
    assert_eq!(block(w.art.read_range(&r, 0..5)).unwrap(), b"touch");
    assert_eq!(rows(&w.env), 1, "登记行在");
}

#[test]
fn recover_cleans_three_kinds_of_residue() {
    let w = world();
    let referenced = block(w.art.put_bytes(ArtifactArea::Artifacts, b"kept".to_vec())).unwrap();
    let lost_ref = block(
        w.art
            .put_bytes(ArtifactArea::Artifacts, b"lost-but-referenced".to_vec()),
    )
    .unwrap();
    let orphan_row = block(
        w.art
            .put_bytes(ArtifactArea::Attachments, b"row-only".to_vec()),
    )
    .unwrap();
    block(w.store.commit(cmd(
        2,
        vec![sent_with_body(31, referenced), sent_with_body(32, lost_ref)],
    )))
    .unwrap();
    // 三类残留：tmp 半截、有行无文件（一个无引用、一个有引用）、有文件无行
    std::fs::write(
        w.env.dir.area_tmp(ArtifactArea::Private).join("x.part"),
        b"half",
    )
    .unwrap();
    let path_of = |r: &ArtifactRef| {
        let hex = r.sha256.to_string();
        w.env
            .dir
            .area_objects(r.area)
            .join(&hex[..2])
            .join(&hex[2..])
    };
    std::fs::remove_file(path_of(&orphan_row)).unwrap();
    std::fs::remove_file(path_of(&lost_ref)).unwrap();
    let stray = w
        .env
        .dir
        .area_objects(ArtifactArea::Artifacts)
        .join("ab")
        .join("c".repeat(62));
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    std::fs::write(&stray, b"stray").unwrap();

    let report = block(w.art.recover()).unwrap();
    assert_eq!(report.tmp_removed, 1);
    assert_eq!(report.rows_removed, 1);
    assert_eq!(report.files_removed, 1);
    assert_eq!(report.missing_referenced, 1, "有引用的只记错误，不删行");
    assert!(!stray.exists());
    assert_eq!(rows(&w.env), 2, "被引用的两行都在");
    assert_eq!(block(w.art.read_range(&referenced, 0..4)).unwrap(), b"kept");

    // 投递开始后再调：拒绝，不动任何文件
    std::fs::write(
        w.env.dir.area_tmp(ArtifactArea::Private).join("y.part"),
        b"half",
    )
    .unwrap();
    w.store.mark_delivery_started();
    assert!(block(w.art.recover()).is_err());
    assert_eq!(tmp_files(&w.env, ArtifactArea::Private).len(), 1);
}

#[test]
fn sent_message_with_artifact_body_survives_gc_and_rebuild() {
    let w = world();
    let r = block(
        w.art
            .put_bytes(ArtifactArea::Artifacts, b"long user message".to_vec()),
    )
    .unwrap();
    block(w.store.commit(cmd(2, vec![sent_with_body(31, r)]))).unwrap();
    w.env.clock.advance(2 * DAY);
    block(w.art.collect_garbage()).unwrap();
    assert!(block(w.art.read_range(&r, 0..4)).is_ok());
    block(w.store.rebuild_projections()).unwrap();
    assert!(block(read_model(&w.store).artifact_is_referenced(&r)).unwrap());
    block(w.art.collect_garbage()).unwrap();
    assert!(block(w.art.read_range(&r, 0..4)).is_ok());
}

#[test]
fn gc_after_conversation_deleted_frees_attachment_copy() {
    let w = world();
    let r = block(
        w.art
            .put_bytes(ArtifactArea::Attachments, b"attached".to_vec()),
    )
    .unwrap();
    block(w.store.commit(cmd(
        2,
        vec![
                AttachmentStored {
                    attachment: attachment(40, 10, AttachmentOrigin::Copied(r)),
                }
                .into(),
            ],
    )))
    .unwrap();
    w.env.clock.advance(2 * DAY);
    assert_eq!(
        block(w.art.collect_garbage()).unwrap().removed,
        0,
        "附件在用"
    );
    block(w.store.commit(cmd(
        3,
        vec![ConversationDeleted { conversation_id: cid(10) }.into()],
    )))
    .unwrap();
    assert_eq!(
        block(w.art.collect_garbage()).unwrap().removed,
        1,
        "删对话后副本进入回收"
    );
    assert!(objects(&w.env, ArtifactArea::Attachments).is_empty());
}

#[test]
fn three_areas_are_separate_dirs() {
    let w = world();
    let mut paths = Vec::new();
    for area in [
        ArtifactArea::Artifacts,
        ArtifactArea::Attachments,
        ArtifactArea::Private,
    ] {
        let r = block(w.art.put_bytes(area, b"same".to_vec())).unwrap();
        assert_eq!(r.area, area);
        let files = objects(&w.env, area);
        assert_eq!(files.len(), 1, "{area:?}");
        assert!(files[0].starts_with(w.env.dir.area(area)));
        paths.push(files[0].clone());
    }
    paths.dedup();
    assert_eq!(paths.len(), 3, "不跨区去重");
    assert_eq!(rows(&w.env), 3);
}

#[cfg(unix)]
#[test]
fn private_area_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let w = world();
    block(w.art.put_bytes(ArtifactArea::Private, b"secret".to_vec())).unwrap();
    let file = &objects(&w.env, ArtifactArea::Private)[0];
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(file), 0o600);
    assert_eq!(mode(file.parent().unwrap()), 0o700);
    assert_eq!(mode(&w.env.dir.area(ArtifactArea::Private)), 0o700);
}

/// 第 1 轮审查 1/F-3：启动恢复时探测文件失败（这里是前缀目录没了执行权限）不能当成「文件不存在」：
/// 登记与 pin 都保留，返回点名路径的错误；权限恢复后再恢复一次，什么都没丢。
#[cfg(unix)]
#[test]
fn recover_keeps_data_when_probe_fails() {
    use std::os::unix::fs::PermissionsExt;
    let w = world();
    let r = block(w.art.put_bytes(ArtifactArea::Artifacts, b"pinned".to_vec())).unwrap();
    block(w.art.retain(&r)).unwrap();
    let prefix = objects(&w.env, ArtifactArea::Artifacts)[0]
        .parent()
        .unwrap()
        .to_path_buf();
    let set_mode =
        |m| std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(m)).unwrap();
    set_mode(0o600);
    let err = block(w.art.recover());
    set_mode(0o700);
    match err {
        Err(ArtifactError::Io { path, .. }) => {
            assert!(path.starts_with(&prefix), "点名路径：{path:?}")
        }
        other => panic!("探测失败应报错：{other:?}"),
    }
    assert_eq!(rows(&w.env), 1, "登记还在");
    assert_eq!(
        scalar(&w.env.db(), "SELECT pins FROM artifact_objects"),
        1,
        "pin 还在"
    );

    let report = block(w.art.recover()).unwrap();
    assert_eq!((report.rows_removed, report.files_removed), (0, 0));
    assert_eq!(block(w.art.read_range(&r, 0..6)).unwrap(), b"pinned");
}
