//! hook 的两条基准（T06 方案第 14 节前两行；S9 收进 `iwork-bench`）：
//! - `hook_cold_start`：起 hook 进程 → 连一个立即答「允许」的桩 → 进程退出，200 次的 P50 / P95；
//! - `hook_binary_size`：这次构建出的 hook 可执行文件字节数（`cargo bench` 用 release 配置）。
//!
//! 每组一行 JSON 到 stdout。只在 unix 上跑（Windows 的通道随桥接在 S6 接入）。

#[cfg(unix)]
fn main() {
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use iwork_claude_hook::client::{ENV_BRIDGE_PID, ENV_DEADLINE, ENV_SOCK, ENV_TOKEN};
    use iwork_claude_hook::wire::{self, Response};

    const HOOK: &str = env!("CARGO_BIN_EXE_iwork-claude-hook");
    const ROUNDS: usize = 200;
    const INPUT: &[u8] = br#"{"session_id":"s","tool_name":"Bash","tool_input":{"command":"ls"}}"#;

    let dir = std::env::temp_dir().join(format!("iwork-hook-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("s");
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            if wire::read_request_header(&mut conn).is_ok()
                && wire::read_request_body(&mut conn).is_ok()
            {
                let _ = wire::write_response(&mut conn, &Response::allow());
            }
        }
    });
    let token = wire::token_hex(&[7u8; wire::TOKEN_BYTES]);
    let pid = std::process::id().to_string();

    let mut samples: Vec<Duration> = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let start = Instant::now();
        let mut child = Command::new(HOOK)
            .arg("pre-tool-use")
            .env_clear()
            .env(ENV_SOCK, &sock)
            .env(ENV_TOKEN, &token)
            .env(ENV_BRIDGE_PID, &pid)
            .env(ENV_DEADLINE, "60")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(INPUT).unwrap();
        let out = child.wait_with_output().unwrap();
        samples.push(start.elapsed());
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("\"allow\""));
    }
    samples.sort();
    let pct = |p: usize| samples[(samples.len() * p / 100).min(samples.len() - 1)];
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "{{\"bench\":\"hook_cold_start\",\"rounds\":{ROUNDS},\"p50_ms\":{:.3},\"p95_ms\":{:.3},\"max_ms\":{:.3}}}",
        ms(pct(50)),
        ms(pct(95)),
        ms(*samples.last().unwrap())
    );
    let size = std::fs::metadata(HOOK).unwrap().len();
    println!(
        "{{\"bench\":\"hook_binary_size\",\"bytes\":{size},\"path\":\"target/.../iwork-claude-hook\"}}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(not(unix))]
fn main() {
    println!("{{\"bench\":\"hook_cold_start\",\"skipped\":\"windows\"}}");
}
