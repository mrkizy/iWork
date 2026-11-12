//! 任务 `autostart-align`（可重启）：让系统的自启注册项跟上**已提交的最新**设置（T04 方案 2.12）。
//!
//! 全进程只有它调 `AutostartPort::set`，天然串行。什么时候跑：任务启动时一次；提交观察者看到
//! `GlobalSettingsChanged{autostart: Some(_)}` 时叫醒一次；失败后退避重试（5s 起翻倍，上限 5 分钟）。
//! 每次都**重新读**读模型里已提交的设置（不用叫醒它的那条事件里的值）和系统状态，按 [`decide`] 的表动手，
//! 所以「开 → 关 → 重发开」「两个相反的设置并发」都停在最后一次提交的值上。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iwork_ports::autostart::{AutostartError, AutostartPort, AutostartState};
use iwork_ports::read_model::ReadModelPort;
use tokio::sync::Notify;

pub const FIRST_BACKOFF: Duration = Duration::from_secs(5);
pub const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// 对齐表的一格要做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Register,
    Unregister,
    Nothing,
    /// 托管实例运行中不注销自己（注销会结束本进程连同在跑的回合）；留到下次启动、`bind` 之前（启动表 9a）。
    KeepUntilNextBoot,
}

/// 方案 2.12 的对齐表（纯函数）。
pub fn decide(intent: bool, state: &AutostartState, managed: bool, program: &Path) -> Action {
    match (intent, state) {
        (true, AutostartState::NotRegistered) => Action::Register,
        // app 挪了位置：注册项指向旧路径 → 重新注册
        (true, AutostartState::Registered { program: p }) if p != program => Action::Register,
        (true, AutostartState::Registered { .. }) => Action::Nothing,
        (false, AutostartState::Registered { .. }) if managed => Action::KeepUntilNextBoot,
        (false, AutostartState::Registered { .. }) => Action::Unregister,
        (false, AutostartState::NotRegistered) => Action::Nothing,
    }
}

/// 对齐一次：读已提交的意图与系统状态，按表动手。
pub async fn align_once<P: AutostartPort, R: ReadModelPort>(
    port: &P,
    read: &R,
    managed: bool,
    program: &Path,
) -> Result<Action, AutostartError> {
    let intent = read
        .settings()
        .await
        .map_err(|e| AutostartError::Failed {
            message: format!("读设置失败：{e}"),
        })?
        .autostart;
    let state = port.state().await?;
    let action = decide(intent, &state, managed, program);
    match action {
        Action::Register => port.set(true).await?,
        Action::Unregister => port.set(false).await?,
        Action::KeepUntilNextBoot => {
            tracing::info!("自启已关，注册项留到本进程下次启动时移除");
        }
        Action::Nothing => {}
    }
    Ok(action)
}

