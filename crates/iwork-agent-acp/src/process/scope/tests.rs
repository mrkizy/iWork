use proptest::prelude::*;

use super::*;
use crate::process::signal::{Ident, Os, verify_then_kill};

/// 发信号的替身：出生时间照模型世界答，信号落到此刻占着这个号的进程上，记下来。
struct WorldOs<'a> {
    world: &'a World,
    landed: std::cell::RefCell<Vec<Pid>>,
}

impl Os for WorldOs<'_> {
    fn birth_of(&self, pid: Pid) -> Option<Birth> {
        self.world
            .procs
            .get(&pid)
            .filter(|p| !p.unreadable)
            .map(|p| Birth(p.birth))
    }
    fn kill(&self, pid: Pid, _sig: libc::c_int) {
        if self.world.procs.contains_key(&pid) {
            self.landed.borrow_mut().push(pid);
        }
    }
}

// ---------- 模拟的进程世界（交错属性测试用） ----------

const INIT: Pid = 1;
const DAEMON: Pid = 2;
const BRIDGE: Pid = 3;
/// 进程号池很小：逼着号码（进程号、组号、会话号）被重用
const MAX_PID: Pid = 9;

#[derive(Debug, Clone)]
struct WP {
    pid: Pid,
    ppid: Pid,
    pgid: Pid,
    sid: Pid,
    birth: u64,
    /// 是不是桥接的后代（模型里的真值，和被测的实现分开）
    ours: bool,
    /// 别的用户的进程：读不到，不进进程表
    unreadable: bool,
}

#[derive(Debug, Clone)]
struct World {
    procs: BTreeMap<Pid, WP>,
    next_birth: u64,
    /// 桥接退出了、还没回收：进程号还占着
    bridge_zombie: bool,
    bridge_reaped: bool,
}

impl World {
    fn new() -> Self {
        let mut w = Self {
            procs: BTreeMap::new(),
            next_birth: 1,
            bridge_zombie: false,
            bridge_reaped: false,
        };
        for (pid, ppid, pgid, sid, ours) in [
            (INIT, INIT, INIT, INIT, false),
            (DAEMON, INIT, DAEMON, DAEMON, false),
            (BRIDGE, DAEMON, BRIDGE, DAEMON, true),
        ] {
            let birth = w.birth();
            w.procs.insert(
                pid,
                WP {
                    pid,
                    ppid,
                    pgid,
                    sid,
                    birth,
                    ours,
                    unreadable: false,
                },
            );
        }
        w
    }

    fn birth(&mut self) -> u64 {
        self.next_birth += 1;
        self.next_birth
    }

    /// 内核的规矩：进程号还被当作进程号、组号、会话号用着时不分配；桥接没回收时它的号也占着。
    fn alloc(&self) -> Option<Pid> {
        (BRIDGE..=MAX_PID).find(|n| {
            !self
                .procs
                .values()
                .any(|p| p.pid == *n || p.pgid == *n || p.sid == *n)
                && !(*n == BRIDGE && !self.bridge_reaped)
        })
    }

    fn alive(&self) -> Vec<Pid> {
        self.procs.keys().copied().collect()
    }

    fn pick(&self, i: usize) -> Option<Pid> {
        let a = self.alive();
        (!a.is_empty()).then(|| a[i % a.len()])
    }

    /// 挑一个能退出的进程（不挑 init 和后台）
    fn pick_movable(&self, i: usize) -> Option<Pid> {
        let a: Vec<Pid> = self.alive().into_iter().filter(|p| *p > DAEMON).collect();
        (!a.is_empty()).then(|| a[i % a.len()])
    }

    /// 挑一个能换会话、换组的进程：桥接也不挑，它启动时确认自己是组长，之后不换（方案 5.4）
    fn pick_regroupable(&self, i: usize) -> Option<Pid> {
        // 原来那个桥接：后台起的、我们的（号码 3 回收之后可能是别的进程，那些照常能动）
        let is_bridge = |p: &WP| p.pid == BRIDGE && p.ppid == DAEMON && p.ours;
        let a: Vec<Pid> = self
            .procs
            .values()
            .filter(|p| p.pid > DAEMON && !is_bridge(p))
            .map(|p| p.pid)
            .collect();
        (!a.is_empty()).then(|| a[i % a.len()])
    }

    fn apply(&mut self, op: &Op, scope: &mut Scope) {
        match *op {
            Op::Fork(i, unreadable) => {
                let (Some(parent), Some(pid)) = (self.pick(i), self.alloc()) else {
                    return;
                };
                let p = self.procs[&parent].clone();
                let birth = self.birth();
                self.procs.insert(
                    pid,
                    WP {
                        pid,
                        ppid: parent,
                        birth,
                        // 只有外人能是读不到的：我们的后代和后台同一个用户
                        unreadable: unreadable && !p.ours,
                        ..p
                    },
                );
            }
            Op::Setsid(i) => {
                let Some(pid) = self.pick_regroupable(i) else {
                    return;
                };
                let p = self.procs.get_mut(&pid).unwrap();
                if p.pgid != pid && pid > DAEMON {
                    p.sid = pid;
                    p.pgid = pid;
                }
            }
            Op::NewGroup(i) => {
                let Some(pid) = self.pick_regroupable(i) else {
                    return;
                };
                let p = self.procs.get_mut(&pid).unwrap();
                if p.sid != pid && pid > DAEMON {
                    p.pgid = pid;
                }
            }
            Op::JoinGroup(i, j) => {
                let (Some(a), Some(b)) = (self.pick_regroupable(i), self.pick(j)) else {
                    return;
                };
                let (pa, pb) = (self.procs[&a].clone(), self.procs[&b].clone());
                // 同一会话里、不是会话首进程才能换组；外人不往我们的组里钻（方案接受的前提）
                if pa.sid == pb.sid && pa.sid != a && pa.ours == pb.ours && a > DAEMON {
                    self.procs.get_mut(&a).unwrap().pgid = pb.pgid;
                }
            }
            Op::Exit(i) => {
                let Some(pid) = self.pick_movable(i) else {
                    return;
                };
                if pid <= DAEMON {
                    return;
                }
                self.procs.remove(&pid);
                if pid == BRIDGE {
                    self.bridge_zombie = true;
                }
                for p in self.procs.values_mut() {
                    if p.ppid == pid {
                        p.ppid = INIT;
                    }
                }
            }
            Op::Reap => {
                if self.bridge_zombie && !self.bridge_reaped {
                    // ProcessTree 的约定：先让根组作废，再回收
                    scope.mark_reaped();
                    self.bridge_reaped = true;
                }
            }
        }
    }

