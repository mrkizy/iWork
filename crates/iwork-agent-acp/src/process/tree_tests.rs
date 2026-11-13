//! 真实进程的交错测试（T06 方案 5.4a「由什么保证」）：不用真实 CLI。根进程是一个 `sh`（代替桥接，以自己为
//! 组长起），命令用 `perl` 的 `setsid` 另立会话（代替 CLI 起的 Bash 命令，k4：命令的 shell 自成会话）。
//! 先后次序用标记文件和 `clear()` 的闸门控制，是确定的；采样任务先停掉，什么时候记范围由测试决定。
//!
//! 每个测试结束时把自己起的、按设计不在范围里的进程亲手杀掉（`Leftover`），不碰别的进程。

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use super::scan;
use super::scope::Pid;
use super::tree::{GateStep, TREE_SCAN_POLL};
use super::{RuntimeLaunch, SupervisedProcess, TREE_CLEAR_LIMIT};

/// 另立会话再执行后面的命令。
const SETSID: &str = "perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV' --";

struct Fixture {
    dir: tempfile::TempDir,
    process: SupervisedProcess,
}

impl Fixture {
    /// 根进程跑 `script`；环境变量 `D` 是这次测试的目录。采样任务停掉。
    fn start(script: &str) -> Self {
        Self::start_files(&[], script)
    }

    /// 先把 `files`（名字，内容）写进测试目录，再起根进程。
    fn start_files(files: &[(&str, &str)], script: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        let (process, _pipes) = SupervisedProcess::spawn(&RuntimeLaunch {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![("D".into(), dir.path().as_os_str().to_owned())],
        })
        .unwrap();
        process.sampler.abort();
        Self { dir, process }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn touch(&self, name: &str) {
        std::fs::write(self.path(name), b"").unwrap();
    }

    /// 等标记文件出现并读出里面的进程号。
    async fn pid(&self, name: &str) -> Pid {
        wait_for(&self.path(name)).await.trim().parse().unwrap()
    }

    fn root(&self) -> Pid {
        self.process.pid() as Pid
    }

    /// 起 `clear()`，走到 `at` 那一步时先做 `act` 再放行；返回 `clear()` 的结果。
    async fn clear_with<F, Fut>(&self, at: GateStep, act: F) -> bool
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let (tx, mut rx) = mpsc::unbounded_channel::<(GateStep, oneshot::Sender<()>)>();
        let tree = self.process.unix_tree().clone();
        tree.set_gate(tx);
        let clearing = tokio::spawn(async move { tree.clear().await });
        let mut act = Some(act);
        while let Some((step, go)) = rx.recv().await {
            if step == at
                && let Some(act) = act.take()
            {
                act().await;
            }
            let _ = go.send(());
            if step == GateStep::BeforeReap {
                break;
            }
        }
        assert!(act.is_none(), "走到过 {at:?}");
        clearing.await.unwrap()
    }
}

