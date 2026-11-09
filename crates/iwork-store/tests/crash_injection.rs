//! 卡验收 1 / 方案 S1：子进程循环提交命令，父进程在随机时刻 kill（Unix SIGKILL / Windows
//! TerminateProcess），重复 ≥ 200 次。每次重启后断言：所有已返回「已提交」的命令都在、没有半个事务、
//! `seq` 连续无洞、`integrity_check` 为 ok、投影与事件重放的结果一致。
//!
//! 子进程就是本测试二进制自己：带环境变量 `IWORK_STORE_CRASH_CHILD=<数据目录>` 只跑
//! `crash_child`，它一直提交直到被杀，每拿到一个 `Ok` 就往 stdout 写一行 `ACK <id> <first> <last>`。
//! 库建在系统临时目录，测试结束自动删除。

mod support;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iwork_domain::event::{ConversationCreated, DomainEvent, ProjectAdded};
use iwork_ports::store::StoragePort;
use iwork_store::DataDir;
use support::*;

const CHILD_ENV: &str = "IWORK_STORE_CRASH_CHILD";
const ROUND_ENV: &str = "IWORK_STORE_CRASH_ROUND";
const ROUNDS: u64 = 200;
/// 每隔几轮（及最后一轮）做一次重建逐表比对：它随库变大线性变慢，每轮都做会让测试变成平方级
const FULL_CHECK_EVERY: u64 = 25;
const WRITERS: u64 = 4;

/// 第 `round` 轮第 `writer` 个线程的第 `k` 条命令：编号全局唯一，事件里的实体 ID 也唯一。
fn command_number(round: u64, writer: u64, k: u64) -> u128 {
    u128::from(round) * 10_000_000 + u128::from(writer) * 1_000_000 + u128::from(k) + 1
}

fn events_for(n: u128) -> Vec<DomainEvent> {
    // 1–2 条事件：多事件命令才测得出「半个事务」
    let mut events: Vec<DomainEvent> = vec![
        ProjectAdded {
            project: project(n),
        }
        .into(),
    ];
    if n.is_multiple_of(2) {
        events.push(
            ConversationCreated {
                conversation: project_conversation(n, n),
            }
            .into(),
        );
    }
    events
}

/// 子进程怎么跑：正常是一直提交直到被杀；另外两种只给对照测试用，模拟「子进程没进提交循环就退了」。
const MODE_ENV: &str = "IWORK_STORE_CRASH_MODE";
const MODE_EXIT_BEFORE_READY: &str = "exit_before_ready";
const MODE_EXIT_AFTER_READY: &str = "exit_after_ready";

