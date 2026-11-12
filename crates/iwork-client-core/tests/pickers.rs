//! 系统文件选择框与拖入（T09 组 2b；ui-m1.md 第 2 节第 6 条、2a 第 7 条）。连假后台走真协议。
//!
//! 网页只说「要选附件」「要重新定位」，不带路径；client-core 请壳弹框（`Effect::PickPaths`），壳把选中的
//! 路径交回（`ShellEvent::Picked`），client-core 再发 `UploadAttachment` 或 `RelocateProject`。网页那次操作的
//! 受理结果等到这一步做完才回，出错照常显示在提示里。

#![cfg(unix)]

mod support;

use std::time::Duration;

use iwork_client_core::{App, DirectDialer, Effect, Options, PickKind, ShellEvent};
use iwork_fake_daemon::{FakeDaemon, self_trusting_identity};
use iwork_proto::pb;
use pb::ui_intent::Kind as I;
use support::web::{Web, WebState};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);
const CONV: &str = "c-basic";
const BLOCKED_CONV: &str = "c-block";
const PROJECT: &str = "p-demo";

struct H {
    daemon: FakeDaemon,
    app: App,
    effects: mpsc::UnboundedReceiver<Effect>,
    web: Web,
    dir: tempfile::TempDir,
}

async fn start(scenario: &str, conversation: &str) -> H {
    let dir = tempfile::Builder::new()
        .prefix("t09-pick")
        .tempdir_in("/tmp")
        .unwrap();
    let daemon = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
    daemon.run_scenario(scenario).await.unwrap();
    let dialer = DirectDialer::new(daemon.endpoint(), self_trusting_identity().unwrap(), "test");
    let (app, effects) = App::start(dialer, Options::default());
    let web = Web::new(app.attach().await.unwrap());
    let mut h = H {
        daemon,
        app,
        effects,
        web,
        dir,
    };
    let ack = h
        .app
        .intent(pb::UiIntent {
            kind: Some(I::SetVisible(pb::SetVisible {
                sidebar: true,
                conversation_id: Some(conversation.into()),
                ..Default::default()
            })),
        })
        .await;
    assert_eq!(ack.error, None);
    until(&mut h, "连上并显示出对话", |s| {
        s.composer.is_some()
            && s.sidebar.is_some()
            && s.connection.as_ref().is_some_and(|c| c.commands_enabled)
    })
    .await;
    h
}

async fn until(h: &mut H, what: &str, cond: impl Fn(&WebState) -> bool) {
    let r = timeout(WAIT, async {
        while !cond(&h.web.state) {
            if let Ok(Ok(c)) = timeout(
                Duration::from_millis(300),
                h.app.next_frame(h.web.request()),
            )
            .await
            {
                h.web.accept(&c);
            }
        }
    })
    .await;
    assert!(r.is_ok(), "等不到：{what}");
}

/// 等壳要做的「弹选择框」效果（别的效果照收不误、跳过）。
async fn pick_effect(rx: &mut mpsc::UnboundedReceiver<Effect>) -> PickKind {
    timeout(WAIT, async {
        loop {
            if let Effect::PickPaths(k) = rx.recv().await.expect("效果通道关了") {
                return k;
            }
        }
    })
    .await
    .expect("等不到弹选择框的效果")
}

/// 发一个操作但不等受理结果（选择框开着时它一直挂着）。
fn act(app: &App, kind: I) -> tokio::task::JoinHandle<pb::IntentAck> {
    let app = app.clone();
    tokio::spawn(async move { app.intent(pb::UiIntent { kind: Some(kind) }).await })
}

fn tray_names(s: &WebState) -> Vec<String> {
    s.composer
        .as_ref()
        .map(|c| c.tray.iter().map(|a| a.name.clone()).collect())
        .unwrap_or_default()
}

fn project_missing(s: &WebState) -> Option<bool> {
    s.sidebar
        .as_ref()?
        .projects
        .iter()
        .find(|p| p.project_id == PROJECT)
        .map(|p| p.path_missing)
}

fn file(h: &H, name: &str) -> String {
    let p = h.dir.path().join(name);
    std::fs::write(&p, b"hello").unwrap();
    p.to_string_lossy().into_owned()
}

fn pick_attachments() -> I {
    I::PickAttachments(pb::PickAttachments {
        conversation_id: CONV.into(),
    })
}

/// 回形针：弹的是选文件的框；选好的文件随后出现在输入框上方的托盘里，受理结果这时才回。
#[tokio::test(flavor = "multi_thread")]
async fn picked_files_land_in_tray() {
    let mut h = start("conversation-basic", CONV).await;
    assert!(
        tray_names(&h.web.state).is_empty(),
        "对照：开始时托盘是空的"
    );
    let ack = act(&h.app, pick_attachments());
    assert_eq!(pick_effect(&mut h.effects).await, PickKind::Files);
    assert!(!ack.is_finished(), "选择框还开着，受理结果不该先回");
    let path = file(&h, "notes.txt");
    h.app.shell(ShellEvent::Picked(vec![path]));
    let ack = timeout(WAIT, ack).await.unwrap().unwrap();
    assert_eq!(ack.error, None);
    until(&mut h, "托盘出现选中的文件", |s| {
        tray_names(s) == ["notes.txt"]
    })
    .await;
}