async fn wait_for(path: &Path) -> String {
    for _ in 0..1000 {
        if let Ok(s) = std::fs::read_to_string(path)
            && s.ends_with('\n')
        {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("10 秒内没等到 {}", path.display());
}

/// 活着、不是僵尸。
fn alive(pid: Pid) -> bool {
    scan::read_table().unwrap().0.contains_key(&pid)
}

async fn wait_gone(pid: Pid) {
    for _ in 0..500 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{pid} 5 秒内没退出");
}

/// 按设计留在范围外的进程：测试结束时亲手杀掉。
struct Leftover(Vec<Pid>);

impl Drop for Leftover {
    fn drop(&mut self) {
        for pid in &self.0 {
            // SAFETY: 只发信号；这些进程号是本测试起的进程，测试期间都活着
            unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
    }
}

/// 1/F-2 的原样：命令组的组长（命令的 shell）已退出，worker 还在；第一遍扫描之后、发 SIGSTOP 之前 worker
/// fork 一个孩子（同组同会话）。孩子被清掉：`clear()` 返回 `true` 时它已经不在。
#[tokio::test]
async fn worker_forks_in_leaderless_group_after_scan() {
    let f = Fixture::start(&format!(
        r#"{SETSID} sh -c '
            sh -c "while [ ! -e $D/fork ]; do sleep 0.01; done; sleep 30 & echo \$! > $D/c.pid; while :; do sleep 1; done" &
            echo $! > $D/w.pid
            while [ ! -e $D/leader_exit ]; do sleep 0.01; done' &
        echo $! > $D/s.pid
        sleep 30"#
    ));
    let (shell, worker) = (f.pid("s.pid").await, f.pid("w.pid").await);
    f.process.record().await;
    f.touch("leader_exit");
    wait_gone(shell).await;
    assert!(alive(worker));
    let child = std::sync::Arc::new(std::sync::Mutex::new(0));
    let c = child.clone();
    let cleared = f
        .clear_with(GateStep::BeforeStop(0), || async {
            f.touch("fork");
            *c.lock().unwrap() = f.pid("c.pid").await;
        })
        .await;
    let child = *child.lock().unwrap();
    assert!(cleared);
    assert!(!alive(child) && !alive(worker), "孩子和 worker 都清掉了");
}

/// worker fork 完立刻退出，孩子被系统收养。两格：
/// - 同一个会话里还有别的已记到的成员活着（命令的 shell 还在）→ 孩子过得了见证，被清掉；
/// - worker 是那个会话里唯一记到过的成员（shell 早退了）→ 孩子不认领：它活着，`clear()` 照样返回 `true`
///   （2/F-1 带来的边界，钉成测试）。
#[tokio::test]
async fn worker_forks_then_exits_before_stop() {
    for shell_stays in [true, false] {
        let f = Fixture::start(&format!(
            r#"{SETSID} sh -c '
                sh -c "while [ ! -e $D/fork ]; do sleep 0.01; done; sleep 30 & echo \$! > $D/c.pid; exit 0" &
                echo $! > $D/w.pid
                while [ ! -e $D/leader_exit ]; do sleep 0.01; done
                sleep 30' &
            echo $! > $D/s.pid
            sleep 30"#
        ));
        let (shell, worker) = (f.pid("s.pid").await, f.pid("w.pid").await);
        f.process.record().await;
        if !shell_stays {
            // shell 退出（它的 `sleep 30` 也不在了才行：杀掉 shell 所在的这条链只留 worker）
            // SAFETY: 只发信号给本测试起的进程
            unsafe { libc::kill(shell, libc::SIGKILL) };
            wait_gone(shell).await;
            // 根进程这时还活着、worker 的父进程已经没了：重新记一次，确认 worker 仍是会话里唯一记到的
            f.process.record().await;
        }
        let child = std::sync::Arc::new(std::sync::Mutex::new(0));
        let c = child.clone();
        let cleared = f
            .clear_with(GateStep::BeforeStop(0), || async {
                f.touch("fork");
                *c.lock().unwrap() = f.pid("c.pid").await;
                wait_gone(worker).await;
            })
            .await;
        let child = *child.lock().unwrap();
        let _cleanup = Leftover(vec![child]);
        assert!(cleared, "shell_stays={shell_stays}");
        assert_eq!(alive(child), !shell_stays, "shell_stays={shell_stays}");
    }
}

/// 2/F-3：一条命令在两遍扫描之间开始并结束，留下一个后台进程；之后 `clear()`。它不在范围里：活着，
/// `clear()` 返回 `true`。对照格：同样的命令跑着时被扫到过一次 → 留下的进程被清掉。
#[tokio::test]
async fn short_lived_command_leftover_is_out_of_scope() {
    for scanned in [false, true] {
        let f = Fixture::start(&format!(
            r#"{SETSID} sh -c '
                sleep 30 & echo $! > $D/l.pid
                while [ ! -e $D/go ]; do sleep 0.01; done' &
            echo $! > $D/s.pid
            sleep 30"#
        ));
        let (shell, left) = (f.pid("s.pid").await, f.pid("l.pid").await);
        if scanned {
            f.process.record().await;
        }
        f.touch("go");
        wait_gone(shell).await;
        let _cleanup = Leftover(vec![left]);
        let cleared = f.process.kill_tree_and_wait().await;
        assert!(cleared, "scanned={scanned}");
        assert_eq!(alive(left), !scanned, "scanned={scanned}");
    }
}

/// 根进程活着时，另立会话的命令和它的孩子顺父链找得到，一起清掉。
#[tokio::test]
async fn child_in_its_own_session_is_cleared() {
    let f = Fixture::start(&format!(
        r#"{SETSID} sh -c 'sleep 30 & echo $! > $D/c.pid; wait' &
        echo $! > $D/s.pid
        wait"#
    ));
    let (shell, child) = (f.pid("s.pid").await, f.pid("c.pid").await);
    let _cleanup = Leftover(vec![shell, child]);
    assert!(f.process.kill_tree_and_wait().await);
    assert!(!alive(shell) && !alive(child));
}

/// 根进程先死，后代只在采样留下的范围里：采样记到过它们，清得掉。
#[tokio::test]
async fn orphan_recorded_by_sampler_is_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let (process, _pipes) = SupervisedProcess::spawn(&RuntimeLaunch {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            format!(
                r#"{SETSID} sh -c 'sleep 30 & echo $! > $D/c.pid; wait' &
                echo $! > $D/s.pid
                while [ ! -e $D/root_exit ]; do sleep 0.01; done"#
            )
            .into(),
        ],
        env: vec![("D".into(), dir.path().as_os_str().to_owned())],
    })
    .unwrap();
    let shell = wait_for(&dir.path().join("s.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    let child = wait_for(&dir.path().join("c.pid"))
        .await
        .trim()
        .parse()
        .unwrap();
    let _cleanup = Leftover(vec![shell, child]);
    // 等采样记到它们：根、shell、孩子都在的一次采样
    let mut samples = process.samples.clone();
    tokio::time::timeout(
        Duration::from_secs(10),
        samples.wait_for(|s| s.is_some_and(|s| s.processes >= 3)),
    )
    .await
    .expect("10 秒内采到整棵树")
    .unwrap();
    process.sampler.abort();
    std::fs::write(dir.path().join("root_exit"), b"").unwrap();
    process.exited().await;
    assert!(alive(shell) && alive(child), "对照：根进程死了，它们还在");
    assert!(process.kill_tree_and_wait().await);
    assert!(!alive(shell) && !alive(child));
}

/// 从叶子往根杀：停着的孩子在父进程被杀之前已经死了。命令的 shell P（另立会话，见证，最后才杀）下面是 Q，
/// Q 的孩子 K 另起一个进程组（同会话）。Q 一死，K 的组就成了带停止成员的孤儿组，系统会给它发 SIGHUP、
/// SIGCONT；K 装了记录这两个信号的处理函数，断言一次都没收到。
#[tokio::test]
async fn kill_order_is_leaf_first() {
    let kid = r#"
        use POSIX ();
        setpgrp(0, 0);
        my $log = "$ENV{D}/sig";
        $SIG{HUP} = sub { open my $f, '>>', $log; print $f "HUP\n" };
        $SIG{CONT} = sub { open my $f, '>>', $log; print $f "CONT\n" };
        open my $p, '>', "$ENV{D}/k.pid.tmp"; print $p "$$\n"; close $p;
        rename "$ENV{D}/k.pid.tmp", "$ENV{D}/k.pid";
        sleep 1 while 1;
    "#;
    let q = r#"perl "$D/k.pl" & echo $! > "$D/q.tmp"; echo $$ > "$D/q.pid"; wait"#;
    let f = Fixture::start_files(
        &[("k.pl", kid), ("q.sh", q)],
        &format!(r#"{SETSID} sh -c 'sh "$D/q.sh" & wait' & echo $! > $D/s.pid; wait"#),
    );
    let (shell, q, kid) = (
        f.pid("s.pid").await,
        f.pid("q.pid").await,
        f.pid("k.pid").await,
    );
    let _cleanup = Leftover(vec![shell, q, kid]);
    f.process.record().await;
    assert!(f.process.kill_tree_and_wait().await);
    assert!(!alive(shell) && !alive(q) && !alive(kid));
    let got = std::fs::read_to_string(f.path("sig")).unwrap_or_default();
    assert_eq!(got, "", "K 收到了 SIGHUP/SIGCONT：Q 先死了");
}

/// 已知限制钉成测试：两次 fork 加 `setsid`、中间进程在扫描之前退出。`clear()` 返回 `true` 而它还活着。
/// 以后范围变了，要有意识地改这条。
#[tokio::test]
async fn daemonized_descendant_is_out_of_scope() {
    // 中间进程 m 起一个另立会话的 shell，自己马上退出；那个 shell 留下 sleep 也退出
    let m = format!(r#"{SETSID} sh "$D/d.sh" & exit 0"#);
    let f = Fixture::start_files(
        &[("m.sh", &m), ("d.sh", r#"sleep 30 & echo $! > "$D/d.pid""#)],
        r#"sh "$D/m.sh"; sleep 30"#,
    );
    let daemon = f.pid("d.pid").await;
    let _cleanup = Leftover(vec![daemon]);
    assert!(f.process.kill_tree_and_wait().await);
    assert!(
        alive(daemon),
        "管不住的：中间进程退了、自己另立会话，没人记到过"
    );
}

/// 3/F-1：桥接退出后、`clear()` 返回前，它还占着进程号（僵尸，不在进程表里）；`clear()` 在期限内返回
/// `true`，僵尸没有把它卡成非空；返回后桥接才被回收。
#[tokio::test]
async fn bridge_is_not_reaped_before_clear_returns() {
    let f = Fixture::start("exit 3");
    let exit = f.process.exited().await;
    assert_eq!(exit.description, "exit status: 3");
    let root = f.root();
    // SAFETY: 信号 0 只检查进程号在不在
    let held = || unsafe { libc::kill(root, 0) } == 0;
    assert!(held() && !alive(root), "对照：退出了、还没回收");
    let started = std::time::Instant::now();
    let cleared = f
        .clear_with(GateStep::BeforeReap, || async {
            assert!(held(), "clear() 返回之前不回收");
        })
        .await;
    assert!(
        cleared && started.elapsed() < TREE_CLEAR_LIMIT,
        "{:?}",
        started.elapsed()
    );
    for _ in 0..200 {
        if !held() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("clear() 返回后 2 秒内没回收");
}

/// 退出通知先到、清树后到，中间故意拖一段：根组里的成员（根进程留下的后台进程）照样清得掉。
#[tokio::test]
async fn exit_then_clear_order() {
    let f = Fixture::start("sleep 30 & echo $! > $D/m.pid; exit 0");
    let member = f.pid("m.pid").await;
    let _cleanup = Leftover(vec![member]);
    f.process.exited().await;
    tokio::time::sleep(TREE_SCAN_POLL * 6).await;
    assert!(alive(member));
    assert!(f.process.kill_tree_and_wait().await);
    assert!(!alive(member));
}

/// 桥接在等的时候已经被别处回收（等的那一下得到 `ECHILD`）：照「已回收」处理，根组作废。
#[test]
fn reaped_elsewhere_disables_root_group() {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id() as Pid;
    child.wait().unwrap();
    let scope = std::sync::Mutex::new(super::scope::Scope::new(pid, None, 1));
    let said = super::tree::wait_exit_no_reap(pid, None, &scope);
    assert_eq!(said, "进程已被别处回收");
    assert!(!scope.lock().unwrap().root_valid());
}

// ---------- 审查 S6-5 第 1 轮：F-2、F-3、F-4 ----------

/// 1/F-4：扫描没做成不是空表。`clear()` 返回 `false`、范围没变、句柄没交出去；`is_empty()` 答「不空」。恢复之后
/// 再清的是同一个范围，清得掉。
#[tokio::test]
async fn scan_failure_is_not_empty() {
    let f = Fixture::start("sleep 30 & echo $! > $D/m.pid; wait");
    let member = f.pid("m.pid").await;
    let _cleanup = Leftover(vec![member]);
    f.process.record().await;
    let tree = f.process.unix_tree().clone();
    let before = tree.scope_copy();
    tree.fail_scans(true);
    assert!(!f.process.kill_tree_and_wait().await, "扫描没做成不报清空");
    assert_eq!(tree.scope_copy(), before, "范围没变");
    assert!(tree.holds_child(), "没回收");
    assert!(!f.process.tree_is_empty().unwrap(), "答「不空」");
    assert!(alive(f.root()) && alive(member), "一个信号都没发");
    tree.fail_scans(false);
    assert!(f.process.kill_tree_and_wait().await);
    assert!(!alive(member));
}

/// 同上，桥接已经退出（僵尸，没回收）、它留下的后台进程还活着：这时「桥接退了没有」「回收拿不拿得到退出状态」都
/// 放行，只剩「扫描没做成不当空表」这一道在挡。
#[tokio::test]
async fn scan_failure_after_root_exit_is_not_empty() {
    let f = Fixture::start("sleep 30 & echo $! > $D/m.pid; exit 0");
    let member = f.pid("m.pid").await;
    let _cleanup = Leftover(vec![member]);
    f.process.exited().await;
    f.process.record().await;
    let tree = f.process.unix_tree().clone();
    tree.fail_scans(true);
    assert!(!f.process.kill_tree_and_wait().await, "扫描没做成不报清空");
    assert!(alive(member) && tree.holds_child());
    tree.fail_scans(false);
    assert!(f.process.kill_tree_and_wait().await);
    assert!(!alive(member));
}

/// 1/F-3（reviewer 的探针）：第一遍读表超过期限，`clear()` 返回 `false`；桥接替身还活着、没被回收、根组仍有效。等迟到
/// 的那一遍读完，范围也没被它弄空。第二次 `clear()` 只许两种结果：`true` 并且原来的根不在了，或者 `false`。
#[tokio::test]
async fn first_scan_timeout_keeps_root_and_retry_is_truthful() {
    let f = Fixture::start("sleep 30");
    let tree = f.process.unix_tree().clone();
    tree.delay_next_scan(TREE_CLEAR_LIMIT + Duration::from_millis(300));
    assert!(!f.process.kill_tree_and_wait().await);
    assert!(alive(f.root()), "根还活着");
    assert!(tree.holds_child(), "clear 是 false：不回收");
    assert!(tree.scope_copy().root_valid(), "根组仍有效");
    // 等迟到的那一遍读完
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(tree.scope_copy().root_valid(), "迟到的扫描碰不到根组");
    let cleared = f.process.kill_tree_and_wait().await;
    assert!(!(cleared && alive(f.root())), "不许返回 true 而根还活着");
    assert!(cleared, "这一次没有注入，清得掉");
}

/// 1/F-3：判定清空之后、回收之前，根组还有效；回收拿到退出状态之后才作废。
#[tokio::test]
async fn root_group_valid_until_wait_returns() {
    let f = Fixture::start("sleep 30");
    let tree = f.process.unix_tree().clone();
    let t = tree.clone();
    let cleared = f
        .clear_with(GateStep::BeforeReap, || async move {
            assert!(t.scope_copy().root_valid() && t.holds_child());
        })
        .await;
    assert!(cleared);
    assert!(!tree.scope_copy().root_valid() && !tree.holds_child());
}

/// 等的时候得到 `ECHILD`，但这个进程按身份还在（这里拿测试进程自己：它不是自己的子进程）：根组不作废。
#[test]
fn echild_with_live_root_keeps_scope() {
    let me = std::process::id() as Pid;
    let birth = scan::birth_of(me);
    assert!(birth.is_some());
    let scope = std::sync::Mutex::new(super::scope::Scope::new(me, birth, 1));
    let said = super::tree::wait_exit_no_reap(me, birth, &scope);
    assert_eq!(said, "等待进程退出失败（ECHILD，进程还在）");
    assert!(scope.lock().unwrap().root_valid());
}

/// 1/F-2：`clear()` 成功（桥接已回收）之后不再给旧进程号发信号；`shutdown()` 照常走完。
#[tokio::test]
async fn no_signal_to_root_after_reap() {
    let f = Fixture::start("sleep 30");
    let tree = f.process.unix_tree().clone();
    assert!(f.process.kill_tree_and_wait().await);
    assert!(!tree.signal_root(libc::SIGTERM).await, "回收之后不发");
    f.process.shutdown().await;
}

/// 1/F-2：桥接被别处回收（`ECHILD`）之后 `shutdown()`：号码已经不归我们，按身份发不出去；之后的清理按身份确认它
/// 不在，照「已回收」处理。
#[tokio::test]
async fn shutdown_after_reaped_elsewhere_sends_nothing() {
    let f = Fixture::start("exit 0");
    f.process.exited().await;
    let root = f.root();
    // 别处把它回收了
    // SAFETY: 回收本测试起的子进程
    let rc = unsafe { libc::waitpid(root, std::ptr::null_mut(), 0) };
    assert_eq!(rc, root);
    let tree = f.process.unix_tree().clone();
    assert!(
        !tree.signal_root(libc::SIGTERM).await,
        "号码不归我们了，不发"
    );
    assert!(f.process.kill_tree_and_wait().await);
    assert!(!tree.scope_copy().root_valid());
}

/// 2/F-2：「复核 + 发送」卡住（读出生时间被注入 3 秒阻塞）时，`clear()` 照样在期限内返回 `false`，单线程运行时上
/// 的定时器没被堵住；迟到的那一批醒来后一个信号都不发（根既没被停住也没被杀）。之后再清，清得掉。
#[tokio::test(flavor = "current_thread")]
async fn blocked_verify_does_not_overrun_the_deadline() {
    let f = Fixture::start("sleep 30");
    let tree = f.process.unix_tree().clone();
    tree.delay_next_signals(Duration::from_secs(3));
    let started = std::time::Instant::now();
    let cleared = f.process.kill_tree_and_wait().await;
    let took = started.elapsed();
    assert!(!cleared);
    assert!(
        took < TREE_CLEAR_LIMIT + Duration::from_millis(300),
        "clear 用了 {took:?}，超过期限"
    );
    // 等那一批醒来：它看到期限过了，不发
    tokio::time::sleep(
        Duration::from_secs(3) - took.min(Duration::from_secs(3)) + Duration::from_millis(300),
    )
    .await;
    let state = scan::read_table().unwrap().0.get(&f.root()).map(|p| p.run);
    assert_eq!(
        state,
        Some(super::scope::Run::Running),
        "迟到的那一批没发信号"
    );
    assert!(f.process.kill_tree_and_wait().await);
}

/// 2/F-1：被超过去的那一遍不拿来下结论。桥接已退出、范围里没有活成员时，那一遍成员是空的，`is_empty()` 照样答
/// 「不空」；每一遍都被超过去时 `clear()` 下不了结论，返回 `false`、不回收。不再被超过去之后照常清得掉。
#[tokio::test]
async fn stale_scan_is_not_a_conclusion() {
    let f = Fixture::start("exit 0");
    f.process.exited().await;
    let tree = f.process.unix_tree().clone();
    tree.overtake_scans(true);
    assert!(!f.process.tree_is_empty().unwrap(), "旧的那一遍答「不空」");
    assert!(!f.process.kill_tree_and_wait().await);
    assert!(tree.holds_child());
    tree.overtake_scans(false);
    assert!(
        f.process.tree_is_empty().unwrap(),
        "对照：不旧的那一遍是空的"
    );
    assert!(f.process.kill_tree_and_wait().await);
}
