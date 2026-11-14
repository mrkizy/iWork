//! 桌面客户端的拨号器 `AppDialer`（T04p 方案 4.7）：取身份、建拨号器放在 `dial()` 里，建不成回 `Environment`、
//! 修好后点「重试」能连上，建成之后不再重建。连假后台，只在 Unix 上编译。
#![cfg(unix)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use iwork_client_core::{App, AppDialer, DialerParts, DirectDialer, Options};
use iwork_fake_daemon::{FakeDaemon, self_trusting_identity};
use iwork_ipc::LocalEndpoint;
use iwork_ipc::peer::SelfIdentity;
use iwork_proto::pb;
use pb::ui_intent::Kind as I;
use support::web::{Web, WebState};
use tokio::time::{Instant, sleep, timeout, timeout_at};

const WAIT: Duration = Duration::from_secs(20);

/// 可计数、可控的两样：身份取不取得到、端点目录安不安全由测试拨；建出来的是连假后台的 `DirectDialer`。
/// 端点按 `LocalEndpoint::in_run_dir` 取：目录权限不对时它报错（和真实客户端建拨号器时同一个检查）。
struct Parts {
    run_dir: std::path::PathBuf,
    identity_fails: Arc<AtomicBool>,
    identities: Arc<AtomicUsize>,
    builds: Arc<AtomicUsize>,
}

impl DialerParts for Parts {
    type Inner = DirectDialer;

    fn identity(&mut self) -> Result<SelfIdentity, String> {
        self.identities.fetch_add(1, Ordering::SeqCst);
        if self.identity_fails.load(Ordering::SeqCst) {
            return Err("测试：取身份失败".into());
        }
        self_trusting_identity().map_err(|e| e.to_string())
    }

    fn build(&mut self, own: &SelfIdentity) -> Result<DirectDialer, String> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        let ep = LocalEndpoint::in_run_dir(&self.run_dir).map_err(|e| e.to_string())?;
        Ok(DirectDialer::new(
            ep.endpoint().clone(),
            own.clone(),
            "test",
        ))
    }
}

struct World {
    _dir: tempfile::TempDir,
    run_dir: std::path::PathBuf,
    daemon: FakeDaemon,
    identity_fails: Arc<AtomicBool>,
    identities: Arc<AtomicUsize>,
    builds: Arc<AtomicUsize>,
}

impl World {
    async fn new() -> World {
        let dir = tempfile::Builder::new()
            .prefix("t04p")
            .tempdir_in("/tmp")
            .unwrap();
        let run_dir = dir.path().join("run");
        std::fs::create_dir(&run_dir).unwrap();
        set_mode(&run_dir, 0o700);
        let ep = LocalEndpoint::in_run_dir(&run_dir).unwrap();
        let socket = match ep.endpoint() {
            iwork_ipc::transport::Endpoint::Unix(p) => p.clone(),
            other => panic!("{other:?}"),
        };
        let daemon = FakeDaemon::start(socket).await.unwrap();
        World {
            _dir: dir,
            run_dir,
            daemon,
            identity_fails: Arc::default(),
            identities: Arc::default(),
            builds: Arc::default(),
        }
    }

    fn dialer(&self) -> AppDialer<Parts> {
        AppDialer::new(Parts {
            run_dir: self.run_dir.clone(),
            identity_fails: self.identity_fails.clone(),
            identities: self.identities.clone(),
            builds: self.builds.clone(),
        })
    }
}