/// 用户取消选择：受理结果照常回、不带错误，什么都不发；之后还能再开一次。
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_pick_sends_nothing_and_allows_next() {
    let mut h = start("conversation-basic", CONV).await;
    let ack = act(&h.app, pick_attachments());
    pick_effect(&mut h.effects).await;
    h.app.shell(ShellEvent::Picked(Vec::new()));
    assert_eq!(timeout(WAIT, ack).await.unwrap().unwrap().error, None);

    let again = act(&h.app, pick_attachments());
    pick_effect(&mut h.effects).await;
    h.app.shell(ShellEvent::Picked(vec![file(&h, "b.txt")]));
    assert_eq!(timeout(WAIT, again).await.unwrap().unwrap().error, None);
    until(&mut h, "第二次选的文件进了托盘", |s| {
        tray_names(s) == ["b.txt"]
    })
    .await;
}

/// 同一时刻只开一个选择框：开着时再点被拒，并点名原因；原来那个不受影响。
#[tokio::test(flavor = "multi_thread")]
async fn second_pick_while_open_is_rejected() {
    let mut h = start("conversation-basic", CONV).await;
    let first = act(&h.app, pick_attachments());
    pick_effect(&mut h.effects).await;
    let second = h
        .app
        .intent(pb::UiIntent {
            kind: Some(I::PickProjectLocation(pb::ProjectIntent {
                project_id: PROJECT.into(),
            })),
        })
        .await;
    assert_eq!(
        second.error.map(|e| e.code).as_deref(),
        Some("picker_open"),
        "选择框开着时第二次被拒"
    );
    h.app.shell(ShellEvent::Picked(vec![file(&h, "a.txt")]));
    assert_eq!(timeout(WAIT, first).await.unwrap().unwrap().error, None);
    until(&mut h, "第一次选的照常进托盘", |s| {
        tray_names(s) == ["a.txt"]
    })
    .await;
}

/// 选中的文件超过 20MB：整批不发，受理结果带上点名文件的错误（网页按提示显示）。
#[tokio::test(flavor = "multi_thread")]
async fn oversized_pick_is_rejected_with_name() {
    let mut h = start("conversation-basic", CONV).await;
    let ack = act(&h.app, pick_attachments());
    pick_effect(&mut h.effects).await;
    let big = h.dir.path().join("big.bin");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(21 * 1024 * 1024)
        .unwrap();
    h.app
        .shell(ShellEvent::Picked(vec![big.to_string_lossy().into_owned()]));
    let e = timeout(WAIT, ack)
        .await
        .unwrap()
        .unwrap()
        .error
        .expect("该被拒");
    assert_eq!(e.code, "attachment_too_large");
    assert!(e.message.contains("big.bin"), "{}", e.message);
}

/// 「重新定位」：弹的是选文件夹的框；选好后发 RelocateProject，侧栏的项目不再「找不到路径」。
#[tokio::test(flavor = "multi_thread")]
async fn picked_folder_relocates_project() {
    let mut h = start("composer-path-missing", BLOCKED_CONV).await;
    until(&mut h, "项目显示找不到路径", |s| {
        project_missing(s) == Some(true)
    })
    .await;
    let ack = act(
        &h.app,
        I::PickProjectLocation(pb::ProjectIntent {
            project_id: PROJECT.into(),
        }),
    );
    assert_eq!(pick_effect(&mut h.effects).await, PickKind::Folder);
    let dir = h.dir.path().to_string_lossy().into_owned();
    h.app.shell(ShellEvent::Picked(vec![dir]));
    assert_eq!(timeout(WAIT, ack).await.unwrap().unwrap().error, None);
    until(&mut h, "重新定位后项目恢复", |s| {
        project_missing(s) == Some(false)
    })
    .await;
}

/// 连接类整页的「重新打开 iWork」：意图直接变成壳效果 Relaunch（只重启客户端，不发任何命令给后台）。
#[tokio::test(flavor = "multi_thread")]
async fn relaunch_intent_becomes_shell_effect() {
    let mut h = start("conversation-basic", CONV).await;
    let ack = act(&h.app, I::Relaunch(pb::RelaunchIntent {}));
    assert_eq!(timeout(WAIT, ack).await.unwrap().unwrap().error, None);
    let e = timeout(WAIT, async {
        loop {
            match h.effects.recv().await {
                Some(Effect::Relaunch) => return Effect::Relaunch,
                Some(_) => continue,
                None => panic!("效果通道关了"),
            }
        }
    })
    .await
    .expect("等不到 Relaunch 效果");
    assert_eq!(e, Effect::Relaunch);
}

