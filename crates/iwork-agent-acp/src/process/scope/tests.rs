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
                for m in &members.0 {
                    let Some(birth) = m.birth else { continue };
                    let os = WorldOs {
                        world: &w,
                        landed: std::cell::RefCell::default(),
                    };
                    let who = Ident { pid: m.pid, birth };
                    verify_then_kill(&os, who, libc::SIGKILL);
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

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
    let order: Vec<Pid> = m.0.iter().map(|m| m.pid).collect();
    assert_eq!(order, vec![21, 20, 11, 10]);
}
