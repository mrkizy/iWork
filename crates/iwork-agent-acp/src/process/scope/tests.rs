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
}

fn staged() -> impl Strategy<Value = Staged> {
    prop_oneof![
        2 => op().prop_map(Staged::World),
        3 => (0..2usize).prop_map(Staged::Advance),
    ]
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
    for (n, s) in steps.iter().enumerate() {
        match s {
            Staged::World(op) => w.apply(op, &mut scope),
            Staged::Advance(k) => {
                let scan = &mut scans[*k];
                if scan.snapshot.is_none() {
                    scan.snapshot = Some(scope.clone());
                    continue;
                }
                if scan.table.is_none() {
                    scan.table = Some(w.table());
                    continue;
                }
                if scan.recheck.is_none() {
                    let pids = scan.snapshot.as_ref().unwrap().witness_pids();
                    scan.recheck = Some(w.recheck(&pids));
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
                // 记到过的、此刻还活着（同一个身份）的，提交之后都还在
                for (pid, r) in &before.recorded {
                    let alive = w.procs.get(pid).is_some_and(|p| p.birth == r.birth.0);
                    if alive && !scope.recorded.contains_key(pid) {
                        return Err(format!(
                            "第 {n} 步的提交删掉了还活着的记到过的进程 {pid}（旧={}）",
                            members.stale
                        ));
                    }
                }
                // 被超过去的那一遍不作废会话
                if members.stale && !before.sessions.is_subset(&scope.sessions) {
                    return Err(format!("第 {n} 步：旧的那一遍作废了会话"));
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
    let cases: [(&str, Recheck, bool); 3] = [
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

/// 最后一个见证没了，下一次求范围就不认这个会话，不等周期删除。
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
    let p = pass(&snap, with_22, &now);
    assert!(!p.next.sessions.contains(&20));
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
    assert!(s.recorded.contains_key(&20), "没读到出生时间的不删");
}

/// 属性测试找到的：一遍扫描取快照时根组还有效，之后桥接被回收、号码被外人重用并建成同号的组；这一遍照快照里的
/// 「根组有效」把外人认成根组成员。提交时发现根组在快照之后作废了，整遍丢掉，什么都不并进来。
#[test]
fn scan_spanning_a_reap_is_discarded() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    let snap = s.clone();
    s.mark_reaped();
    // 号码 10 被外人拿到、自成一组
    let table = vec![
        proc(1, 1, 1, 1, 1),
        proc(2, 1, 2, 2, 2),
        proc(10, 1, 10, 1, 99),
    ];
    let got = s.commit(pass(&snap, table.clone(), &table));
    assert!(got.stale && got.is_empty());
    assert!(
        !s.recorded.contains_key(&10) || s.recorded[&10].birth == Birth(10),
        "外人没被记下"
    );
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

/// 快照之后会话 S 作废（清理杀掉了它最后的见证），中间没有别的扫描提交，再提交那一遍：S 仍然是作废的，表里同号会话
/// 的进程不被认领（作废也让代次加一，那一遍因此只加不删；作废过的会话不恢复）。
#[test]
fn dropped_session_is_not_restored_by_an_older_snapshot() {
    let mut s = Scope::new(10, Some(Birth(10)), 2);
    observe(&mut s, base_table());
    let snap = s.clone();
    s.drop_session(20);
    // 那一遍读到的表里会话 20 的进程都还在（父链已断）
    let mut t: Vec<Proc> = base_table().into_iter().filter(|p| p.pid != 11).collect();
    t.iter_mut().find(|p| p.pid == 20).unwrap().ppid = 1;
    let got = s.commit(pass(&snap, t.clone(), &t));
    assert!(got.stale, "作废让代次变了，这一遍被超过去了");
    assert!(
        !s.sessions.contains(&20) && s.dropped.contains(&20),
        "S 仍然作废"
    );
    // 之后的完整扫描里，会话 20 里一个不靠身份的新进程不被认领
    t.push(proc(25, 1, 25, 20, 25));
    assert!(!observe(&mut s, t).contains(&25));
}
