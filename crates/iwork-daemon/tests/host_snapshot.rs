//! 每个回合开始前重算会话快照（T10a 方案 D8、G5）：已提交的清单与此刻磁盘上的路径 → 同一份交给运行时（开会话的
//! 约束）与判定方（审批）。判据：假运行时记下的每次开会话的约束、它发出的写请求得到的判定、库里有没有审批卡片。

mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::event::{
    ApprovalModeChangeCause, ApprovalModeChanged, ApprovalModeTarget, DomainEvent, RuleRemembered,
    SecurityListEntryAdded, SecurityListEntryRemoved,
};
use iwork_domain::id::TurnId;
use iwork_domain::rule::{RuleEffect, RuleEntry, RuleLayer, RuleMatcher, RuleScope};
use iwork_domain::session::AllowWrite;
use iwork_domain::turn::TurnState;
use iwork_ports::agent::{PermissionDecision, SessionSpec};
use iwork_ports::store::StoragePort;
use support::fixtures::*;
use support::turns::*;

const WRITABLE_ENTRY: u128 = 80;

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap()
}

fn writable_entry(dir: &Path) -> RuleEntry {
    let mut e = rule(WRITABLE_ENTRY, RuleLayer::WritableDirs, RuleScope::Global);
    e.matcher = RuleMatcher::PathUnder(dir.to_path_buf());
    e.effect = RuleEffect::Allow;
    e
}

/// 对话切到自动档：写在可写范围里放行、范围外要问，清单的有无直接决定判定（手动档下写一律要问，看不出差别）。
fn auto_mode() -> DomainEvent {
    ApprovalModeChanged {
        target: ApprovalModeTarget::Conversation(conversation()),
        mode: Some(ApprovalMode::Auto),
        cause: ApprovalModeChangeCause::User,
        released_approvals: None,
    }
    .into()
}

fn may_write(spec: &SessionSpec, dir: &Path) -> bool {
    matches!(&spec.constraints.allow_write, AllowWrite::Whitelist(v) if v.iter().any(|p| p == dir))
}

#[cfg(unix)]
fn denies(spec: &SessionSpec, dir: &Path) -> bool {
    spec.constraints.deny_read_write.iter().any(|p| p == dir)
}

/// 发一条消息，回合里运行时要写 `file`，等这个回合停下（跑完或因等审批到期中断）。
async fn turn_writing(h: &H, n: u128, file: &Path) -> TurnId {
    h.agent.ask.store(true, Ordering::SeqCst);
    *h.agent.wait_limit.lock().unwrap() = Some(std::time::Duration::from_millis(200));
    *h.agent.ask_with.lock().unwrap() = Some((
        "Write".into(),
        serde_json::json!({ "file_path": file, "content": "x" }).to_string(),
    ));
    let t = h.send(n, "写一下").await;
    h.wait_state(t, |s| {
        s.is_terminal() || matches!(s, TurnState::Interrupted { .. })
    })
    .await;
    t
}

async fn asked_user(h: &H, t: TurnId) -> bool {
    events(&h.raw)
        .await
        .iter()
        .any(|(_, e)| matches!(e, DomainEvent::ApprovalRequested(a) if a.approval.turn_id == t))
}

fn decisions(h: &H) -> Vec<PermissionDecision> {
    h.agent.decisions.lock().unwrap().clone()
}

/// D8（1/F-1）压住 `engine-sync` 的一格：可写目录的清单条目在两轮之间删掉，`engine-sync` 一直不跑。下一个回合的
/// 约束里没有那个目录，判定方也按没有它判（往里写要问用户）；上一个回合两边都有它（写放行、不问）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_removed_while_sync_is_held() {
    // 临时目录收窄到一个无关的位置：夹具的数据都在系统临时目录下，不收窄的话哪儿都算可写
    let mut h = harness(Opts {
        extra: vec![auto_mode()],
        tmp_dirs: Some(vec![PathBuf::from("/nonexistent-iwork-test-tmp")]),
        ..Opts::default()
    })
    .await;
    let w = h.env.tmp.path().join("w");
    std::fs::create_dir_all(&w).unwrap();
    let w = canonical(&w);
    h.raw
        .commit(cmd(
            299,
            vec![
                SecurityListEntryAdded {
                    rule: writable_entry(&w),
                }
                .into(),
            ],
        ))
        .await
        .unwrap();
    h.start_host_only();
    let file = w.join("a.txt");
    let a = turn_writing(&h, 1, &file).await;
    assert!(
        may_write(&h.agent.specs.lock().unwrap()[0], &w),
        "第一轮的约束里有它"
    );
    assert_eq!(decisions(&h), [PermissionDecision::Allow], "第一轮写放行");
    assert!(!asked_user(&h, a).await);
    h.raw
        .commit(cmd(
            300,
            vec![
                SecurityListEntryRemoved {
                    entry_id: rid(WRITABLE_ENTRY),
                }
                .into(),
            ],
        ))
        .await
        .unwrap();
    let b = turn_writing(&h, 2, &file).await;
    let specs = h.agent.specs.lock().unwrap().clone();
    // 换了线：新线带着这个对话最后一条绑定开会话（之后因等审批到期中断，运行时会再开，不在这里数）
    assert_eq!(
        h.agent.opened_with.lock().unwrap().get(1).cloned(),
        Some(Some("rt-0".to_owned())),
        "清单变了，换了一条线"
    );
    assert!(
        specs[1..].iter().all(|sp| !may_write(sp, &w)),
        "删掉之后每次开会话的约束里都没有它"
    );
    assert!(asked_user(&h, b).await, "判定方也按没有它判：要问用户");
    h.stop().await;
}