/// 设置 › 项目位置的「更换」：同样由 Rust 弹框，选好后设成新建项目的默认父目录。
#[tokio::test(flavor = "multi_thread")]
async fn picked_folder_sets_projects_root() {
    let mut h = start("conversation-basic", CONV).await;
    let ack = h
        .app
        .intent(pb::UiIntent {
            kind: Some(I::SetVisible(pb::SetVisible {
                sidebar: true,
                settings: true,
                conversation_id: Some(CONV.into()),
                ..Default::default()
            })),
        })
        .await;
    assert_eq!(ack.error, None);
    let ack = act(&h.app, I::PickProjectsRoot(pb::PickProjectsRoot {}));
    assert_eq!(pick_effect(&mut h.effects).await, PickKind::Folder);
    let dir = h.dir.path().to_string_lossy().into_owned();
    h.app.shell(ShellEvent::Picked(vec![dir.clone()]));
    assert_eq!(timeout(WAIT, ack).await.unwrap().unwrap().error, None);
    until(&mut h, "设置里的项目位置换成选的目录", |s| {
        s.settings.as_ref().is_some_and(|v| v.projects_root == dir)
    })
    .await;
}

/// 后台拒了重新定位（这里：项目不存在）：受理结果带上后台的错误。
#[tokio::test(flavor = "multi_thread")]
async fn relocate_rejected_by_daemon_reports_error() {
    let mut h = start("conversation-basic", CONV).await;
    let ack = act(
        &h.app,
        I::PickProjectLocation(pb::ProjectIntent {
            project_id: "p-gone".into(),
        }),
    );
    pick_effect(&mut h.effects).await;
    h.app.shell(ShellEvent::Picked(vec![
        h.dir.path().to_string_lossy().into_owned(),
    ]));
    let ack = timeout(WAIT, ack).await.unwrap().unwrap();
    assert!(ack.error.is_some(), "后台拒了要带错误");
}

fn drop_hint(s: &WebState) -> Option<&str> {
    s.header.as_ref()?.drop_hint.as_deref()
}

/// 拖文件进窗口：对话区出现「松开以添加」；拖出去就收起。
#[tokio::test(flavor = "multi_thread")]
async fn drag_enter_and_leave_toggle_drop_hint() {
    let mut h = start("conversation-basic", CONV).await;
    assert_eq!(drop_hint(&h.web.state), None, "对照：没在拖时没有虚线框");
    h.app.shell(ShellEvent::Dragging(true));
    until(&mut h, "拖进来出现提示", |s| {
        drop_hint(s) == Some("松开以添加")
    })
    .await;
    h.app.shell(ShellEvent::Dragging(false));
    until(&mut h, "拖出去提示收起", |s| drop_hint(s).is_none()).await;
}

/// 松开：提示收起，文件附到当前对话。
#[tokio::test(flavor = "multi_thread")]
async fn drop_clears_hint_and_attaches() {
    let mut h = start("conversation-basic", CONV).await;
    h.app.shell(ShellEvent::Dragging(true));
    until(&mut h, "拖进来出现提示", |s| drop_hint(s).is_some()).await;
    let path = file(&h, "dropped.txt");
    h.app.shell(ShellEvent::Dropped(vec![path]));
    until(&mut h, "提示收起、文件进托盘", |s| {
        drop_hint(s).is_none() && tray_names(s) == ["dropped.txt"]
    })
    .await;
}

/// 断线时点回形针：不弹框（选完也发不出去），直接回 offline。
#[tokio::test(flavor = "multi_thread")]
async fn pick_while_offline_opens_nothing() {
    let mut h = start("conversation-basic", CONV).await;
    h.daemon.stop_listening().await;
    h.daemon.disconnect_all().await;
    until(&mut h, "断线后命令不可点", |s| {
        s.connection.as_ref().is_some_and(|c| !c.commands_enabled)
    })
    .await;
    let ack = h
        .app
        .intent(pb::UiIntent {
            kind: Some(pick_attachments()),
        })
        .await;
    assert_eq!(ack.error.map(|e| e.code).as_deref(), Some("offline"));
    assert_eq!(
        attach_unavailable(&h.web.state),
        Some("正在重新连接，连上后再试"),
        "回形针按 Rust 给的原因置灰"
    );
    while let Ok(e) = h.effects.try_recv() {
        assert!(!matches!(e, Effect::PickPaths(_)), "断线时不该弹框");
    }
}

fn attach_unavailable(s: &WebState) -> Option<&str> {
    s.composer.as_ref()?.attach_unavailable.as_deref()
}

/// 回形针能不能点由 Rust 给：对话里连着能点；首页还没有对话，给出原因。
#[tokio::test(flavor = "multi_thread")]
async fn paperclip_availability_comes_from_rust() {
    let mut h = start("conversation-basic", CONV).await;
    assert_eq!(attach_unavailable(&h.web.state), None, "对话里能点");
    let ack = h
        .app
        .intent(pb::UiIntent {
            kind: Some(I::SetVisible(pb::SetVisible {
                sidebar: true,
                home: Some(pb::HomeQuery {
                    project_id: PROJECT.into(),
                }),
                ..Default::default()
            })),
        })
        .await;
    assert_eq!(ack.error, None);
    until(&mut h, "首页的回形针给出原因", |s| {
        s.home
            .as_ref()
            .and_then(|v| v.composer.as_ref())
            .and_then(|c| c.attach_unavailable.as_deref())
            == Some("发出第一条消息后可以添加附件")
    })
    .await;
}
