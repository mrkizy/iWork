//! 帧引擎（方案 2.4、第 7 节「帧」、验收 1）：直接驱动 Store + FrameEngine，网页一侧用 support::web 模型。

mod support;

use std::time::{Duration, Instant, SystemTime};

use iwork_client_core::derive::Ctx;
use iwork_client_core::frame::{FRAME_INTERVAL, FrameEngine, Pull};
use iwork_client_core::store::{Connection, Store, Visible};
use iwork_proto::pb;
use pb::domain_event::Event as E;
use support::web::Web;
use support::*;

fn ctx() -> Ctx {
    Ctx {
        now: SystemTime::UNIX_EPOCH + Duration::from_secs(1_762_100_000),
    }
}

fn setup(chunk_max: usize) -> (Store, FrameEngine, Web, Instant) {
    let mut s = store_with_open();
    s.set_connection(Connection::Live);
    let mut e = FrameEngine::new(chunk_max);
    let epoch = e.attach(&mut s);
    (s, e, Web::new(epoch), Instant::now())
}

/// 取到这一帧交完（或没有可交的）为止；返回应用了几帧。
fn drain(s: &mut Store, e: &mut FrameEngine, w: &mut Web, now: Instant) -> u64 {
    let before = w.frames;
    loop {
        let req = w.request();
        let applied = req.applied.map(|p| (p.frame, p.index));
        match e.pull(s, req.epoch, applied, now, ctx()) {
            Pull::Chunk(c) => {
                w.accept(&c);
            }
            Pull::Wait(_) | Pull::StaleEpoch => return w.frames - before,
        }
    }
}

fn msg_event(seq: u64, id: &str) -> iwork_client_core::store::Item {
    persisted(
        seq,
        &[CONV],
        appended(assistant_msg(id, 0, pb::MessageStatus::Streaming, "")),
    )
}

