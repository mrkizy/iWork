use std::path::PathBuf;

use proptest::prelude::*;

use super::*;

const ALL_REASONS: [StopReason; 4] = [
    StopReason::UserQuit,
    StopReason::Signal,
    StopReason::Upgrade,
    StopReason::Fatal,
];

fn exited(code: ExitCode, reason: Option<StopReason>) -> Phase {
    Phase::Exited {
        code,
        reason,
        message: None,
    }
}

/// T04 方案 2.2 第一张表，逐行照抄（期望值不经实现推导）。
#[test]
fn relaunchable_table() {
    use Relaunchable::*;
    let rows: Vec<(Phase, Relaunchable)> = vec![
        (
            Phase::Starting {
                step: "store".into(),
            },
            Yes,
        ),
        (Phase::Ready, Yes),
        (
            Phase::Stopping {
                reason: StopReason::UserQuit,
            },
            NotUntilUserReturns,
        ),
        (
            Phase::Stopping {
                reason: StopReason::Signal,
            },
            NotUntilUserReturns,
        ),
        (
            Phase::Stopping {
                reason: StopReason::Upgrade,
            },
            Yes,
        ),
        (
            Phase::Stopping {
                reason: StopReason::Fatal,
            },
            Yes,
        ),
        (
            exited(ExitCode::Stopped, Some(StopReason::UserQuit)),
            NotUntilUserReturns,
        ),
        (
            exited(ExitCode::Stopped, Some(StopReason::Signal)),
            NotUntilUserReturns,
        ),
        (
            exited(ExitCode::RestartRequested, Some(StopReason::Upgrade)),
            Yes,
        ),
        (exited(ExitCode::Fatal, Some(StopReason::Fatal)), Yes),
        (
            Phase::Refused(Refusal::ProtocolMajorMismatch {
                database: 2,
                program: 1,
                backup: PathBuf::from("/b"),
            }),
            NotWithThisProgram,
        ),
        (
            Phase::Refused(Refusal::StoreOpenFailed {
                message: "schema 比程序新".into(),
            }),
            NotWithThisProgram,
        ),
    ];
    for (phase, want) in rows {
        assert_eq!(relaunchable(&phase), want, "{phase:?}");
    }
}

/// 方案 2.2 第二张表：没有停机原因时按逻辑码，八个码每个都有答案。
#[test]
fn relaunchable_by_code_table() {
    use Relaunchable::*;
    let rows = [
        (ExitCode::Stopped, NotUntilUserReturns),
        (ExitCode::Usage, NotWithThisProgram),
        (ExitCode::Unsupported, NotWithThisProgram),
        (ExitCode::EndpointRefused, NotWithThisProgram),
        (ExitCode::StoreRefused, NotWithThisProgram),
        (ExitCode::BindFailed, Yes),
        (ExitCode::IdentityUnsettled, Yes),
        (ExitCode::Fatal, Yes),
        (ExitCode::RestartRequested, Yes),
    ];
    assert_eq!(rows.len(), ExitCode::ALL.len());
    for (code, want) in rows {
        assert_eq!(relaunchable(&exited(code, None)), want, "{code:?}");
    }
}

#[test]
fn exit_code_numbers_match_table() {
    let numbers: Vec<u8> = ExitCode::ALL.iter().map(|c| c.code()).collect();
    assert_eq!(numbers, [0, 2, 10, 11, 12, 13, 14, 20, 21]);
}

fn any_reason() -> impl Strategy<Value = StopReason> {
    prop::sample::select(ALL_REASONS.to_vec())
}

fn any_code() -> impl Strategy<Value = ExitCode> {
    prop::sample::select(ExitCode::ALL.to_vec())
}

proptest! {
    /// 意图在写下 `Stopping{r}` 时就定了：之后不管以哪个码退出，`Exited{_, Some(r)}` 的答案都相同。
    #[test]
    fn stopping_and_exited_agree(reason in any_reason(), code in any_code(), message in proptest::option::of(".{0,8}")) {
        let stopping = relaunchable(&Phase::Stopping { reason });
        let exited = relaunchable(&Phase::Exited { code, reason: Some(reason), message });
        prop_assert_eq!(stopping, exited);
    }
}

fn sample_status(phase: Phase) -> StartupStatus {
    StartupStatus {
        pid: 4242,
        version: "0.1.0".into(),
        protocol_major: 1,
        exe_stamp: ExeStamp {
            len: 1234,
            modified_unix_ns: 1_700_000_000_000_000_000,
        },
        log_dir: PathBuf::from("/data/logs"),
        updated_at_ms: 1_700_000_000_000,
        phase,
        ipc_faults: 3,
        not_cleared: 1,
    }
}

#[test]
fn status_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let phases = [
        Phase::Starting {
            step: "recovery".into(),
        },
        Phase::Ready,
        Phase::Stopping {
            reason: StopReason::Upgrade,
        },
        Phase::Refused(Refusal::ProtocolMajorMismatch {
            database: 2,
            program: 1,
            backup: PathBuf::from("/data/backups/x.db"),
        }),
        exited(ExitCode::BindFailed, None),
        Phase::Exited {
            code: ExitCode::Fatal,
            reason: Some(StopReason::Fatal),
            message: Some("存储失效".into()),
        },
    ];
    for phase in phases {
        let status = sample_status(phase);
        write_status(dir.path(), &status).unwrap();
        assert_eq!(read_status(dir.path()), Some(status));
    }
    // 临时文件不留下
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [STATUS_FILE]);
}

