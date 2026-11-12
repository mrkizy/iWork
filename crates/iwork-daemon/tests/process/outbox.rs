//! outbox 跨进程崩溃的重投（卡验收 3「outbox 未投递项被重投且对端去重后不重复执行」；方案 2.11）。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use iwork_domain::event::GlobalSettingsChanged;
use iwork_ports::store::StoragePort;

use crate::fixtures::{block, cmd, outbox};
use crate::seed;
use crate::support::{Sandbox, code, signal};

const START: Duration = Duration::from_secs(20);

/// 两条：一条去可重投的去向、一条去不能幂等的去向。投递返回之后、记结果之前（「已派发、未记结果」）`kill -9`；
/// 重启后：可重投的那条再投一次（接收方收到两次、按 id 去重后执行一次）并送达；不能幂等的那条标失败、没有重投。
#[test]
fn outbox_redelivered_once_at_receiver() {
    let sb = Sandbox::new();
    let store = seed::open(&sb);
    let mut req = cmd(
        1,
        vec![
            GlobalSettingsChanged {
                playful_ui: None,
                autostart: None,
                projects_root: None,
            }
            .into(),
        ],
    );
    req.outbox = vec![outbox("dev-redeliver:1", b"a"), outbox("dev-fail:1", b"b")];
    block(store.commit(req)).unwrap();
    store.close().unwrap();
    let receiver = sb.home.join("receiver.log");
    let recv_env = receiver.display().to_string();

    let mut a = sb.spawn(
        &[],
        &[
            ("IWORK_DEV_OUTBOX_RECEIVER", &recv_env),
            ("IWORK_DEV_PAUSE_AT", "outbox-after-dispatch"),
        ],
    );
    let deadline = Instant::now() + START;
    while a
        .stderr()
        .matches("IWORK-PAUSED outbox-after-dispatch")
        .count()
        < 2
    {
        assert!(
            Instant::now() < deadline,
            "两条道没都停在暂停点：{}",
            a.stderr()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    a.kill9();
    assert_eq!(seed::outbox_states(&sb), ["dispatched", "dispatched"]);

    let mut b = sb.spawn(&[], &[("IWORK_DEV_OUTBOX_RECEIVER", &recv_env)]);
    sb.wait_ready(b.pid(), START);
    let deadline = Instant::now() + START;
    while seed::outbox_states(&sb) != ["accepted_by_runtime", "failed"] {
        assert!(Instant::now() < deadline, "{:?}", seed::outbox_states(&sb));
        std::thread::sleep(Duration::from_millis(20));
    }
    signal(b.pid(), libc::SIGTERM);
    assert_eq!(code(b.wait_exit(START)), Some(0));

    let lines = std::fs::read_to_string(&receiver).unwrap();
    let mut received: BTreeMap<&str, usize> = BTreeMap::new();
    for l in lines.lines() {
        *received.entry(l).or_default() += 1;
    }
    // outbox 的 id：可重投的是 1，不能幂等的是 2
    assert_eq!(
        received,
        BTreeMap::from([("1", 2), ("2", 1)]),
        "可重投的收到两次（去重后执行一次），不能幂等的没被重投"
    );
}
