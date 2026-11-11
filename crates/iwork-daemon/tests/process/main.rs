//! 真进程测试（T04 方案第 7 节）：真起 `iwork-daemon`，断言进程退出状态、状态文件、锁与库。
//! 只在 Unix 上跑（M1 不支持 Windows）；要 `dev` feature（测试进程不是受信映像，要 `--dev-trust`）：
//! `cargo test -p iwork-daemon --features dev --test process`。

#![cfg(unix)]

mod support;

mod boot;
mod instance;