    fn table(&self) -> Table {
        Table::from_procs(self.procs.values().filter(|p| !p.unreadable).map(|p| Proc {
            pid: p.pid,
            ppid: p.ppid,
            pgid: p.pgid,
            sid: Some(p.sid),
            birth: Some(Birth(p.birth)),
            run: Run::Running,
        }))
    }

    fn recheck(&self, pids: &[Pid]) -> Recheck {
        pids.iter()
            .map(|pid| {
                let now = self
                    .procs
                    .get(pid)
                    .filter(|p| !p.unreadable)
                    .map(|p| (Birth(p.birth), p.sid));
                (*pid, now)
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
enum Op {
    Fork(usize, bool),
    Setsid(usize),
    NewGroup(usize),
    JoinGroup(usize, usize),
    Exit(usize),
    Reap,
}

#[derive(Debug, Clone)]
enum Step {
    World(Op),
    /// 读进程表 → 中间又发生了这些事 → 复核见证 → 求范围
    Observe(Vec<Op>),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<usize>(), prop::bool::weighted(0.2)).prop_map(|(i, u)| Op::Fork(i, u)),
        3 => any::<usize>().prop_map(Op::Setsid),
        2 => any::<usize>().prop_map(Op::NewGroup),
        1 => (any::<usize>(), any::<usize>()).prop_map(|(i, j)| Op::JoinGroup(i, j)),
        4 => any::<usize>().prop_map(Op::Exit),
        1 => Just(Op::Reap),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => op().prop_map(Step::World),
        1 => prop::collection::vec(op(), 0..3).prop_map(Step::Observe),
    ]
}

/// 被测的实现跑一遍交错，返回第一条违反的规则。
fn run(steps: &[Step]) -> Result<(), String> {
    let mut w = World::new();
    let bridge_birth = w.procs[&BRIDGE].birth;
    let mut scope = Scope::new(BRIDGE, Some(Birth(bridge_birth)), DAEMON);
    // 上一遍认下的（进程号，出生时间）
    let mut last: BTreeSet<(Pid, u64)> = BTreeSet::new();
    for (n, s) in steps.iter().enumerate() {
        match s {
            Step::World(op) => w.apply(op, &mut scope),
            Step::Observe(mid) => {
                let table = w.table();
                let before = w.clone();
                for op in mid {
                    w.apply(op, &mut scope);
                }
                let recheck = w.recheck(&scope.witness_pids());
                let members = scope.observe(&table, &recheck);
                // 清理照这次求出的范围发信号，发的时候是读表之后、中间那些事都发生过了的世界（5.4a「发信号按身份」）。
                // 每个信号按它发出那一刻的世界断言：落在我们的进程上，或者根本没发
                for m in &members.list {
                    let Some(birth) = m.birth else { continue };
                    let os = WorldOs {
                        world: &w,
                        landed: std::cell::RefCell::default(),
                    };
                    let who = Ident { pid: m.pid, birth };
                    verify_then_kill(&os, who, libc::SIGKILL, None);
                    for pid in os.landed.into_inner() {
                        if !w.procs[&pid].ours {
                            return Err(format!(
                                "第 {n} 步把信号发到了外人 {pid}：{:?}",
                                w.procs[&pid]
                            ));
                        }
                    }
                }
                let got = members.pids();
                for pid in &got {
                    // 安全：认下的都是读表那一刻桥接的后代
                    if !before.procs[pid].ours {
                        return Err(format!(
                            "第 {n} 步认下了外人 {pid}：{:?}",
                            before.procs[pid]
                        ));
                    }
                }
                for p in table.0.values() {
                    let wp = &before.procs[&p.pid];
                    // 不漏 1：根组有效、父链在表里连得到活着的桥接
                    let mut cur = p.pid;
                    let mut reaches = false;
                    for _ in 0..table.0.len() {
                        // 按身份认桥接：号码 3 回收之后可能是外人
                        if cur == BRIDGE
                            && before
                                .procs
                                .get(&BRIDGE)
                                .is_some_and(|b| b.birth == bridge_birth)
                        {
                            reaches = true;
                            break;
                        }
                        let Some(q) = table.0.get(&cur) else { break };
                        if q.ppid == cur {
                            break;
                        }
                        cur = q.ppid;
                    }
                    if reaches && scope.root_valid() && !got.contains(&p.pid) {
                        return Err(format!("第 {n} 步漏了连得到桥接的 {}", p.pid));
                    }
                    // 不漏 2：上一遍认下的、还是同一个进程的
                    if last.contains(&(p.pid, wp.birth)) && !got.contains(&p.pid) {
                        return Err(format!("第 {n} 步漏了上一遍认过的 {}", p.pid));
                    }
                }
                last = got
                    .iter()
                    .map(|pid| (*pid, before.procs[pid].birth))
                    .collect();
            }
        }
    }
    Ok(())
}

/// 两遍扫描（编号 0、1）各自走四步（锁里取快照 → 读进程表 → 复核快照里的见证 → 算、拿锁提交，走完再从头来），
/// 和世界里的事任意穿插（方案 5.4a「扫描怎么提交到范围」）。
#[derive(Debug, Clone)]
enum Staged {
    World(Op),
    /// 第 k 遍扫描往前走一步
    Advance(usize),
    /// 同上；走到复核这一步时，读每个见证都拆成「读出生时间 → 读会话号 → 再读出生时间」三次调用（生产代码的
    /// `read_consistent`），第一次读出生时间和读会话号之间插入这些事（审查 S6-5 5/F-1）
    Torn(usize, Vec<Op>),
}

fn staged() -> impl Strategy<Value = Staged> {
    prop_oneof![
        2 => op().prop_map(Staged::World),
        3 => (0..2usize).prop_map(Staged::Advance),
        2 => ((0..2usize), prop::collection::vec(op(), 1..4)).prop_map(|(k, ops)| Staged::Torn(k, ops)),
    ]
}

/// 读一个进程的系统调用，照模型世界答；第一次读会话号之前先让插进来的事发生。
struct TornSys<'a> {
    world: std::cell::RefCell<&'a mut World>,
    pending: std::cell::RefCell<Vec<Op>>,
}

impl crate::process::scan::ProcSys for TornSys<'_> {
    fn bsd(&self, pid: Pid) -> std::io::Result<Option<crate::process::scan::Bsd>> {
        let w = self.world.borrow();
        Ok(w.procs
            .get(&pid)
            .filter(|p| !p.unreadable)
            .map(|p| crate::process::scan::Bsd {
                ppid: p.ppid,
                pgid: p.pgid,
                birth: Birth(p.birth),
                run: Run::Running,
            }))
    }
    fn sid(&self, pid: Pid) -> Option<Pid> {
        let ops = std::mem::take(&mut *self.pending.borrow_mut());
        if !ops.is_empty() {
            // 回收桥接要动范围，不在读一个进程的这一瞬间里；别的照常发生
            let mut dummy = Scope::new(BRIDGE, None, DAEMON);
            let mut w = self.world.borrow_mut();
            for op in ops.iter().filter(|o| !matches!(o, Op::Reap)) {
                w.apply(op, &mut dummy);
            }
        }
        let w = self.world.borrow();
        w.procs.get(&pid).filter(|p| !p.unreadable).map(|p| p.sid)
    }
}