fn set_mode(dir: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

async fn start(w: &World) -> (App, Web) {
    let (app, effects) = App::start(w.dialer(), Options::default());
    std::mem::forget(effects);
    let web = Web::new(app.attach().await.unwrap());
    (app, web)
}

async fn until(app: &App, web: &mut Web, what: &str, pred: impl Fn(&WebState) -> bool) {
    until_by(Instant::now() + WAIT, app, web, what, pred).await;
}

/// 同 [`until`]，等到 `deadline` 为止（几步共用一个截止时间时用）。
async fn until_by(
    deadline: Instant,
    app: &App,
    web: &mut Web,
    what: &str,
    pred: impl Fn(&WebState) -> bool,
) {
    let r = timeout_at(deadline, async {
        while !pred(&web.state) {
            if let Ok(Ok(c)) =
                timeout(Duration::from_millis(300), app.next_frame(web.request())).await
            {
                web.accept(&c);
            }
        }
    })
    .await;
    assert!(
        r.is_ok(),
        "到截止时间也没等到：{what}；连接状态 {:?}",
        web.state.connection
    );
}

fn state_is(s: &WebState, state: pb::ConnectionState) -> bool {
    s.connection
        .as_ref()
        .is_some_and(|c| c.state == state as i32)
}

fn live(s: &WebState) -> bool {
    state_is(s, pb::ConnectionState::Live)
}

async fn retry(app: &App) {
    app.intent(pb::UiIntent {
        kind: Some(I::Retry(pb::Retry {})),
    })
    .await;
}

/// 端点目录的权限被改坏：第一次拨号回 Environment，整页显示原因，不自动重拨；把权限改对、点「重试」：连上。
/// 全程是同一个会话、同一个拨号器。
#[tokio::test(flavor = "multi_thread")]
async fn environment_error_then_fix_then_retry_connects() {
    let w = World::new().await;
    set_mode(&w.run_dir, 0o777);
    let (app, mut web) = start(&w).await;
    until(&app, &mut web, "整页说明", |s| {
        state_is(s, pb::ConnectionState::Environment)
    })
    .await;
    let builds_after_failure = w.builds.load(Ordering::SeqCst);
    sleep(Duration::from_secs(1)).await;
    assert_eq!(
        w.builds.load(Ordering::SeqCst),
        builds_after_failure,
        "Environment 不自动重拨"
    );
    set_mode(&w.run_dir, 0o700);
    retry(&app).await;
    until(&app, &mut web, "修好后重试连上", live).await;
}

/// 这条用例的全部步骤共用的截止时间。它守的是「多次重连之后身份和拨号器仍只建一次」，不测重连快慢：每次建连两端
/// 都要全量校验对端映像（调试版测试程序很大），CI 的慢机器上一次重连要好几秒，每步固定等 20 秒会偶发超时。按慢机器
/// 定一个总上限；建连卡住出不来由 iwork-ipc 的 `peer_identity_timeout` 守（每次建连最多一个握手时限）。
const BUILT_ONCE_TOTAL: Duration = Duration::from_secs(180);

/// 连上之后断线重拨三次、再手动重试一次：取身份 1 次、建拨号器 1 次。
#[tokio::test(flavor = "multi_thread")]
async fn identity_and_launcher_are_built_once() {
    let w = World::new().await;
    let (app, mut web) = start(&w).await;
    let deadline = Instant::now() + BUILT_ONCE_TOTAL;
    until_by(deadline, &app, &mut web, "连上", live).await;
    for round in 0..3 {
        w.daemon.disconnect_all().await;
        until_by(
            deadline,
            &app,
            &mut web,
            &format!("第 {round} 次断线"),
            |s| !live(s),
        )
        .await;
        until_by(
            deadline,
            &app,
            &mut web,
            &format!("第 {round} 次重连"),
            live,
        )
        .await;
    }
    retry(&app).await;
    until_by(deadline, &app, &mut web, "手动重试后在线", live).await;
    assert_eq!(w.identities.load(Ordering::SeqCst), 1, "身份只取一次");
    assert_eq!(w.builds.load(Ordering::SeqCst), 1, "拨号器只建一次");
}

/// 取身份失败：客户端启动不 panic，视图是整页原因；之后让取身份成功、点重试：连上。
#[tokio::test(flavor = "multi_thread")]
async fn first_dial_failure_does_not_panic() {
    let w = World::new().await;
    w.identity_fails.store(true, Ordering::SeqCst);
    let (app, mut web) = start(&w).await;
    until(&app, &mut web, "整页说明", |s| {
        state_is(s, pb::ConnectionState::Environment)
    })
    .await;
    assert_eq!(w.builds.load(Ordering::SeqCst), 0, "身份取不到就不建拨号器");
    w.identity_fails.store(false, Ordering::SeqCst);
    retry(&app).await;
    until(&app, &mut web, "重试连上", live).await;
    assert_eq!(w.identities.load(Ordering::SeqCst), 2);
    assert_eq!(w.builds.load(Ordering::SeqCst), 1);
}

/// 发布构建（不开 dev）里桌面客户端的真正拨号器就是 Launcher：编译期定的，dev 构建里没有这个类型等式。
#[cfg(not(feature = "dev"))]
#[test]
fn release_dialer_is_launcher() {
    use iwork_client_core::DesktopDialer;
    use iwork_client_core::launcher::{DetachedSpawn, IpcConnect, Launcher};
    fn same(d: DesktopDialer) -> Launcher<IpcConnect, DetachedSpawn> {
        d
    }
    let _ = same;
}

/// dev 构建：真正的拨号器是 DirectDialer（不拉起后台）；设了 IWORK_DEV_ENDPOINT 时连那个假后台、走开发信任。
#[cfg(feature = "dev")]
#[tokio::test(flavor = "multi_thread")]
async fn dev_endpoint_uses_direct_dialer() {
    use iwork_client_core::{DesktopDialer, DesktopParts, Dialer};
    fn same(d: DesktopDialer) -> DirectDialer {
        d
    }
    let _ = same;
    let w = World::new().await;
    let socket = match LocalEndpoint::in_run_dir(&w.run_dir).unwrap().endpoint() {
        iwork_ipc::transport::Endpoint::Unix(p) => p.clone(),
        other => panic!("{other:?}"),
    };
    // SAFETY：本测试文件里只有这一条读写这个变量
    unsafe { std::env::set_var("IWORK_DEV_ENDPOINT", &socket) };
    let mut dialer = AppDialer::new(DesktopParts::new("test"));
    assert!(dialer.dial().await.is_ok(), "连上假后台");
}
