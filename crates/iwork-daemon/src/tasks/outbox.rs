//! 任务 `outbox`（可重启）：outbox 投递循环（T04 方案 2.11；需求 12「至少一次传输，结果不确定时先对账再决定」）。
//!
//! - 去向登记表：每个去向一行（前缀、结果不确定时怎么办、单次时限、次数上限、投递实现）。加一种去向 = 加一行。
//!   T04 交付时登记表是空的，T10 登记真实去向。
//! - 每次启动先对账：停在 `Dispatched` 的条目，能幂等的改回 `Queued`、不能的标 `Failed`（只重启这个任务时也走这里）。
//! - 每个去向一条道，互不等待；每条道都「取到取不到为止」，之后才等下一次叫醒（提交观察者在有新 outbox 条目时叫醒）。
//!   不轮询。超时、panic 后改回排队的那条先于同去向后面的条目再投（保序）。
//! - 没有去向认领的条目原样留在 `queued`，只报不动（产品负责人 2026-11-11 定，审查 1/F-4）：不标失败、不加次数、
//!   不派发，装回认识这个去向的版本后照常按先后投出。启动时查一次、之后每次叫醒查一次，只报新出现的。
//! - 「结果不确定」只有一个处理函数 [`settle_uncertain`]：上一次投递没了（启动对账）、单次超时、投递自己 panic 都走它。

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use iwork_ports::store::{DeliveryState, OutboxItem, OutboxSelector, StorageError, StoragePort};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::dev;

/// 投递结果不确定时怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnUncertain {
    /// 重投：接收方按 `OutboxId` 去重，必须幂等。
    Redeliver,
    /// 不能幂等的去向：标 `Failed`，留给上层按失败处理。
    Fail,
}

/// 一次投递的失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchError {
    pub retryable: bool,
    pub message: String,
}

pub type DispatchFuture<'a> = Pin<Box<dyn Future<Output = Result<(), DispatchError>> + Send + 'a>>;

/// 一种去向的投递实现。
pub trait OutboxDispatch: Send + Sync {
    /// 投递一条。`item.id` 就是幂等键，接收方凭它去重。
    /// 约定：future 被丢弃后不留任何还在跑的后台工作（不自己 spawn 脱离的任务）。
    fn dispatch<'a>(&'a self, item: &'a OutboxItem) -> DispatchFuture<'a>;
}

/// 去向登记表的一行。`target` 以 `prefix` 开头的条目归它。
pub struct OutboxRoute {
    pub prefix: &'static str,
    pub on_uncertain: OnUncertain,
    /// 单次投递的时限；到点算「结果不确定」。
    pub timeout: Duration,
    /// 一条最多派发几次（库里的 `attempts`）；用完标 `Failed`。
    pub max_attempts: u32,
    pub dispatch: Box<dyn OutboxDispatch>,
}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_MAX_ATTEMPTS: u32 = 8;
/// 可重试错误之后这条道的退避：1s 起翻倍，上限 60s，成功一次清零。
pub const FIRST_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
const LANE_PAGE: usize = 16;
const UNCLAIMED_PAGE: usize = 64;

/// 开发构建的暂停点：投递已返回、结果还没记（「已派发、未记结果」）。
pub const PAUSE_AFTER_DISPATCH: &str = "outbox-after-dispatch";

impl OutboxRoute {
    pub fn new(
        prefix: &'static str,
        on_uncertain: OnUncertain,
        dispatch: Box<dyn OutboxDispatch>,
    ) -> Self {
        Self {
            prefix,
            on_uncertain,
            timeout: DEFAULT_TIMEOUT,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            dispatch,
        }
    }
}

/// 登记表里的前缀不许为空、不许互为前缀（启动时检查；否则一个条目归两条道）。
pub fn validate(routes: &[OutboxRoute]) -> Result<(), String> {
    for (i, a) in routes.iter().enumerate() {
        if a.prefix.is_empty() {
            return Err("outbox 去向的前缀不能为空".into());
        }
        for b in &routes[i + 1..] {
            if a.prefix.starts_with(b.prefix) || b.prefix.starts_with(a.prefix) {
                return Err(format!(
                    "outbox 去向的前缀互为前缀：{:?} 与 {:?}",
                    a.prefix, b.prefix
                ));
            }
        }
    }
    Ok(())
}