/// 一遍扫描走到哪一步了。
#[derive(Default, Clone)]
struct Scan {
    snapshot: Option<Scope>,
    table: Option<Table>,
    recheck: Option<Recheck>,
}

fn run_staged(steps: &[Staged]) -> Result<(), String> {
    let mut w = World::new();
    let bridge_birth = w.procs[&BRIDGE].birth;
    let mut scope = Scope::new(BRIDGE, Some(Birth(bridge_birth)), DAEMON);
    let mut scans = [Scan::default(), Scan::default()];
    let mut claimed: BTreeSet<(Pid, u64)> = BTreeSet::new();
    for (n, s) in steps.iter().enumerate() {
        match s {
            Staged::World(op) => w.apply(op, &mut scope),
            Staged::Advance(k) | Staged::Torn(k, _) => {
                let torn = match s {
                    Staged::Torn(_, ops) => Some(ops.clone()),
                    _ => None,
                };
                let scan = &mut scans[*k];
                if scan.snapshot.is_none() {
                    scan.snapshot = Some(scope.begin_scan());
                    continue;
                }
                if scan.table.is_none() {
                    scan.table = Some(w.table());
                    continue;
                }
                if scan.recheck.is_none() {
                    let pids = scan.snapshot.as_ref().unwrap().witness_pids();
                    scan.recheck = Some(match torn {
                        None => w.recheck(&pids),
                        Some(ops) => {
                            let sys = TornSys {
                                world: std::cell::RefCell::new(&mut w),
                                pending: std::cell::RefCell::new(ops),
                            };
                            let mut r = Recheck::new();
                            for pid in pids {
                                // 一直在换人：真实代码里这一遍扫描算没做成；模型里当这个见证读不到
                                let now = crate::process::scan::read_consistent(&sys, pid)
                                    .ok()
                                    .flatten()
                                    .and_then(|p| Some((p.birth?, p.sid?)));
                                r.insert(pid, now);
                            }
                            r
                        }
                    });
                    continue;
                }
                let Scan {
                    snapshot: Some(snap),
                    table: Some(table),
                    recheck: Some(recheck),
                } = std::mem::take(scan)
                else {
                    unreachable!("三样都有了");
                };
                let before = scope.clone();
                let pass = Scope::compute(&snap, &table, &recheck);
                let members = scope.commit(pass);
                scope.end_scan();
                // 这一遍凭什么认下了谁：记着，之后判「清空了」时它们一个都不能还活着
                for m in &members.list {
                    if let Some(b) = m.birth {
                        claimed.insert((m.pid, b.0));
                    }
                }
                // clear() 下「清空了」的结论（空的、不旧、提交时没有别的在飞）：任何一遍读过表的扫描（提交了的、还在飞的）
                // 曾经认下的我们的进程，此刻一个都不能还活着（方案 5.4a 第 4a 条）
                if members.is_empty() && !members.stale && !members.busy {
                    let mut seen: Vec<(Pid, u64)> = claimed.iter().copied().collect();
                    for other in scans.iter().filter(|o| o.table.is_some()) {
                        let (Some(s0), Some(t0)) = (&other.snapshot, &other.table) else {
                            continue;
                        };
                        let r0 = other.recheck.clone().unwrap_or_default();
                        for m in Scope::compute(s0, t0, &r0).members_for_test() {
                            if let Some(b) = m.birth {
                                seen.push((m.pid, b.0));
                            }
                        }
                    }
                    for (pid, birth) in seen {
                        if w.procs
                            .get(&pid)
                            .is_some_and(|p| p.birth == birth && p.ours)
                        {
                            return Err(format!(
                                "第 {n} 步判了清空，但被扫到过的我们的进程 {pid} 还活着"
                            ));
                        }
                    }
                }
                // 记到过的、此刻还活着（同一个身份）的，提交之后都还在
                for (pid, birth) in before.recorded.keys() {
                    let alive = w.procs.get(pid).is_some_and(|p| p.birth == birth.0);
                    if alive && !scope.is_recorded(*pid, *birth) {
                        return Err(format!(
                            "第 {n} 步的提交删掉了还活着的记到过的进程 {pid}（旧={}）",
                            members.stale
                        ));
                    }
                }
                // 范围里除了记到过的进程和根组的有效期，没有别的会影响认领的状态：把代次、在飞数清零，结果相同
                let mut stripped = snap.clone();
                stripped.generation = 0;
                stripped.in_flight = 0;
                if Scope::compute(&stripped, &table, &recheck).members_for_test()
                    != Scope::compute(&snap, &table, &recheck).members_for_test()
                {
                    return Err(format!("第 {n} 步：范围里有证据以外的状态影响了认领"));
                }
                // 认下的都是读表那一刻桥接的后代（读表时的世界里）
                for m in &members.list {
                    let wp = table.0.get(&m.pid).expect("成员来自这张表");
                    if wp.birth.is_none() {
                        continue;
                    }
                    if let Some(p) = w
                        .procs
                        .get(&m.pid)
                        .filter(|p| Some(Birth(p.birth)) == wp.birth)
                        && !p.ours
                    {
                        return Err(format!("第 {n} 步认下了外人 {}", m.pid));
                    }
                }
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    /// 两遍扫描的四步任意穿插：被某一遍已经提交的扫描记下、此刻还活着的进程，不管之后的提交以什么次序落下，都还在
    /// 范围里；被超过去的那一遍不作废会话（审查 S6-5 2/F-1）。
    #[test]
    fn interleaved_scans_never_lose_live_recorded(steps in prop::collection::vec(staged(), 0..80)) {
        if let Err(e) = run_staged(&steps) {
            prop_assert!(false, "{}", e);
        }
    }

    /// 任意的 fork、另立会话、换组、退出、号码重用、回收桥接，和「读表 → 中间有事 → 复核」任意交错：
    /// 范围里只有桥接的后代（不误杀），连得到桥接的、上一遍认过的都在（不漏）。
    #[test]
    fn any_interleaving_claims_only_descendants(steps in prop::collection::vec(step(), 0..80)) {
        if let Err(e) = run(&steps) {
            prop_assert!(false, "{}", e);
        }
    }
}

/// reviewer 的反例（审查 S6-5 1/F-1）：桥接起一个孩子；记一次；下一次读表之后、发信号之前孩子退出、号码被后台的
/// 另一个孩子（外人）拿到。按身份发：一次都不落在外人身上。
#[test]
fn reused_pid_between_scan_and_signal_gets_no_signal() {
    let steps = vec![
        Step::World(Op::Fork(2, false)),
        Step::Observe(vec![]),
        Step::Observe(vec![Op::Exit(1), Op::Fork(1, false)]),
    ];
    // 对照：这个序列里号码确实被外人重用了
    let mut w = World::new();
    let mut scope = Scope::new(BRIDGE, None, DAEMON);
    w.apply(&Op::Fork(2, false), &mut scope);
    let child = *w.procs.keys().max().unwrap();
    assert!(w.procs[&child].ours);
    w.apply(&Op::Exit(1), &mut scope);
    w.apply(&Op::Fork(1, false), &mut scope);
    assert!(!w.procs[&child].ours, "号码 {child} 换成了外人");
    assert_eq!(run(&steps), Ok(()));
}

/// 剩下的窗口（待产品负责人确认的已知限制）：复核出生时间与 `kill` 之间号码换了人，信号落到外人身上。只在没有
/// 句柄可用的做法里有这个插入点（macOS；`pidfd_open` 用不了的 Linux）。以后有了更好的办法，要有意识地改这条。
#[test]
fn reuse_between_verify_and_kill_is_the_known_window() {
    let mut w = World::new();
    let mut scope = Scope::new(BRIDGE, None, DAEMON);
    w.apply(&Op::Fork(2, false), &mut scope);
    let child = *w.procs.keys().max().unwrap();
    let birth = Birth(w.procs[&child].birth);
    // 复核：对得上
    let verified = {
        let os = WorldOs {
            world: &w,
            landed: std::cell::RefCell::default(),
        };
        os.birth_of(child) == Some(birth)
    };
    assert!(verified);
    // 复核与 kill 之间：孩子退出、号码被外人拿到
    w.apply(&Op::Exit(1), &mut scope);
    w.apply(&Op::Fork(1, false), &mut scope);
    let os = WorldOs {
        world: &w,
        landed: std::cell::RefCell::default(),
    };
    os.kill(child, libc::SIGKILL);
    let landed = os.landed.into_inner();
    assert_eq!(landed, vec![child]);
    assert!(!w.procs[&child].ours, "落到了外人身上：这就是那个窗口");
}

// ---------- 方案 5.4a 点名的快照测试 ----------

fn proc(pid: Pid, ppid: Pid, pgid: Pid, sid: Pid, birth: u64) -> Proc {
    Proc {
        pid,
        ppid,
        pgid,
        sid: Some(sid),
        birth: Some(Birth(birth)),
        run: Run::Running,
    }
}

fn witnesses_alive(table: &Table, scope: &Scope) -> Recheck {
    scope
        .witness_pids()
        .into_iter()
        .map(|pid| {
            let now = table.0.get(&pid).and_then(|p| Some((p.birth?, p.sid?)));
            (pid, now)
        })
        .collect()
}

/// 后台 2、桥接 10（组 10，会话 2）、CLI 11、命令的 shell 20 自成会话 20、它的孩子 21。
fn base_table() -> Vec<Proc> {
    vec![
        proc(1, 1, 1, 1, 1),
        proc(2, 1, 2, 2, 2),
        proc(10, 2, 10, 2, 10),
        proc(11, 10, 10, 2, 11),
        proc(20, 11, 20, 20, 20),
        proc(21, 20, 20, 20, 21),
    ]
}

fn observe(scope: &mut Scope, procs: Vec<Proc>) -> BTreeSet<Pid> {
    let t = Table::from_procs(procs);
    let r = witnesses_alive(&t, scope);
    scope.observe(&t, &r).pids()
}

/// 表驱动：每一种进范围的方式各一行，外加不该进的几行。
#[test]
fn scope_membership() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    // 第一遍：根组 10、11，顺父链 20、21；后台、init 不在
    assert_eq!(
        observe(&mut s, base_table()),
        BTreeSet::from([10, 11, 20, 21])
    );
    // CLI 死了、shell 被收养：shell 是记到过的进程；21 也是；shell 新起的 22 过会话的见证
    let mut t = base_table();
    t.retain(|p| p.pid != 11);
    t.iter_mut().find(|p| p.pid == 20).unwrap().ppid = 1;
    t.push(proc(22, 1, 22, 20, 22));
    assert_eq!(observe(&mut s, t.clone()), BTreeSet::from([10, 20, 21, 22]));
    // 只和记到过的进程同组、别的都不沾的不算：外人 30 被放进组 20 的号（会话 2）
    t.push(proc(30, 2, 20, 2, 30));
    assert!(!observe(&mut s, t).contains(&30));
}

/// 3/F-2：同一个进程号、同一秒、原生出生时间不同 → 不是记到过的那个进程，也不能当见证。
#[test]
fn same_pid_same_second_different_birth_is_another_process() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(&mut s, base_table());
    // 20 和 21 都没了，号码 21 被外人拿到（出生时间不同），自成会话 21
    let mut t = base_table();
    t.retain(|p| p.pid != 20 && p.pid != 21);
    t.push(proc(21, 1, 21, 21, 999));
    let got = observe(&mut s, t);
    assert!(!got.contains(&21), "{got:?}");
    assert!(s.witness_pids().is_empty(), "外人不能当见证");
}

/// 出生时间读不到的进程不算任何记到过的进程，也不被记下。
#[test]
fn unreadable_birth_is_never_matched() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(&mut s, base_table());
    let mut t = base_table();
    t.retain(|p| p.pid != 11);
    let shell = t.iter_mut().find(|p| p.pid == 20).unwrap();
    shell.ppid = 1;
    shell.birth = None;
    let child = t.iter_mut().find(|p| p.pid == 21).unwrap();
    child.birth = None;
    // 20、21 都读不到出生时间：不按记到过的认；会话 20 的见证（记的是 20、21 的出生时间）复核不过
    let tt = Table::from_procs(t);
    let recheck: Recheck = s.witness_pids().into_iter().map(|p| (p, None)).collect();
    let got = s.observe(&tt, &recheck).pids();
    assert_eq!(got, BTreeSet::from([10]));
}

