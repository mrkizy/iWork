//! 属性测试 P2、P3（不连后台）与行登记表对拍、P1（连假后台）。方案第 7 节。

mod support;

use std::time::{Duration, Instant, SystemTime};

use iwork_client_core::derive::Ctx;
use iwork_client_core::frame::{FRAME_INTERVAL, FrameEngine, Pull};
use iwork_client_core::session::reconnect::ReconnectState;
use iwork_client_core::store::{Connection, Store, Visible};
use iwork_proto::pb;
use pb::domain_event::Event as E;
use proptest::prelude::*;
use support::web::Web;
use support::*;

fn ctx() -> Ctx {
    Ctx {
        now: SystemTime::UNIX_EPOCH + Duration::from_secs(1_762_100_000),
    }
}

// ---------- P2 patches_compose_to_full ----------

#[derive(Debug, Clone)]
enum Op {
    NewMessage,
    Delta(usize),
    BigPrompt(usize),
    Settings(bool),
    Window(i64),
    /// 取一块；`lose` 时回包丢了（网页没收到）。
    Pull {
        lose: bool,
    },
    /// 网页重新挂上；带着上一代最后一块回包迟到。
    Reattach,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::NewMessage),
        4 => (1usize..200).prop_map(Op::Delta),
        1 => (0usize..3000).prop_map(Op::BigPrompt),
        1 => any::<bool>().prop_map(Op::Settings),
        1 => (0i64..400).prop_map(Op::Window),
        8 => any::<bool>().prop_map(|lose| Op::Pull { lose }),
        1 => Just(Op::Reattach),
    ]
}

fn assistants(seq: u64, prompt_len: usize) -> iwork_client_core::store::Item {
    view_result(
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
                            prompt: "p".repeat(prompt_len),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    activities: vec![],
                },
            )),
        },
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// 任意事件 × 任意取帧时刻 × 回包丢失与重复取 × 任意 attach × 64 字节分块 × 超大字段：
    /// 网页接受的块拼回、折叠，等于终态一次全量；旧代回包迟到不改新代状态；任何时刻 Rust 只留一帧。
    #[test]
    fn patches_compose_to_full(ops in proptest::collection::vec(op(), 1..80)) {
        let mut s = store_with_open();
        s.set_connection(Connection::Live);
        let mut v = s.visible.clone();
        v.settings = true;
        v.assistant_id = Some("a1".into());
        s.set_visible(v);
        s.apply(assistants(1, 10));
        let mut e = FrameEngine::new(64);
        let mut w = Web::new(e.attach(&mut s));
        let mut t = Instant::now();
        let mut seq = 10u64;
        let mut msgs: Vec<(String, u64)> = Vec::new();
        let mut last_chunk: Option<pb::FrameChunk> = None;
        for op in ops {
            seq += 1;
            match op {
                Op::NewMessage => {
                    let id = format!("m{seq}");
                    s.apply(persisted(seq, &[CONV], appended(assistant_msg(&id, 0, pb::MessageStatus::Streaming, ""))));
                    msgs.push((id, 0));
                }
                Op::Delta(n) => {
                    if let Some((id, len)) = msgs.last_mut() {
                        let text = "字".repeat(n % 7 + 1);
                        s.apply(delta(&[CONV], id, *len, &text));
                        *len += text.len() as u64;
                    }
                }
                Op::BigPrompt(n) => { s.apply(assistants(seq, n)); }
                Op::Settings(on) => {
                    s.apply(persisted(seq, &[SIDEBAR], E::GlobalSettingsChanged(pb::GlobalSettingsChanged { playful_ui: Some(on), ..Default::default() })));
                }
                Op::Window(first) => s.set_window(CID, first, first + 40),
                Op::Pull { lose } => {
                    t += FRAME_INTERVAL;
                    if let Pull::Chunk(c) = e.pull(&mut s, w.epoch, w.applied, t, ctx()) {
                        prop_assert!(c.data.len() <= 64);
                        if !lose { w.accept(&c); }
                        last_chunk = Some(c);
                    }
                }
                Op::Reattach => {
                    let epoch = e.attach(&mut s);
                    let before = w.state.clone();
                    let mut fresh = Web::new(epoch);
                    if let Some(old) = &last_chunk {
                        prop_assert!(!fresh.accept(old), "旧代回包不进新代");
                    }
                    prop_assert_eq!(&fresh.state, &Default::default());
                    let _ = before;
                    w = fresh;
                }
            }
        }
        // 收尾：取到没有可交的为止。
        loop {
            t += FRAME_INTERVAL;
            match e.pull(&mut s, w.epoch, w.applied, t, ctx()) {
                Pull::Chunk(c) => { w.accept(&c); }
                _ => break,
            }
        }
        let incremental = w.state.clone();
        let mut full = Web::new(e.attach(&mut s));
        loop {
            t += FRAME_INTERVAL;
            match e.pull(&mut s, full.epoch, full.applied, t, ctx()) {
                Pull::Chunk(c) => { full.accept(&c); }
                _ => break,
            }
        }
        prop_assert_eq!(incremental, full.state);
    }

    /// P3：from_seq 非 0 ⇒ 全部视图 Ready、游标非 0、同游标被关不到两次。
    #[test]
    fn from_seq_nonzero_implies_all_ready(ops in proptest::collection::vec((0u8..3, 0u64..4, any::<bool>()), 0..40)) {
        let mut r = ReconnectState::default();
        for (kind, cursor, loading) in ops {
            match kind {
                0 => r.on_closed(cursor),
                1 => r.on_applied_seq(),
                _ => {}
            }
            let from = r.from_seq(cursor, loading);
            if from != 0 {
                prop_assert!(!loading);
                prop_assert_eq!(from, cursor);
                prop_assert!(r.same_cursor_closes() < 2);
            }
        }
    }
}