#[cfg(unix)]
#[test]
fn status_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    write_status(dir.path(), &sample_status(Phase::Ready)).unwrap();
    let mode = std::fs::metadata(dir.path().join(STATUS_FILE))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn unreadable_status_is_none() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_status(dir.path()), None, "没有文件");
    let path = dir.path().join(STATUS_FILE);
    std::fs::write(&path, b"{ not json").unwrap();
    assert_eq!(read_status(dir.path()), None, "坏 JSON");
    // 新版本加的、本版不认识的退出码：整份解析不了
    let mut v = serde_json::to_value(sample_status(exited(ExitCode::Fatal, None))).unwrap();
    v["phase"]["Exited"]["code"] = "SomethingNew".into();
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    assert_eq!(read_status(dir.path()), None, "不认识的退出码");
    // 对照：同一份改回认识的码就能读
    v["phase"]["Exited"]["code"] = "Fatal".into();
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    assert!(read_status(dir.path()).is_some());
}

/// 临时文件的位置上已经有东西（别人放好的链接、同名文件）→ 新建失败，不顺着链接写，被指向的文件不变。
#[cfg(unix)]
#[test]
fn temp_file_is_created_exclusively() {
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "keep me").unwrap();
    let planted = dir.path().join("planted");
    std::os::unix::fs::symlink(&victim, &planted).unwrap();
    assert!(create_new_private(&planted).is_err());
    assert!(create_new_private(&victim).is_err());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
    assert!(create_new_private(&dir.path().join("fresh")).is_ok());
}

/// 状态文件的位置上是指向别处的链接 → 改名替换掉链接本身，被指向的文件不变；不留临时文件。
#[cfg(unix)]
#[test]
fn write_status_does_not_follow_links() {
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "keep me").unwrap();
    std::os::unix::fs::symlink(&victim, dir.path().join(STATUS_FILE)).unwrap();
    let st = sample_status(Phase::Ready);
    write_status(dir.path(), &st).unwrap();
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me");
    let meta = std::fs::symlink_metadata(dir.path().join(STATUS_FILE)).unwrap();
    assert!(meta.file_type().is_file());
    assert_eq!(read_status(dir.path()), Some(st));
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.len(), 2, "只有状态文件与 victim：{names:?}");
}

/// 一边反复写两份不同的状态，一边反复读：每次读到的都是两份之一，从不是半份或 `None`。
/// 只在 Unix 上跑：Windows runner 上它红过一次（CI run 43215005205）。日志里能看到的只有：60s 内只完成 168 次写入，
/// 主测试因此判失败；随后写线程的 `write_status` 返回 NotFound（可能发生在失败之后、临时目录清理期间）。原因没有
/// 查实。M1 不支持 Windows，后台在启动表第 1 步就拒绝、走不到写状态文件（方案 2.15），按 AGENTS.md 的约定限定
/// `cfg(unix)`，不为它改产品代码。
#[cfg(unix)]
#[test]
fn write_is_atomic() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let a = sample_status(Phase::Ready);
    let b = sample_status(Phase::Exited {
        code: ExitCode::Fatal,
        reason: None,
        message: Some("x".repeat(64 * 1024)),
    });
    write_status(dir.path(), &a).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writes = Arc::new(AtomicU32::new(0));
    let writer = {
        let (dir, a, b, stop, writes) = (
            dir.path().to_path_buf(),
            a.clone(),
            b.clone(),
            stop.clone(),
            writes.clone(),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let n = writes.load(Ordering::Relaxed);
                write_status(&dir, if n.is_multiple_of(2) { &b } else { &a }).unwrap();
                writes.store(n + 1, Ordering::Relaxed);
            }
        })
    };
    // 按进度停，不按读的次数停：慢机器（Windows CI）上写线程可能在读完固定次数之前还没被调度到。
    const MIN_WRITES: u32 = 200;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let (mut reads, mut seen_b) = (0usize, 0usize);
    while reads < 2000 || writes.load(Ordering::Relaxed) < MIN_WRITES || seen_b == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "60s 内写了 {} 次、读了 {reads} 次、读到第二份 {seen_b} 次",
            writes.load(Ordering::Relaxed)
        );
        let got = read_status(dir.path()).expect("读到了半份或没有文件");
        assert!(got == a || got == b);
        seen_b += usize::from(got == b);
        reads += 1;
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
}

#[test]
fn lock_info_roundtrip_and_truncates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(LOCK_FILE);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let long = LockInfo {
        pid: 1,
        version: "0.1.0-a-very-long-version-string".into(),
        started_at_ms: 1,
    };
    let short = LockInfo {
        pid: 2,
        version: "0.2".into(),
        started_at_ms: 2,
    };
    write_lock_info(&mut file, &long).unwrap();
    assert_eq!(read_lock_info(dir.path()), Some(long));
    // 后写的短内容不留下前一份的尾巴
    write_lock_info(&mut file, &short).unwrap();
    assert_eq!(read_lock_info(dir.path()), Some(short));
    std::fs::write(&path, b"").unwrap();
    assert_eq!(read_lock_info(dir.path()), None, "空文件");
}

#[test]
fn exe_stamp_changes_when_file_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("iwork-daemon");
    std::fs::write(&path, b"version a").unwrap();
    let first = ExeStamp::of(&path).unwrap();
    assert_eq!(ExeStamp::of(&path).unwrap(), first, "没动就相同");
    std::fs::write(&path, b"version b, longer").unwrap();
    assert_ne!(ExeStamp::of(&path).unwrap(), first);
    assert!(ExeStamp::of(&dir.path().join("missing")).is_err());
}
