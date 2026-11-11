//! 导出需凭据命令的对拍语料（T07 方案 3.7 第 4 条），供 T06 的 O1 ④ 对固定版本的真实 CLI 跑，
//! 把 `claude` 列填回 `credential_corpus.rs`。
//!
//! 用法：`cargo run -p iwork-core --example dump-credential-corpus > corpus.json`

use iwork_core::approval::action::credential_corpus::CORPUS;
use iwork_core::approval::rules::builtin::CREDENTIAL_COMMANDS;
use iwork_domain::action::SandboxPlacement;

fn placement(p: SandboxPlacement) -> &'static str {
    match p {
        SandboxPlacement::Inside => "inside",
        SandboxPlacement::Outside => "outside",
        SandboxPlacement::Ambiguous => "ambiguous",
    }
}

/// 清单条目按结构导出（程序名 + 子命令）；翻成 `excludedCommands` 文字只在 T06 一处做（方案 3.7）。
fn pattern(program: &str, sub: Option<&str>) -> serde_json::Value {
    serde_json::json!({ "program": program, "subcommand": sub })
}

fn main() {
    let rows: Vec<serde_json::Value> = CORPUS
        .iter()
        .map(|r| {
            let excluded: Vec<serde_json::Value> = CREDENTIAL_COMMANDS
                .iter()
                .chain(r.extra)
                .map(|(p, s)| pattern(p, *s))
                .collect();
            serde_json::json!({
                "command": r.command,
                "excluded_commands": excluded,
                "ours": placement(r.ours),
                "claude": r.claude.map(placement),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&rows).expect("语料可序列化")
    );
}
