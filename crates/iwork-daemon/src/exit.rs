//! 进程退出状态（T04 方案 2.2）。`process_status` 是唯一决定进程退出状态的函数：正常退出、启动失败、
//! 硬上限强退都调它。「再拉有没有用」不在这里判断，只查 [`relaunchable`]。

use iwork_ipc::rendezvous::{ExitCode, Phase, Relaunchable, StopReason, relaunchable};

/// 一个 `Phase` 对应的逻辑退出码：写进状态文件、日志与 stderr 的那个。
///
/// `Starting` / `Ready` 正常不会走到退出（停在这两种状态说明进程没来得及写别的就没了）；
/// 真走到了说明是启动或运行中的程序缺陷，按 `Fatal` 报。
pub fn logical_code(phase: &Phase) -> ExitCode {
    match phase {
        Phase::Starting { .. } | Phase::Ready => ExitCode::Fatal,
        Phase::Stopping { reason } => stop_code(*reason),
        Phase::Refused(_) => ExitCode::StoreRefused,
        Phase::Exited { code, .. } => *code,
    }
}

/// 停机走完后的逻辑码（停机表第 8 步）。升级的那一列走到退出只可能是 `exec` 没成。
pub fn stop_code(reason: StopReason) -> ExitCode {
    match reason {
        StopReason::UserQuit | StopReason::Signal => ExitCode::Stopped,
        StopReason::Upgrade => ExitCode::RestartRequested,
        StopReason::Fatal => ExitCode::Fatal,
    }
}

/// 进程报给操作系统的退出状态。
///
/// 非托管实例（客户端拉起的、测试起的）报逻辑码；托管实例（`--service`）照 2.2 的角色表：
/// `relaunchable` 是 `Yes` 报逻辑码（非零，服务管理器会再拉），否则报 0（服务管理器不拉）。
pub fn process_status(phase: &Phase, managed: bool) -> u8 {
    let code = logical_code(phase).code();
    if !managed {
        return code;
    }
    match relaunchable(phase) {
        // 「再拉有用」必须报非零，否则服务管理器不拉。逻辑码与停机原因对不上的 `Exited`（如码 0 而原因是升级）
        // 本程序不会写出，真遇到了按故障报，不让它悄悄变成「不拉」
        Relaunchable::Yes if code == 0 => ExitCode::Fatal.code(),
        Relaunchable::Yes => code,
        Relaunchable::NotUntilUserReturns | Relaunchable::NotWithThisProgram => 0,
    }
}

/// 还没拿到锁就退出的实例不写状态文件；它的退出状态同样对一个本地构造的 `Exited` 算。
pub fn exited(code: ExitCode) -> Phase {
    Phase::Exited {
        code,
        reason: None,
        message: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iwork_ipc::rendezvous::Refusal;
    use std::path::PathBuf;

    const REASONS: [StopReason; 4] = [
        StopReason::UserQuit,
        StopReason::Signal,
        StopReason::Upgrade,
        StopReason::Fatal,
    ];

    /// 每一种 Phase 的代表。
    fn all_phases() -> Vec<Phase> {
        let mut v = vec![
            Phase::Starting {
                step: "store".into(),
            },
            Phase::Ready,
            Phase::Refused(Refusal::ProtocolMajorMismatch {
                database: 2,
                program: 1,
                backup: PathBuf::from("/b"),
            }),
            Phase::Refused(Refusal::StoreOpenFailed {
                message: "x".into(),
            }),
        ];
        for r in REASONS {
            v.push(Phase::Stopping { reason: r });
            for code in ExitCode::ALL {
                v.push(Phase::Exited {
                    code,
                    reason: Some(r),
                    message: None,
                });
            }
        }
        for code in ExitCode::ALL {
            v.push(exited(code));
        }
        v
    }

    /// 托管实例：报不报零只由 `relaunchable` 决定；非托管：报逻辑码。期望值从角色表独立写出。
    #[test]
    fn process_status_uses_relaunchable() {
        for phase in all_phases() {
            let unmanaged = process_status(&phase, false);
            assert_eq!(unmanaged, logical_code(&phase).code(), "{phase:?}");
            let managed = process_status(&phase, true);
            let expected_managed = match relaunchable(&phase) {
                Relaunchable::Yes if logical_code(&phase) == ExitCode::Stopped => 20,
                Relaunchable::Yes => logical_code(&phase).code(),
                _ => 0,
            };
            assert_eq!(managed, expected_managed, "{phase:?}");
            // 托管实例「再拉有用」时一定报非零：否则服务管理器不会再拉
            if relaunchable(&phase) == Relaunchable::Yes {
                assert_ne!(managed, 0, "{phase:?}");
            }
        }
    }

    /// 几个具体格子（角色表里点名的）。
    #[test]
    fn named_cells() {
        let refused = Phase::Refused(Refusal::StoreOpenFailed {
            message: "x".into(),
        });
        assert_eq!(process_status(&refused, false), 12);
        assert_eq!(process_status(&refused, true), 0);
        assert_eq!(process_status(&exited(ExitCode::Usage), false), 2);
        assert_eq!(process_status(&exited(ExitCode::Usage), true), 0);
        assert_eq!(process_status(&exited(ExitCode::BindFailed), true), 13);
        assert_eq!(process_status(&exited(ExitCode::Fatal), true), 20);
        let user_quit = Phase::Stopping {
            reason: StopReason::UserQuit,
        };
        assert_eq!(process_status(&user_quit, true), 0);
        assert_eq!(process_status(&user_quit, false), 0);
        let upgrade = Phase::Stopping {
            reason: StopReason::Upgrade,
        };
        assert_eq!(process_status(&upgrade, true), 21);
        let fatal = Phase::Stopping {
            reason: StopReason::Fatal,
        };
        assert_eq!(process_status(&fatal, true), 20);
    }

    /// 硬上限强退时用 `Stopping{r}` 算，走完全程用 `Exited{stop_code(r), Some(r)}` 算：两者相同。
    #[test]
    fn forced_and_completed_stop_agree() {
        for r in REASONS {
            for managed in [false, true] {
                let forced = process_status(&Phase::Stopping { reason: r }, managed);
                let done = process_status(
                    &Phase::Exited {
                        code: stop_code(r),
                        reason: Some(r),
                        message: None,
                    },
                    managed,
                );
                assert_eq!(forced, done, "{r:?} managed={managed}");
            }
        }
    }
}
