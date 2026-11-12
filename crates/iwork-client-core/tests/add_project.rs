//! 添加项目（T09 方案 15.2、15.7）：连假后台走真协议。网页的意图都不带路径；路径从选择框（`ShellEvent::Picked`）、
//! 拖入、`CloneFinished` 进来，记在 Rust 的草稿里。假后台不碰盘：目录照它的假「盘」（`Tables::disk`）判。

#![cfg(unix)]

mod support;

use std::time::Duration;

use iwork_client_core::{App, DirectDialer, Effect, Options, PickKind, ShellEvent};
use iwork_fake_daemon::tables::FakeDir;
use iwork_fake_daemon::{FakeDaemon, self_trusting_identity};
use iwork_proto::pb;
use pb::ui_intent::Kind as I;
use support::web::{Web, WebState};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);
const HOME: &str = "/Users/me";

struct H {
    daemon: FakeDaemon,
    app: App,
    effects: mpsc::UnboundedReceiver<Effect>,
    web: Web,
    _dir: tempfile::TempDir,
}

async fn start() -> H {
    let dir = tempfile::Builder::new()
        .prefix("t09-add")
        .tempdir_in("/tmp")
        .unwrap();
    let daemon = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
    daemon.run_scenario("empty").await.unwrap();
    let dialer = DirectDialer::new(daemon.endpoint(), self_trusting_identity().unwrap(), "test");
    let (app, effects) = App::start(
        dialer,
        Options {
            home_dir: Some(HOME.into()),
            ..Options::default()
        },
    );
    let web = Web::new(app.attach().await.unwrap());
    let mut h = H {
        daemon,
        app,
        effects,
        web,
        _dir: dir,
    };
    let ack = h
        .app
        .intent(pb::UiIntent {
            kind: Some(I::SetVisible(pb::SetVisible {
                sidebar: true,
                settings: true,
                add_project: true,
                ..Default::default()
            })),
        })
        .await;
    assert_eq!(ack.error, None);
    until(&mut h, "连上", |s| {
        s.connection.as_ref().is_some_and(|c| c.commands_enabled) && s.add_project.is_some()
    })
    .await;
    h
}

/// 往假「盘」上登记一个目录。
fn disk(h: &H, path: &str, dir: FakeDir) {
    let (path, dir) = (path.to_owned(), dir);
    h.daemon.mutate_tables(move |t| {
        t.disk.insert(path, dir);
    });
}

