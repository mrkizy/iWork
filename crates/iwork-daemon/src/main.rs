//! 后台服务组合入口：唯一同时知道全部实现的 crate，在这里把实现注入端口。T04 填实现。

use std::process::ExitCode;

mod args;

fn main() -> ExitCode {
    match args::parse(std::env::args_os().skip(1)) {
        Ok(args) => {
            #[cfg(feature = "dev")]
            if args.trust == iwork_ipc::peer::TrustPolicy::DevTrust {
                eprintln!(
                    "iwork-daemon：--dev-trust 已开启，跳过签名校验（同用户照查），只用于开发"
                );
            }
            let _ = args;
            // T04：SelfIdentity::detect → LocalEndpoint → daemon.lock → 存储与崩溃恢复 → Server::new → bind → serve。
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("iwork-daemon：{e}，拒绝启动");
            ExitCode::from(2)
        }
    }
}
