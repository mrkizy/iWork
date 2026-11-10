//! 分配量回归（3/F-2、7/F-2）：命令原文在一次调用、一条审计里只存一份，归一与审批审计的分配量随
//! 命令长度线性；资源拒绝前不先物化巨大结果。
//!
//! 用计数分配器量**峰值存活字节**（不看 RSS）。整个测试二进制只有一个测试函数，量的时候没有别的线程
//! 在分配。

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use iwork_core::approval::engine::{ApprovalEngine, EnginePorts, SessionKeys};
use iwork_core::approval::rules::builtin::builtin_rule_id;
use iwork_core::approval::session::tool_set_for;
use iwork_core::turn::PermissionDecider;
use iwork_domain::approval_mode::ApprovalMode;
use iwork_domain::common::{ModelId, ModelSelection, ToolGroups};
use iwork_domain::event::DomainEvent;
use iwork_domain::id::{RawUlid, TurnId};
use iwork_domain::project::AiReviewSettings;
use iwork_domain::rule::{DenyReason, Verdict};
use iwork_domain::turn::TurnKind;
use iwork_ports::agent::PermissionDecision;
use support::engine_fakes::*;
use support::fold_fx;
use support::*;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: 原样转交系统分配器，只多记账
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: 原样转交系统分配器
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// 跑 `f` 期间相对起点的峰值存活字节。
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

fn rm_command(n: usize, tail: &str) -> String {
    let words: String = (0..n).map(|i| format!(" x{i}")).collect();
    format!("timeout 5 rm{words}{tail}")
}

fn keys() -> SessionKeys {
    SessionKeys {
        conversation: conversation(),
        assistant: assistant(),
        runtime_id: iwork_domain::id::RuntimeId::from_raw(RawUlid::from_u128(0x888)),
        assistant_version: 1,
        project: Some(project()),
        turn_kind: TurnKind::Assistant,
        snapshot: Arc::new(snapshot()),
        tool_set: tool_set_for(TurnKind::Assistant, &ToolGroups::new()),
        disabled_groups: ToolGroups::new(),
        cwd: cp(WS),
        paths: env(),
        ai_review: AiReviewSettings {
            enabled: false,
            runtime_id: None,
            model: None,
            timeout_secs: 60,
        },
        review_model: ModelSelection {
            model_id: ModelId("m".into()),
            effort: None,
        },
    }
}

#[test]
fn allocation_is_linear_and_bounded() {
    let h = Harness::default();
    // ① 归一 + 求值：峰值字节随词数线性（旧实现每个动作复制整条原文，1000/2000/4000 词 14.7/65.5/275 MB）
    let mut peaks = Vec::new();
    for n in [1000usize, 2000, 4000] {
        let req = bash_req(&rm_command(n, ""));
        let ((call, _), peak) = peak_during(|| h.eval(&req, ApprovalMode::Manual));
        assert!(
            call.actions.iter().all(|a| a.raw_summary.is_empty()),
            "子动作不带原文"
        );
        peaks.push(peak);
    }
    let [p1, p2, p4] = [peaks[0], peaks[1], peaks[2]];
    eprintln!("归一峰值 {p1}/{p2}/{p4}");
    assert!(p4 < 64 * 1024 * 1024, "4000 词峰值 {p4} 字节");
    // 线性：词数翻 4 倍，峰值不超过 8 倍（平方级是 16 倍）
    assert!(p4 < p1 * 8, "1000/2000/4000 词峰值 {p1}/{p2}/{p4}");

    // ② 引擎审批审计：子动作进审计事件不复制原文；底线拒的调用也提交审计
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut engine_peaks = Vec::new();
    for n in [1000usize, 4000] {
        let store = MemStore::default();
        let turn = TurnId::from_raw(RawUlid::from_u128(0x700 + n as u128));
        store.seed(fold_fx::world_with(
            project(),
            conversation(),
            &[assistant()],
            None,
        ));
        store.seed(fold_fx::waiting_turn(turn, conversation(), assistant()));
        let (engine, _) = rt
            .block_on(ApprovalEngine::start(
                EnginePorts {
                    clock: TickClock::default(),
                    ids: Ids::default(),
                    store: store.clone(),
                    reviewer: FakeReviewer::default(),
                    timer: FakeTimer::default(),
                    fs: fs(),
                },
                Default::default(),
                |_, _| None,
            ))
            .unwrap();
        let d = engine.decider_for(keys());
        let req = bash_req(&rm_command(n, " ~/.ssh/id_rsa"));
        let (decision, peak) = peak_during(|| rt.block_on(d.decide(turn, &req)));
        assert!(matches!(decision.decision, PermissionDecision::Deny { .. }));
        let audit = store
            .all()
            .into_iter()
            .find_map(|e| match e {
                DomainEvent::ApprovalEvaluated(a) => Some(a),
                _ => None,
            })
            .expect("拒绝也提交审计");
        assert_eq!(audit.action.raw_summary, req_command(&req));
        assert!(audit.sub_actions.iter().all(|a| a.raw_summary.is_empty()));
        engine_peaks.push(peak);
    }
    eprintln!("审计峰值 {engine_peaks:?}");
    assert!(
        engine_peaks[1] < engine_peaks[0] * 8,
        "审计 1000/4000 词峰值 {engine_peaks:?}"
    );

    // ③ 20000 词：三档按资源上限拒，分配有上界（旧实现峰值 RSS 2.28 GB）
    let req = bash_req(&rm_command(20_000, ""));
    for m in MODES {
        let ((_, e), peak) = peak_during(|| h.eval(&req, m));
        assert_eq!(
            e.evaluation.verdict,
            Verdict::Deny(DenyReason::DenyRule(builtin_rule_id("deny.resource-limit"))),
            "{m:?}"
        );
        eprintln!("20000 词 {m:?} 峰值 {peak}");
        assert!(peak < 128 * 1024 * 1024, "{m:?}：峰值 {peak} 字节");
    }
}

fn req_command(req: &iwork_ports::agent::PermissionRequest) -> String {
    let v: serde_json::Value = serde_json::from_str(&req.raw_input.0).unwrap();
    v["command"].as_str().unwrap().to_owned()
}
