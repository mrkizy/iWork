//! 助手配置页的接线（步 7c）：连假后台走真协议。
//! - 一口气改好几项（不等帧、不等回包）：全部落到后台，不因版本冲突被拒，配置页显示的是改过的值；
//! - 名字为空：后台校验拒（校验在应用服务，AGENTS.md「能力与界面分离」），配置页挂后台的错误，下一次改动清掉；
//!   新建助手名字为空同样由后台拒，错误回到操作的受理结果里（网页走 toast）；
//! - 新建助手：侧栏出现它；删除：侧栏去掉它。内置 iWork 不能删。
//!
//! 闸本身的交错性质见 tests/assistant_edits.rs，这里只证明会话层把它接对了。

#![cfg(unix)]

mod support;

use std::time::Duration;

use iwork_client_core::{App, DirectDialer, Options};
use iwork_fake_daemon::{FakeDaemon, self_trusting_identity};
use iwork_proto::pb;
use pb::ui_intent::Kind as I;
use pb::update_assistant_field_intent::Field as F;
use support::web::{Web, WebState};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);
const ID: &str = "as-asen";
const DM: &str = "c-dm-asen";

async fn start() -> (FakeDaemon, App, Web, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix("t09-7c")
        .tempdir_in("/tmp")
        .unwrap();
    let daemon = FakeDaemon::start(dir.path().join("d.sock")).await.unwrap();
    daemon.run_scenario("assistant-profile").await.unwrap();
    let dialer = DirectDialer::new(daemon.endpoint(), self_trusting_identity().unwrap(), "test");
    let (app, _effects) = App::start(dialer, Options::default());
    let web = Web::new(app.attach().await.unwrap());
    let ack = act(
        &app,
        I::SetVisible(pb::SetVisible {
            sidebar: true,
            conversation_id: Some(DM.into()),
            assistant_id: Some(ID.into()),
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(ack.error, None);
    (daemon, app, web, dir)
}

async fn act(app: &App, kind: I) -> pb::IntentAck {
    app.intent(pb::UiIntent { kind: Some(kind) }).await
}

fn field(f: F) -> I {
    I::UpdateAssistantField(pb::UpdateAssistantFieldIntent {
        assistant_id: ID.into(),
        field: Some(f),
    })
}

async fn until(app: &App, web: &mut Web, what: &str, cond: impl Fn(&WebState) -> bool) {
    let r = timeout(WAIT, async {
        while !cond(&web.state) {
            if let Ok(Ok(c)) =
                timeout(Duration::from_millis(300), app.next_frame(web.request())).await
            {
                web.accept(&c);
            }
        }
    })
    .await;
    assert!(r.is_ok(), "等不到：{what}");
}

fn backend(d: &FakeDaemon, id: &str) -> Option<pb::Assistant> {
    let mut out = None;
    d.mutate_tables(|t| out = t.assistants.get(id).cloned());
    out
}

async fn ready(app: &App, web: &mut Web) {
    until(
        app,
        web,
        "连上、资料栏有了、模型清单取到了",
        |s| {
            s.connection
                .as_ref()
                .is_some_and(|c| c.state == pb::ConnectionState::Live as i32)
                && s.assistant
                    .as_ref()
                    .and_then(|a| a.model_menu.as_ref())
                    .is_some_and(|m| !m.models_loading && !m.models.is_empty())
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rapid_edits_all_land_without_conflict() {
    let (d, app, mut web, _dir) = start().await;
    ready(&app, &mut web).await;
    let before = backend(&d, ID).unwrap();
    let menu = web
        .state
        .assistant
        .as_ref()
        .unwrap()
        .model_menu
        .clone()
        .unwrap();
    let model = menu
        .models
        .iter()
        .find(|m| !m.selected)
        .expect("对照：清单里有不是当前的模型")
        .value
        .clone();
    let edit_files = pb::ToolGroup::EditFiles as i32;
    let web_group = pb::ToolGroup::Web as i32;
    // 一口气发五项：不等帧、不等回包（摘要追上之前，后面几项都会带着旧版本，没有闸就会被拒）
    for f in [
        F::Tool(pb::ToolChoice {
            group: edit_files,
            enabled: false,
        }),
        F::Tool(pb::ToolChoice {
            group: web_group,
            enabled: false,
        }),
        F::Name("阿森二号".into()),
        F::Prompt("先读懂再改".into()),
        F::Model(pb::ModelChoice {
            model_id: model.clone(),
            effort: None,
        }),
    ] {
        let ack = act(&app, field(f)).await;
        assert_eq!(ack.error, None);
    }
    until(
        &app,
        &mut web,
        "配置页显示改过的值、没有错误",
        |s| {
            s.assistant.as_ref().is_some_and(|a| {
                a.name == "阿森二号"
                    && a.prompt == "先读懂再改"
                    && a.model_id == model
                    && a.save_error.is_none()
                    && a.tools.iter().filter(|t| !t.enabled).count() == 2
            })
        },
    )
    .await;
    // 后台：五项都落了，版本只涨不冲突
    let r = timeout(WAIT, async {
        loop {
            let a = backend(&d, ID).unwrap();
            let c = a.config.clone().unwrap();
            if c.name == "阿森二号"
                && c.prompt == "先读懂再改"
                && c.model.as_ref().map(|m| m.model_id.as_str()) == Some(model.as_str())
                && c.disabled_groups.contains(&edit_files)
                && c.disabled_groups.contains(&web_group)
            {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let after = r.expect("五项没有全落到后台");
    assert!(after.version > before.version, "对照：版本涨了");
    assert_eq!(
        after.config.as_ref().unwrap().runtime_id,
        before.config.as_ref().unwrap().runtime_id,
        "没改的字段原样"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_name_is_rejected_by_backend_and_reported() {
    let (d, app, mut web, _dir) = start().await;
    ready(&app, &mut web).await;
    let before = backend(&d, ID).unwrap();
    let ack = act(&app, field(F::Name("   ".into()))).await;
    assert_eq!(
        ack.error, None,
        "受理了这次操作（错误挂在配置页上，不弹提示）"
    );
    until(&app, &mut web, "配置页挂上错误", |s| {
        s.assistant.as_ref().and_then(|a| a.save_error.as_deref()) == Some("名字不能为空")
    })
    .await;
    assert_eq!(
        backend(&d, ID).unwrap().version,
        before.version,
        "后台拒了，版本没涨"
    );
    act(&app, field(F::Prompt("换个提示词".into()))).await;
    until(&app, &mut web, "下一次改动清掉错误", |s| {
        s.assistant
            .as_ref()
            .is_some_and(|a| a.save_error.is_none() && a.prompt == "换个提示词")
    })
    .await;
}

/// 资料栏手动收起是全局偏好（ui-m1 2a 第 18 条 d）：意图变成 SetProfilePanelCollapsed，后台受理后经
/// GlobalSettingsChanged 回到资料栏视图；再展开也一样。
#[tokio::test(flavor = "multi_thread")]
async fn profile_panel_collapse_is_a_backend_setting() {
    let (_d, app, mut web, _dir) = start().await;
    ready(&app, &mut web).await;
    assert!(
        !web.state
            .assistant
            .as_ref()
            .unwrap()
            .profile_panel_collapsed,
        "对照：默认不收起"
    );
    let toggle = |on| I::SetProfilePanelCollapsed(pb::ToggleIntent { on });
    assert_eq!(act(&app, toggle(true)).await.error, None);
    until(&app, &mut web, "后台受理后视图变成收起", |s| {
        s.assistant
            .as_ref()
            .is_some_and(|a| a.profile_panel_collapsed)
    })
    .await;
    assert_eq!(act(&app, toggle(false)).await.error, None);
    until(&app, &mut web, "再展开", |s| {
        s.assistant
            .as_ref()
            .is_some_and(|a| !a.profile_panel_collapsed)
    })
    .await;
}

/// 头像配色（ui-m1 2a 第 18 条 a）：选一块，令牌名存进后台的配置，资料栏的头像和色块跟着变；
/// 不是 8 块之一的值在意图入口就拒，后台不动。
#[tokio::test(flavor = "multi_thread")]
async fn avatar_color_saves_token_and_rejects_unknown() {
    let (d, app, mut web, _dir) = start().await;
    ready(&app, &mut web).await;
    let before = backend(&d, ID).unwrap();
    let ack = act(&app, field(F::AvatarColor("ch3".into()))).await;
    assert_eq!(
        ack.error.as_ref().map(|e| e.code.as_str()),
        Some("invalid_intent"),
        "ch3 不在 8 块里"
    );
    assert_eq!(
        act(&app, field(F::AvatarColor("ch6".into()))).await.error,
        None
    );
    until(&app, &mut web, "头像与色块换成淡紫", |s| {
        s.assistant.as_ref().is_some_and(|a| {
            a.avatar.as_ref().is_some_and(|v| v.color == "ch6")
                && a.avatar_colors
                    .iter()
                    .filter(|c| c.selected)
                    .map(|c| c.token.as_str())
                    .eq(["ch6"])
        })
    })
    .await;
    let after = backend(&d, ID).unwrap();
    assert_eq!(
        after.config.and_then(|c| c.avatar).map(|a| a.color),
        Some("ch6".to_owned()),
        "后台存的是令牌名"
    );
    assert_eq!(
        after.version,
        before.version + 1,
        "只落了一次（ch3 没发出去）"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn create_and_delete_assistant() {
    let (d, app, mut web, _dir) = start().await;
    ready(&app, &mut web).await;
    let menu = web
        .state
        .sidebar
        .as_ref()
        .and_then(|s| s.new_assistant_menu.clone())
        .expect("侧栏带着新建助手的菜单");
    let runtime = menu
        .runtimes
        .iter()
        .find(|r| r.selected)
        .unwrap()
        .value
        .clone();
    let model = menu
        .models
        .iter()
        .find(|m| m.selected)
        .map(|m| m.value.clone());
    // 空名字：后台拒，错误原样回到受理结果
    let ack = act(
        &app,
        I::CreateAssistant(pb::AssistantEditIntent {
            name: "  ".into(),
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(ack.error.map(|e| e.message), Some("名字不能为空".into()));
    let ack = act(
        &app,
        I::CreateAssistant(pb::AssistantEditIntent {
            name: "小林".into(),
            prompt: "它负责写文档".into(),
            runtime_id: runtime,
            model_id: model.unwrap_or_default(),
            ..Default::default()
        }),
    )
    .await;
    assert_eq!(ack.error, None);
    until(&app, &mut web, "侧栏出现小林", |s| {
        s.sidebar.as_ref().is_some_and(|b| {
            b.assistants
                .iter()
                .any(|a| a.name == "小林" && !a.dm_conversation_id.is_empty())
        })
    })
    .await;
    let new_id = web
        .state
        .sidebar
        .as_ref()
        .unwrap()
        .assistants
        .iter()
        .find(|a| a.name == "小林")
        .unwrap()
        .assistant_id
        .clone();
    assert!(backend(&d, &new_id).is_some());
    let ack = act(
        &app,
        I::DeleteAssistant(pb::AssistantIntent {
            assistant_id: new_id.clone(),
        }),
    )
    .await;
    assert_eq!(ack.error, None);
    until(&app, &mut web, "侧栏去掉小林", |s| {
        s.sidebar
            .as_ref()
            .is_some_and(|b| !b.assistants.iter().any(|a| a.assistant_id == new_id))
    })
    .await;
    // 内置 iWork 不能删：侧栏的行就不给删除
    let builtin = web
        .state
        .sidebar
        .as_ref()
        .unwrap()
        .assistants
        .iter()
        .find(|a| a.builtin)
        .unwrap()
        .clone();
    assert!(!builtin.can_delete);
}

/// 审查 g3 1/F-4：「创建」连点两次（第一次的回包还没到）：只建一个助手，第二次被拒；在途期间侧栏视图说「正在创建」，
/// 回包到了就收掉，可以再建。
#[tokio::test(flavor = "multi_thread")]
async fn create_assistant_twice_creates_one() {
    let (d, app, mut web, _dir) = start().await;
    ready(&app, &mut web).await;
    let count = |d: &FakeDaemon| {
        let mut n = 0;
        d.mutate_tables(|t| {
            n = t
                .assistants
                .values()
                .filter(|a| a.config.as_ref().is_some_and(|c| c.name == "小林"))
                .count()
        });
        n
    };
    let create = || {
        act(
            &app,
            I::CreateAssistant(pb::AssistantEditIntent {
                name: "小林".into(),
                ..Default::default()
            }),
        )
    };
    // 两次一起交给会话：第二次在第一次的回包到之前处理
    let (a, b) = tokio::join!(create(), create());
    let errors: Vec<_> = [a, b].into_iter().filter_map(|x| x.error).collect();
    assert_eq!(errors.len(), 1, "恰好一次被拒：{errors:?}");
    assert_eq!(errors[0].code, "not_ready");
    until(
        &app,
        &mut web,
        "回包到了：不再「正在创建」",
        |s| s.sidebar.as_ref().is_some_and(|b| !b.creating_assistant),
    )
    .await;
    assert_eq!(count(&d), 1, "只建了一个");
    let again = create().await;
    assert_eq!(again.error, None, "对照：上一次有结果后可以再建");
    assert_eq!(count(&d), 2);
}