/// D8 只改链接的一格：两轮之间把家目录下受保护的 `.ssh` 换成指进可写目录的链接，不发任何命令。下一个回合的快照里
/// 有它真实位置的别名：交给运行时的禁读禁写里有那个位置，往那里写被拦下；上一个回合往同一处写是放行的。
///
/// 只在 unix 上跑：要在测试里建符号链接；M1 不支持 Windows（AGENTS.md「M1 不支持 Windows」，跟真实文件系统对拍的
/// 测试限定 unix）。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_only_change_is_seen_next_turn() {
    let mut h = harness(Opts {
        extra: vec![auto_mode()],
        ..Opts::default()
    })
    .await;
    let home = h.env.tmp.path().join("home");
    let ssh = home.join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    let w = h.env.tmp.path().join("w");
    let real = w.join("sshreal");
    std::fs::create_dir_all(&real).unwrap();
    let (w, real) = (canonical(&w), canonical(&real));
    h.raw
        .commit(cmd(
            300,
            vec![
                SecurityListEntryAdded {
                    rule: writable_entry(&w),
                }
                .into(),
            ],
        ))
        .await
        .unwrap();
    h.start_host();
    let file = real.join("k");
    turn_writing(&h, 1, &file).await;
    assert!(!denies(&h.agent.specs.lock().unwrap()[0], &real));
    assert_eq!(decisions(&h), [PermissionDecision::Allow], "第一轮写放行");
    // 只改磁盘：受保护的目录换成指进可写目录的链接
    std::fs::remove_dir(&ssh).unwrap();
    std::os::unix::fs::symlink(&real, &ssh).unwrap();
    turn_writing(&h, 2, &file).await;
    let specs = h.agent.specs.lock().unwrap().clone();
    assert_eq!(specs.len(), 2, "快照变了，换了一条线");
    assert!(
        denies(&specs[1], &real),
        "下一轮的禁读禁写里有真实位置：{:?}",
        specs[1].constraints.deny_read_write
    );
    let d = decisions(&h);
    assert!(
        matches!(d.get(1), Some(PermissionDecision::Deny { .. })),
        "往真实位置写被拦下：{d:?}"
    );
    h.stop().await;
}

/// D8 的对照：清单与磁盘都没变时连跑两个回合用同一条线（只开过一次会话）；只有记审计用的 `lists_seq` 前进
/// （rule 流上来了一条和清单无关的事件）也照样沿用。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unchanged_snapshot_reuses_line() {
    let mut h = harness(Opts::default()).await;
    h.start_host();
    let a = h.send(1, "一").await;
    h.wait_state(a, |s| s == TurnState::Completed).await;
    let b = h.send(2, "二").await;
    h.wait_state(b, |s| s == TurnState::Completed).await;
    // rule 流上一条和清单无关的记住规则：lists_seq 前进，清单内容不变
    let mut remembered = rule(81, RuleLayer::Remembered, RuleScope::Global);
    remembered.source = iwork_domain::rule::RuleSource::Remembered {
        approval_id: apid(81),
        by: iwork_domain::common::Actor("user".into()),
    };
    h.raw
        .commit(cmd(300, vec![RuleRemembered { rule: remembered }.into()]))
        .await
        .unwrap();
    let c = h.send(3, "三").await;
    h.wait_state(c, |s| s == TurnState::Completed).await;
    assert_eq!(h.agent.specs.lock().unwrap().len(), 1, "一条线一个会话");
    assert_eq!(*h.agent.opened_with.lock().unwrap(), vec![None]);
    h.stop().await;
}
