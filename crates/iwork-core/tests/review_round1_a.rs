//! 第 1 轮代码审查 A 块的反例（2026-11-10-m1-t07/1/F-3、F-4、F-5、F-8、F-10、F-11；F-9 在 security_list.rs）。

mod support;

use iwork_core::approval::denial::{DenialEvent, DenialMemory};
use iwork_core::approval::session::snapshot_for;
use iwork_domain::action::CallShapeKind;
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::id::Seq;
use iwork_domain::rule::{
    AskReason, DenyReason, RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleScope, RuleSource,
    Verdict,
};
use support::fake_fs::FakeFs;
use support::*;

use ApprovalMode::{Auto, FullAccess, Manual};

fn denied(v: &Verdict) -> bool {
    matches!(
        v,
        Verdict::Deny(DenyReason::Baseline(_)) | Verdict::UserAction(_)
    )
}

/// 用给定的文件系统重算会话快照（规则路径按它解析）。
fn harness_on(fs: FakeFs, lists: &[RuleEntry]) -> Harness {
    let snap = snapshot_for(Manual, lists, &scope(), &env(), Seq(0), &fs);
    Harness {
        snap,
        fs,
        ..Harness::default()
    }
}

/// F-3：受保护路径本身是链接时，经真实位置的访问同样命中底线。
#[test]
fn protected_rule_symlink() {
    let mut fs = FakeFs::new(HOME, false);
    fs.add(format!("{WS}/src/a.rs"))
        .add("/tmp/relocated/config");
    fs.link("/home/u/.ssh", "/tmp/relocated");
    let h = harness_on(fs, &[]);
    for m in [Manual, Auto, FullAccess] {
        assert!(
            denied(&h.verdict(&read_req("/home/u/.ssh/config"), m)),
            "{m:?}"
        );
        assert!(
            denied(&h.verdict(&read_req("/tmp/relocated/config"), m)),
            "{m:?}"
        );
        assert!(
            denied(&h.verdict(&bash_req("cat /tmp/relocated/config"), m)),
            "{m:?}"
        );
    }
    // 对照：真实位置旁边的文件不受影响
    assert_eq!(h.verdict(&read_req("/tmp/other"), Manual), Verdict::Allow);
}

/// F-3：macOS 的 /etc 是 /private/etc 的别名，删除真实位置同样是关键路径。
#[test]
fn critical_alias() {
    let mut fs = FakeFs::new(HOME, false);
    fs.add(format!("{WS}/src/a.rs")).add("/private/etc/hosts");
    fs.link("/etc", "/private/etc");
    let h = harness_on(fs, &[]);
    for m in [Manual, FullAccess] {
        assert!(
            denied(&h.verdict(&bash_req("rm -r /private/etc"), m)),
            "{m:?}"
        );
        assert!(denied(&h.verdict(&bash_req("rm -r /etc"), m)), "{m:?}");
    }
}

/// F-3：用户清单里的目录是链接时，真实位置同样在清单内。
#[test]
fn user_list_symlink() {
    let mut fs = FakeFs::new(HOME, false);
    fs.add(format!("{WS}/src/a.rs")).add("/srv/secret/x");
    fs.link("/home/u/secret", "/srv/secret");
    let entry = RuleEntry {
        id: iwork_domain::id::RuleId::from_raw(iwork_domain::id::RawUlid::from_u128(0x55)),
        layer: RuleLayer::SensitiveDirs,
        matcher: RuleMatcher::PathUnder("/home/u/secret".into()),
        effect: RuleEffect::Ask,
        source: RuleSource::User,
        scope: RuleScope::Global,
        created_at: iwork_domain::common::Timestamp(0),
        label: "~/secret".into(),
        applies_in: Default::default(),
    };
    let h = harness_on(fs, &[entry]);
    assert_eq!(
        h.verdict(&read_req("/srv/secret/x"), Manual),
        Verdict::Ask(AskReason::OutOfBounds)
    );
}

/// F-4：重定向与兜底里真实的 `-` 是文件名；白名单操作数位置的 `-` 才是标准输入。
#[test]
fn redirect_dash() {
    let mut h = Harness::default();
    h.fs.link(format!("{WS}/-"), ".git/config");
    for cmd in [
        "printf x > -",
        "LC_ALL=C printf x > -",
        "tee -a - < src/a.rs",
        "cmd -",
    ] {
        for m in [Auto, FullAccess] {
            assert!(denied(&h.verdict(&bash_req(cmd), m)), "{cmd} {m:?}");
        }
    }
    // 对照：cat - 读标准输入
    assert_eq!(h.verdict(&bash_req("cat -"), Manual), Verdict::Allow);
}