/// 3/F-1：根组已标成回收，快照里有进程的组号等于旧根组号 → 一个都不认领。
#[test]
fn root_group_reused_after_reap_is_not_claimed() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(
        &mut s,
        vec![
            proc(1, 1, 1, 1, 1),
            proc(2, 1, 2, 2, 2),
            proc(10, 2, 10, 2, 10),
        ],
    );
    s.mark_reaped();
    // 号码 10 被重用成外人的进程与组
    let got = observe(
        &mut s,
        vec![
            proc(1, 1, 1, 1, 1),
            proc(2, 1, 2, 2, 2),
            proc(10, 1, 10, 10, 500),
            proc(12, 10, 10, 10, 501),
        ],
    );
    assert!(got.is_empty(), "{got:?}");
}

/// 2/F-1：会话 S 的两个记到过的成员都没了，同号 S 的新会话里每个进程都和记到的不同 → 一个都不认；
/// S 从此作废，以后再出现会话号 S 的进程也不认。
#[test]
fn reused_session_number_is_not_claimed() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(&mut s, base_table());
    let foreign = |pid, ppid, birth| proc(pid, ppid, 20, 20, birth);
    let mut t: Vec<Proc> = base_table().into_iter().filter(|p| p.pid < 20).collect();
    t.push(foreign(20, 1, 700));
    t.push(foreign(23, 20, 701));
    let got = observe(&mut s, t.clone());
    assert!(!got.contains(&20) && !got.contains(&23), "{got:?}");
    assert!(s.witness_pids().is_empty());
    // 第三份：会话号 20 里又出现进程，照样不认
    t.push(foreign(24, 23, 702));
    let got = observe(&mut s, t);
    assert!(!got.contains(&24), "{got:?}");
}