/// 「结果不确定」的唯一处理：按去向的 `on_uncertain` 改回 `Queued`（次数在下次取出时把关）或标 `Failed`。
/// 返回改成了哪个状态：改回 `Queued` 的条目要先重投完，同一去向后面的条目才能投（保持先后）。
pub async fn settle_uncertain<St: StoragePort>(
    store: &St,
    route: &OutboxRoute,
    item: &OutboxItem,
) -> Result<DeliveryState, StorageError> {
    let state = match route.on_uncertain {
        OnUncertain::Redeliver => DeliveryState::Queued,
        OnUncertain::Fail => DeliveryState::Failed,
    };
    tracing::warn!(id = item.id.0, target = %item.target.0, ?state, "outbox 投递结果不确定");
    store.outbox_mark(item.id, state).await?;
    Ok(state)
}

/// 任务体。被监督器重启时从头来：先对账，再按登记表起各条道。
pub async fn run<St>(
    store: Arc<St>,
    routes: Arc<Vec<OutboxRoute>>,
    wake: watch::Sender<u64>,
    report: UnclaimedSink,
) where
    St: StoragePort + Send + Sync + 'static,
{
    let redeliver: Vec<String> = routes
        .iter()
        .filter(|r| r.on_uncertain == OnUncertain::Redeliver)
        .map(|r| r.prefix.to_owned())
        .collect();
    match store.outbox_requeue_dispatched(redeliver).await {
        Ok(report) => {
            if report.requeued + report.failed > 0 {
                tracing::info!(
                    requeued = report.requeued,
                    failed = report.failed,
                    "outbox 对账完成"
                );
            }
        }
        // 对账做不了就不开工：任务结束，监督器退避后重起
        Err(e) => {
            tracing::error!(error = %e, "outbox 对账失败");
            return;
        }
    }
    let mut lanes = JoinSet::new();
    for i in 0..routes.len() {
        lanes.spawn(lane(store.clone(), routes.clone(), i, wake.subscribe()));
    }
    lanes.spawn(unclaimed_lane(
        store.clone(),
        routes.clone(),
        wake.subscribe(),
        report,
    ));
    // 道不该结束；一条道 panic 让整个任务 panic，由监督器重启（丢弃 JoinSet 会取消其余的道）
    if let Some(Err(e)) = lanes.join_next().await
        && e.is_panic()
    {
        std::panic::resume_unwind(e.into_panic());
    }
}

enum Next {
    /// 这一条处理完了（送达或失败），取下一条。
    Continue,
    /// 这一条还在排队（可重试的错误、结果不确定后改回排队）：这条道退避后从头重取，它排在最前，
    /// 同一去向后面的条目不越过它。
    Backoff,
}

async fn lane<St: StoragePort>(
    store: Arc<St>,
    routes: Arc<Vec<OutboxRoute>>,
    i: usize,
    mut wake: watch::Receiver<u64>,
) {
    let route = &routes[i];
    let mut backoff = FIRST_BACKOFF;
    loop {
        wake.mark_unchanged();
        'drain: loop {
            let items = match store
                .outbox_pending_where(OutboxSelector::Prefix(route.prefix.into()), LANE_PAGE)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(prefix = route.prefix, error = %e, "取 outbox 条目失败，稍后再试");
                    tokio::time::sleep(backoff).await;
                    continue;
                }
            };
            if items.is_empty() {
                break 'drain;
            }
            for item in items {
                match deliver(store.as_ref(), route, &item).await {
                    Next::Continue => backoff = FIRST_BACKOFF,
                    Next::Backoff => {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        continue 'drain;
                    }
                }
            }
        }
        if wake.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

