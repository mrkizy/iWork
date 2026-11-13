//! 出包前的核对（T04p 方案 4.4）：`check-install <目录> [--expect-leaf <叶证书 SHA-256>]`。
//!
//! `<目录>` 是包里放程序的目录（macOS 是 `X.app/Contents/MacOS`，Linux 是成品目录）。三件事各打一行
//! `<事> PASS|FAIL <细节>`，判法见 `iwork_daemon::install::package_check`。退出码：0 三件都过；1 有一件没过；
//! 2 用法错。打包命令（`pnpm -C client tauri build`）在发布之前跑它，CI 跑打包命令即是。

use std::path::PathBuf;
use std::process::ExitCode;

use iwork_daemon::install::package_check::check_package;

const USAGE: &str = "用法：check-install <目录> [--expect-leaf <叶证书 SHA-256，64 个十六进制位>]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dir, leaf) = match args.as_slice() {
        [dir] => (PathBuf::from(dir), None),
        [dir, flag, hex] if flag == "--expect-leaf" => match parse_leaf(hex) {
            Some(leaf) => (PathBuf::from(dir), Some(leaf)),
            None => return usage(&format!("--expect-leaf 不是 64 个十六进制位：{hex}")),
        },
        _ => return usage(USAGE),
    };
    if !dir.is_dir() {
        return usage(&format!("不是目录：{}", dir.display()));
    }
    let findings = check_package(&dir, leaf.as_ref());
    for f in &findings {
        println!(
            "{} {} {}",
            f.item,
            if f.passed { "PASS" } else { "FAIL" },
            f.detail
        );
    }
    if findings.iter().all(|f| f.passed) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn parse_leaf(text: &str) -> Option<[u8; 32]> {
    hex::decode(text).ok()?.try_into().ok()
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("check-install：{msg}");
    ExitCode::from(2)
}