/// 3/F-3：组长与成员换出去、原组清空、组长原地重建同号组、成员换回。进程组号在哪一步都不参与认领：
/// 只要它们还在受管会话里、见证在，就照会话认领；离开了会话（另立会话）就靠记到过的身份。
#[test]
fn regrouped_members_are_not_a_witness() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(&mut s, base_table());
    // 外人 40 在根会话里，换进组 20 的号（同号重建出来的组）：不认
    let mut t = base_table();
    t.iter_mut().find(|p| p.pid == 21).unwrap().pgid = 21;
    t.push(proc(40, 2, 20, 2, 40));
    let got = observe(&mut s, t.clone());
    assert!(got.contains(&21) && !got.contains(&40), "{got:?}");
    // 成员换回组 20：照旧认，靠的是会话与身份，不是组号
    t.iter_mut().find(|p| p.pid == 21).unwrap().pgid = 20;
    let got = observe(&mut s, t);
    assert!(got.contains(&21) && !got.contains(&40), "{got:?}");
}

/// 复核在读完进程表之后：读到 p 在会话 S，复核时见证已经不在 → 不认；见证还在但会话号变了 → 不认；
/// 见证还在、会话号没变 → 认。
#[test]
fn witness_must_hold_after_the_table_read() {
    let setup = || {
        let mut s = Scope::new(10, Some(Birth(10)), 2);
        observe(&mut s, base_table());
        // CLI 死了、shell 20 与 21 被收养；表里新出现同会话的 22，父链断了，只能靠见证
        let mut t: Vec<Proc> = base_table().into_iter().filter(|p| p.pid != 11).collect();
        t.iter_mut().find(|p| p.pid == 20).unwrap().ppid = 1;
        t.iter_mut().find(|p| p.pid == 21).unwrap().ppid = 1;
        t.push(proc(22, 1, 20, 20, 22));
        (s, Table::from_procs(t))
    };
    let cases: [(&str, Recheck, bool); 4] = [
        (
            "见证都不在了",
            BTreeMap::from([(20, None), (21, None)]),
            false,
        ),
        (
            "见证在、会话号变了",
            BTreeMap::from([(20, Some((Birth(20), 20))), (21, Some((Birth(21), 21)))])
                .into_iter()
                .map(|(p, v)| (p, v.map(|(b, _)| (b, p + 100))))
                .collect(),
            false,
        ),
        (
            "读表时见证在，复核时号码已经换了人（出生时间不同、会话号碰巧相同）",
            BTreeMap::from([(20, Some((Birth(999), 20))), (21, None)]),
            false,
        ),
        (
            "见证在、会话号没变",
            BTreeMap::from([(20, Some((Birth(20), 20))), (21, None)]),
            true,
        ),
    ];
    for (name, recheck, want) in cases {
        let (mut s, t) = setup();
        let got = s.observe(&t, &recheck).pids();
        assert_eq!(got.contains(&22), want, "{name}：{got:?}");
    }
}

/// 最后一个见证没了，下一次求范围就不按这个会话认领（每一遍现算，不等周期删除，也不记「作废」）。
#[test]
fn session_is_dropped_the_moment_its_last_witness_is_gone() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(&mut s, base_table());
    assert_eq!(s.witness_pids(), vec![20, 21]);
    let mut t: Vec<Proc> = base_table().into_iter().filter(|p| p.pid < 20).collect();
    // 父链断了的 25 在会话 20 里（假如号码还被它占着），20、21 都已经没了
    t.push(proc(25, 1, 25, 20, 25));
    let got = observe(&mut s, t);
    assert!(!got.contains(&25), "{got:?}");
    assert!(s.witness_pids().is_empty());
}

