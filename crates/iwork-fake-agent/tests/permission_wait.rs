//! 权限请求的等待与作废：T05 验收 4（判定方返回前一直挂起，≥ 60s 不超时），
//! 以及等审批时回合失败 / 取消会作废挂着的审批。

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iwork_agent_acp::AcpRuntime;
use iwork_core::turn::{PermissionDecider, SessionLine, TurnPlan, TurnPorts};
use iwork_domain::approval::VoidReason;
use iwork_domain::event::{ApprovalVoided, DomainEvent};
use iwork_domain::id::{ApprovalId, RawUlid, TurnId};
use iwork_domain::turn::TurnState;
use iwork_ports::agent::{PermissionDecision, PermissionRequest};
use serde_json::json;
use support::*;
use tokio::sync::watch;

/// 由测试决定何时答复的判定方；作废时记下原因，并交回一条作废事件。
struct Gated {
    answer: watch::Sender<Option<PermissionDecision>>,
    asked: Mutex<Vec<String>>,
    voided: Mutex<Vec<(TurnId, VoidReason)>>,
}

const PENDING_APPROVAL: u128 = 77;

impl Gated {
    fn new() -> Self {
        Self {
            answer: watch::channel(None).0,
            asked: Mutex::new(Vec::new()),
            voided: Mutex::new(Vec::new()),
        }
    }
}

impl PermissionDecider for Gated {
    async fn decide(&self, _turn_id: TurnId, req: &PermissionRequest) -> PermissionDecision {
        self.asked.lock().unwrap().push(req.tool.0.clone());
        let mut rx = self.answer.subscribe();
        let answer = rx.wait_for(Option::is_some).await.expect("发送端活着");
        answer.clone().expect("刚等到 Some")
    }

    async fn void_pending(&self, turn_id: TurnId, reason: VoidReason) -> Vec<DomainEvent> {
        self.voided.lock().unwrap().push((turn_id, reason));
        vec![
            ApprovalVoided {
                approval_id: ApprovalId::from_raw(RawUlid::from_u128(PENDING_APPROVAL)),
                reason,
            }
            .into(),
        ]
    }
}

fn waiting_script() -> serde_json::Value {
    json!({ "turns": [{ "steps": [
        { "type": "tool_call", "id": "t1", "name": "Bash", "input": { "command": "rm -rf build" } },
        { "type": "permission", "tool_call_id": "t1",
          "allowed": [ { "type": "tool_result", "id": "t1", "ok": true }, { "type": "text", "text": "删好了" } ],
          "denied": [ { "type": "text", "text": "不删" } ] }
    ]}]})
}

struct Run {
    h: Arc<Harness>,
    decider: Arc<Gated>,
    task: tokio::task::JoinHandle<TurnState>,
    turn_id: TurnId,
    canceller: iwork_core::turn::TurnCanceller,
}

struct Harness {
    store: MemStore,
    clock: RealClock,
    ids: CountingIds,
    live: Deltas,
}

fn start(fake: &Fake) -> Run {
    let h = Arc::new(Harness {
        store: MemStore::default(),
        clock: RealClock::default(),
        ids: CountingIds::default(),
        live: Deltas::default(),
    });
    let decider = Arc::new(Gated::new());
    let mut line = SessionLine::new(decider.clone(), None);
    let canceller = line.canceller();
    let runtime = AcpRuntime::new(fake.launch.clone());
    let plan = TurnPlan {
        turn: queued_turn(&h.ids),
        assistant_name: "小助手".into(),
        context_project_id: None,
        spec: spec(fake.dir.path()),
        blocks: text("清理"),
    };
    let turn_id = plan.turn.id;
    let task = {
        let h = h.clone();
        tokio::spawn(async move {
            let ports = TurnPorts {
                store: &h.store,
                clock: &h.clock,
                ids: &h.ids,
                live: &h.live,
            };
            line.run_turn(&runtime, ports, plan).await.unwrap()
        })
    };
    Run {
        h,
        decider,
        task,
        turn_id,
        canceller,
    }
}