/// 子进程入口。没有环境变量时什么都不做（正常跑测试时它是个空测试）。
/// 打开存储后先写一行 `READY <轮次>` 握手，再进提交循环。
#[test]
fn crash_child() {
    let Some(root) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let round: u64 = std::env::var(ROUND_ENV).unwrap().parse().unwrap();
    let mode = std::env::var(MODE_ENV).unwrap_or_default();
    if mode == MODE_EXIT_BEFORE_READY {
        return;
    }
    let env = Env::new();
    let dir = DataDir::from_root(PathBuf::from(root));
    let codec = Arc::new(iwork_app::codec::ProtoEventCodec);
    let store = Arc::new(
        iwork_store::SqliteStore::open(
            &dir,
            codec.clone(),
            Arc::new(iwork_store::SqliteProjector::new(codec)),
            env.clock.clone(),
            Env::config(),
        )
        .expect("子进程打开存储"),
    );
    let out = Arc::new(std::sync::Mutex::new(std::io::stdout()));
    {
        let mut out = out.lock().unwrap();
        writeln!(out, "READY {round}").unwrap();
        out.flush().unwrap();
    }
    if mode == MODE_EXIT_AFTER_READY {
        return;
    }
    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let (store, out) = (store.clone(), out.clone());
            std::thread::spawn(move || {
                // 一直提交，直到被父进程杀掉
                for k in 0..u64::MAX {
                    let n = command_number(round, w, k);
                    let mut req = cmd(n, events_for(n));
                    req.outbox = vec![outbox("t", &n.to_le_bytes())];
                    let r = block(store.commit(req)).expect("提交");
                    let mut out = out.lock().unwrap();
                    writeln!(out, "ACK {n} {} {}", r.first_seq.0, r.last_seq.0).unwrap();
                    out.flush().unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

/// 不引随机数 crate：xorshift，种子取当前时间，失败时打印出来便于复现。
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

type Acks = Vec<(u128, (u64, u64))>;

/// 跑一轮：起子进程 → 等 `READY` 握手 → 再等 `delay` → 确认子进程还在跑 → kill → 收 ACK。
/// 子进程没握手就退出、或被杀之前已经自己退出，都返回带退出状态与 stderr 的错误：
/// 这样的一轮没有在提交中途被杀，不能算数。
fn run_round(
    exe: &Path,
    root: &Path,
    round: u64,
    delay: Duration,
    mode: &str,
) -> Result<Acks, String> {
    let mut child = Command::new(exe)
        .args(["--exact", "crash_child", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, root)
        .env(ROUND_ENV, round.to_string())
        .env(MODE_ENV, mode)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("起子进程失败：{e}"))?;
    let (line_tx, line_rx) = std::sync::mpsc::channel::<String>();
    let stdout = child.stdout.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    let early_exit = |child: &mut std::process::Child, what: &str| -> Option<String> {
        let status = child.try_wait().ok().flatten()?;
        Some(format!("第 {round} 轮子进程{what}就退出了（{status}）"))
    };
    // 握手
    let mut acks: Acks = Vec::new();
    let handshake_deadline = Instant::now() + Duration::from_secs(30);
    let mut ready = false;
    while !ready {
        match line_rx.recv_timeout(Duration::from_millis(20)) {
            // libtest 先打印 `test crash_child ... ` 不换行，握手串跟在它后面
            Ok(line) => ready = line.ends_with(&format!("READY {round}")),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !ready && Instant::now() > handshake_deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "第 {round} 轮子进程 30 秒内没有握手；stderr：{}",
                err_reader.join().unwrap_or_default()
            ));
        }
    }
    if !ready {
        let msg = early_exit(&mut child, "在握手前")
            .unwrap_or_else(|| format!("第 {round} 轮子进程关了 stdout 却没握手"));
        let _ = child.kill();
        let _ = child.wait();
        let _ = out_reader.join();
        return Err(format!(
            "{msg}；stderr：{}",
            err_reader.join().unwrap_or_default()
        ));
    }
    std::thread::sleep(delay);
    if let Some(msg) = early_exit(&mut child, "在被杀之前") {
        let _ = out_reader.join();
        return Err(format!(
            "{msg}；stderr：{}",
            err_reader.join().unwrap_or_default()
        ));
    }
    child.kill().map_err(|e| format!("kill 失败：{e}"))?; // Unix 上是 SIGKILL
    let status = child.wait().map_err(|e| format!("wait 失败：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if status.signal() != Some(9) {
            return Err(format!(
                "第 {round} 轮子进程不是被 SIGKILL 结束的（{status}）"
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = status;
    out_reader
        .join()
        .map_err(|_| "读 stdout 的线程 panic".to_owned())?;
    // 读到的 ACK 一定是子进程在 COMMIT 返回之后才写的；被杀时写了一半的行被 parse 丢掉
    acks.extend(line_rx.try_iter().filter_map(|l| {
        let mut it = l.strip_prefix("ACK ")?.split(' ');
        let n: u128 = it.next()?.parse().ok()?;
        let first: u64 = it.next()?.parse().ok()?;
        let last: u64 = it.next()?.parse().ok()?;
        Some((n, (first, last)))
    }));
    drop(err_reader);
    Ok(acks)
}

#[test]
fn kill_during_commits_never_loses_acknowledged_commands() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;
    let mut rng = Rng(seed);
    eprintln!("crash_injection 种子 {seed}");
    let env = Env::new();
    let mut acked: BTreeMap<u128, (u64, u64)> = BTreeMap::new();
    let started = Instant::now();

    for round in 0..ROUNDS {
        let delay = Duration::from_millis(10 + rng.next() % 191);
        let acks = run_round(&exe, env.dir.root(), round, delay, "")
            .unwrap_or_else(|e| panic!("种子 {seed}：{e}"));
        acked.extend(acks);

        let t = Instant::now();
        let full = round % FULL_CHECK_EVERY == 0 || round + 1 == ROUNDS;
        let head = verify(&env, &acked, full);
        eprintln!(
            "第 {round} 轮：MAX(seq) = {head}，累计已确认 {} 条，校验 {:?}",
            acked.len(),
            t.elapsed()
        );
    }
    assert!(
        acked.len() as u64 >= ROUNDS,
        "对照：确实有命令被确认（{}）",
        acked.len()
    );
    eprintln!("crash_injection：{ROUNDS} 轮，用时 {:?}", started.elapsed());
}

/// 对照（第 1 轮审查 1/F-7）：子进程没进提交循环就退出的一轮，`run_round` 必须报错，
/// 不能当成一次有效的提交中途崩溃。
#[test]
fn child_that_exits_early_is_not_counted_as_a_round() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let env = Env::new();
    for mode in [MODE_EXIT_BEFORE_READY, MODE_EXIT_AFTER_READY] {
        let r = run_round(&exe, env.dir.root(), 0, Duration::from_millis(300), mode);
        let err = r.expect_err(mode);
        assert!(err.contains("退出"), "{mode}：{err}");
    }
    // 正常模式能跑通一轮（对照：判定不是一律失败）
    run_round(&exe, env.dir.root(), 1, Duration::from_millis(50), "").expect("正常一轮");
}

/// 重启后的断言；`full` 时再加重建逐表比对。返回 `MAX(seq)`。
fn verify(env: &Env, acked: &BTreeMap<u128, (u64, u64)>, full: bool) -> i64 {
    let db = env.db();
    // 重启：打开存储（走协议门、迁移、next_seq 的启动路径）
    let store = env.open();
    let conn = rusqlite::Connection::open(&db).unwrap();
    let ok: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ok, "ok");

    // seq 从 1 连续到 MAX
    let (count, max): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(MAX(seq), 0) FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, max, "seq 有洞");
    let min: i64 = conn
        .query_row("SELECT COALESCE(MIN(seq), 1) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(min, 1);

    // 已确认的命令都在，序号与回执一致
    let mut stmt = conn
        .prepare("SELECT first_seq, last_seq FROM commands WHERE command_id = ?1")
        .unwrap();
    for (n, (first, last)) in acked {
        let row: (i64, i64) = stmt
            .query_row([cmd_id(*n).to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap_or_else(|e| panic!("已确认的命令 {n} 不见了：{e}"));
        assert_eq!(row, (*first as i64, *last as i64), "命令 {n}");
    }

    // 没有半个事务：每条记下的命令，事件数、outbox 条数都完整；没有不属于任何命令的事件
    let broken: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM commands c \
             LEFT JOIN (SELECT command_id, COUNT(*) AS n FROM events GROUP BY command_id) e USING (command_id) \
             LEFT JOIN (SELECT command_id, COUNT(*) AS n FROM outbox GROUP BY command_id) o USING (command_id) \
             WHERE COALESCE(e.n, 0) <> c.last_seq - c.first_seq + 1 OR COALESCE(o.n, 0) <> 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(broken, 0, "有命令只写了一半");
    let orphan: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events e WHERE NOT EXISTS \
             (SELECT 1 FROM commands c WHERE c.command_id = e.command_id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphan, 0, "有事件不属于任何已提交的命令");

    // 投影与事件一致：项目数 = ProjectAdded 事件数；重建后逐表相同
    let projects: i64 = conn
        .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
        .unwrap();
    let added: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE type = 'project_added'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(projects, added);
    if full {
        let online = dump_projections(&db);
        block(store.rebuild_projections()).unwrap();
        assert_eq!(dump_projections(&db), online, "投影与事件重放结果不一致");
    }
    drop(stmt);
    drop(conn);
    store.close().unwrap();
    max
}