/// F-5：引号里的 ~ 是字面目录名，不是主目录；词中间的 ~ 两种解读都检查。
#[test]
fn quoted_tilde_is_relative() {
    let mut h = Harness::default();
    h.fs.link(format!("{WS}/~"), ".git");
    for cmd in [
        "printf x > '~/config'",
        "printf x > \"~\"/config",
        "printf x > \\~/config",
        "cmd --out=~/config",
        "make OUT=~/config",
    ] {
        for m in [Auto, FullAccess] {
            assert!(denied(&h.verdict(&bash_req(cmd), m)), "{cmd} {m:?}");
        }
    }
    // 对照：未引用的 ~ 是主目录（写主目录是越界，不是 .git）
    let c = norm_with(&bash_req("printf x > ~/config"), &h.snap, &h.fs);
    assert!(has(
        &c,
        iwork_domain::action::ActionKind::Write,
        "/home/u/config"
    ));
}

/// F-8：带路径前缀的 git / npm / sudo 与不带前缀的同样归类。
#[test]
fn absolute_path_programs() {
    let h = Harness::default();
    // 带前缀的需凭据命令判 Ambiguous，按沙箱外求值并另带沙箱外动作（方案 3.7、3.8）；必问约束仍在
    for m in [Manual, Auto, FullAccess] {
        let (_, e) = h.eval(&bash_req("/usr/bin/git push origin main"), m);
        assert!(e.constraints.must_ask, "{m:?}");
        assert!(
            matches!(
                e.evaluation.verdict,
                Verdict::OutsideSandbox(_) | Verdict::Ask(AskReason::MustAsk)
            ),
            "{m:?}：{:?}",
            e.evaluation.verdict
        );
    }
    assert_eq!(
        three(&bash_req("/usr/bin/git push origin main")),
        ["Outside", "Outside", "Ask"]
    );
    let (_, e) = h.eval(&bash_req("/usr/local/bin/npm publish"), Auto);
    assert!(matches!(e.evaluation.verdict, Verdict::OutsideSandbox(_)));
    assert!(e.constraints.must_ask);
    assert!(!e.constraints.review_eligible());
    // 带前缀的 git 判 Ambiguous：丢弃改动的 Delete 种类保留，另加沙箱外动作
    assert_eq!(
        three(&bash_req("/usr/bin/git reset --hard")),
        ["Outside", "Outside", "Allow"]
    );
    assert_eq!(
        three(&bash_req("git reset --hard")),
        ["Ask", "Ask", "Allow"]
    );
    for cmd in [
        "/usr/bin/sudo rm -rf /",
        "/usr/bin/env git pull",
        "/bin/bash -c 'x'",
    ] {
        assert_eq!(
            norm(&bash_req(cmd)).shape.kind(),
            CallShapeKind::Unparsable,
            "{cmd}"
        );
    }
    // 对照：命令目标保留原文（指纹区分带不带前缀）
    let a = norm(&bash_req("/usr/bin/git status"));
    let b = norm(&bash_req("git status"));
    assert_ne!(a.fingerprints(), b.fingerprints());
}

/// F-10：大小写不敏感的文件系统上，换大小写写同一个尚不存在的文件仍被拒绝记忆挡住（跨工具也一样）。
#[test]
fn case_insensitive_denial_on_missing_file() {
    let mut h = Harness {
        fs: fs_ci(true),
        ..Harness::default()
    };
    let (call, _) = h.eval(&write_req("new.txt"), Auto);
    h.denial = DenialMemory::replay(&[DenialEvent::UserDenied(call.fingerprints())]);
    for req in [
        write_req("NEW.txt"),
        write_req(&format!("{WS}/New.TXT")),
        bash_req("printf x > NEW.TXT"),
    ] {
        let v = h.verdict(&req, Auto);
        assert!(
            matches!(v, Verdict::Deny(DenyReason::DenialMemory(_))),
            "{}：{v:?}",
            req.raw_input.0
        );
    }
    // 对照：大小写敏感时是两个文件
    let mut cs = Harness::default();
    let (call, _) = cs.eval(&write_req("new.txt"), Auto);
    cs.denial = DenialMemory::replay(&[DenialEvent::UserDenied(call.fingerprints())]);
    assert_eq!(cs.verdict(&write_req("NEW.txt"), Auto), Verdict::Allow);
}

/// F-11：前面有字面花括号、后面才是展开的词不算字面。
#[test]
fn brace_after_literal_braces() {
    for cmd in ["touch {}{a,b}", "touch {x}{1..3}", "cat a{b}c{d,e}"] {
        let c = norm(&bash_req(cmd));
        assert_eq!(c.shape.kind(), CallShapeKind::Unparsable, "{cmd}");
        assert_eq!(
            bash_verdict(cmd, FullAccess),
            Verdict::Ask(AskReason::UnparsableCommand),
            "{cmd}"
        );
    }
    // 对照：单独的 {} 仍是字面
    assert_ne!(
        norm(&bash_req("touch {}")).shape.kind(),
        CallShapeKind::Unparsable
    );
}
