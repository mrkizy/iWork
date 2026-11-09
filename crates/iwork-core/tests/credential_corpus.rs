//! 需凭据命令匹配的对拍语料（T07 方案 3.7 第 4 条；验收 6c「需凭据命令的判定与 excludedCommands 条目一致」）。

mod support;

use iwork_core::approval::action::credential::classify;
use iwork_core::approval::action::credential_corpus::CORPUS;
use iwork_core::approval::session::snapshot_for;
use iwork_domain::action::SandboxPlacement;
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::id::Seq;
use iwork_domain::session::CommandPattern;
use support::*;

fn builtin_patterns() -> Vec<CommandPattern> {
    snapshot_for(ApprovalMode::Manual, &[], &scope(), &env(), Seq(0))
        .constraints
        .excluded_commands
}

#[test]
fn credential_corpus_matches_ours_and_is_conservative() {
    assert!(CORPUS.len() >= 40, "对照：至少 40 条");
    let base = builtin_patterns();
    let mut untested = Vec::new();
    for row in CORPUS {
        let mut patterns = base.clone();
        patterns.extend(row.extra.iter().map(|(p, s)| CommandPattern {
            program: (*p).into(),
            subcommand: s.map(Into::into),
        }));
        let script = iwork_shell::parse(row.command)
            .unwrap_or_else(|u| panic!("{:?} 应能解析：{}", row.command, u.why));
        assert_eq!(classify(&script, &patterns), row.ours, "{:?}", row.command);
        match row.claude {
            None => untested.push(row.command),
            Some(SandboxPlacement::Outside) => assert_ne!(
                row.ours,
                SandboxPlacement::Inside,
                "{:?}：Claude 排除出沙箱而我们判沙箱内",
                row.command
            ),
            Some(_) => {}
        }
        if row.ours == SandboxPlacement::Inside
            && let Some(c) = row.claude
        {
            assert_eq!(c, SandboxPlacement::Inside, "{:?}", row.command);
        }
    }
    // 第四列待 T06 的 O1 ④ 回填；列出未测的行数，供报告引用
    eprintln!("credential_corpus：{} 行未实测（untested）", untested.len());
    // 三种结果都出现过（对照：语料真的在比东西）
    for want in [
        SandboxPlacement::Inside,
        SandboxPlacement::Outside,
        SandboxPlacement::Ambiguous,
    ] {
        assert!(CORPUS.iter().any(|r| r.ours == want));
    }
}

/// hook 侧求值用的清单与写进 excludedCommands 的清单出自同一份快照数据。
#[test]
fn excluded_commands_equal_classifier_patterns() {
    let s = snapshot_for(ApprovalMode::Auto, &[], &scope(), &env(), Seq(0));
    assert_eq!(
        s.constraints.excluded_commands,
        s.boundaries.credential_commands
    );
    assert!(!s.constraints.excluded_commands.is_empty());
}
