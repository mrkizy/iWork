//! 长期任务监督器（T04 方案 2.10；设计 10.4；卡范围 8）。
//!
//! 任务登记表里每个任务一行，策略两种：
//! - **致命**：意外返回或 panic → 整体退出（停机「故障」列，逻辑码 20）。`ipc-serve`、`store-health`、`publish-check`。
//! - **可重启**：panic 或意外返回 → `error` 日志 → 退避后只重起这一个（200ms 起翻倍，上限 10s；稳定运行 60s 后清零）；
//!   60s 内同一个任务失败 5 次 → 升级为整体退出。
//!
//! 可重启的任务每次启动都从真值重建自己的状态（方案 2.10 的逐任务核对表）；监督器保证旧的那一次已经结束
//! （future 已完成或已被丢弃，它名下 `JoinSet` 里的子任务随之取消）才起新的。

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::task::{AbortHandle, Id, JoinSet};
use tokio::time::Instant;

pub type TaskFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Fatal,
    Restartable,
}

enum Make {
    /// 致命任务只起一次（如 `serve` 消费了 `Server`）。
    Once(Option<TaskFuture>),
    /// 可重启任务：每次重起都新造一个 future。
    Repeat(Box<dyn FnMut() -> TaskFuture + Send>),
}

/// 登记表的一行。
pub struct TaskSpec {
    pub name: &'static str,
    make: Make,
}

impl TaskSpec {
    pub fn fatal(name: &'static str, fut: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            name,
            make: Make::Once(Some(Box::pin(fut))),
        }
    }

    pub fn restartable<F, Fut>(name: &'static str, mut make: F) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self {
            name,
            make: Make::Repeat(Box::new(move || Box::pin(make()))),
        }
    }

    pub fn policy(&self) -> Policy {
        match self.make {
            Make::Once(_) => Policy::Fatal,
            Make::Repeat(_) => Policy::Restartable,
        }
    }
}

/// 重启策略的常量（方案 2.10）。
#[derive(Debug, Clone, Copy)]
pub struct RestartPolicy {
    pub first_delay: Duration,
    pub max_delay: Duration,
    /// 稳定运行这么久后退避清零。
    pub stable_after: Duration,
    /// 这么长的窗口里失败 `max_failures` 次就升级。
    pub window: Duration,
    pub max_failures: usize,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            first_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(10),
            stable_after: Duration::from_secs(60),
            window: Duration::from_secs(60),
            max_failures: 5,
        }
    }
}

/// 要整体退出的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// 致命任务意外返回或 panic。
    FatalTaskEnded {
        task: &'static str,
        panic: Option<String>,
    },
    /// 可重启任务在窗口内失败太多次。
    CrashLoop { task: &'static str },
}

impl Fault {
    pub fn task(&self) -> &'static str {
        match self {
            Fault::FatalTaskEnded { task, .. } | Fault::CrashLoop { task } => task,
        }
    }
}

struct Entry {
    spec: TaskSpec,
    abort: Option<AbortHandle>,
    running_since: Instant,
    delay: Duration,
    failures: VecDeque<Instant>,
    done: bool,
}

pub struct Supervisor {
    set: JoinSet<()>,
    ids: HashMap<Id, usize>,
    entries: Vec<Entry>,
    policy: RestartPolicy,
}

fn panic_text(e: tokio::task::JoinError) -> Option<String> {
    if !e.is_panic() {
        return None;
    }
    let p = e.into_panic();
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .or_else(|| Some("<非字符串 panic>".into()))
}

impl Supervisor {
    /// 按登记表起全部任务（启动表第 11 步）。
    pub fn start(specs: Vec<TaskSpec>, policy: RestartPolicy) -> Self {
        let mut s = Self {
            set: JoinSet::new(),
            ids: HashMap::new(),
            entries: Vec::new(),
            policy,
        };
        for spec in specs {
            s.entries.push(Entry {
                spec,
                abort: None,
                running_since: Instant::now(),
                delay: policy.first_delay,
                failures: VecDeque::new(),
                done: false,
            });
            let i = s.entries.len() - 1;
            s.spawn(i, Duration::ZERO);
        }
        s
    }

