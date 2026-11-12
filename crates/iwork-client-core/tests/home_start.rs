//! 首页「新建并发出」碰上断线重连给的是快照（审查 2a 1/F-3）：快照里的对话不带建出它的 seq，
//! 认不出哪个是新建的，停下报原因、不跳转、不发；快照之前没盖到的照常等事件配上。

mod support;

use iwork_client_core::store::created::Purpose;
use iwork_client_core::store::home::StartError;
use iwork_fake_daemon::fixtures as f;
use support::{PID, SIDEBAR, persisted, snapshot, store_with_open};

#[test]
fn snapshot_covering_the_create_stops_the_start() {
    let mut s = store_with_open();
    s.start_begin(PID.into(), "首条".into());
    s.create_accepted(5, 6, Purpose::Start);
    s.apply(snapshot(SIDEBAR, 10, vec![]));
    assert_eq!(s.home.error, Some(StartError::Lost));
    assert!(s.home.start.is_none(), "不再转圈");
    assert!(s.navigate.is_none(), "不跳");
}

/// 审查 2a 2/F-6：重连时先订阅、后重发在飞命令，快照先到、受理后到。受理来时就认定配不上，报原因、不转圈。
#[test]
fn accept_after_the_covering_snapshot_stops_the_start() {
    let mut s = store_with_open();
    s.start_begin(PID.into(), "首条".into());
    s.apply(snapshot(SIDEBAR, 10, vec![]));
    assert_eq!(s.home.error, None, "快照先到时还没有受理范围");
    s.create_accepted(5, 6, Purpose::Start);
    assert_eq!(s.home.error, Some(StartError::Lost));
    assert!(s.home.start.is_none(), "不再转圈，下一次能发");
    assert!(s.navigate.is_none(), "不跳");
}

#[test]
fn snapshot_before_the_create_leaves_it_waiting_for_the_event() {
    let mut s = store_with_open();
    s.start_begin(PID.into(), "首条".into());
    s.create_accepted(20, 21, Purpose::Start);
    s.apply(snapshot(SIDEBAR, 10, vec![]));
    assert_eq!(s.home.error, None, "对照：快照没盖到");
    let created = f::project_conversation("c-new", PID).event.unwrap();
    s.apply(persisted(21, &[SIDEBAR], created));
    assert_eq!(
        s.home
            .start
            .as_ref()
            .and_then(|x| x.conversation_id.as_deref()),
        Some("c-new"),
        "配上了，等会话层发首条消息"
    );
    assert!(s.navigate.is_none(), "发出去之前不跳");
}