/// 任务体。
pub async fn run<P: AutostartPort + 'static, R: ReadModelPort + 'static>(
    port: Arc<P>,
    read: Arc<R>,
    managed: bool,
    program: PathBuf,
    wake: Arc<Notify>,
) {
    if let Err(e) = port.supported() {
        // 做不了自启：受理时已经拒绝过；这里不动，也不刷屏
        tracing::info!(reason = %e, "这台机器做不了登录自启，对齐任务不做事");
        return std::future::pending().await;
    }
    let mut backoff = FIRST_BACKOFF;
    loop {
        match align_once(port.as_ref(), read.as_ref(), managed, &program).await {
            Ok(_) => {
                backoff = FIRST_BACKOFF;
                wake.notified().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, retry_secs = backoff.as_secs(), "自启对齐失败，稍后重试");
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    () = wake.notified() => {}
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use iwork_ports::store::StoragePort;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// 假端口：记下 `set` 的调用；`set` 里检测重入；可设前几次失败。
    #[derive(Default)]
    struct FakePort {
        state: Mutex<Option<PathBuf>>,
        sets: Mutex<Vec<bool>>,
        in_set: AtomicBool,
        reentered: AtomicBool,
        fail_first: AtomicUsize,
        program: PathBuf,
    }

    impl AutostartPort for FakePort {
        async fn state(&self) -> Result<AutostartState, AutostartError> {
            Ok(match self.state.lock().unwrap().clone() {
                Some(program) => AutostartState::Registered { program },
                None => AutostartState::NotRegistered,
            })
        }
        fn supported(&self) -> Result<(), AutostartError> {
            Ok(())
        }
        async fn set(&self, enabled: bool) -> Result<(), AutostartError> {
            if self.in_set.swap(true, Ordering::SeqCst) {
                self.reentered.store(true, Ordering::SeqCst);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
            self.sets.lock().unwrap().push(enabled);
            let r = if self.fail_first.load(Ordering::SeqCst) > 0 {
                self.fail_first.fetch_sub(1, Ordering::SeqCst);
                Err(AutostartError::Failed {
                    message: "注册失败".into(),
                })
            } else {
                *self.state.lock().unwrap() = enabled.then(|| self.program.clone());
                Ok(())
            };
            self.in_set.store(false, Ordering::SeqCst);
            r
        }
    }

    fn port(program: &str) -> Arc<FakePort> {
        Arc::new(FakePort {
            program: PathBuf::from(program),
            ..Default::default()
        })
    }

    /// 方案 2.12 的对齐表逐行。
    #[test]
    fn table() {
        let me = Path::new("/app/iwork-daemon");
        let reg = |p: &str| AutostartState::Registered {
            program: PathBuf::from(p),
        };
        let none = AutostartState::NotRegistered;
        for managed in [false, true] {
            assert_eq!(decide(true, &none, managed, me), Action::Register);
            assert_eq!(
                decide(true, &reg("/old/iwork-daemon"), managed, me),
                Action::Register,
                "路径不对 → 重新注册"
            );
            assert_eq!(
                decide(true, &reg("/app/iwork-daemon"), managed, me),
                Action::Nothing
            );
            assert_eq!(decide(false, &none, managed, me), Action::Nothing);
        }
        assert_eq!(
            decide(false, &reg("/app/iwork-daemon"), false, me),
            Action::Unregister
        );
        assert_eq!(
            decide(false, &reg("/app/iwork-daemon"), true, me),
            Action::KeepUntilNextBoot,
            "托管实例运行中不调 set(false)"
        );
    }

    struct World {
        _env: Env,
        store: Arc<Store>,
        read: Arc<iwork_store::SqliteReadModel<iwork_app::codec::ProtoEventCodec>>,
    }

    type Store = iwork_store::SqliteStore<iwork_app::codec::ProtoEventCodec>;

    fn world() -> World {
        let env = Env::new();
        let store = Arc::new(env.open());
        let read = Arc::new(read_model(&store));
        World {
            _env: env,
            store,
            read,
        }
    }

    async fn submit(
        w: &World,
        port: &FakePort,
        n: u128,
        enabled: bool,
    ) -> iwork_ports::store::CommitReceipt {
        iwork_app::lifecycle::set_autostart(
            port,
            w.store.as_ref(),
            cmd_id(n),
            iwork_domain::common::Actor("user".into()),
            enabled,
        )
        .await
        .unwrap()
    }

    /// 失败后退避重试，成功后状态跟上。
    #[tokio::test(start_paused = true)]
    async fn retries_after_failure() {
        let w = world();
        let p = port("/app/iwork-daemon");
        p.fail_first.store(2, Ordering::SeqCst);
        submit(&w, &p, 1, true).await;
        let wake = Arc::new(Notify::new());
        let t = tokio::spawn(run(
            p.clone(),
            w.read.clone(),
            false,
            p.program.clone(),
            wake,
        ));
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if p.state.lock().unwrap().is_some() {
                break;
            }
        }
        assert_eq!(
            *p.sets.lock().unwrap(),
            [true, true, true],
            "失败两次后第三次成功"
        );
        assert!(p.state.lock().unwrap().is_some());
        t.abort();
    }

    /// S11b：A 开 → B 关 → 以 A 的 command_id 重发 A：回执 replayed、不产生事件；对齐后端口未注册，`set(true)` 只调过一次。
    #[tokio::test]
    async fn replayed_set_autostart_has_no_effect() {
        let w = world();
        let p = port("/app/iwork-daemon");
        let wake = Arc::new(Notify::new());
        let t = tokio::spawn(run(
            p.clone(),
            w.read.clone(),
            false,
            p.program.clone(),
            wake.clone(),
        ));
        let settle = || async {
            for _ in 0..200 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let intent = w.read.settings().await.unwrap().autostart;
                if intent == p.state.lock().unwrap().is_some() && !p.in_set.load(Ordering::SeqCst) {
                    return;
                }
            }
            panic!("对齐没静止");
        };
        submit(&w, &p, 1, true).await;
        wake.notify_one();
        settle().await;
        submit(&w, &p, 2, false).await;
        wake.notify_one();
        settle().await;
        let head = w.store.head_seq().await.unwrap();
        let again = submit(&w, &p, 1, true).await;
        assert!(again.replayed);
        assert_eq!(w.store.head_seq().await.unwrap(), head, "没产生事件");
        wake.notify_one();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            p.state.lock().unwrap().is_none(),
            "停在最后一次提交的值（关）"
        );
        assert_eq!(p.sets.lock().unwrap().iter().filter(|x| **x).count(), 1);
        t.abort();
    }

    /// S11b：并发提交 100 对开/关，等对齐静止：端口状态等于读模型里最后提交的值；`set` 的调用是串行的。
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_opposite_settings_converge() {
        let w = Arc::new(world());
        let p = port("/app/iwork-daemon");
        let wake = Arc::new(Notify::new());
        let t = tokio::spawn(run(
            p.clone(),
            w.read.clone(),
            false,
            p.program.clone(),
            wake.clone(),
        ));
        let mut tasks = Vec::new();
        for i in 0..200u128 {
            let (w, p, wake) = (w.clone(), p.clone(), wake.clone());
            tasks.push(tokio::spawn(async move {
                submit(&w, &p, 1000 + i, i % 2 == 0).await;
                wake.notify_one();
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        wake.notify_one();
        let mut ok = false;
        for _ in 0..500 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let intent = w.read.settings().await.unwrap().autostart;
            if intent == p.state.lock().unwrap().is_some() && !p.in_set.load(Ordering::SeqCst) {
                ok = true;
                break;
            }
        }
        assert!(ok, "端口状态没跟上最后提交的值");
        assert!(!p.reentered.load(Ordering::SeqCst), "set 被并发调用了");
        t.abort();
    }
}
