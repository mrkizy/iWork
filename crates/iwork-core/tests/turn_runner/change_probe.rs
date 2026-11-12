//! 改动摘要与「搞定」章（T10a 方案 D10，编排那一半）：回合以 `Completed` 收尾时限时问一次探针；
//! 别的结局不问；探针给不出、panic、不返回都不挡收尾，只是不带摘要、不盖章。

use super::*;

use iwork_core::turn::CHANGE_SUMMARY_LIMIT;
use iwork_domain::event::{ChangeSummary, TurnFinished};
use iwork_ports::vcs::ChangeProbe;

enum Answer {
    Summary(Option<ChangeSummary>),
    Panic,
    Hang,
}

struct FakeProbe {
    answer: Answer,
    calls: AtomicUsize,
}

impl FakeProbe {
    fn new(answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            calls: AtomicUsize::new(0),
        })
    }
}

impl ChangeProbe for FakeProbe {
    fn summary(&self) -> Pin<Box<dyn Future<Output = Option<ChangeSummary>> + Send + '_>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match &self.answer {
                Answer::Summary(s) => s.clone(),
                Answer::Panic => panic!("探针坏了"),
                Answer::Hang => std::future::pending().await,
            }
        })
    }
}

fn summary(files: u32) -> ChangeSummary {
    ChangeSummary {
        files_changed: files,
        insertions: 3,
        deletions: 1,
        paths: (0..files).map(|i| format!("f{i}").into()).collect(),
    }
}

fn finished(b: &Bench, t: TurnId) -> TurnFinished {
    b.store
        .events
        .lock()
        .unwrap()
        .iter()
        .find_map(|e| match e {
            DomainEvent::TurnFinished(f) if f.turn_id == t => Some(f.clone()),
            _ => None,
        })
        .expect("回合有结束事件")
}

/// 带着探针跑一个回合，让运行时以 `end` 结束，返回回合 id 与结局。
async fn run_with(
    b: &Arc<Bench>,
    probe: Option<Arc<FakeProbe>>,
    end: AgentUpdate,
) -> (TurnId, TurnState) {
    let (line, _) = b.line();
    let mut plan = b.plan();
    plan.changes = probe.map(|p| p as Arc<dyn ChangeProbe>);
    let t = plan.turn.id;
    let task = b.spawn(line, plan);
    b.wait_prompts(1).await;
    b.ctl.send(end);
    let (result, _) = task.await.unwrap();
    (t, result.unwrap())
}

#[tokio::test(flavor = "current_thread")]
async fn completed_turn_carries_change_summary() {
    let b = Bench::new();
    let probe = FakeProbe::new(Answer::Summary(Some(summary(2))));
    let (t, state) = run_with(
        &b,
        Some(probe.clone()),
        AgentUpdate::Finished(TurnOutcome::Completed),
    )
    .await;
    assert_eq!(state, TurnState::Completed);
    let f = finished(&b, t);
    assert_eq!(f.change_summary, Some(summary(2)));
    assert!(f.celebrate, "有文件变了才盖章");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    // 摘要和 Completed 同一笔：结束事件紧跟在状态变化与消息收尾之后
    let trail = b.store.trail_of(t);
    assert_eq!(
        &trail[trail.len() - 3..],
        [
            "Running -> Completed",
            "completed Completed",
            "finished Completed"
        ]
    );

    // 对照：没有探针时不带摘要、不盖章
    let b = Bench::new();
    let (t, _) = run_with(&b, None, AgentUpdate::Finished(TurnOutcome::Completed)).await;
    let f = finished(&b, t);
    assert_eq!((f.change_summary, f.celebrate), (None, false));
}

/// 这一趟期间没有文件变（开始前就有的改动探针不算）：有摘要，不盖章。
#[tokio::test(flavor = "current_thread")]
async fn preexisting_changes_are_not_counted() {
    let b = Bench::new();
    let probe = FakeProbe::new(Answer::Summary(Some(summary(0))));
    let (t, _) = run_with(
        &b,
        Some(probe),
        AgentUpdate::Finished(TurnOutcome::Completed),
    )
    .await;
    let f = finished(&b, t);
    assert_eq!(f.change_summary, Some(summary(0)));
    assert!(!f.celebrate);
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_and_failed_turns_skip_probe() {
    for end in [
        AgentUpdate::Finished(TurnOutcome::Cancelled),
        AgentUpdate::Failed(AgentError::Protocol {
            message: "坏了".into(),
        }),
    ] {
        let b = Bench::new();
        let probe = FakeProbe::new(Answer::Summary(Some(summary(2))));
        let (t, state) = run_with(&b, Some(probe.clone()), end).await;
        assert!(
            state.is_terminal() && state != TurnState::Completed,
            "{state:?}"
        );
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0, "{state:?}");
        let f = finished(&b, t);
        assert_eq!((f.change_summary, f.celebrate), (None, false));
    }
}

/// 收尾那一问报错（给不出）、panic、不返回：回合照常 Completed，不带摘要、不盖章。不返回的那格用暂停的时钟，
/// 等满 `CHANGE_SUMMARY_LIMIT` 就放手。
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn change_probe_never_blocks_completion() {
    for answer in [Answer::Summary(None), Answer::Panic, Answer::Hang] {
        let b = Bench::new();
        let probe = FakeProbe::new(answer);
        let started = tokio::time::Instant::now();
        let (t, state) = run_with(
            &b,
            Some(probe.clone()),
            AgentUpdate::Finished(TurnOutcome::Completed),
        )
        .await;
        assert_eq!(state, TurnState::Completed);
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        let f = finished(&b, t);
        assert_eq!((f.change_summary, f.celebrate), (None, false));
        assert!(started.elapsed() <= CHANGE_SUMMARY_LIMIT + Duration::from_secs(1));
    }
}
