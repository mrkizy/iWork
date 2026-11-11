//! iWork T04 N3（方案第 8 节）：原地 exec 自行重启的三件事。用 rustc 直接编译（只用 std），由
//! iwork-t04-exec-restart.sh 调用。三个模式：
//!   exec <dir>      持有 try_lock 的锁 + 一个监听中的 Unix socket + 一个 setsid 起的子进程（stdin 是管道），
//!                   然后 exec 自己（带 --after）。新映像：旧 socket 还连不连得上、能不能再拿到锁、
//!                   能不能 unlink 后重新 bind、子进程有没有读到 EOF。
//!   hold <dir>      拿锁后一直睡（给 waitlock 当前任）
//!   waitlock <dir>  阻塞 File::lock() 等锁；拿到后打印等了多久，再 exec 自己（带 --after）看新映像能不能再拿到锁
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

unsafe extern "C" {
    fn setsid() -> i32;
}

fn lock_file(dir: &Path) -> File {
    OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("daemon.lock")).unwrap()
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

fn exec_self(extra: &str) -> ! {
    let exe = std::env::current_exe().unwrap();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    args.push(extra.into());
    let err = Command::new(exe).args(args).exec();
    println!("exec_failed={err}");
    std::process::exit(21);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args[0].as_str();
    let dir = PathBuf::from(&args[1]);
    let after = args.iter().any(|a| a == "--after");
    let sock = dir.join("d.sock");
    match (mode, after) {
        ("exec", false) => {
            let lock = lock_file(&dir);
            lock.try_lock().expect("第一次拿锁");
            let _listener = UnixListener::bind(&sock).unwrap();
            let marker = dir.join("child-eof");
            let mut cmd = Command::new("/bin/sh");
            cmd.arg("-c")
                .arg(format!("cat >/dev/null; echo eof > '{}'", marker.display()))
                .stdin(Stdio::piped())
                .stdout(Stdio::null());
            // SAFETY: pre_exec 里只调 async-signal-safe 的 setsid。
            unsafe {
                cmd.pre_exec(|| {
                    setsid();
                    Ok(())
                });
            }
            let child = cmd.spawn().unwrap();
            println!("before pid={} child={} lock=held listening={}", std::process::id(), child.id(), sock.display());
            std::mem::forget(child); // 管道写端随进程映像 exec 时关闭（std 建管道都带 CLOEXEC）
            std::io::stdout().flush().unwrap();
            exec_self("--after");
        }
        ("exec", true) => {
            let old_conn = UnixStream::connect(&sock).map(|_| "connected").unwrap_or_else(|e| if e.kind() == std::io::ErrorKind::ConnectionRefused { "refused" } else { "other-error" });
            let relock = lock_file(&dir).try_lock().map(|_| "ok").unwrap_or("held-by-someone");
            let _ = std::fs::remove_file(&sock);
            let rebind = UnixListener::bind(&sock).map(|_| "ok").unwrap_or("failed");
            let t = Instant::now();
            let marker = dir.join("child-eof");
            while !marker.exists() && t.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_millis(10));
            }
            println!(
                "after pid={} old_socket={old_conn} relock={relock} rebind={rebind} child_saw_eof={} within_ms={}",
                std::process::id(),
                marker.exists(),
                t.elapsed().as_millis()
            );
        }
        ("hold", _) => {
            // 锁跟着 File 走：绑定到变量，进程活着就一直持有
            let held = lock_file(&dir);
            held.try_lock().expect("hold 拿锁");
            println!("holding pid={}", std::process::id());
            std::io::stdout().flush().unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(60));
                let _ = &held;
            }
        }
        ("waitlock", false) => {
            let f = lock_file(&dir);
            println!("waiting pid={} at_ms={}", std::process::id(), now_ms());
            std::io::stdout().flush().unwrap();
            f.lock().unwrap();
            println!("acquired at_ms={}", now_ms());
            std::io::stdout().flush().unwrap();
            exec_self("--after");
        }
        ("waitlock", true) => {
            let relock = lock_file(&dir).try_lock().map(|_| "ok").unwrap_or("held-by-someone");
            println!("after_exec pid={} relock={relock}", std::process::id());
        }
        _ => panic!("未知模式"),
    }
}
