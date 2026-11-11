//! 只给测试用的小程序（`cargo test` 会编 examples，产品二进制里没有它）：装上和 hook 一样的
//! panic 钩子，然后按参数在主线程或子线程里 panic。`tests/panic_denies.rs` 起它做进程级断言
//! （T06 方案 4.6 第 15 行）。panic 文字里放一个暗号，测试断言它没从任何输出里漏出去。

/// panic 文字里的暗号。
const SECRET: &str = "panic-secret-t06-7f3a";

fn main() {
    iwork_claude_hook::verdict::install_panic_hook();
    match std::env::args().nth(1).as_deref() {
        Some("main") => panic!("{SECRET}"),
        Some("thread") => {
            let _ = std::thread::spawn(|| panic!("{SECRET}")).join();
            // 钩子里就结束了进程，走不到这里
            std::process::exit(0)
        }
        _ => std::process::exit(64),
    }
}