/// 见证选停住了的、记到过的、离根最近的那个；没停住的不当见证。
#[test]
fn witness_is_the_stopped_member_closest_to_the_root() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    let mut t = base_table();
    for p in &mut t {
        p.run = Run::Stopped;
    }
    let tt = Table::from_procs(t.clone());
    let m = s.observe(&tt, &Recheck::new());
    assert_eq!(s.witnesses(&m), BTreeMap::from([(20, 20)]));
    t.iter_mut().find(|p| p.pid == 20).unwrap().run = Run::Running;
    let tt = Table::from_procs(t);
    let r = witnesses_alive(&tt, &s);
    let m = s.observe(&tt, &r);
    assert_eq!(s.witnesses(&m), BTreeMap::from([(20, 21)]));
    // 从叶子到根：21（深 3）在 20 之前，桥接 10 在最后
    let order: Vec<Pid> = m.list.iter().map(|m| m.pid).collect();
    assert_eq!(order, vec![21, 20, 11, 10]);
}

// ---------- 扫描按代次提交（审查 S6-5 2/F-1） ----------

fn recheck_of(table: &[Proc], pids: &[Pid]) -> Recheck {
    pids.iter()
        .map(|pid| {
            let now = table
                .iter()
                .find(|p| p.pid == *pid)
                .and_then(|p| Some((p.birth?, p.sid?)));
            (*pid, now)
        })
        .collect()
}

/// 一遍扫描，四步分开做：快照取自 `snap`、表是 `table`、复核照 `now`（复核那一刻的世界）。
fn pass(snap: &Scope, table: Vec<Proc>, now: &[Proc]) -> Pass {
    let recheck = recheck_of(now, &snap.witness_pids());
    Scope::compute(snap, &Table::from_procs(table), &recheck)
}

/// reviewer 的探针：旧表只有桥接和 CLI；新表提交后范围里有会话 20 与后代 21；再照旧表提交。21 仍在范围里、会话 20
/// 没作废；21 的父进程退出后的下一遍扫描仍然认领 21。
#[test]
fn stale_scan_commit_only_adds() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    let old: Vec<Proc> = base_table().into_iter().filter(|p| p.pid < 20).collect();
    let snap_a = s.clone();
    // 乙：新表，完整提交
    observe(&mut s, base_table());
    assert_eq!(s.witness_pids(), vec![20, 21]);
    // 甲：照旧表提交（复核时 20、21 都活着）
    let got = s.commit(pass(&snap_a, old, &base_table()));
    assert!(got.stale);
    assert_eq!(s.witness_pids(), vec![20, 21], "21 还在，会话 20 没作废");
    // 20 退出、21 被收养
    let mut later: Vec<Proc> = base_table().into_iter().filter(|p| p.pid != 20).collect();
    later.iter_mut().find(|p| p.pid == 21).unwrap().ppid = 1;
    assert!(observe(&mut s, later).contains(&21));
}

/// 甲的表里有 X 和它的孩子 Y；乙的表里 X 没了、Y 被收养（乙认不出 Y）。乙先提交、甲后提交：Y 被记下。
#[test]
fn stale_scan_still_contributes_what_it_saw() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    // X = 30 是 CLI 的孩子、自成会话 30；Y = 31 是 X 的孩子
    let mut a_table = base_table();
    a_table.push(proc(30, 11, 30, 30, 30));
    a_table.push(proc(31, 30, 30, 30, 31));
    let snap_a = s.clone();
    let mut b_table: Vec<Proc> = base_table();
    b_table.push(proc(31, 1, 30, 30, 31));
    let b = observe(&mut s, b_table.clone());
    assert!(!b.contains(&31), "对照：乙认不出 Y");
    let got = s.commit(pass(&snap_a, a_table, &b_table));
    assert!(got.stale && got.pids().contains(&31));
    assert!(observe(&mut s, b_table).contains(&31), "Y 被记下了");
}

/// 被超过去的那一遍返回「旧的」，哪怕成员是空的：调用方不拿它下结论。
#[test]
fn stale_scan_is_not_a_conclusion() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    let snap = s.clone();
    observe(&mut s, base_table());
    let got = s.commit(pass(&snap, vec![proc(1, 1, 1, 1, 1)], &[]));
    assert!(got.stale && got.is_empty());
}

/// 迟到但没被超过去的那一遍照常完整提交：删掉已经退出的记到过的进程。
#[test]
fn late_scan_without_overtake_commits_fully() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(&mut s, base_table());
    let snap = s.clone();
    let without_21: Vec<Proc> = base_table().into_iter().filter(|p| p.pid != 21).collect();
    let got = s.commit(pass(&snap, without_21.clone(), &without_21));
    assert!(!got.stale);
    assert_eq!(s.witness_pids(), vec![20], "21 删掉了");
}

/// 读表期间别的扫描新记下的成员，不当这一遍的见证：这一遍复核的是快照里的见证。
#[test]
fn witnesses_come_from_the_snapshot() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(&mut s, base_table());
    let snap = s.clone();
    // 别的扫描记下了会话 20 里的新成员 22
    let mut with_22 = base_table();
    with_22.push(proc(22, 20, 20, 20, 22));
    observe(&mut s, with_22.clone());
    assert!(s.witness_pids().contains(&22));
    assert!(!snap.witness_pids().contains(&22));
    // 这一遍：快照里的见证 20、21 复核时都没了，只有 22 活着；22 不算这一遍的见证 → 会话 20 照这一遍算是作废的
    let now: Vec<Proc> = base_table()
        .into_iter()
        .filter(|p| p.pid < 20)
        .chain([proc(22, 1, 20, 20, 22)])
        .collect();
    let p = pass(&snap, now.clone(), &now);
    assert!(
        !p.members_for_test().iter().any(|m| m.pid == 22),
        "这一遍会话 20 核不出见证，22 不被认领"
    );
}

