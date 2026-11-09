//! 需凭据命令匹配的对拍语料（T07 方案 3.7 第 4 条）。
//!
//! 每行：命令文字、清单（内置清单之外追加的条目）、我们的结果、Claude Code 实测结果。T06 的 O1 ④
//! 用同一张表（`cargo run -p iwork-core --example dump-credential-corpus` 导出 JSON）对固定版本的
//! 真实 CLI 跑，把第四列填回来；填不上的为 `None`（untested），CI 跳过但报告里列出。
//! 测试断言：Claude 为 Outside 的行，我们必须是 Outside 或 Ambiguous；我们为 Inside 的行，Claude 必须是 Inside。

use iwork_domain::action::SandboxPlacement;

use SandboxPlacement::{Ambiguous as A, Inside as I, Outside as O};

/// 语料的一行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorpusRow {
    pub command: &'static str,
    /// 内置需凭据清单之外追加的条目
    pub extra: &'static [(&'static str, Option<&'static str>)],
    pub ours: SandboxPlacement,
    /// Claude Code 2.1.319 实测；`None` = 还没测
    pub claude: Option<SandboxPlacement>,
}

const fn row(command: &'static str, ours: SandboxPlacement) -> CorpusRow {
    CorpusRow {
        command,
        extra: &[],
        ours,
        claude: None,
    }
}

const fn with(
    command: &'static str,
    extra: &'static [(&'static str, Option<&'static str>)],
    ours: SandboxPlacement,
) -> CorpusRow {
    CorpusRow {
        command,
        extra,
        ours,
        claude: None,
    }
}

const TF: &[(&str, Option<&str>)] = &[("terraform", Some("apply"))];
const AWS: &[(&str, Option<&str>)] = &[("aws", None)];

pub const CORPUS: &[CorpusRow] = &[
    // 精确命中：程序 + 子命令
    row("git pull", O),
    row("git pull origin main", O),
    row("git fetch --all", O),
    row("git push origin HEAD", O),
    row("git clone https://example.com/r.git", O),
    row("docker login", O),
    row("npm publish", O),
    row("cargo publish --dry-run", O),
    row("gh pr list", O),
    row("gh", O),
    row("ssh host uptime", O),
    with("terraform apply -auto-approve", TF, O),
    with("aws s3 ls", AWS, O),
    // 不命中：子命令不对、程序不在清单
    row("git status", I),
    row("git log --oneline", I),
    row("git commit -m x", I),
    row("docker build .", I),
    row("npm test", I),
    row("cargo build", I),
    row("ls -la", I),
    with("terraform plan", TF, I),
    row("terraform apply", I),
    // 组合：每个都命中才算沙箱外
    row("git pull && git push", O),
    row("git fetch; git pull", O),
    row("git pull && cargo build", I),
    row("cargo build && git push", I),
    row("git pull | cat", I),
    row("(git pull)", O),
    row("{ git pull; }", O),
    row("if true; then git pull; fi", I),
    // 带路径前缀：文档说不匹配，我们不赌
    row("/usr/bin/git pull", A),
    row("./git pull", A),
    row("/usr/local/bin/gh pr list", A),
    row("/usr/bin/make", I),
    // 子命令前有选项
    row("git -c core.sshCommand=x pull", A),
    row("git -C sub pull", A),
    row("git --no-pager fetch", A),
    row("git -C sub status", I),
    row("docker --context x push img", A),
    // 都命中但有重定向
    row("git pull > log.txt", A),
    row("git pull 2>&1", O),
    row("git fetch < /dev/null", A),
    row("git pull > log && git push", A),
    // 只有重定向
    row("> a.txt", I),
];
