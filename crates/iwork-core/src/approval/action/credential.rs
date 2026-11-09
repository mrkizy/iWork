//! 需凭据命令匹配（T07 方案 3.7、ADR-T07-b）：hook 侧与 `excludedCommands` 用同一份会话快照、
//! 同一套规则，匹配器三态且只许比 Claude Code 更保守。
//!
//! 规则（按 Claude Code 文档「Sandbox settings › excludedCommands」）：整条调用里**每一个**简单命令
//! 都命中某个条目才排除出沙箱；匹配的是调用文字。拿不准的写法判 `Ambiguous`，求值按沙箱外处理：
//! - 程序名带路径前缀、且去掉前缀后命中条目（`/usr/bin/git pull`：文档说不匹配，我们不赌）；
//! - 命中条目的程序在子命令前有以 `-` 开头的词（`git -c x=y pull`：`pull` 是不是子命令文档没说）；
//! - 每个命令都命中，但有重定向（文档说留在沙箱内，我们记 `Ambiguous` 保险）。
//!
//! 前置赋值、包装词、`cd`、`$()` 已让整条解析不了，到不了这里。

use iwork_domain::action::SandboxPlacement;
use iwork_domain::session::CommandPattern;
use iwork_shell::{CommandNode, ShellScript, SimpleCommand};

/// 一条（已解析的）Bash 调用会在沙箱里还是沙箱外运行。
pub fn classify(script: &ShellScript, patterns: &[CommandPattern]) -> SandboxPlacement {
    let commands = simple_commands(script);
    if commands.is_empty() {
        return SandboxPlacement::Inside;
    }
    let mut all_match = true;
    let mut ambiguous = false;
    let mut redirects = false;
    for c in &commands {
        redirects |= !c.redirects.is_empty();
        match match_command(c, patterns) {
            Match::Yes => {}
            Match::No => all_match = false,
            Match::Unsure => {
                ambiguous = true;
                all_match = false;
            }
        }
    }
    if ambiguous || (all_match && redirects) {
        SandboxPlacement::Ambiguous
    } else if all_match {
        SandboxPlacement::Outside
    } else {
        SandboxPlacement::Inside
    }
}

enum Match {
    Yes,
    No,
    Unsure,
}

fn match_command(c: &SimpleCommand, patterns: &[CommandPattern]) -> Match {
    let Some(first) = c.words.first() else {
        return Match::No;
    };
    let program = first.literal.as_str();
    let basename = program.rsplit('/').next().unwrap_or(program);
    if basename != program {
        return if patterns.iter().any(|p| p.program == basename) {
            Match::Unsure
        } else {
            Match::No
        };
    }
    let mut result = Match::No;
    for p in patterns.iter().filter(|p| p.program == program) {
        match &p.subcommand {
            None => return Match::Yes,
            Some(sub) => {
                let second = c.words.get(1).map(|w| w.literal.as_str());
                if second == Some(sub.as_str()) {
                    return Match::Yes;
                }
                if second.is_some_and(|w| w.starts_with('-'))
                    && c.words.iter().skip(2).any(|w| &w.literal == sub)
                {
                    result = Match::Unsure;
                }
            }
        }
    }
    result
}

/// 全部简单命令（展平子 shell、花括号组、if、循环）。
pub fn simple_commands(script: &ShellScript) -> Vec<&SimpleCommand> {
    let mut out = Vec::new();
    walk(script, &mut out);
    out
}

fn walk<'a>(s: &'a ShellScript, out: &mut Vec<&'a SimpleCommand>) {
    for item in &s.items {
        for c in &item.pipeline {
            match c {
                CommandNode::Simple(sc) => out.push(sc),
                CommandNode::Subshell(x) | CommandNode::BraceGroup(x) => walk(x, out),
                CommandNode::If { branches } => branches.iter().for_each(|b| walk(b, out)),
                CommandNode::Loop { condition, body } => {
                    walk(condition, out);
                    walk(body, out);
                }
            }
        }
    }
}
