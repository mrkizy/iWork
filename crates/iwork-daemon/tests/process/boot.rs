//! 启动表每一步的失败落在 2.2 启动失败表的哪一格（S3、S11e）。第 11 步在接上时补。

use std::path::PathBuf;
use std::time::Duration;

use iwork_ipc::rendezvous::{ExitCode, Phase, Refusal, Relaunchable, relaunchable};
use rusqlite::Connection;

use crate::support::{Sandbox, code};

const START: Duration = Duration::from_secs(20);

/// 起一次后台建库、用 SIGTERM 停掉，再把库里的协议主版本改成别的。
pub fn make_mismatched_db(sb: &Sandbox) {
    let mut d = sb.spawn(&[], &[]);
    sb.wait_ready(d.pid(), START);
    crate::support::signal(d.pid(), libc::SIGTERM);
    assert_eq!(code(d.wait_exit(START)), Some(0), "{}", d.stderr());
    let conn = Connection::open(sb.db()).unwrap();
    conn.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'protocol_major'",
        [(iwork_proto::PROTOCOL_MAJOR + 1).to_string()],
    )
    .unwrap();
}

fn file_hashes(sb: &Sandbox) -> Vec<(String, Vec<u8>)> {
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            let p = PathBuf::from(format!("{}{suffix}", sb.db().display()));
            std::fs::read(&p).ok().map(|b| (suffix.to_string(), b))
        })
        .collect()
}

fn backups(sb: &Sandbox) -> usize {
    std::fs::read_dir(sb.data_root().join("backups"))
        .map(|d| d.count())
        .unwrap_or(0)
}

/// 卡验收 3a：协议主版本不符 → 逻辑码 12、状态 `Refused(ProtocolMajorMismatch)` 带两个主版本号与备份路径、
/// 多一份备份、原库文件不变。
#[test]
fn protocol_major_mismatch_refuses_with_status() {
    let sb = Sandbox::new();
    make_mismatched_db(&sb);
    let before = file_hashes(&sb);
    let backups_before = backups(&sb);
    let (status, err) = sb.run_to_exit(&[], &[]);
    assert_eq!(code(status), Some(12), "{err}");
    match sb.status().unwrap().phase {
        Phase::Refused(Refusal::ProtocolMajorMismatch {
            database,
            program,
            backup,
        }) => {
            assert_eq!(database, iwork_proto::PROTOCOL_MAJOR + 1);
            assert_eq!(program, iwork_proto::PROTOCOL_MAJOR);
            assert!(backup.exists(), "备份 {} 在", backup.display());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(backups(&sb), backups_before + 1);
    assert_eq!(file_hashes(&sb), before, "原库三个文件不变");
}

/// S3：托管实例被存储拒绝 → 进程退出状态 0（服务管理器不再拉），状态文件照样是 `Refused`。
#[test]
fn service_mode_refusal_exits_zero() {
    let sb = Sandbox::new();
    make_mismatched_db(&sb);
    let (status, err) = sb.run_to_exit(&["--service"], &[]);
    assert_eq!(code(status), Some(0), "{err}");
    let phase = sb.status().unwrap().phase;
    assert!(matches!(phase, Phase::Refused(_)), "{phase:?}");
    assert_eq!(relaunchable(&phase), Relaunchable::NotWithThisProgram);
}

/// 参数写错：托管实例报 0，非托管报 2；都不碰锁与状态文件。
#[test]
fn service_mode_usage_exits_zero() {
    let sb = Sandbox::new();
    let (status, err) = sb.run_to_exit(&["--service", "--bogus"], &[]);
    assert_eq!(code(status), Some(0), "{err}");
    assert!(err.contains("未知参数"), "{err}");
    assert!(sb.untouched());
}

#[test]
fn usage_exits_2_when_not_managed() {
    let sb = Sandbox::new();
    let (status, err) = sb.run_to_exit(&["--bogus"], &[]);
    assert_eq!(code(status), Some(2), "{err}");
    assert!(err.contains("未知参数"), "{err}");
    assert!(sb.untouched());
}

/// 第 1 步取运行映像身份失败 → 10（托管报 0），不写状态文件。
#[test]
fn identity_snapshot_failure_exits_10() {
    let sb = Sandbox::new();
    let env = [("IWORK_DEV_FAIL_AT", "identity")];
    let (status, err) = sb.run_to_exit(&[], &env);
    assert_eq!(code(status), Some(10), "{err}");
    assert!(err.contains("运行映像"), "{err}");
    assert!(sb.untouched());
    let (status, _) = sb.run_to_exit(&["--service"], &env);
    assert_eq!(code(status), Some(0));
}

/// 一格的期望：逻辑码、状态文件里的阶段（不写为 `None`）。
struct Cell {
    step: &'static str,
    logical: ExitCode,
    writes_status: bool,
}

/// S11e：启动表每一步各失败一次，逐个断言逻辑码、状态文件写没写与写了什么、`relaunchable`、托管与非托管的退出状态。
/// 期望值从方案 2.2 的启动失败表抄来，不从实现推。第 11 步在接上时补。
#[test]
fn every_step_failure_lands_in_table() {
    let cells = [
        Cell {
            step: "identity",
            logical: ExitCode::Unsupported,
            writes_status: false,
        },
        Cell {
            step: "endpoint",
            logical: ExitCode::EndpointRefused,
            writes_status: false,
        },
        Cell {
            step: "store",
            logical: ExitCode::StoreRefused,
            writes_status: true,
        },
        // 5a 别的读取错误、6、8、9：20，`Exited{20}`，`Yes`
        Cell {
            step: "compat",
            logical: ExitCode::Fatal,
            writes_status: true,
        },
        Cell {
            step: "artifacts",
            logical: ExitCode::Fatal,
            writes_status: true,
        },
        Cell {
            step: "recovery",
            logical: ExitCode::Fatal,
            writes_status: true,
        },
        Cell {
            step: "server",
            logical: ExitCode::Fatal,
            writes_status: true,
        },
        Cell {
            step: "bind",
            logical: ExitCode::BindFailed,
            writes_status: true,
        },
    ];
    for cell in cells {
        for managed in [false, true] {
            let sb = Sandbox::new();
            let args: &[&str] = if managed { &["--service"] } else { &[] };
            let (status, err) = sb.run_to_exit(args, &[("IWORK_DEV_FAIL_AT", cell.step)]);
            let status_file = sb.status();
            assert_eq!(
                status_file.is_some(),
                cell.writes_status,
                "{}：状态文件 {status_file:?}",
                cell.step
            );
            let phase = match &status_file {
                Some(s) => s.phase.clone(),
                None => Phase::Exited {
                    code: cell.logical,
                    reason: None,
                    message: None,
                },
            };
            let logical = match &phase {
                Phase::Refused(_) => ExitCode::StoreRefused,
                Phase::Exited { code, .. } => *code,
                other => panic!("{}：{other:?}", cell.step),
            };
            assert_eq!(logical, cell.logical, "{}", cell.step);
            let want = match (managed, relaunchable(&phase)) {
                (false, _) | (true, Relaunchable::Yes) => i32::from(cell.logical.code()),
                (true, _) => 0,
            };
            assert_eq!(
                code(status),
                Some(want),
                "{} managed={managed}：{err}",
                cell.step
            );
        }
    }
}