/// 验收 1：同一帧内对同一对象的多次修改合并为一次。
#[test]
fn same_frame_edits_merge_once() {
    let (mut s, mut e, mut w, t) = setup(1 << 20);
    drain(&mut s, &mut e, &mut w, t);
    s.apply(msg_event(10, "m"));
    for (i, piece) in ["a", "b", "c"].iter().enumerate() {
        s.apply(delta(&[CONV], "m", i as u64, piece));
    }
    let t2 = t + FRAME_INTERVAL;
    let Pull::Chunk(c) = e.pull(&mut s, w.epoch, w.applied, t2, ctx()) else {
        panic!("该出帧")
    };
    let patch = <pb::ViewPatch as prost::Message>::decode(c.data.as_ref()).unwrap();
    let conv: Vec<_> = patch
        .changes
        .iter()
        .filter_map(|c| match &c.change {
            Some(pb::view_change::Change::Conversation(p)) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(conv.len(), 1, "一帧里对话只出现一次");
    let rows: Vec<_> = conv[0].rows.iter().filter(|r| r.key == "m").collect();
    assert_eq!(rows.len(), 1, "同一行只发一次");
    assert!(conv[0].appends.is_empty(), "同一帧里整行重发时不再发追加");
    let Some(pb::row_view::Row::Message(m)) = &rows[0].row else {
        panic!()
    };
    assert_eq!(m.text, "abc");
}

/// 新出现的行整行发；之后只追加正文。
#[test]
fn streaming_text_goes_as_appends() {
    let (mut s, mut e, mut w, t) = setup(1 << 20);
    s.apply(msg_event(10, "m"));
    drain(&mut s, &mut e, &mut w, t);
    s.apply(delta(&[CONV], "m", 0, "hello"));
    let Pull::Chunk(c) = e.pull(&mut s, w.epoch, w.applied, t + FRAME_INTERVAL, ctx()) else {
        panic!()
    };
    let patch = <pb::ViewPatch as prost::Message>::decode(c.data.as_ref()).unwrap();
    let Some(pb::view_change::Change::Conversation(p)) =
        patch.changes.iter().find_map(|c| match &c.change {
            Some(x @ pb::view_change::Change::Conversation(_)) => Some(x.clone()),
            _ => None,
        })
    else {
        panic!()
    };
    assert!(p.rows.is_empty());
    assert_eq!(p.appends.len(), 1);
    assert_eq!(
        (p.appends[0].offset, p.appends[0].text.as_str()),
        (0, "hello")
    );
    w.accept(&c);
    let Some(pb::row_view::Row::Message(m)) = &w.state.rows["m"].row else {
        panic!()
    };
    assert_eq!(m.text, "hello");
}

/// 验收 1：不可见视图不产生增量。
#[test]
fn invisible_view_emits_nothing() {
    let (mut s, mut e, mut w, t) = setup(1 << 20);
    drain(&mut s, &mut e, &mut w, t);
    // 设置页不可见：改设置不出帧。
    s.apply(persisted(
        10,
        &[SIDEBAR],
        E::GlobalSettingsChanged(pb::GlobalSettingsChanged {
            playful_ui: Some(true),
            ..Default::default()
        }),
    ));
    // 设置变了连带回合结束行（印章）记脏，但这里没有回合结束行；对话可见所以对话会出。只看设置。
    let n = drain(&mut s, &mut e, &mut w, t + FRAME_INTERVAL);
    assert!(n <= 1);
    assert!(w.state.settings.is_none(), "设置页不可见，没有设置 patch");
    // 对照：设成可见后有。
    let mut v = s.visible.clone();
    v.settings = true;
    s.set_visible(v);
    drain(&mut s, &mut e, &mut w, t + FRAME_INTERVAL * 2);
    assert!(w.state.settings.as_ref().is_some_and(|x| x.playful_ui));
}

/// 验收 1：帧间隔不小于 16ms。
#[test]
fn frame_interval_at_least_16ms() {
    let (mut s, mut e, mut w, t) = setup(1 << 20);
    assert_eq!(drain(&mut s, &mut e, &mut w, t), 1);
    s.apply(msg_event(10, "m"));
    let req = w.request();
    match e.pull(
        &mut s,
        req.epoch,
        w.applied,
        t + Duration::from_millis(5),
        ctx(),
    ) {
        Pull::Wait(Some(due)) => assert_eq!(due, t + FRAME_INTERVAL),
        other => panic!("{other:?}"),
    }
    assert_eq!(drain(&mut s, &mut e, &mut w, t + FRAME_INTERVAL), 1);
}

/// S6：网页不来取，就没有一帧被算出来；状态库照常应用事件。
#[test]
fn no_pull_produces_nothing() {
    let (mut s, e, _w, _t) = setup(1 << 20);
    let built = e.frames_built();
    for i in 0..10_000u64 {
        s.apply(msg_event(10 + i, &format!("m{i}")));
    }
    assert_eq!(e.frames_built(), built);
    assert_eq!(e.held_bytes(), 0);
    let tl = s.open.as_ref().unwrap();
    // 状态库照常应用事件；停在最新时也按上限截掉最老的行，标成还能往上翻取回。
    assert_eq!(tl.len(), 5000);
    assert!(tl.row("m9999").is_some() && tl.row("m0").is_none());
    assert!(tl.has_older);
}

/// 取了第一块就停：Rust 只留这一帧，后来的变化不另算帧。
#[test]
fn slow_pull_holds_one_frame() {
    let (mut s, mut e, mut w, t) = setup(64);
    let Pull::Chunk(c) = e.pull(&mut s, w.epoch, None, t, ctx()) else {
        panic!()
    };
    w.accept(&c);
    let held = e.held_bytes();
    assert!(held > 64, "全量帧被切成多块");
    for i in 0..100 {
        s.apply(msg_event(10 + i, &format!("m{i}")));
    }
    assert_eq!(e.held_bytes(), held, "网页不来取，不出新帧");
    assert_eq!(e.frames_built(), 1);
}

/// 回包丢了：原样再交那一块，网页按 (frame, index) 去重。
#[test]
fn lost_reply_is_resent_once() {
    let (mut s, mut e, w, t) = setup(64);
    let Pull::Chunk(first) = e.pull(&mut s, w.epoch, None, t, ctx()) else {
        panic!()
    };
    // 网页没收到 first，仍带 applied = None 来取。
    let Pull::Chunk(again) = e.pull(&mut s, w.epoch, None, t, ctx()) else {
        panic!()
    };
    assert_eq!(first, again);
    // 收到后带上 (frame, 0)：交下一块。
    let Pull::Chunk(next) = e.pull(&mut s, w.epoch, Some((first.frame, 0)), t, ctx()) else {
        panic!()
    };
    assert_eq!(next.index, 1);
}

#[test]
fn stale_epoch_gets_no_data() {
    let (mut s, mut e, w, t) = setup(64);
    let old = w.epoch;
    let new = e.attach(&mut s);
    assert_ne!(old, new);
    let bytes = e.handed_bytes();
    assert_eq!(e.pull(&mut s, old, None, t, ctx()), Pull::StaleEpoch);
    assert_eq!(e.handed_bytes(), bytes);
}

/// attach 换代次：上一代交到一半的帧丢掉，下一帧是全量。
#[test]
fn reattach_bumps_epoch_and_sends_full() {
    let (mut s, mut e, mut w, t) = setup(64);
    s.apply(msg_event(10, "m"));
    let Pull::Chunk(c) = e.pull(&mut s, w.epoch, None, t, ctx()) else {
        panic!()
    };
    w.accept(&c);
    assert!(!c.last, "交到一半");
    let epoch = e.attach(&mut s);
    let mut fresh = Web::new(epoch);
    // 旧代的块迟到：新网页按代次丢掉。
    assert!(!fresh.accept(&c));
    drain(&mut s, &mut e, &mut fresh, t + FRAME_INTERVAL);
    assert_eq!(fresh.row_keys(), ["m"]);
    assert!(fresh.state.sidebar.is_some() && fresh.state.connection.is_some());
}

/// 每种 patch 都能被切开再拼回（ViewChange 的变体列表驱动；加了变体而这里没覆盖时失败）。
#[test]
fn every_patch_kind_survives_chunking() {
    let fds = <prost_types::FileDescriptorSet as prost::Message>::decode(
        iwork_proto::FILE_DESCRIPTOR_SET,
    )
    .unwrap();
    let variants: Vec<String> = fds
        .file
        .iter()
        .flat_map(|f| f.message_type.iter())
        .find(|m| m.name() == "ViewChange")
        .unwrap()
        .field
        .iter()
        .map(|f| f.name().to_owned())
        .collect();
    let (mut s, mut e, mut w, t) = setup(64);
    let mut v = Visible {
        sidebar: true,
        conversation_id: Some(CID.into()),
        pending: true,
        settings: true,
        assistant_id: Some("a1".into()),
        add_project: true,
        conversation_list: Some(iwork_client_core::store::ListQuery {
            project_id: PID.into(),
            filter: String::new(),
        }),
        home: Some(String::new()),
    };
    s.set_visible(v.clone());
    // 助手需要在摘要里。
    s.apply(snapshot(
        SIDEBAR,
        5,
        vec![pb::QueryResult {
            seq: 5,
            kind: Some(pb::query_result::Kind::ListAssistants(
                pb::ListAssistantsResult {
                    assistants: vec![pb::Assistant {
                        id: "a1".into(),
                        config: Some(pb::AssistantConfig {
                            name: "小林".into(),
                            prompt: "x".repeat(500),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    activities: vec![],
                },
            )),
        }],
    ));
    s.apply(msg_event(10, "m"));
    s.request_navigate(CID.into(), "m".into());
    v.conversation_list = v.conversation_list.clone();
    drain(&mut s, &mut e, &mut w, t);
    let st = &w.state;
    let got = [
        ("sidebar", st.sidebar.is_some()),
        ("conversation", st.conversation_id.is_some()),
        ("pending", st.pending.is_some()),
        ("settings", st.settings.is_some()),
        ("assistant", st.assistant.is_some()),
        ("add_project", st.add_project.is_some()),
        ("connection", st.connection.is_some()),
        ("conversation_list", st.list.is_some()),
        ("navigate", st.navigate.is_some()),
        ("home", st.home.is_some()),
    ];
    for name in &variants {
        let hit = got
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("ViewChange 新变体 {name} 没有覆盖"));
        assert!(hit.1, "{name} 拼回后应当存在");
    }
    assert!(e.handed_bytes() > 64 * 3, "确实被切成多块");
}

/// 2 MiB 助手提示词：切块逐块交，交到一半再改一次，最终等于最后的值。
#[test]
fn oversized_assistant_model_converges() {
    let (mut s, mut e, mut w, t) = setup(256 * 1024);
    let mut v = s.visible.clone();
    v.assistant_id = Some("a1".into());
    s.set_visible(v);
    let set_prompt = |s: &mut Store, seq: u64, prompt: String| {
        s.apply(view_result(
            iwork_client_core::store::QueryPurpose::Summary {
                round: 0,
                table: iwork_client_core::store::SummaryTable::Assistants,
            },
            pb::QueryResult {
                seq,
                kind: Some(pb::query_result::Kind::ListAssistants(
                    pb::ListAssistantsResult {
                        assistants: vec![pb::Assistant {
                            id: "a1".into(),
                            config: Some(pb::AssistantConfig {
                                name: "x".into(),
                                prompt,
                                ..Default::default()
                            }),
                            ..Default::default()
                        }],
                        activities: vec![],
                    },
                )),
            },
        ));
    };
    set_prompt(&mut s, 5, "a".repeat(2 << 20));
    let Pull::Chunk(c) = e.pull(&mut s, w.epoch, None, t, ctx()) else {
        panic!()
    };
    w.accept(&c);
    assert!(e.held_bytes() >= 2 << 20);
    set_prompt(&mut s, 6, "b".repeat(2 << 20));
    drain(&mut s, &mut e, &mut w, t + FRAME_INTERVAL);
    assert_eq!(
        w.state.assistant.as_ref().unwrap().prompt,
        "b".repeat(2 << 20)
    );
}

/// 审查 1/F-10：跳转只在请求时发一次；之后的 SetVisible、attach 都不重发。
#[test]
fn navigate_is_sent_once() {
    let (mut s, mut e, mut w, t) = setup(1 << 20);
    drain(&mut s, &mut e, &mut w, t);
    s.request_navigate(CID.into(), "m".into());
    drain(&mut s, &mut e, &mut w, t + FRAME_INTERVAL);
    assert_eq!(w.state.navigate.as_ref().map(|n| n.seq), Some(1));
    w.state.navigate = None;
    let v = s.visible.clone();
    s.set_visible(Visible { pending: true, ..v });
    drain(&mut s, &mut e, &mut w, t + FRAME_INTERVAL * 2);
    assert!(w.state.navigate.is_none(), "SetVisible 不重发");
    let mut fresh = Web::new(e.attach(&mut s));
    drain(&mut s, &mut e, &mut fresh, t + FRAME_INTERVAL * 3);
    assert!(fresh.state.navigate.is_none(), "attach 不重发");
}