/// 网页窗口外的行不发：窗口移走时网页删掉旧行（帧差量的对照）。
#[test]
fn window_moves_remove_rows() {
    let mut s = store_with_open();
    s.set_connection(Connection::Live);
    for i in 0..200u64 {
        s.apply(persisted(
            10 + i,
            &[CONV],
            appended(assistant_msg(
                &format!("m{i}"),
                0,
                pb::MessageStatus::Completed,
                "x",
            )),
        ));
    }
    let mut e = FrameEngine::new(1 << 20);
    let mut w = Web::new(e.attach(&mut s));
    let t = Instant::now();
    let drain = |s: &mut Store, e: &mut FrameEngine, w: &mut Web, t: Instant| {
        while let Pull::Chunk(c) = e.pull(s, w.epoch, w.applied, t, ctx()) {
            w.accept(&c);
        }
    };
    drain(&mut s, &mut e, &mut w, t);
    assert_eq!(w.state.rows.len(), 60, "没声明窗口时给最新 60 行");
    // 窗口移到最前：发前面的行，删掉离开窗口的。
    let first = s.open.as_ref().unwrap().identities()[0].0.ordinal();
    s.set_window(CID, first, first + 20);
    drain(&mut s, &mut e, &mut w, t + FRAME_INTERVAL);
    assert_eq!(w.state.rows.len(), 11 + 30, "窗口 11 行 + 后面多带 30 行");
    assert!(w.state.rows.contains_key("m0"));
    assert!(!w.state.rows.contains_key("m199"));
    let _: Visible = s.visible.clone();
}

// ---------- 连假后台的两条 ----------

// 假后台只在 Unix 上监听；M1 暂不支持 Windows。
#[cfg(unix)]
mod live {
    use super::*;
    use iwork_client_core::{App, DirectDialer, Options};
    use iwork_fake_daemon::{FakeDaemon, fixtures as f, self_trusting_identity};
    use pb::ui_intent::Kind as I;
    use std::collections::BTreeSet;
    use support::web::WebState;
    use tokio::time::timeout;

    const WAIT: Duration = Duration::from_secs(20);

    async fn connect(d: &FakeDaemon, conv: Option<&str>) -> (App, Web) {
        let dialer = DirectDialer::new(d.endpoint(), self_trusting_identity().unwrap(), "test");
        let (app, _effects) = App::start(dialer, Options { chunk_max: 4096 });
        std::mem::forget(_effects);
        let web = Web::new(app.attach().await.unwrap());
        set_conv(&app, conv).await;
        (app, web)
    }

