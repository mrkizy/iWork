//! 内置助手 iWork 与运行时占位行的登记（T10a 方案 2.8 第 1、1a 步，R8）。

mod support;

use std::sync::Arc;

use iwork_daemon::assembly;
use iwork_daemon::gate::{GatedStore, SealGate};
use iwork_daemon::time::{MonotonicIds, SystemClock};
use iwork_domain::assistant::builtin_iwork_config;
use iwork_domain::event::{AssistantUpdated, DomainEvent};
use iwork_domain::id::{AssistantId, RuntimeId};
use iwork_domain::runtime::RuntimeState;
use iwork_domain::turn::TurnState;
use iwork_ports::discovery::RuntimeProbe;
use iwork_ports::read_model::ReadModelPort;
use iwork_ports::store::StoragePort;
use support::fixtures::*;
use support::turns::*;

fn builtin_created(evs: &[(u64, DomainEvent)]) -> usize {
    evs.iter()
        .filter(|(_, e)| matches!(e, DomainEvent::AssistantCreated(a) if a.assistant.builtin))
        .count()
}

async fn register(raw: &Arc<Store>) -> Result<(), iwork_ports::store::StorageError> {
    let gated = GatedStore::new(raw.clone(), Arc::new(SealGate::default()));
    assembly::register_builtin_assistant(&gated, &read_model(raw), &SystemClock::new()).await
}

/// 空库启动两次：库里只有一个内置助手、只有一条 `AssistantCreated`。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builtin_assistant_registered_once() {
    let env = Env::new();
    let raw = Arc::new(env.open());
    register(&raw).await.unwrap();
    register(&raw).await.unwrap();
    assert_eq!(builtin_created(&events(&raw).await), 1);
    let builtins: Vec<_> = read_model(&raw)
        .assistants()
        .await
        .unwrap()
        .into_iter()
        .filter(|a| a.builtin)
        .collect();
    assert_eq!(builtins.len(), 1);
    assert_eq!(builtins[0].id, AssistantId::BUILTIN_IWORK);
    assert_eq!(builtins[0].current.config, builtin_iwork_config());
}

/// 启动在读之后、提交之前死掉（提交没入队）：再启动后有且只有一个。用封了口的存储让那一笔入不了队来造这个窗口。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builtin_registration_survives_kill_before_commit() {
    let env = Env::new();
    let raw = Arc::new(env.open());
    let sealed = SealGate::default();
    sealed.seal();
    let gated = GatedStore::new(raw.clone(), Arc::new(sealed));
    let first =
        assembly::register_builtin_assistant(&gated, &read_model(&raw), &SystemClock::new()).await;
    assert!(first.is_err(), "这一笔没落盘");
    assert_eq!(builtin_created(&events(&raw).await), 0);
    register(&raw).await.unwrap();
    register(&raw).await.unwrap();
    assert_eq!(builtin_created(&events(&raw).await), 1);
}

/// 用户改过内置助手（提示词）后重启：还是改过的，不被默认配置盖回去。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_edited_builtin_is_not_overwritten() {
    let env = Env::new();
    let raw = Arc::new(env.open());
    register(&raw).await.unwrap();
    let mut edited = builtin_iwork_config();
    edited.prompt = "用户自己写的提示词".into();
    raw.commit(cmd(
        50,
        vec![
            AssistantUpdated {
                assistant_id: AssistantId::BUILTIN_IWORK,
                version: 2,
                config: edited.clone(),
            }
            .into(),
        ],
    ))
    .await
    .unwrap();
    register(&raw).await.unwrap();
    let a = read_model(&raw)
        .assistants()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.builtin)
        .unwrap();
    assert_eq!(a.current.config, edited);
    assert_eq!(builtin_created(&events(&raw).await), 1);
}

/// 探测不到运行时：助手照样在，运行时那一行是 `Missing`；发消息照常受理（不是 NOT_FOUND），
/// 回合派发时以「运行时缺失」失败。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn builtin_registered_while_runtime_missing() {
    let mut h = harness(Opts::default()).await;
    let read = read_model(&h.raw);
    let gated = GatedStore::new(h.raw.clone(), Arc::new(SealGate::default()));
    let probe = RuntimeProbe {
        cli_path: None,
        cli_version: None,
        login: None,
    };
    assembly::commit_runtime_row(
        &gated,
        &read,
        &MonotonicIds::new(SystemClock::new()),
        assembly::runtime_row(&probe, "t"),
    )
    .await
    .unwrap();
    let runtime = read.runtimes().await.unwrap();
    assert_eq!(runtime.len(), 1);
    assert_eq!(runtime[0].id, RuntimeId::DEFAULT_RUNTIME);
    assert_eq!(runtime[0].state, RuntimeState::Missing);
    assert!(read.assistants().await.unwrap().iter().any(|a| a.builtin));
    // 运行时缺失：开会话报「找不到运行时程序」
    h.agent
        .missing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    h.start_host();
    let t = h.send(1, "hi").await;
    h.wait_state(t, |s| s == TurnState::Failed).await;
    let reason = events(&h.raw).await.into_iter().find_map(|(_, e)| match e {
        DomainEvent::TurnStateChanged(s) if s.turn_id == t && s.to == TurnState::Failed => s.reason,
        _ => None,
    });
    assert!(
        reason
            .as_deref()
            .is_some_and(|r| r.contains("找不到运行时")),
        "失败原因点名运行时缺失：{reason:?}"
    );
    h.stop().await;
}
