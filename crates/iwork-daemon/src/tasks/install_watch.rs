//! 任务 `install-watch`（可重启）：磁盘上装齐了另一版就自行重启（T04 方案 2.9）。
//!
//! - 只有一种判定动作 [`evaluate`]，每次都把两问放在一起答、不拿上一次的答案接着用；第二问答「装齐」之后、
//!   动手之前再答一次第一问（磁盘可能已经恢复成我这一版）。
//! - 定时只决定什么时候调它：启动时一次；`Unchanged` → 30s 后；`Installing` → 2s、4s、8s、16s，之后每 30s。
//!   新客户端被拒（审计旁路 `PeerRejectedByServer`，最多每秒一次）、睡醒（`sleep-watch`）时立刻再调。
//! - 冷却：每个进程启动后 30s 内不做升级重启；到点**重新**调一次 `evaluate`，照那一次的结论做。计时基准只有本进程
//!   启动表第 1 步记下的单调读数，不靠上一个进程传话。
//! - 「安装中」持续超过 5 分钟记一条 `warn`（写明各形态卡在哪），之后每小时一条。
//! - 任务没有要保存的状态：参照物是运行映像的身份（启动快照），其余每次从磁盘重新看。被监督器重启后结论不变。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iwork_ipc::peer::{ImageId, SelfIdentity};
use iwork_ipc::rendezvous::StopReason;
use tokio::sync::{Notify, broadcast};
use tokio::time::Instant;

use crate::install::{self, Completeness, PackageForm};
use crate::logging::AuditSignal;
use crate::stop::StopSwitch;

/// 计时常量（方案 2.9）。
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub unchanged: Duration,
    pub installing: [Duration; 4],
    pub installing_steady: Duration,
    pub cooldown: Duration,
    pub warn_after: Duration,
    pub warn_every: Duration,
    pub accel_min_gap: Duration,
}

impl Timing {
    pub const REAL: Timing = Timing {
        unchanged: Duration::from_secs(30),
        installing: [
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
        ],
        installing_steady: Duration::from_secs(30),
        cooldown: Duration::from_secs(30),
        warn_after: Duration::from_secs(5 * 60),
        warn_every: Duration::from_secs(3600),
        accel_min_gap: Duration::from_secs(1),
    };

    /// 全部时长除以 `div`（开发构建的真进程测试用，见 `IWORK_DEV_INSTALL_FAST`；单测用暂停的时钟量真值）。
    pub fn scaled(div: u32) -> Timing {
        let d = |x: Duration| x / div.max(1);
        let r = Timing::REAL;
        Timing {
            unchanged: d(r.unchanged),
            installing: r.installing.map(d),
            installing_steady: d(r.installing_steady),
            cooldown: d(r.cooldown),
            warn_after: d(r.warn_after),
            warn_every: d(r.warn_every),
            accel_min_gap: d(r.accel_min_gap),
        }
    }

    /// 第 `n` 次（从 0 起）连续判出「安装中」之后等多久。
    pub fn installing_delay(&self, n: usize) -> Duration {
        self.installing
            .get(n)
            .copied()
            .unwrap_or(self.installing_steady)
    }
}

/// 两问的来源。真实现读磁盘；测试换成假的。
pub trait Disk: Send + Sync + 'static {
    /// 第一问：磁盘上的和我还是同一份吗？
    fn same_as_running(&self) -> bool;
    /// 第二问：磁盘上有没有装齐的包？每次都完整地查。
    fn completeness(&self) -> Completeness;
}

/// 真实的磁盘。
pub struct RealDisk {
    pub daemon_path: PathBuf,
    pub running: ImageId,
    pub own: SelfIdentity,
    pub forms: Vec<Box<dyn PackageForm>>,
}

impl Disk for RealDisk {
    fn same_as_running(&self) -> bool {
        let view = install::read_disk(&self.daemon_path, &self.running);
        install::same_as_running(&self.running, &self.own, &view)
    }

