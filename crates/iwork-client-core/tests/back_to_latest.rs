//! Q9（产品负责人 2026-11-11）：重连走了快照、时间线重建时用户正在看旧消息 → 回到最新并提示；
//! 停在最新时重建不提示（两个方向都判）。

mod support;

use iwork_client_core::derive::conversation::header;
use iwork_client_core::store::Store;
use iwork_proto::pb;
use support::*;

fn with_rows() -> Store {
    let mut s = store_with_open();
    for i in 0..5u64 {
        let seq = 10 + i;
        s.apply(persisted(
            seq,
            &[CONV],
            appended(assistant_msg(
                &format!("m{i}"),
                seq,
                pb::MessageStatus::Completed,
                "x",
            )),
        ));
    }
    s.take_dirty();
    s
}

fn newest_ordinal(s: &Store) -> i64 {
    s.open
        .as_ref()
        .unwrap()
        .identities()
        .last()
        .unwrap()
        .0
        .ordinal()
}

fn reconnect_snapshot(s: &mut Store) {
    let page = page(
        30,
        vec![assistant_msg(
            "m9",
            29,
            pb::MessageStatus::Completed,
            "断线期间的",
        )],
        true,
    );
    s.apply(snapshot(CONV, 30, vec![page]));
}

fn notice(s: &Store) -> Option<String> {
    header(s, s.open.as_ref().unwrap()).notice
}

#[test]
fn reading_old_messages_goes_back_to_latest_with_notice() {
    let mut s = with_rows();
    let oldest = s.open.as_ref().unwrap().identities()[0].0.ordinal();
    s.set_window(CID, oldest, oldest);
    reconnect_snapshot(&mut s);
    assert_eq!(notice(&s).as_deref(), Some("已重新连接，回到最新"));
    assert_eq!(s.window, None, "旧窗口清掉，下一帧从最新一页开始");
    assert!(s.take_back_to_latest_started(), "会话层据此开始计时");
    s.clear_back_to_latest();
    assert_eq!(notice(&s), None, "到时撤掉");
}

#[test]
fn at_latest_rebuild_has_no_notice() {
    let mut s = with_rows();
    let newest = newest_ordinal(&s);
    s.set_window(CID, newest - 4, newest);
    reconnect_snapshot(&mut s);
    assert_eq!(notice(&s), None);
    assert!(!s.take_back_to_latest_started());
}