async fn wait_until(limit: Duration, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < limit, "等了 {limit:?} 条件仍不成立");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 验收 4：判定方 61 秒不答复，回合一直停在 WaitingApproval，运行时也一直没收到答复；
/// 答复之后回合照常走完。
#[tokio::test(flavor = "multi_thread")]
async fn permission_stays_pending_until_decider_answers() {
    let fake = Fake::new(&waiting_script());
    let run = start(&fake);
    wait_until(Duration::from_secs(10), || {
        run.h.store.last_turn_state() == Some(TurnState::WaitingApproval)
    })
    .await;

    tokio::time::sleep(Duration::from_secs(61)).await;

    assert!(!run.task.is_finished(), "61 秒后回合仍在进行");
    assert_eq!(
        run.h.store.last_turn_state(),
        Some(TurnState::WaitingApproval)
    );
    assert_eq!(*run.decider.asked.lock().unwrap(), ["Bash"]);
    assert!(
        fake.records()
            .iter()
            .all(|r| r.get("permission_response").is_none()),
        "运行时没有收到任何答复"
    );

    run.decider
        .answer
        .send_replace(Some(PermissionDecision::Allow));
    assert_eq!(run.task.await.unwrap(), TurnState::Completed);
    let snapshot = run.h.store.snapshot();
    assert!(
        snapshot.contains(&"message Completed \"删好了\"".to_string()),
        "{snapshot:?}"
    );
    assert!(
        run.decider.voided.lock().unwrap().is_empty(),
        "正常结束不作废"
    );
}

/// 等审批时运行时进程被杀（后台服务还在）：回合 WaitingApproval → Failed，
/// 挂着的审批以 TurnFailed 作废，与状态变化在同一次提交里。
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn process_death_while_waiting_fails_turn_and_voids_approval() {
    let fake = Fake::new(&waiting_script());
    let run = start(&fake);
    wait_until(Duration::from_secs(10), || {
        run.h.store.last_turn_state() == Some(TurnState::WaitingApproval)
    })
    .await;
    let pid = fake
        .records()
        .iter()
        .find_map(|r| r["started_pid"].as_u64())
        .expect("假运行时记下了进程号");
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());

    assert_eq!(run.task.await.unwrap(), TurnState::Failed);
    let snapshot = run.h.store.snapshot();
    let tail = &snapshot[snapshot.len() - 4..];
    assert!(
        tail[0].starts_with("turn WaitingApproval -> Failed (运行时进程意外退出"),
        "{tail:?}"
    );
    assert_eq!(
        &tail[1..],
        [
            "approval_voided",
            "message Failed \"\"",
            "turn finished Failed"
        ]
    );
    assert_eq!(
        *run.decider.voided.lock().unwrap(),
        [(run.turn_id, VoidReason::TurnFailed)]
    );
}

/// 等审批时用户取消：回合 WaitingApproval → Cancelling，审批以 TurnCancelled 作废，
/// 运行时收到「已取消」答复后以 cancelled 结束。
#[tokio::test(flavor = "multi_thread")]
async fn cancel_while_waiting_voids_approval_and_answers_cancelled() {
    let fake = Fake::new(&waiting_script());
    let run = start(&fake);
    wait_until(Duration::from_secs(10), || {
        run.h.store.last_turn_state() == Some(TurnState::WaitingApproval)
    })
    .await;

    run.canceller.cancel(run.turn_id);

    assert_eq!(run.task.await.unwrap(), TurnState::Cancelled);
    assert_eq!(
        *run.decider.voided.lock().unwrap(),
        [(run.turn_id, VoidReason::TurnCancelled)]
    );
    let snapshot = run.h.store.snapshot();
    assert_eq!(
        &snapshot[snapshot.len() - 5..],
        [
            "turn WaitingApproval -> Cancelling",
            "approval_voided",
            "turn Cancelling -> Cancelled",
            "message Interrupted \"\"",
            "turn finished Cancelled",
        ]
    );
    let answers: Vec<_> = fake
        .records()
        .into_iter()
        .filter_map(|r| r.get("permission_response").cloned())
        .collect();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0]["outcome"]["outcome"], "cancelled");
}