    async fn set_conv(app: &App, conv: Option<&str>) {
        app.intent(pb::UiIntent {
            kind: Some(I::SetVisible(pb::SetVisible {
                sidebar: true,
                conversation_id: conv.map(Into::into),
                ..Default::default()
            })),
        })
        .await;
    }

    fn is_live(s: &WebState) -> bool {
        s.connection
            .as_ref()
            .is_some_and(|c| c.state == pb::ConnectionState::Live as i32)
    }

    async fn settle(app: &App, web: &mut Web) {
        let r = timeout(WAIT, async {
            loop {
                let before = web.state.clone();
                let _ = timeout(Duration::from_millis(300), async {
                    loop {
                        if let Ok(c) = app.next_frame(web.request()).await {
                            web.accept(&c);
                        }
                    }
                })
                .await;
                if is_live(&web.state) && web.state == before {
                    return;
                }
            }
        })
        .await;
        assert!(r.is_ok(), "界面没稳定下来");
    }

    fn keys_without_notes(w: &Web) -> Vec<String> {
        w.row_keys()
            .into_iter()
            .filter(|k| !k.starts_with("note:"))
            .collect()
    }

    fn kinds(w: &Web) -> BTreeSet<&'static str> {
        use pb::row_view::Row as R;
        w.state
            .rows
            .values()
            .filter_map(|r| {
                Some(match r.row.as_ref()? {
                    R::Message(_) => "message",
                    R::Tool(_) => "tool",
                    R::Approval(_) => "approval",
                    R::UserAction(_) => "user_action",
                    R::TurnEnd(_) => "turn_end",
                    R::Resume(_) => "resume",
                    R::ModelSwitch(_) => "model_switch",
                    R::Divider(_) => "divider",
                    R::Note(_) => "note",
                })
            })
            .collect()
    }

    /// 行登记表对拍：实时收完的行键顺序 == 关掉重开（走快照）后的顺序，当次说明除外；场景里每种行至少一次。
    #[tokio::test(flavor = "multi_thread")]
    async fn row_order_live_equals_rebuilt() {
        let cases: &[(&str, &str)] = &[
            ("conversation-basic", "c-basic"),
            ("approval-states", "c-approval"),
            ("user-action-states", "c-user-action"),
            ("turn-end", "c-turn-end"),
            ("resume-card", "c-resume"),
            ("", "c-kinds"),
        ];
        let mut seen = BTreeSet::new();
        for (scenario, conv) in cases {
            let dir = tempfile::Builder::new()
                .prefix("t09")
                .tempdir_in("/tmp")
                .unwrap();
            let d = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
            d.run_scenario("empty").await.unwrap();
            // 先把对话建出来并打开，再让场景的事件实时到达。
            let (app, mut web) = connect(&d, Some(conv)).await;
            // 等订阅完成再追加，事件才是实时到达（否则会被折进快照）。
            settle(&app, &mut web).await;
            if scenario.is_empty() {
                kinds_scenario(&d, conv);
            } else {
                d.run_scenario(scenario).await.unwrap();
            }
            settle(&app, &mut web).await;
            let live_keys = keys_without_notes(&web);
            seen.extend(kinds(&web));
            set_conv(&app, None).await;
            settle(&app, &mut web).await;
            set_conv(&app, Some(conv)).await;
            settle(&app, &mut web).await;
            assert_eq!(
                keys_without_notes(&web),
                live_keys,
                "{scenario} 实时与重建的行序不同"
            );
        }
        let fds = <prost_types::FileDescriptorSet as prost::Message>::decode(
            iwork_proto::FILE_DESCRIPTOR_SET,
        )
        .unwrap();
        let variants: BTreeSet<String> = fds
            .file
            .iter()
            .flat_map(|f| f.message_type.iter())
            .find(|m| m.name() == "RowView")
            .unwrap()
            .field
            .iter()
            .filter(|f| f.oneof_index.is_some())
            .map(|f| f.name().to_owned())
            .collect();
        for v in &variants {
            assert!(seen.contains(v.as_str()), "行登记表的 {v} 没有场景覆盖");
        }
    }

    /// 模型切换、项目分隔线、当次说明：场景库里没有，这里直接追加（对话 c-kinds 在空世界上新建）。
    fn kinds_scenario(d: &FakeDaemon, conv: &str) {
        d.append(f::dm(conv, f::IWORK));
        let ev = f::ev(E::ApprovalModeChanged(pb::ApprovalModeChanged {
            target: Some(pb::ApprovalModeTarget {
                kind: Some(pb::approval_mode_target::Kind::ConversationId(conv.into())),
            }),
            mode: Some(pb::ApprovalMode::Auto as i32),
            cause: pb::ApprovalModeChangeCause::User as i32,
        }));
        d.append(ev);
        d.append(f::user_message("k-1", conv, "第一条"));
        d.append(f::turn_queued("kt-1", conv, f::IWORK));
        let mut second = f::turn_queued("kt-2", conv, f::IWORK);
        if let Some(E::TurnQueued(t)) = second.event.as_mut() {
            t.turn.as_mut().unwrap().model = Some(pb::ModelSelection {
                model_id: "other".into(),
                effort: None,
            });
        }
        d.append(second);
        let mut in_project = f::user_message("k-2", conv, "在项目里说");
        if let Some(E::MessageAppended(m)) = in_project.event.as_mut() {
            m.message.as_mut().unwrap().context_project_id =
                Some(iwork_fake_daemon::scenario::PROJECT.into());
        }
        d.append(in_project);
        d.append(f::ev(E::ApprovalModeChanged(pb::ApprovalModeChanged {
            target: Some(pb::ApprovalModeTarget {
                kind: Some(pb::approval_mode_target::Kind::ConversationId(conv.into())),
            }),
            mode: Some(pb::ApprovalMode::Auto as i32),
            cause: pb::ApprovalModeChangeCause::User as i32,
        })));
    }

    /// P1：随机事件 + 随机断线点（含连续断、断在订阅途中）+ 打开 / 关闭对话；收敛后的界面 == 新客户端看到的。
    #[tokio::test(flavor = "multi_thread")]
    async fn reconnect_equals_fresh() {
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 12,
            ..ProptestConfig::default()
        });
        let strategy = proptest::collection::vec(0u8..5, 5..40);
        let rt = tokio::runtime::Handle::current();
        runner
            .run(&strategy, |ops| {
                let r: Result<(), TestCaseError> =
                    tokio::task::block_in_place(|| rt.block_on(case(ops)));
                r
            })
            .unwrap();
    }

    async fn case(ops: Vec<u8>) -> Result<(), TestCaseError> {
        let dir = tempfile::Builder::new()
            .prefix("t09")
            .tempdir_in("/tmp")
            .unwrap();
        let d = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
        d.run_scenario("conversation-basic").await.unwrap();
        // 一条老的流式消息，后面跟着 60 条：它在最新页之外。
        d.append(f::assistant_message(
            "old",
            "c-basic",
            "t-basic",
            f::IWORK,
            "iWork",
        ));
        for i in 0..60 {
            d.append(f::user_message(&format!("pad-{i}"), "c-basic", "填充"));
        }
        let (app, mut web) = connect(&d, Some("c-basic")).await;
        let mut open = true;
        for (i, op) in ops.into_iter().enumerate() {
            match op {
                0 => {
                    d.append(f::user_message(&format!("n-{i}"), "c-basic", "新消息"));
                }
                1 => d.disconnect_all().await,
                2 => {
                    // 断线期间老消息完成（不在最新页）。
                    d.disconnect_all().await;
                    d.append(f::message_completed("c-basic", "old", "老消息完成了"));
                }
                3 => {
                    open = !open;
                    set_conv(&app, open.then_some("c-basic")).await;
                }
                _ => {
                    let _ = timeout(Duration::from_millis(20), app.next_frame(web.request()))
                        .await
                        .map(|r| r.map(|c| web.accept(&c)));
                }
            }
        }
        set_conv(&app, Some("c-basic")).await;
        settle(&app, &mut web).await;
        let (fresh_app, mut fresh) = connect(&d, Some("c-basic")).await;
        settle(&fresh_app, &mut fresh).await;
        prop_assert_eq!(&web.state.rows, &fresh.state.rows);
        prop_assert_eq!(&web.state.sidebar, &fresh.state.sidebar);
        Ok(())
    }
}
