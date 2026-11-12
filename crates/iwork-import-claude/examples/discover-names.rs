//! 在本机跑一次发现（T10f 方案 A6 的 D1）：打印每一项的 id、种类、名字、连接方式、键名，末尾一行条数与耗时。
//!
//! `cargo run -p iwork-import-claude --example discover-names`
//!
//! 名字照原样印（产品负责人接受它们显示在界面上，PD-A1）；值、命令、参数、地址、路径不在输出里。
//! 配置目录照后台的规则取：环境里有 `CLAUDE_CONFIG_DIR` 就用它，没有用家目录下的 `.claude`。

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use iwork_import_claude::{ClaudeLocalConfig, ConfigPaths, StdFiles, render_listing};

fn main() -> ExitCode {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        eprintln!("环境里没有 HOME");
        return ExitCode::FAILURE;
    };
    let paths = ConfigPaths::resolve(&home, std::env::var_os("CLAUDE_CONFIG_DIR").as_deref());
    let started = Instant::now();
    match ClaudeLocalConfig::new(paths, StdFiles).discover() {
        Ok(items) => {
            print!("{}", render_listing(&items));
            println!("发现耗时 {} 毫秒", started.elapsed().as_millis());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
