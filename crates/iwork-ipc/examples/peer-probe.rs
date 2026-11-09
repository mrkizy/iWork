//! 对端身份探针（T03 方案 8.1、8.3 的实测工具，只在 `dev` feature 下编译）。
//!
//! - `peer-probe --server <sock> [--hold N]`：监听 UDS；每个连入的对端每秒重算一次 `PeerIdentity` 并打印一行，
//!   `N` 秒后退出（默认 30）。换文件反例就是在这期间动对端的可执行文件，看打印的身份变不变。
//! - `peer-probe --connect <sock>`：连上后停住，直到被杀。复制本二进制再各自签名，就得到不同身份的被测对端。
//!
//! 输出一行一个观测，字段用空格分隔，方便脚本 grep：
//! `tick=<秒> conn=<序号> pid=<pid> user=<uid> exe=<路径> signer=<变体:完整十六进制 | unavailable:原因>`。

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    unix::main()
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    eprintln!("peer-probe 目前只支持 Unix domain socket（macOS、Linux）");
    std::process::ExitCode::from(2)
}

#[cfg(unix)]
mod unix {
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::time::Duration;

    use iwork_ipc::peer::{PeerIdentity, SignerIdentity};
    use iwork_ipc::transport::{Connection, Connector, Endpoint, Listener};
    use iwork_ipc::{LocalConnector, LocalListener};

    const DEFAULT_HOLD_SECS: u64 = 30;
    const USAGE: &str =
        "用法：peer-probe --server <sock> [--hold 秒] | peer-probe --connect <sock>";

    enum Mode {
        Server { sock: PathBuf, hold: Duration },
        Connect { sock: PathBuf },
    }

    pub fn main() -> ExitCode {
        let mode = match parse(std::env::args().skip(1).collect()) {
            Ok(mode) => mode,
            Err(e) => {
                eprintln!("{e}\n{USAGE}");
                return ExitCode::from(2);
            }
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("建 tokio 运行时");
        let result = runtime.block_on(async {
            match mode {
                Mode::Server { sock, hold } => serve(sock, hold).await,
                Mode::Connect { sock } => connect(sock).await,
            }
        });
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("peer-probe：{e}");
                ExitCode::FAILURE
            }
        }
    }

    fn parse(args: Vec<String>) -> Result<Mode, String> {
        let mut args = args.into_iter();
        let mut server = None;
        let mut connect = None;
        let mut hold = Duration::from_secs(DEFAULT_HOLD_SECS);
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} 缺参数值"));
            match arg.as_str() {
                "--server" => server = Some(PathBuf::from(value()?)),
                "--connect" => connect = Some(PathBuf::from(value()?)),
                "--hold" => {
                    let raw = value()?;
                    let secs = raw
                        .parse()
                        .map_err(|_| format!("--hold 要整数秒，收到 {raw}"))?;
                    hold = Duration::from_secs(secs);
                }
                other => return Err(format!("不认识的参数：{other}")),
            }
        }
        match (server, connect) {
            (Some(sock), None) => Ok(Mode::Server { sock, hold }),
            (None, Some(sock)) => Ok(Mode::Connect { sock }),
            _ => Err("--server 与 --connect 要且只要一个".into()),
        }
    }

    async fn serve(sock: PathBuf, hold: Duration) -> Result<(), String> {
        let mut listener =
            LocalListener::bind(&sock).map_err(|e| format!("监听 {} 失败：{e}", sock.display()))?;
        println!("listening sock={} hold={}s", sock.display(), hold.as_secs());
        let accept_loop = async {
            let mut serial = 0u32;
            loop {
                let conn = listener
                    .accept()
                    .await
                    .map_err(|e| format!("accept 失败：{e}"))?;
                serial += 1;
                tokio::spawn(watch(serial, conn));
            }
        };
        tokio::select! {
            r = accept_loop => r,
            () = tokio::time::sleep(hold) => {
                println!("hold elapsed");
                Ok(())
            }
        }
    }

    /// 每秒对同一条连接重算一次身份，直到进程退出。
    async fn watch<C: Connection>(serial: u32, conn: C) {
        let mut tick = 0u64;
        loop {
            match conn.peer().await {
                Ok(peer) => println!("tick={tick} conn={serial} {}", describe(&peer)),
                Err(e) => println!("tick={tick} conn={serial} error={e}"),
            }
            tick += 1;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn connect(sock: PathBuf) -> Result<(), String> {
        let _conn = LocalConnector
            .connect(&Endpoint::Unix(sock.clone()))
            .await
            .map_err(|e| format!("连接 {} 失败：{e}", sock.display()))?;
        println!("connected pid={}", std::process::id());
        std::future::pending::<()>().await;
        Ok(())
    }

    fn describe(peer: &PeerIdentity) -> String {
        let exe = peer
            .exe
            .as_deref()
            .map_or_else(|| "-".into(), |p| p.display().to_string());
        let signer = match &peer.signer {
            SignerIdentity::Certificate { leaf_sha256 } => {
                format!("certificate:{}", hex::encode(leaf_sha256))
            }
            SignerIdentity::Image(id) => id.to_string(),
            SignerIdentity::Unavailable { reason } => format!("unavailable:{reason}"),
        };
        format!(
            "pid={} user={} exe={exe} signer={signer}",
            peer.pid, peer.user
        )
    }
}