    fn spawn(&mut self, i: usize, delay: Duration) {
        let entry = &mut self.entries[i];
        let fut = match &mut entry.spec.make {
            Make::Once(f) => f.take().expect("致命任务只起一次"),
            Make::Repeat(make) => make(),
        };
        let name = entry.spec.name;
        entry.running_since = Instant::now() + delay;
        let abort = self.set.spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            tracing::debug!(task = name, "任务启动");
            fut.await;
        });
        self.ids.insert(abort.id(), i);
        self.entries[i].abort = Some(abort);
    }

    /// 等到出现要整体退出的故障为止；可重启任务的失败在这里就地重起。取消安全，可以放进 `select!`。
    pub async fn watch(&mut self) -> Fault {
        loop {
            let Some(joined) = self.set.join_next_with_id().await else {
                // 没有任务了：登记表为空，永远不会有故障
                return std::future::pending().await;
            };
            let (id, panic) = match joined {
                Ok((id, ())) => (id, None),
                Err(e) => {
                    let id = e.id();
                    if e.is_cancelled() {
                        self.ids.remove(&id);
                        continue;
                    }
                    (id, panic_text(e))
                }
            };
            let Some(i) = self.ids.remove(&id) else {
                continue;
            };
            if let Some(fault) = self.on_task_ended(i, panic) {
                return fault;
            }
        }
    }

    fn on_task_ended(&mut self, i: usize, panic: Option<String>) -> Option<Fault> {
        let policy = self.policy;
        let entry = &mut self.entries[i];
        let name = entry.spec.name;
        match entry.spec.policy() {
            Policy::Fatal => {
                entry.done = true;
                tracing::error!(
                    task = name,
                    panic = panic.as_deref(),
                    "致命任务结束，整体退出"
                );
                Some(Fault::FatalTaskEnded { task: name, panic })
            }
            Policy::Restartable => {
                let now = Instant::now();
                tracing::error!(
                    task = name,
                    panic = panic.as_deref(),
                    "任务意外结束，退避后重起"
                );
                if now.duration_since(entry.running_since) >= policy.stable_after {
                    entry.delay = policy.first_delay;
                }
                entry.failures.push_back(now);
                while entry
                    .failures
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > policy.window)
                {
                    entry.failures.pop_front();
                }
                if entry.failures.len() >= policy.max_failures {
                    entry.done = true;
                    tracing::error!(
                        task = name,
                        failures = entry.failures.len(),
                        "任务反复崩溃，整体退出"
                    );
                    return Some(Fault::CrashLoop { task: name });
                }
                let delay = entry.delay;
                entry.delay = (entry.delay * 2).min(policy.max_delay);
                self.spawn(i, delay);
                None
            }
        }
    }

    /// 停机第 4 步：停掉除 `keep` 之外的全部任务，等它们真的结束（最多 `limit`）。
    pub async fn stop_all_except(&mut self, keep: &str, limit: Duration) {
        let mut waiting = Vec::new();
        for e in &mut self.entries {
            if e.spec.name != keep
                && let Some(a) = e.abort.take()
            {
                a.abort();
                waiting.push(a.id());
            }
        }
        let wait = async {
            while !waiting.is_empty() {
                match self.set.join_next_with_id().await {
                    Some(Ok((id, ()))) => waiting.retain(|w| *w != id),
                    Some(Err(e)) => waiting.retain(|w| *w != e.id()),
                    None => break,
                }
            }
        };
        if tokio::time::timeout(limit, wait).await.is_err() {
            tracing::warn!("停可重启任务到点还没全部结束");
        }
    }

    /// 停机第 5 步：等名为 `name` 的任务结束（它的停止由调用方触发，如取消 `serve` 的 token），最多 `limit`。
    pub async fn wait_for(&mut self, name: &str, limit: Duration) -> bool {
        let Some(target) = self
            .entries
            .iter()
            .find(|e| e.spec.name == name)
            .and_then(|e| e.abort.as_ref().map(AbortHandle::id))
        else {
            return true;
        };
        let wait = async {
            loop {
                match self.set.join_next_with_id().await {
                    Some(Ok((id, ()))) if id == target => return,
                    Some(Err(e)) if e.id() == target => return,
                    Some(_) => {}
                    None => return,
                }
            }
        };
        tokio::time::timeout(limit, wait).await.is_ok()
    }

    /// 全部任务（停机收尾或测试结束）。
    pub fn abort_all(&mut self) {
        self.set.abort_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// S13：一个可重启任务 panic，只有它被重起；别的任务不受影响。
    #[tokio::test(start_paused = true)]
    async fn panicking_task_restarts_alone() {
        let flaky_runs = Arc::new(AtomicUsize::new(0));
        let steady_runs = Arc::new(AtomicUsize::new(0));
        let (f, s) = (flaky_runs.clone(), steady_runs.clone());
        let mut sup = Supervisor::start(
            vec![
                TaskSpec::restartable("flaky", move || {
                    let n = f.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if n < 2 {
                            panic!("第 {n} 次");
                        }
                        std::future::pending::<()>().await;
                    }
                }),
                TaskSpec::restartable("steady", move || {
                    s.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>()
                }),
            ],
            RestartPolicy::default(),
        );
        let r = tokio::time::timeout(Duration::from_secs(30), sup.watch()).await;
        assert!(r.is_err(), "两次 panic 不该升级：{r:?}");
        assert_eq!(
            flaky_runs.load(Ordering::SeqCst),
            3,
            "panic 两次后第三次稳定"
        );
        assert_eq!(steady_runs.load(Ordering::SeqCst), 1, "别的任务没被重起");
        sup.abort_all();
    }

    /// S13：60s 内同一个任务失败 5 次 → 升级为整体退出；退避是 200、400、800、1600ms。
    #[tokio::test(start_paused = true)]
    async fn crash_loop_escalates() {
        let started = Instant::now();
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let mut sup = Supervisor::start(
            vec![TaskSpec::restartable("bad", move || {
                r.fetch_add(1, Ordering::SeqCst);
                async { panic!("每次都崩") }
            })],
            RestartPolicy::default(),
        );
        let fault = sup.watch().await;
        assert_eq!(fault, Fault::CrashLoop { task: "bad" });
        assert_eq!(runs.load(Ordering::SeqCst), 5);
        let elapsed = started.elapsed();
        assert_eq!(elapsed, Duration::from_millis(200 + 400 + 800 + 1600));
    }

    /// 对照：失败间隔超过窗口（且稳定运行过）时不升级、退避清零。
    #[tokio::test(start_paused = true)]
    async fn slow_failures_do_not_escalate() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let mut sup = Supervisor::start(
            vec![TaskSpec::restartable("slow", move || {
                r.fetch_add(1, Ordering::SeqCst);
                async {
                    tokio::time::sleep(Duration::from_secs(61)).await;
                    panic!("跑了一分钟后崩");
                }
            })],
            RestartPolicy::default(),
        );
        let r = tokio::time::timeout(Duration::from_secs(10 * 62), sup.watch()).await;
        assert!(r.is_err(), "{r:?}");
        assert!(runs.load(Ordering::SeqCst) >= 9);
        sup.abort_all();
    }

    /// 致命任务意外返回 → 整体退出；panic 的信息带出来。
    #[tokio::test]
    async fn fatal_task_exit_stops_daemon() {
        let mut sup = Supervisor::start(
            vec![
                TaskSpec::restartable("other", std::future::pending::<()>),
                TaskSpec::fatal("ipc-serve", async {}),
            ],
            RestartPolicy::default(),
        );
        assert_eq!(
            sup.watch().await,
            Fault::FatalTaskEnded {
                task: "ipc-serve",
                panic: None
            }
        );
        let mut sup = Supervisor::start(
            vec![TaskSpec::fatal("store-health", async { panic!("坏了") })],
            RestartPolicy::default(),
        );
        assert_eq!(
            sup.watch().await,
            Fault::FatalTaskEnded {
                task: "store-health",
                panic: Some("坏了".into())
            }
        );
    }

    #[tokio::test]
    async fn stop_all_except_keeps_the_named_task() {
        let mut sup = Supervisor::start(
            vec![
                TaskSpec::restartable("a", std::future::pending::<()>),
                TaskSpec::fatal("ipc-serve", async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }),
            ],
            RestartPolicy::default(),
        );
        sup.stop_all_except("ipc-serve", Duration::from_secs(1))
            .await;
        assert!(sup.wait_for("ipc-serve", Duration::from_secs(1)).await);
    }
}