/// 投递一条：次数用完就标 `Failed`；否则 `Dispatched` → 在时限内等结果。
async fn deliver<St: StoragePort>(store: &St, route: &OutboxRoute, item: &OutboxItem) -> Next {
    let mark = |state: DeliveryState| async move {
        if let Err(e) = store.outbox_mark(item.id, state).await {
            tracing::error!(id = item.id.0, ?state, error = %e, "记 outbox 结果失败");
        }
    };
    if item.attempts >= route.max_attempts {
        tracing::error!(id = item.id.0, target = %item.target.0, attempts = item.attempts, "outbox 条目次数用完，标为失败");
        mark(DeliveryState::Failed).await;
        return Next::Continue;
    }
    if let Err(e) = store.outbox_mark(item.id, DeliveryState::Dispatched).await {
        tracing::error!(id = item.id.0, error = %e, "标 outbox 已派发失败");
        return Next::Backoff;
    }
    // 投递实现的同步部分在道上跑：它 panic 就是这条道 panic（整个任务由监督器重启，启动对账收尾）。
    // 返回的 future 的 panic 在这里接住，按「结果不确定」就地收尾
    let fut = route.dispatch.dispatch(item);
    let outcome = tokio::time::timeout(route.timeout, AssertUnwindSafe(fut).catch_unwind()).await;
    match outcome {
        Ok(Ok(Ok(()))) => {
            dev::pause_at(PAUSE_AFTER_DISPATCH);
            mark(DeliveryState::AcceptedByRuntime).await;
            Next::Continue
        }
        Ok(Ok(Err(e))) if e.retryable => {
            tracing::warn!(id = item.id.0, target = %item.target.0, reason = %e.message, "outbox 投递失败，可重试");
            mark(DeliveryState::Queued).await;
            Next::Backoff
        }
        Ok(Ok(Err(e))) => {
            tracing::error!(id = item.id.0, target = %item.target.0, reason = %e.message, "outbox 投递失败，不可重试");
            mark(DeliveryState::Failed).await;
            Next::Continue
        }
        Ok(Err(_panic)) => {
            tracing::error!(id = item.id.0, target = %item.target.0, "outbox 投递 panic");
            after_uncertain(store, route, item).await
        }
        Err(_elapsed) => after_uncertain(store, route, item).await,
    }
}

/// 结果不确定（超时、投递 panic）之后：改回排队的要先重投它，标失败的往下走。
async fn after_uncertain<St: StoragePort>(
    store: &St,
    route: &OutboxRoute,
    item: &OutboxItem,
) -> Next {
    match settle_uncertain(store, route, item).await {
        Ok(DeliveryState::Failed) => Next::Continue,
        Ok(_) => Next::Backoff,
        Err(e) => {
            tracing::error!(id = item.id.0, error = %e, "收尾结果不确定的投递失败");
            Next::Backoff
        }
    }
}

/// 一次「没有去向认领」的报告：新出现的条目，按 target 计数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unclaimed {
    /// (target, 条数)，按 target 排序。
    pub targets: Vec<(String, usize)>,
    pub total: usize,
}

/// 报告的去处。生产用 [`log_unclaimed`]（一条 `error` 日志）；测试换成记录器。
pub type UnclaimedSink = Arc<dyn Fn(&Unclaimed) + Send + Sync>;