/// 完整提交里，表里有这个号但这一遍没读到出生时间的：不删，这一遍也不凭身份算它是成员。
#[test]
fn unread_birth_keeps_the_record() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(&mut s, base_table());
    let mut t = base_table();
    let shell = t.iter_mut().find(|p| p.pid == 20).unwrap();
    shell.birth = None;
    shell.ppid = 1;
    // 21 也断了父链、会话 20 的见证复核不过（20 读不到、21 不在）：20 不凭身份进范围
    t.retain(|p| p.pid != 21 && p.pid != 11);
    let tt = Table::from_procs(t.clone());
    let recheck = recheck_of(&t, &s.witness_pids());
    let got = s.observe(&tt, &recheck);
    assert!(!got.pids().contains(&20));
    assert!(s.is_recorded(20, Birth(20)), "没读到出生时间的不删");
}

/// 3a（审查 S6-5 3/F-2）：快照取在回收之前；读表时根组号被一个外人拿到、建了同号的组；表里另有记到过的进程 X 和它
/// 刚生的孩子 Y。提交时照「根组无效」重算、只加不删：外人不在范围里，Y 被记下了，返回的成员标成旧的。
#[test]
fn scan_spanning_a_reap_drops_only_root_group_claims() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(&mut s, base_table());
    let snap = s.clone();
    s.mark_reaped();
    let table = vec![
        proc(1, 1, 1, 1, 1),
        proc(2, 1, 2, 2, 2),
        // 号码 10 被外人拿到、自成一组（组号等于旧根组号）
        proc(10, 1, 10, 10, 99),
        // X = 20 还活着（父进程没了），它刚生的孩子 Y = 23
        proc(20, 1, 20, 20, 20),
        proc(23, 20, 20, 20, 23),
    ];
    let got = s.commit(pass(&snap, table.clone(), &table));
    assert!(got.stale);
    assert!(
        !got.pids().contains(&10) && !s.is_recorded(10, Birth(99)),
        "外人没被记下"
    );
    assert!(s.is_recorded(23, Birth(23)), "Y 被记下了");
}

/// 3/F-1：同一个进程号、出生时间不同的两个后代。旧的那一遍（看到号 21 上原来那个）先提交，新的那一遍（看到号 21 上后来
/// 那个，也是我们的）后提交、只加不删：两条身份都在；之后那个活着的成了孤儿（父链断、自成会话），仍凭身份在范围里。
#[test]
fn stale_merge_is_by_identity() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    let snap = s.clone();
    // 甲：号 21 上是原来那个（出生时间 21）
    let a = base_table();
    // 乙：原来那个已经没了，号 21 被 CLI 新生的孩子拿到（出生时间 77）
    let mut b: Vec<Proc> = base_table().into_iter().filter(|p| p.pid != 21).collect();
    b.push(proc(21, 11, 21, 21, 77));
    assert!(!s.commit(pass(&snap, a.clone(), &a)).stale);
    let got = s.commit(pass(&snap, b.clone(), &b));
    assert!(got.stale);
    assert!(
        s.is_recorded(21, Birth(21)) && s.is_recorded(21, Birth(77)),
        "两条身份都在"
    );
    // CLI 退出，新的 21 成了孤儿
    let mut later: Vec<Proc> = b.into_iter().filter(|p| p.pid != 11).collect();
    later.iter_mut().find(|p| p.pid == 21).unwrap().ppid = 1;
    assert!(observe(&mut s, later).contains(&21));
}

/// 3/F-1：记到过的进程另立了会话。旧表（它还在原会话里）先提交，新表（会话号等于它自己的进程号）后提交、只加不删：
/// 它记着的会话号是新的，下一遍完整提交不会把它的新会话作废。
#[test]
fn stale_merge_takes_the_newer_session() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(&mut s, base_table());
    let snap = s.clone();
    // 21 另立会话 21
    let mut newer = base_table();
    let x = newer.iter_mut().find(|p| p.pid == 21).unwrap();
    x.sid = Some(21);
    x.pgid = 21;
    assert!(!s.commit(pass(&snap, base_table(), &base_table())).stale);
    assert!(s.commit(pass(&snap, newer.clone(), &newer)).stale);
    assert_eq!(s.recorded[&(21, Birth(21))], 21, "取等于自己进程号的那个");
    // 下一遍完整提交：20 没了，会话 21 靠 21 自己当见证；会话 21 里的孤儿 24 被认领
    let mut next: Vec<Proc> = newer.into_iter().filter(|p| p.pid != 20).collect();
    next.push(proc(24, 1, 21, 21, 24));
    assert!(observe(&mut s, next).contains(&24));
}

/// reviewer 探针在四步模型里的样子：甲取快照、读表；桥接 fork 出一个孩子；乙走完四步、把孩子记下；甲复核、提交。
/// 孩子还活着，甲的旧表里却没有它：甲的提交不能删掉它。
#[test]
fn staged_probe_keeps_the_child() {
    use Staged::{Advance, World as W};
    let steps = vec![
        Advance(0), // 甲：快照
        Advance(0), // 甲：读表
        W(Op::Fork(2, false)),
        Advance(1), // 乙：快照
        Advance(1), // 乙：读表
        Advance(1), // 乙：复核
        Advance(1), // 乙：提交
        Advance(0), // 甲：复核
        Advance(0), // 甲：提交
    ];
    assert_eq!(run_staged(&steps), Ok(()));
}

// ---------- 范围只存证据（审查 S6-5 4/F-1） ----------

/// 第 4 轮的反例：会话 20 记到的见证只有 20。扫描乙读到 20 与新孩子 21、复核完，停在提交之前；20 退出，21 还活着；
/// 扫描甲完整提交（这一遍核不出会话 20 的见证，不按它认领，也不记任何「作废」）；乙随后只加不删，记下 21（会话号 20）；
/// 之后会话 20 里出现一个孤儿 22。下一遍扫描里 21 当见证，会话 20 受管，22 被认领。
#[test]
fn witness_lost_then_evidence_arrives_session_is_claimed_again() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    let base: Vec<Proc> = base_table().into_iter().filter(|p| p.pid != 21).collect();
    observe(&mut s, base.clone());
    let snap_b = s.clone();
    let snap_a = s.clone();
    // 乙读到 20 和它的新孩子 21，复核时 20 还活着
    let mut b_table = base.clone();
    b_table.push(proc(21, 20, 20, 20, 21));
    let b_pass = pass(&snap_b, b_table.clone(), &b_table);
    // 20 退出，21 成了孤儿；甲读到这张表，完整提交
    let mut a_table: Vec<Proc> = base.into_iter().filter(|p| p.pid != 20).collect();
    a_table.push(proc(21, 1, 20, 20, 21));
    let a = s.commit(pass(&snap_a, a_table.clone(), &a_table));
    assert!(!a.stale && !a.pids().contains(&21), "对照：甲认不出 21");
    // 乙提交：只加不删，21 记下了
    assert!(s.commit(b_pass).stale);
    assert!(s.is_recorded(21, Birth(21)));
    // 会话 20 里出现孤儿 22
    let mut later = a_table;
    later.push(proc(22, 1, 20, 20, 22));
    assert!(observe(&mut s, later).contains(&22), "21 当见证，22 被认领");
}