    fn completeness(&self) -> Completeness {
        let dir = self
            .daemon_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default();
        install::installed_complete(&self.forms, &dir)
    }
}

/// 一次判定的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Unchanged,
    Installing(Completeness),
    Restart,
}

/// 唯一的判定动作：先答第一问，「是」就结束；否则完整答第二问，没装齐就是安装中；装齐了，动手之前再答一次第一问。
pub fn evaluate(disk: &impl Disk) -> Outcome {
    if disk.same_as_running() {
        return Outcome::Unchanged;
    }
    let c = disk.completeness();
    if !c.complete {
        return Outcome::Installing(c);
    }
    // 第二问可能花了几百毫秒：期间磁盘恢复成我这一版的话就不动
    if disk.same_as_running() {
        return Outcome::Unchanged;
    }
    Outcome::Restart
}

fn describe(c: &Completeness) -> String {
    c.per_form
        .iter()
        .map(|(name, v)| format!("{name}: {v:?}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// 任务体。`started` 是本进程启动表第 1 步的单调读数。判出要重启时请求「升级」停机后就不再做事。
pub async fn run<D: Disk>(
    disk: Arc<D>,
    timing: Timing,
    started: Instant,
    stop: StopSwitch,
    mut audit: broadcast::Receiver<AuditSignal>,
    woke: Arc<Notify>,
) {
    let mut installing_streak = 0usize;
    let mut installing_since: Option<Instant> = None;
    let mut last_warn: Option<Instant> = None;
    let mut last_accel: Option<Instant> = None;
    loop {
        let d = disk.clone();
        let outcome = tokio::task::spawn_blocking(move || evaluate(d.as_ref()))
            .await
            .expect("安装检查的判定 panic");
        let delay = match outcome {
            Outcome::Unchanged => {
                if installing_since.take().is_some() {
                    tracing::info!("离开「安装中」：磁盘上的后台就是我");
                }
                installing_streak = 0;
                last_warn = None;
                timing.unchanged
            }
            Outcome::Installing(c) => {
                let now = Instant::now();
                let since = *installing_since.get_or_insert_with(|| {
                    tracing::info!(forms = %describe(&c), "安装中：磁盘上的后台不是我，也还没有装齐的包");
                    now
                });
                tracing::debug!(n = installing_streak, forms = %describe(&c), "安装检查：完整重查");
                let due = match last_warn {
                    None => now.duration_since(since) >= timing.warn_after,
                    Some(w) => now.duration_since(w) >= timing.warn_every,
                };
                if due {
                    tracing::warn!(
                        installing_secs = now.duration_since(since).as_secs(),
                        forms = %describe(&c),
                        "安装中持续太久：磁盘上的后台不是我，也没有装齐的包"
                    );
                    last_warn = Some(now);
                }
                let d = timing.installing_delay(installing_streak);
                installing_streak += 1;
                d
            }
            Outcome::Restart => {
                let cooled = started + timing.cooldown;
                if Instant::now() < cooled {
                    tracing::info!("磁盘上装齐了另一版，本进程启动不到冷却时间，到点重新判定");
                    tokio::time::sleep_until(cooled).await;
                    continue;
                }
                tracing::info!("磁盘上装齐了另一版，自行重启");
                stop.request(StopReason::Upgrade);
                return std::future::pending().await;
            }
        };
        // 等到点，或被加速
        let deadline = Instant::now() + delay;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => break,
                () = woke.notified() => break,
                sig = audit.recv() => match sig {
                    Ok(AuditSignal::PeerRejectedByServer) => {
                        let now = Instant::now();
                        if last_accel.is_none_or(|t| now.duration_since(t) >= timing.accel_min_gap) {
                            last_accel = Some(now);
                            tracing::info!("有客户端被拒，立刻做一次安装检查");
                            break;
                        }
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {
                        tokio::time::sleep_until(deadline).await;
                        break;
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::FormVerdict;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 假磁盘：第一问按脚本依次回答（用完后重复最后一个），第二问同样；记下各问了几次。
    struct Script {
        first: Mutex<Vec<bool>>,
        second: Mutex<Vec<bool>>,
        firsts: AtomicUsize,
        seconds: AtomicUsize,
    }

    impl Script {
        fn new(first: &[bool], second: &[bool]) -> Arc<Self> {
            Arc::new(Self {
                first: Mutex::new(first.to_vec()),
                second: Mutex::new(second.to_vec()),
                firsts: AtomicUsize::new(0),
                seconds: AtomicUsize::new(0),
            })
        }
        fn next(v: &Mutex<Vec<bool>>) -> bool {
            let mut v = v.lock().unwrap();
            if v.len() > 1 { v.remove(0) } else { v[0] }
        }
        fn set(&self, first: &[bool], second: &[bool]) {
            *self.first.lock().unwrap() = first.to_vec();
            *self.second.lock().unwrap() = second.to_vec();
        }
    }

    impl Disk for Script {
        fn same_as_running(&self) -> bool {
            self.firsts.fetch_add(1, Ordering::SeqCst);
            Self::next(&self.first)
        }
        fn completeness(&self) -> Completeness {
            self.seconds.fetch_add(1, Ordering::SeqCst);
            let complete = Self::next(&self.second);
            Completeness {
                complete,
                per_form: vec![(
                    "fake",
                    if complete {
                        FormVerdict::Complete
                    } else {
                        FormVerdict::Incomplete("还差文件".into())
                    },
                )],
            }
        }
    }

    struct Task {
        stop: StopSwitch,
        audit: broadcast::Sender<AuditSignal>,
        woke: Arc<Notify>,
        handle: tokio::task::JoinHandle<()>,
    }

    fn start(disk: Arc<Script>, started: Instant) -> Task {
        let stop = StopSwitch::new();
        let (audit, rx) = broadcast::channel(8);
        let woke = Arc::new(Notify::new());
        let handle = tokio::spawn(run(
            disk,
            Timing::REAL,
            started,
            stop.clone(),
            rx,
            woke.clone(),
        ));
        Task {
            stop,
            audit,
            woke,
            handle,
        }
    }

    /// S7f：第一次读答「否」、第二问「装齐」、动手前再读答「是」→ Unchanged，不停机。
    #[test]
    fn evaluate_rereads_first_question_before_restart() {
        let d = Script::new(&[false, true], &[true]);
        assert_eq!(evaluate(d.as_ref()), Outcome::Unchanged);
        assert_eq!(d.firsts.load(Ordering::SeqCst), 2);
        // 对照：再读仍是「否」→ Restart
        let d = Script::new(&[false], &[true]);
        assert_eq!(evaluate(d.as_ref()), Outcome::Restart);
    }

    /// S7f：连调两次，中间磁盘变了：第二次的结论只取决于第二次读到的。
    #[test]
    fn evaluate_never_reuses_previous_answer() {
        let d = Script::new(&[false], &[false]);
        assert!(matches!(evaluate(d.as_ref()), Outcome::Installing(_)));
        d.set(&[false], &[true]);
        assert_eq!(evaluate(d.as_ref()), Outcome::Restart);
        d.set(&[true], &[true]);
        assert_eq!(evaluate(d.as_ref()), Outcome::Unchanged);
        assert_eq!(
            d.seconds.load(Ordering::SeqCst),
            2,
            "第一问是「是」时不问第二问"
        );
    }

    /// S7c：第 N 次完整重查才装齐（磁盘上没有任何 stat 变化，假磁盘里根本没有文件）→ 照样在那次之后重启。
    /// 重查间隔是 2s、4s、8s、16s，之后每 30s。
    #[tokio::test(start_paused = true)]
    async fn recheck_does_not_depend_on_stat() {
        let started = Instant::now() - Duration::from_secs(3600);
        let d = Script::new(&[false], &[false, false, false, false, false, true]);
        let t = start(d.clone(), started);
        let t0 = Instant::now();
        let reason = tokio::time::timeout(Duration::from_secs(600), t.stop.wait())
            .await
            .expect("应在第 6 次重查后重启");
        assert_eq!(reason, StopReason::Upgrade);
        assert_eq!(d.seconds.load(Ordering::SeqCst), 6);
        assert_eq!(t0.elapsed(), Duration::from_secs(2 + 4 + 8 + 16 + 30));
        t.handle.abort();
    }

    /// S7：装到一半被丢下不管：完整重查的间隔有界（2、4、8、16，之后每 30s），没有更密。
    #[tokio::test(start_paused = true)]
    async fn abandoned_install_rechecks_are_bounded() {
        let d = Script::new(&[false], &[false]);
        let t = start(d.clone(), Instant::now() - Duration::from_secs(3600));
        tokio::time::sleep(Duration::from_secs(180) - Duration::from_millis(1)).await;
        // 0、2、6、14、30、60、90、120、150 → 9 次
        assert_eq!(d.seconds.load(Ordering::SeqCst), 9);
        t.handle.abort();
    }

    /// S7/S7g：进程启动后 30s 内判出 Restart 不动手；到点重新判定，照新结论做——到点时磁盘已恢复就不重启。
    #[tokio::test(start_paused = true)]
    async fn no_upgrade_restart_within_30s_of_own_start() {
        let started = Instant::now();
        let d = Script::new(&[false], &[true]);
        let t = start(d.clone(), started);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(t.stop.requested(), None, "冷却期内不动手");
        let reason = t.stop.wait().await;
        assert_eq!(reason, StopReason::Upgrade);
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(30),
            "到点重新判定后才重启"
        );
        t.handle.abort();

        // 到点时磁盘已恢复成我这一版 → 不重启
        let started = Instant::now();
        let d = Script::new(&[false], &[true]);
        let t = start(d.clone(), started);
        tokio::time::sleep(Duration::from_secs(10)).await;
        d.set(&[true], &[true]);
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(t.stop.requested(), None);
        t.handle.abort();
    }

    /// 任务被监督器重启后结论不变：它没有自己的状态（同一份磁盘，起两次，第一次判定的结论相同）。
    #[tokio::test(start_paused = true)]
    async fn task_restart_same_verdict() {
        let old = Instant::now() - Duration::from_secs(3600);
        for _ in 0..2 {
            let d = Script::new(&[false], &[true]);
            let t = start(d, old);
            assert_eq!(t.stop.wait().await, StopReason::Upgrade);
            t.handle.abort();
        }
    }

    /// 新客户端被拒 → 立刻再判一次（最多每秒一次）；睡醒同样。
    #[tokio::test(start_paused = true)]
    async fn peer_rejected_speeds_up_check() {
        let d = Script::new(&[true], &[false]);
        let t = start(d.clone(), Instant::now() - Duration::from_secs(3600));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(d.firsts.load(Ordering::SeqCst), 1);
        t.audit.send(AuditSignal::PeerRejectedByServer).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(d.firsts.load(Ordering::SeqCst), 2, "被拒后立刻判");
        t.audit.send(AuditSignal::PeerRejectedByServer).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(d.firsts.load(Ordering::SeqCst), 2, "1 秒内不再加速");
        tokio::time::sleep(Duration::from_secs(1)).await;
        t.audit.send(AuditSignal::PeerRejectedByServer).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(d.firsts.load(Ordering::SeqCst), 3);
        t.woke.notify_one();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(d.firsts.load(Ordering::SeqCst), 4, "睡醒后立刻判");
        t.handle.abort();
    }

    #[test]
    fn scaled_timing_divides_everything() {
        let t = Timing::scaled(10);
        assert_eq!(t.unchanged, Duration::from_secs(3));
        assert_eq!(t.cooldown, Duration::from_secs(3));
        assert_eq!(t.installing_delay(0), Duration::from_millis(200));
        assert_eq!(t.installing_delay(9), Duration::from_secs(3));
    }
}