/// 把报告记成一条 `error` 日志，点名各 target 与条数。
pub fn log_unclaimed() -> UnclaimedSink {
    Arc::new(|u: &Unclaimed| {
        let targets = u
            .targets
            .iter()
            .map(|(t, n)| format!("{t}×{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::error!(
            total = u.total,
            %targets,
            "outbox 有没有登记去向的条目，原样留在 queued（装回认识这个去向的版本后会照常投出）"
        );
    })
}

/// 一次最多看这么多条没人认领的（都不动，取不到后面的；超出的部分下次条目减少后才看得到）。
const UNCLAIMED_SCAN: usize = 10_000;

/// 纯函数：这次查到的条目里，上次没报过的那些。
pub fn new_unclaimed(
    seen: &std::collections::BTreeSet<u64>,
    items: &[OutboxItem],
) -> Option<Unclaimed> {
    let mut by_target: std::collections::BTreeMap<String, usize> = Default::default();
    for item in items.iter().filter(|i| !seen.contains(&i.id.0)) {
        *by_target.entry(item.target.0.clone()).or_default() += 1;
    }
    let total = by_target.values().sum();
    (total > 0).then(|| Unclaimed {
        targets: by_target.into_iter().collect(),
        total,
    })
}

/// 没有去向认领的条目：在存储里筛「不属于任何已登记前缀」的，只报不动。
/// 取到的都留在 `queued`，所以不能「取到取不到为止」：启动时查一次，之后每次叫醒查一次。
async fn unclaimed_lane<St: StoragePort>(
    store: Arc<St>,
    routes: Arc<Vec<OutboxRoute>>,
    mut wake: watch::Receiver<u64>,
    report: UnclaimedSink,
) {
    let prefixes: Vec<String> = routes.iter().map(|r| r.prefix.to_owned()).collect();
    // 上次查到的条目 id：这次只报新出现的，同一批不反复刷屏
    let mut seen = std::collections::BTreeSet::new();
    loop {
        wake.mark_unchanged();
        match store
            .outbox_pending_where(OutboxSelector::NoneOf(prefixes.clone()), UNCLAIMED_SCAN)
            .await
        {
            Ok(items) => {
                if let Some(u) = new_unclaimed(&seen, &items) {
                    report(&u);
                }
                seen = items.iter().map(|i| i.id.0).collect();
            }
            Err(e) => tracing::warn!(error = %e, "查无人认领的 outbox 条目失败，下次叫醒再查"),
        }
        if wake.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use crate::supervisor::{RestartPolicy, Supervisor, TaskSpec};
    use iwork_domain::event::GlobalSettingsChanged;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    type Seen = Arc<Mutex<Vec<u64>>>;

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Ok,
        Hang,
        AlwaysRetry,
        /// 第一次：返回的 future panic；之后正常
        PanicInFuture,
        /// 第一次：记下收到、然后在同步部分 panic（道 panic → 整个任务 panic）；之后正常
        PanicInCall,
        /// 第一次：永不返回（超时）；之后正常
        HangFirst,
    }

    struct Fake {
        mode: Mode,
        seen: Seen,
        calls: AtomicUsize,
    }

    impl OutboxDispatch for Fake {
        fn dispatch<'a>(&'a self, item: &'a OutboxItem) -> DispatchFuture<'a> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(item.id.0);
            if self.mode == Mode::PanicInCall && n == 0 {
                panic!("投递实现在同步部分 panic");
            }
            let mode = self.mode;
            Box::pin(async move {
                match mode {
                    Mode::Ok | Mode::PanicInCall => Ok(()),
                    Mode::PanicInFuture if n == 0 => panic!("投递的 future panic"),
                    Mode::PanicInFuture => Ok(()),
                    Mode::Hang => std::future::pending().await,
                    Mode::HangFirst if n == 0 => std::future::pending().await,
                    Mode::HangFirst => Ok(()),
                    Mode::AlwaysRetry => Err(DispatchError {
                        retryable: true,
                        message: "稍后再试".into(),
                    }),
                }
            })
        }
    }

    fn route(prefix: &'static str, on: OnUncertain, mode: Mode, seen: &Seen) -> OutboxRoute {
        let mut r = OutboxRoute::new(
            prefix,
            on,
            Box::new(Fake {
                mode,
                seen: seen.clone(),
                calls: AtomicUsize::new(0),
            }),
        );
        r.timeout = Duration::from_millis(200);
        r.max_attempts = 3;
        r
    }

    struct World {
        env: Env,
        store: Arc<Store>,
        wake: watch::Sender<u64>,
        next_cmd: AtomicUsize,
        /// 无人认领的报告
        reports: Arc<Mutex<Vec<Unclaimed>>>,
    }

    type Store = iwork_store::SqliteStore<iwork_app::codec::ProtoEventCodec>;

    impl World {
        fn new() -> Self {
            let env = Env::new();
            let store = Arc::new(env.open());
            Self {
                env,
                store,
                wake: watch::Sender::new(0),
                next_cmd: AtomicUsize::new(100),
                reports: Arc::default(),
            }
        }

        /// 一次提交写进这些 outbox 条目（不叫醒；叫醒由测试决定）。
        async fn add(&self, targets: &[&str]) {
            let n = self.next_cmd.fetch_add(1, Ordering::SeqCst) as u128;
            let mut req = cmd(
                n,
                vec![
                    GlobalSettingsChanged {
                        playful_ui: None,
                        autostart: None,
                        projects_root: None,
                    }
                    .into(),
                ],
            );
            req.outbox = targets.iter().map(|t| outbox(t, b"p")).collect();
            self.store.commit(req).await.unwrap();
        }

        fn reports(&self) -> Vec<Unclaimed> {
            self.reports.lock().unwrap().clone()
        }

        fn ids(&self) -> Vec<u64> {
            let conn = rusqlite::Connection::open(self.env.db()).unwrap();
            let mut st = conn.prepare("SELECT id FROM outbox ORDER BY id").unwrap();
            st.query_map([], |r| r.get::<_, i64>(0).map(|v| v as u64))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        }

        fn wake(&self) {
            self.wake.send_modify(|n| *n += 1);
        }

        fn states(&self) -> Vec<(String, String, i64)> {
            let conn = rusqlite::Connection::open(self.env.db()).unwrap();
            let mut stmt = conn
                .prepare("SELECT target, state, attempts FROM outbox ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        }

        fn count(&self, target_prefix: &str, state: &str) -> usize {
            self.states()
                .iter()
                .filter(|(t, s, _)| t.starts_with(target_prefix) && s == state)
                .count()
        }

        fn start(&self, routes: Vec<OutboxRoute>) -> tokio::task::JoinHandle<()> {
            validate(&routes).unwrap();
            let reports = self.reports.clone();
            tokio::spawn(run(
                self.store.clone(),
                Arc::new(routes),
                self.wake.clone(),
                Arc::new(move |u: &Unclaimed| reports.lock().unwrap().push(u.clone())),
            ))
        }

        async fn until(&self, what: &str, limit: Duration, f: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + limit;
            while !f(self) {
                assert!(
                    Instant::now() < deadline,
                    "{limit:?} 内没等到：{what}；{:?}",
                    self.states()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    fn seen() -> Seen {
        Arc::default()
    }

    #[test]
    fn prefixes_must_not_overlap() {
        let s = seen();
        assert!(
            validate(&[
                route("a:", OnUncertain::Fail, Mode::Ok, &s),
                route("b:", OnUncertain::Fail, Mode::Ok, &s)
            ])
            .is_ok()
        );
        let e = validate(&[
            route("a:", OnUncertain::Fail, Mode::Ok, &s),
            route("a:x", OnUncertain::Fail, Mode::Ok, &s),
        ])
        .unwrap_err();
        assert!(e.contains("a:x"), "{e}");
        assert!(validate(&[route("", OnUncertain::Fail, Mode::Ok, &s)]).is_err());
    }

    /// 1/F-4：没有登记去向的条目原样留在 `queued`（不标失败、不加次数、不派发），点名报一次。
    #[tokio::test(flavor = "multi_thread")]
    async fn unclaimed_target_is_reported_and_kept() {
        let w = World::new();
        let s = seen();
        let _t = w.start(vec![route("a:", OnUncertain::Redeliver, Mode::Ok, &s)]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        w.add(&["nobody:1"]).await;
        w.wake();
        w.until("报出来", Duration::from_secs(5), |w| {
            w.reports().len() == 1
        })
        .await;
        assert_eq!(
            w.reports()[0],
            Unclaimed {
                targets: vec![("nobody:1".into(), 1)],
                total: 1
            }
        );
        assert_eq!(
            w.states(),
            [("nobody:1".to_string(), "queued".to_string(), 0)]
        );
        assert!(s.lock().unwrap().is_empty(), "没投给任何去向");
    }

    /// S17a（1/F-4 改）：一次提交 65 条未知去向 → 全部仍是 `queued`、次数不变，一条报告点名全部；
    /// 再叫醒不重复报；之后新来的只报新的那条。
    #[tokio::test(flavor = "multi_thread")]
    async fn unclaimed_65_kept_and_reported_once() {
        let w = World::new();
        let _t = w.start(vec![]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let targets: Vec<String> = (0..65).map(|i| format!("nobody:{}", i % 5)).collect();
        let refs: Vec<&str> = targets.iter().map(String::as_str).collect();
        w.add(&refs).await;
        w.wake();
        w.until("报出来", Duration::from_secs(5), |w| {
            !w.reports().is_empty()
        })
        .await;
        let r = &w.reports()[0];
        assert_eq!(r.total, 65);
        assert_eq!(
            r.targets,
            (0..5)
                .map(|i| (format!("nobody:{i}"), 13))
                .collect::<Vec<_>>()
        );
        for _ in 0..3 {
            w.wake();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(w.reports().len(), 1, "同一批不反复报");
        assert_eq!(w.count("nobody:", "queued"), 65);
        assert!(w.states().iter().all(|(_, _, n)| *n == 0), "次数不变");
        w.add(&["later:1"]).await;
        w.wake();
        w.until("新来的报出来", Duration::from_secs(5), |w| {
            w.reports().len() == 2
        })
        .await;
        assert_eq!(w.reports()[1].targets, [("later:1".to_string(), 1)]);
    }

    /// S17a（1/F-4 改）：一条无人认领的排在 100 条某去向的条目后面（那条道卡在第一条上）→ 启动时就报出，
    /// 前面 100 条不受影响。
    #[tokio::test(flavor = "multi_thread")]
    async fn unclaimed_behind_claimed_is_reported() {
        let w = World::new();
        let s = seen();
        let mut hung = route("a:", OnUncertain::Redeliver, Mode::Hang, &s);
        hung.timeout = Duration::from_secs(600);
        let targets: Vec<String> = (0..100).map(|i| format!("a:{i}")).collect();
        let refs: Vec<&str> = targets.iter().map(String::as_str).collect();
        w.add(&refs).await;
        w.add(&["nobody:x"]).await;
        let _t = w.start(vec![hung]);
        w.until("那 1 条报出来", Duration::from_secs(5), |w| {
            w.reports().len() == 1
        })
        .await;
        assert_eq!(w.reports()[0].targets, [("nobody:x".to_string(), 1)]);
        assert_eq!(w.count("nobody:", "queued"), 1);
        w.until("道卡在第一条上", Duration::from_secs(5), |w| {
            w.count("a:", "dispatched") == 1
        })
        .await;
        assert_eq!(w.count("a:", "queued"), 99);
    }

    /// S17a（1/F-4 改）：登记表为空时写下的条目，换成登记了这个去向的配置启动后，按先后全部送达（装回新版补投）。
    #[tokio::test(flavor = "multi_thread")]
    async fn unclaimed_delivered_after_route_is_registered() {
        let w = World::new();
        w.add(&["a:1", "a:2", "a:3"]).await;
        let old = w.start(vec![]);
        w.until("报出来", Duration::from_secs(5), |w| {
            w.reports().len() == 1
        })
        .await;
        old.abort();
        let _ = old.await;
        assert_eq!(w.count("a:", "queued"), 3);
        let s = seen();
        let _new = w.start(vec![route("a:", OnUncertain::Redeliver, Mode::Ok, &s)]);
        w.until("全部送达", Duration::from_secs(5), |w| {
            w.count("a:", "accepted_by_runtime") == 3
        })
        .await;
        assert_eq!(*s.lock().unwrap(), w.ids(), "按先后");
    }

    #[test]
    fn new_unclaimed_reports_only_new_items() {
        let item = |id: u64, t: &str| OutboxItem {
            id: iwork_ports::store::OutboxId(id),
            target: iwork_ports::store::OutboxTarget(t.into()),
            payload: vec![],
            state: DeliveryState::Queued,
            attempts: 0,
        };
        let items = [item(1, "x"), item(2, "y"), item(3, "x")];
        let none = std::collections::BTreeSet::new();
        assert_eq!(
            new_unclaimed(&none, &items),
            Some(Unclaimed {
                targets: vec![("x".into(), 2), ("y".into(), 1)],
                total: 3
            })
        );
        let seen: std::collections::BTreeSet<u64> = [1, 2, 3].into();
        assert_eq!(new_unclaimed(&seen, &items), None);
        let seen: std::collections::BTreeSet<u64> = [1, 2].into();
        assert_eq!(
            new_unclaimed(&seen, &items).unwrap().targets,
            [("x".to_string(), 1)]
        );
    }

    /// 某个去向一次来 40 条、只叫醒一次 → 全部送达（取到取不到为止，不止一页）。
    #[tokio::test(flavor = "multi_thread")]
    async fn lane_drains_more_than_one_page() {
        let w = World::new();
        let s = seen();
        let _t = w.start(vec![route("a:", OnUncertain::Redeliver, Mode::Ok, &s)]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let targets: Vec<String> = (0..40).map(|i| format!("a:{i}")).collect();
        let refs: Vec<&str> = targets.iter().map(String::as_str).collect();
        w.add(&refs).await;
        w.wake();
        w.until("40 条送达", Duration::from_secs(5), |w| {
            w.count("a:", "accepted_by_runtime") == 40
        })
        .await;
        assert_eq!(s.lock().unwrap().len(), 40);
    }

    /// S18：去向 A 永不返回，B 正常：B 立刻送达；A 的第一条在时限后按 `Redeliver` 退避重投，最多派发 3 次后失败，
    /// 之后才轮到 A 的下一条。
    #[tokio::test(flavor = "multi_thread")]
    async fn hung_route_does_not_block_others() {
        let w = World::new();
        let (sa, sb) = (seen(), seen());
        w.add(&["a:1", "a:2", "b:1"]).await;
        let started = Instant::now();
        let _t = w.start(vec![
            route("a:", OnUncertain::Redeliver, Mode::Hang, &sa),
            route("b:", OnUncertain::Redeliver, Mode::Ok, &sb),
        ]);
        w.until("B 送达", Duration::from_secs(5), |w| {
            w.count("b:", "accepted_by_runtime") == 1
        })
        .await;
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "B 没等 A 的时限"
        );
        // 每次超时后改回排队、按可重试的退避（1s、2s、4s）重取：两条各 3 次，约 15s
        w.until("A 两条都失败", Duration::from_secs(30), |w| {
            w.count("a:", "failed") == 2
        })
        .await;
        let a = sa.lock().unwrap().clone();
        assert_eq!(a.len(), 6, "每条最多派发 3 次：{a:?}");
        assert!(
            a[..3].iter().all(|x| *x == a[0]) && a[3..].iter().all(|x| *x == a[3]) && a[0] < a[3],
            "同一去向保持先后：第一条投完（失败）才轮到第二条：{a:?}"
        );
        assert!(
            w.states()
                .iter()
                .filter(|(t, ..)| t.starts_with("a:"))
                .all(|(_, _, n)| *n == 3)
        );
    }

    /// 方案 2.11「同一个去向保持先后」：第一条结果不确定（超时、投递 panic）被改回排队后，要先把它重投完，
    /// 才轮到后面的条目；接收方看到的次序是 [1, 1, 2]，不是 [1, 2, 1]。
    #[tokio::test(flavor = "multi_thread")]
    async fn uncertain_redelivery_keeps_order() {
        for mode in [Mode::HangFirst, Mode::PanicInFuture] {
            let w = World::new();
            let s = seen();
            w.add(&["a:1", "a:2"]).await;
            let ids: Vec<u64> = {
                let conn = rusqlite::Connection::open(w.env.db()).unwrap();
                let mut st = conn.prepare("SELECT id FROM outbox ORDER BY id").unwrap();
                st.query_map([], |r| r.get::<_, i64>(0).map(|v| v as u64))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap()
            };
            let _t = w.start(vec![route("a:", OnUncertain::Redeliver, mode, &s)]);
            w.until("两条都送达", Duration::from_secs(10), |w| {
                w.count("a:", "accepted_by_runtime") == 2
            })
            .await;
            assert_eq!(
                *s.lock().unwrap(),
                [ids[0], ids[0], ids[1]],
                "{}",
                if mode == Mode::HangFirst {
                    "超时"
                } else {
                    "panic"
                }
            );
        }
    }

    /// 对照：不能幂等的去向结果不确定时标失败，道照常往下走，不重投。
    #[tokio::test(flavor = "multi_thread")]
    async fn uncertain_fail_moves_on() {
        let w = World::new();
        let s = seen();
        w.add(&["a:1", "a:2"]).await;
        let _t = w.start(vec![route("a:", OnUncertain::Fail, Mode::HangFirst, &s)]);
        w.until("一条失败一条送达", Duration::from_secs(5), |w| {
            w.count("a:", "failed") == 1 && w.count("a:", "accepted_by_runtime") == 1
        })
        .await;
        assert_eq!(s.lock().unwrap().len(), 2);
    }

    /// S18：不能幂等的去向超时一次就失败，不重投。
    #[tokio::test(flavor = "multi_thread")]
    async fn timeout_never_redelivers_non_idempotent() {
        let w = World::new();
        let s = seen();
        w.add(&["a:1"]).await;
        let _t = w.start(vec![route("a:", OnUncertain::Fail, Mode::Hang, &s)]);
        w.until("失败", Duration::from_secs(5), |w| {
            w.count("a:", "failed") == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(s.lock().unwrap().len(), 1, "只派发一次");
    }

    /// S18：永远报可重试的错误 → 派发 `max_attempts` 次后失败，道继续处理下一条。
    #[tokio::test(flavor = "multi_thread")]
    async fn always_retryable_is_bounded() {
        let w = World::new();
        let s = seen();
        w.add(&["a:1"]).await;
        let _t = w.start(vec![route(
            "a:",
            OnUncertain::Redeliver,
            Mode::AlwaysRetry,
            &s,
        )]);
        // 3 次派发之间退避 1s、2s
        w.until("失败", Duration::from_secs(10), |w| {
            w.count("a:", "failed") == 1
        })
        .await;
        assert_eq!(s.lock().unwrap().len(), 3);
    }

    /// S17：投递的 future panic → 就地按「结果不确定」收尾（`Redeliver` → 重投一次后送达），任务不重启。
    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_panic_is_settled_in_place() {
        let w = World::new();
        let s = seen();
        w.add(&["a:1"]).await;
        let t = w.start(vec![route(
            "a:",
            OnUncertain::Redeliver,
            Mode::PanicInFuture,
            &s,
        )]);
        w.until("重投后送达", Duration::from_secs(5), |w| {
            w.count("a:", "accepted_by_runtime") == 1
        })
        .await;
        assert_eq!(s.lock().unwrap().len(), 2);
        assert!(!t.is_finished(), "任务没有因此结束");
    }

    /// S17：投递在「已派发」之后让整个投递任务 panic，监督器只重启这个任务：重启后先对账——
    /// `Redeliver` 的条目再投一次（接收方按 id 去重后执行一次），`Fail` 的变失败、不重投。全程不重启进程。
    #[tokio::test(flavor = "multi_thread")]
    async fn task_restart_reconciles_dispatched() {
        for (on, want_state, want_deliveries) in [
            (OnUncertain::Redeliver, "accepted_by_runtime", 2),
            (OnUncertain::Fail, "failed", 1),
        ] {
            let w = World::new();
            let s = seen();
            w.add(&["a:1"]).await;
            let starts = Arc::new(AtomicUsize::new(0));
            // 去向的状态（第一次 panic）要跨任务重启保留：只建一次
            let routes = Arc::new(vec![route("a:", on, Mode::PanicInCall, &s)]);
            let (store, wake, st) = (w.store.clone(), w.wake.clone(), starts.clone());
            let mut sup = Supervisor::start(
                vec![TaskSpec::restartable("outbox", move || {
                    st.fetch_add(1, Ordering::SeqCst);
                    run(store.clone(), routes.clone(), wake.clone(), log_unclaimed())
                })],
                RestartPolicy::default(),
            );
            let watcher = tokio::spawn(async move { sup.watch().await });
            w.until("对账后收尾", Duration::from_secs(5), |w| {
                w.count("a:", want_state) == 1
            })
            .await;
            assert_eq!(starts.load(Ordering::SeqCst), 2, "任务被重启了一次");
            let deliveries = s.lock().unwrap().clone();
            assert_eq!(deliveries.len(), want_deliveries, "{on:?}：{deliveries:?}");
            let unique: std::collections::BTreeSet<_> = deliveries.iter().collect();
            assert_eq!(unique.len(), 1, "接收方去重后只执行一次");
            watcher.abort();
        }
    }
}