/// 「为什么不记永久作废」第 2 条：我们的会话 20 成员全死之后，表里出现外人的同号会话。三格都不认领：
/// ① 外人进程的号与出生时间都和记过的不同；② 外人拿到了我们某个旧身份的进程号（20），出生时间不同；
/// ③ 同时还有一遍旧的扫描把我们已经死了的旧身份并回范围（它们复核不到活着）。
#[test]
fn outsider_session_with_the_same_number_is_never_claimed() {
    for cell in 1..=3 {
        let mut s = Scope::new(10, Some(Birth(10)), 2);
        observe(&mut s, base_table());
        let stale_snap = s.clone();
        // 我们的会话 20 全死了
        let ours_gone: Vec<Proc> = base_table().into_iter().filter(|p| p.pid < 20).collect();
        observe(&mut s, ours_gone.clone());
        if cell == 3 {
            // 一遍旧扫描（读表时 20、21 还活着）现在才提交：把旧身份并回来
            assert!(
                s.commit(pass(&stale_snap, base_table(), &base_table()))
                    .stale
            );
            assert!(s.is_recorded(20, Birth(20)) && s.is_recorded(21, Birth(21)));
        }
        let mut t = ours_gone;
        let outsider: Vec<Proc> = match cell {
            1 => vec![proc(30, 1, 30, 20, 300), proc(31, 30, 30, 20, 301)],
            _ => vec![proc(20, 1, 20, 20, 999), proc(31, 20, 20, 20, 301)],
        };
        t.extend(outsider.clone());
        let got = observe(&mut s, t);
        for p in &outsider {
            assert!(!got.contains(&p.pid), "格 {cell}：外人 {} 被认领了", p.pid);
        }
    }
}

/// 范围里只存证据：字段只有记到过的进程、根组的有效期（根组号与根会话）、代次、在飞的遍数。加了别的字段要改这条测试，
/// 改的人得回答「它是证据还是结论」。
#[test]
fn scope_holds_only_evidence() {
    let src = include_str!("../scope.rs");
    let start = src.find("pub struct Scope {").expect("有 Scope 的定义");
    let body = &src[start..start + src[start..].find("\n}").unwrap()];
    let fields: Vec<&str> = body
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && l.contains(':'))
        .map(|l| l.split(':').next().unwrap().trim())
        .collect();
    assert_eq!(
        fields,
        ["root", "root_sid", "recorded", "generation", "in_flight"]
    );
}

// ---------- 读一个进程的两次系统调用之间换人（审查 S6-5 5/F-1） ----------

/// reviewer 的场景：我们的 21（出生时间 21）原在会话 20，后来另立会话 21；会话 20 原来的成员死光，外人重用了号 20 建了
/// 会话 20，外人的孩子 30 在里面。复核见证 21 时：先读到 21 的出生时间，随后 21 退出、被回收，外人会话 20 的新孩子拿到
/// 号 21，getsid(21) 读到 20。只按「先 bsd 后 getsid」读会拼出（出生时间 21，会话 20），把 21 当成会话 20 的旧见证，
/// 认下外人 30。照 getsid 前后各读一次出生时间的读法，拼不出这一条，30 不被认领。
#[test]
fn torn_read_cannot_forge_a_witness() {
    use crate::process::scan::tests::{Scripted, bsd};
    use crate::process::scan::{ProcSys, read_consistent};
    // 范围：21 记着会话 20（另立会话之前记下的）
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    let mut t0: Vec<Proc> = base_table().into_iter().filter(|p| p.pid != 21).collect();
    t0.push(proc(21, 20, 20, 20, 21));
    observe(&mut s, t0);
    // 这一遍读到的表：20 是外人（重用了号，出生时间 900），30 是它的孩子；我们的 21 已另立会话 21
    let table = Table::from_procs(vec![
        proc(1, 1, 1, 1, 1),
        proc(2, 1, 2, 2, 2),
        proc(10, 2, 10, 2, 10),
        proc(20, 1, 20, 20, 900),
        proc(30, 20, 20, 20, 300),
        proc(21, 1, 21, 21, 21),
    ]);
    // 复核 21：第一次 bsd 读到 21 自己，getsid 时号码已经换给外人（会话 20、出生时间 999）
    let torn = || {
        Scripted::new(
            vec![bsd(21), bsd(999), bsd(999), bsd(999)],
            vec![Some(20), Some(20)],
        )
    };
    let recheck_with = |read: &dyn Fn(&Scripted) -> Option<(Birth, Pid)>| -> Recheck {
        let sys = torn();
        s.witness_pids()
            .into_iter()
            .filter(|p| *p == 21)
            .map(|p| (p, read(&sys)))
            .collect()
    };
    // 旧读法（先 bsd 后 getsid，不核第二次）：拼出假见证，认下外人 30
    let old = recheck_with(&|sys| {
        let b = sys.bsd(21).ok()??;
        Some((b.birth, sys.sid(21)?))
    });
    assert_eq!(old[&21], Some((Birth(21), 20)), "对照：旧读法拼出了假见证");
    let mut old_scope = s.clone();
    assert!(
        old_scope.observe(&table, &old).pids().contains(&30),
        "对照：旧读法认下了外人"
    );
    // 新读法：拼不出，30 不被认领
    let new = recheck_with(&|sys| {
        let p = read_consistent(sys, 21).ok()??;
        Some((p.birth?, p.sid?))
    });
    assert_eq!(new[&21], Some((Birth(999), 20)));
    assert!(
        !s.observe(&table, &new).pids().contains(&30),
        "外人 30 不被认领"
    );
}