fn dir(root: &str, versioned: bool, agents: bool) -> FakeDir {
    FakeDir {
        root_path: root.into(),
        versioned,
        has_agents_md: agents,
    }
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

async fn act(h: &H, kind: I) -> pb::IntentAck {
    h.app.intent(pb::UiIntent { kind: Some(kind) }).await
}

/// 网页点「打开本机文件夹」，壳弹框，用户选了 `path`。
async fn pick(h: &mut H, path: &str) {
    let app = h.app.clone();
    let ack = tokio::spawn(async move {
        app.intent(pb::UiIntent {
            kind: Some(I::PickProjectFolder(pb::PickProjectFolder {})),
        })
        .await
    });
    let kind = timeout(WAIT, async {
        loop {
            if let Effect::PickPaths(k) = h.effects.recv().await.expect("效果通道关了") {
                return k;
            }
        }
    })
    .await
    .expect("等不到弹选择框");
    assert_eq!(kind, PickKind::Folder);
    h.app.shell(ShellEvent::Picked(vec![path.into()]));
    assert_eq!(timeout(WAIT, ack).await.unwrap().unwrap().error, None);
}

fn candidate(s: &WebState) -> Option<&pb::CandidateView> {
    s.add_project.as_ref()?.candidate.as_ref()
}

fn checked(s: &WebState) -> bool {
    candidate(s).is_some_and(|c| !c.checking)
}

fn project_root(h: &H, name: &str) -> Option<String> {
    let mut out = None;
    let name = name.to_owned();
    h.daemon.mutate_tables(|t| {
        out = t
            .projects
            .values()
            .find(|p| p.name == name)
            .map(|p| p.root_path.clone());
    });
    out
}

fn drafts(h: &H) -> usize {
    let mut n = 0;
    h.daemon.mutate_tables(|t| n = t.drafts.len());
    n
}

/// 选文件夹 → 检查 → 确认：假后台收到的 AddProject 的路径是仓库根（选的是子目录也一样），勾着「生成草稿」而且
/// 没有 AGENTS.md 就一起排上草稿；收到 ProjectAdded 后跳到新项目，只跳一次。界面路径写成 ~。
#[tokio::test(flavor = "multi_thread")]
async fn pick_then_inspect_then_add() {
    let mut h = start().await;
    disk(
        &h,
        "/Users/me/code/svc/src",
        dir("/Users/me/code/svc", true, false),
    );
    pick(&mut h, "/Users/me/code/svc/src").await;
    until(&mut h, "检查完：能加", |s| {
        checked(s) && candidate(s).is_some_and(|c| c.can_confirm)
    })
    .await;
    let c = candidate(&h.web.state).unwrap().clone();
    assert_eq!(
        c.path_label, "~/code/svc",
        "选了子目录：显示仓库根，家目录写成 ~"
    );
    assert_eq!(c.notice_title, "将添加整个仓库");
    assert_eq!(c.agents_md_label, "未找到");
    assert!(c.can_propose_draft && c.propose_draft, "默认勾着");
    assert_eq!(c.confirm_label, "添加并开始对话");
    assert_eq!(
        act(&h, I::ConfirmAddProject(pb::ConfirmAddProject {}))
            .await
            .error,
        None
    );
    until(&mut h, "跳到新项目", |s| {
        s.navigate.as_ref().is_some_and(|n| n.project_id.is_some())
    })
    .await;
    assert_eq!(
        project_root(&h, "svc").as_deref(),
        Some("/Users/me/code/svc")
    );
    assert_eq!(drafts(&h), 1, "勾着、没有 AGENTS.md：排上了草稿");
    let first = h.web.state.navigate.clone().unwrap();
    until(&mut h, "草稿清掉了", |s| {
        s.add_project
            .as_ref()
            .is_some_and(|a| a.candidate.is_none())
    })
    .await;
    assert_eq!(h.web.state.navigate, Some(first), "只跳一次");
}

/// 不勾「生成草稿」：不排草稿。
#[tokio::test(flavor = "multi_thread")]
async fn unchecked_draft_is_not_proposed() {
    let mut h = start().await;
    disk(&h, "/w/a", dir("/w/a", true, false));
    pick(&mut h, "/w/a").await;
    until(&mut h, "检查完", checked).await;
    act(
        &h,
        I::SetProposeAgentsMdDraft(pb::ToggleIntent { on: false }),
    )
    .await;
    until(&mut h, "勾选框去掉了", |s| {
        candidate(s).is_some_and(|c| !c.propose_draft)
    })
    .await;
    act(&h, I::ConfirmAddProject(pb::ConfirmAddProject {})).await;
    until(&mut h, "加进来了", |s| {
        s.navigate.as_ref().is_some_and(|n| n.project_id.is_some())
    })
    .await;
    assert_eq!(drafts(&h), 0);
}

/// 没有版本控制：主按钮先不可点；初始化受理后重查，变成可点。
#[tokio::test(flavor = "multi_thread")]
async fn unversioned_then_init_then_add() {
    let mut h = start().await;
    disk(&h, "/w/raw", dir("/w/raw", false, true));
    pick(&mut h, "/w/raw").await;
    until(&mut h, "检查完", checked).await;
    let c = candidate(&h.web.state).unwrap().clone();
    assert_eq!(c.notice_title, "这个文件夹还没有版本控制");
    assert!(c.init_label.is_some() && !c.can_confirm);
    assert_eq!(c.agents_md_label, "已有");
    assert!(!c.can_propose_draft, "已有 AGENTS.md：不显示勾选框");
    let rejected = act(&h, I::ConfirmAddProject(pb::ConfirmAddProject {})).await;
    assert!(rejected.error.is_some(), "没版本控制时确认被拒");
    assert_eq!(
        act(
            &h,
            I::InitCandidateRepository(pb::InitCandidateRepository {})
        )
        .await
        .error,
        None
    );
    until(&mut h, "重查后能加", |s| {
        candidate(s).is_some_and(|c| c.can_confirm && c.init_label.is_none())
    })
    .await;
}

/// 已经是项目：不发命令，直接跳到那个项目。
#[tokio::test(flavor = "multi_thread")]
async fn known_project_navigates_without_command() {
    let mut h = start().await;
    let mut root = String::new();
    h.daemon.mutate_tables(|t| {
        root = t.projects[iwork_fake_daemon::scenario::PROJECT]
            .root_path
            .clone();
    });
    disk(&h, &root, dir(&root, true, true));
    pick(&mut h, &root).await;
    until(&mut h, "检查完：已知", |s| {
        candidate(s).is_some_and(|c| c.confirm_label == "打开已有项目" && c.can_confirm)
    })
    .await;
    let before = h.daemon.head();
    act(&h, I::ConfirmAddProject(pb::ConfirmAddProject {})).await;
    until(&mut h, "跳到已有项目", |s| {
        s.navigate.as_ref().and_then(|n| n.project_id.as_deref())
            == Some(iwork_fake_daemon::scenario::PROJECT)
    })
    .await;
    assert_eq!(h.daemon.head(), before, "没发任何命令");
}

/// 检查在飞的时候断线：重连后重发、画出结果。
#[tokio::test(flavor = "multi_thread")]
async fn inspect_resent_after_reconnect() {
    let mut h = start().await;
    disk(&h, "/w/b", dir("/w/b", true, false));
    // 检查扣着不回，这时断线：这一次检查丢了
    h.daemon.hold_queries(Some(Box::new(|q: &pb::Query| {
        matches!(q.kind, Some(pb::query::Kind::InspectProjectPath(_)))
    })));
    pick(&mut h, "/w/b").await;
    until(&mut h, "检查中", |s| {
        candidate(s).is_some_and(|c| c.checking)
    })
    .await;
    h.daemon.disconnect_all().await;
    h.daemon.hold_queries(None);
    until(&mut h, "重连后画出结果", |s| {
        checked(s) && candidate(s).is_some_and(|c| c.can_confirm)
    })
    .await;
}

/// 添加被后台拒了（检查之后目录被挪走）：原因放进 error，重查（这次检查失败，主按钮不可点）。
#[tokio::test(flavor = "multi_thread")]
async fn add_rejected_shows_reason_and_reinspects() {
    let mut h = start().await;
    disk(&h, "/w/gone", dir("/w/gone", true, false));
    pick(&mut h, "/w/gone").await;
    until(&mut h, "检查完", checked).await;
    h.daemon.mutate_tables(|t| {
        t.disk.remove("/w/gone");
    });
    let ack = act(&h, I::ConfirmAddProject(pb::ConfirmAddProject {})).await;
    assert!(ack.error.is_some(), "受理结果带着后台的原因（网页弹提示）");
    until(&mut h, "原因留在弹窗上、重查成失败", |s| {
        s.add_project.as_ref().is_some_and(|a| a.error.is_some())
            && candidate(s)
                .is_some_and(|c| !c.checking && !c.can_confirm && !c.notice_body.is_empty())
    })
    .await;
}

/// 初始化失败（没配提交身份）：候选还在、提示还在、主按钮仍不可点，原因放进 error。
#[tokio::test(flavor = "multi_thread")]
async fn init_failure_keeps_candidate() {
    let mut h = start().await;
    disk(&h, "/w/noid", dir("/w/noid", false, false));
    h.daemon.mutate_tables(|t| {
        t.init_failures
            .insert("/w/noid".into(), "没有配置提交身份".into());
    });
    pick(&mut h, "/w/noid").await;
    until(&mut h, "检查完", checked).await;
    let ack = act(
        &h,
        I::InitCandidateRepository(pb::InitCandidateRepository {}),
    )
    .await;
    assert_eq!(
        ack.error.map(|e| e.message).as_deref(),
        Some("没有配置提交身份")
    );
    until(&mut h, "原因进弹窗", |s| {
        s.add_project.as_ref().and_then(|a| a.error.as_deref()) == Some("没有配置提交身份")
    })
    .await;
    let c = candidate(&h.web.state).unwrap();
    assert!(c.init_label.is_some() && !c.can_confirm && !c.initializing);
}

/// 拖进来恰好一个文件夹：设成候选、请网页打开添加项目弹窗。对照：一个文件、两个文件夹照旧走附件。
#[tokio::test(flavor = "multi_thread")]
async fn dropped_single_directory_opens_add_project() {
    let mut h = start().await;
    let real = tempfile::tempdir_in("/tmp").unwrap();
    let path = real.path().to_string_lossy().into_owned();
    disk(&h, &path, dir(&path, true, false));
    h.app.shell(ShellEvent::Dropped(vec![path.clone()]));
    until(
        &mut h,
        "请网页打开添加项目、候选检查完",
        |s| s.navigate.as_ref().is_some_and(|n| n.add_project) && checked(s),
    )
    .await;
    // 对照：两个文件夹不当成添加项目
    let other = tempfile::tempdir_in("/tmp").unwrap();
    let seq = h.web.state.navigate.as_ref().unwrap().seq;
    h.app.shell(ShellEvent::Dropped(vec![
        path,
        other.path().to_string_lossy().into_owned(),
    ]));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = timeout(
        Duration::from_millis(300),
        h.app.next_frame(h.web.request()),
    )
    .await
    .map(|r| r.map(|c| h.web.accept(&c)));
    assert_eq!(
        h.web.state.navigate.as_ref().unwrap().seq,
        seq,
        "两个文件夹不打开添加项目"
    );
}

/// 克隆：后台拒了地址（错误码照真后台）→ 受理结果带原因（网页弹提示），草稿不留候选；
/// 接受的地址 → 克隆完的目录成了候选，之后和本机文件夹同一条路。
#[tokio::test(flavor = "multi_thread")]
async fn clone_rejected_then_clone_finished_becomes_candidate() {
    let mut h = start().await;
    h.daemon.mutate_tables(|t| {
        t.rejected_clone_urls.insert(
            "file:///etc".into(),
            "只支持远程地址；本机已有的仓库请用『打开本机文件夹』".into(),
        );
    });
    let ack = act(
        &h,
        I::StartClone(pb::StartCloneIntent {
            url: "file:///etc".into(),
            name: None,
        }),
    )
    .await;
    let e = ack.error.expect("后台拒了地址");
    assert_eq!(e.code, "invalid_argument");
    until(&mut h, "原因进弹窗、没有候选、不忙", |s| {
        s.add_project
            .as_ref()
            .is_some_and(|a| a.error.is_some() && a.candidate.is_none() && !a.busy)
    })
    .await;
    let ack = act(
        &h,
        I::StartClone(pb::StartCloneIntent {
            url: "https://example.com/team/svc.git".into(),
            name: None,
        }),
    )
    .await;
    assert_eq!(ack.error, None);
    until(
        &mut h,
        "克隆完的目录成了候选、检查完能加",
        |s| candidate(s).is_some_and(|c| c.path_label.ends_with("/svc") && c.can_confirm),
    )
    .await;
}

/// 新建项目：受理后等 ProjectAdded 再跳到它。
#[tokio::test(flavor = "multi_thread")]
async fn create_project_navigates_on_project_added() {
    let mut h = start().await;
    assert_eq!(
        act(
            &h,
            I::CreateProject(pb::CreateProjectIntent {
                name: "新服务".into()
            })
        )
        .await
        .error,
        None
    );
    until(&mut h, "跳到新项目", |s| {
        s.navigate.as_ref().is_some_and(|n| n.project_id.is_some())
    })
    .await;
    assert!(project_root(&h, "新服务").is_some());
}
