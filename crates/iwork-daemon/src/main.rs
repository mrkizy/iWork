//! 后台服务的进程入口：只做「参数 → `iwork_daemon::run` → 退出状态」。

fn main() -> std::process::ExitCode {
    iwork_daemon::run(std::env::args_os().skip(1).collect())
}
