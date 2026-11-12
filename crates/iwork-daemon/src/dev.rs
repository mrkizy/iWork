//! 开发构建的故障注入点（只在 `dev` feature 下有内容；发布构建里两个函数是空的，读不到环境变量）。
//!
//! - `IWORK_DEV_FAIL_AT=<点名>`：走到这个点时当作这一步失败（真进程测试用它覆盖启动表每一步的失败）。
//! - `IWORK_DEV_PAUSE_AT=<点名>`：走到这个点时往 stderr 写一行 `IWORK-PAUSED <点名>`，然后一直停着，
//!   等测试 `kill -9`（崩溃恢复、停机卡住等用例）。
//!
//! 点名就是启动表的步骤名，或各模块里写明的暂停点名。

#[cfg(feature = "dev")]
pub const FAIL_ENV: &str = "IWORK_DEV_FAIL_AT";
#[cfg(feature = "dev")]
pub const PAUSE_ENV: &str = "IWORK_DEV_PAUSE_AT";

/// 是否要在 `point` 注入失败。
#[cfg(feature = "dev")]
pub fn fail_at(point: &str) -> bool {
    std::env::var(FAIL_ENV).is_ok_and(|v| v == point)
}

#[cfg(not(feature = "dev"))]
#[inline(always)]
pub fn fail_at(_point: &str) -> bool {
    false
}

/// 要在 `point` 停住就停住（阻塞当前线程，不返回）。
#[cfg(feature = "dev")]
pub fn pause_at(point: &str) {
    if std::env::var(PAUSE_ENV).is_ok_and(|v| v == point) {
        use std::io::Write;
        let mut err = std::io::stderr();
        let _ = writeln!(err, "IWORK-PAUSED {point}");
        let _ = err.flush();
        loop {
            std::thread::park();
        }
    }
}

#[cfg(not(feature = "dev"))]
#[inline(always)]
pub fn pause_at(_point: &str) {}
