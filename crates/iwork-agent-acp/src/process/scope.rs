//! 清理范围（T06 方案 5.4a）：一棵运行时进程树「有谁」，只在这里定义。纯：进程表与复核结果由调用方读好
//! 传进来，这里不碰系统。
//!
//! 范围有三样成员：
//!
//! | 成员 | 怎么进 | 什么时候失效 |
//! | --- | --- | --- |
//! | 记到过的进程 | （进程号，系统原生的出生时间）两样都对得上 | 这个进程没了 |
//! | 根组 | 桥接的进程组 | 桥接被回收的那一刻（[`Scope::mark_reaped`]） |
//! | 受管会话 | 记到过的进程所在、不是根会话的会话 | 读完进程表之后复核，没有一个记到过的成员还活着、还在这个会话里 |
//!
//! 一个活着的进程在范围里，当且仅当：它是记到过的进程；或根组有效、它在根组里；或它的会话是受管会话、
//! 并且过了复核；或顺着父进程号能走到上面任何一种。进程组号除了根组以外不参与认领（方案「为什么拿掉
//! 受管进程组」）。证明不了的宁可漏，不认领、不发信号。

use std::collections::{BTreeMap, BTreeSet};

/// 进程号（也用作进程组号、会话号）。
pub type Pid = i32;

/// 系统原生的出生时间：macOS 是微秒，Linux 是开机以来的时钟滴答。只拿来比相等。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Birth(pub u64);

/// 进程是不是停住了（SIGSTOP 之后）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run {
    Running,
    Stopped,
}

/// 进程表里的一行。已经退出、等回收的进程（僵尸）和读不到的进程（别的用户的、读的那一刻刚好没了的）
/// 不进进程表：它们不算活成员，也不能当任何东西的证明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: Pid,
    pub ppid: Pid,
    pub pgid: Pid,
    /// 会话号；读不到时为 `None`（这个进程不能被记下，也不能按会话认领）
    pub sid: Option<Pid>,
    /// 原生出生时间；读不到时为 `None`（这个进程不算任何记到过的进程，也不能被记下）
    pub birth: Option<Birth>,
    pub run: Run,
}

/// 一遍扫描读到的进程表。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table(pub BTreeMap<Pid, Proc>);

impl Table {
    pub fn from_procs(procs: impl IntoIterator<Item = Proc>) -> Self {
        Self(procs.into_iter().map(|p| (p.pid, p)).collect())
    }
}

/// 读完进程表**之后**对见证的复核：进程号 → 此刻的（出生时间，会话号）；没了或读不到为 `None`。
pub type Recheck = BTreeMap<Pid, Option<(Birth, Pid)>>;

/// 范围里的一个活进程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub pid: Pid,
    /// 这一遍扫描读到的出生时间：发信号时按它认人（5.4a「发信号按身份」）。读不到的成员不发信号
    pub birth: Option<Birth>,
    pub ppid: Pid,
    pub sid: Option<Pid>,
    pub run: Run,
    /// 往上数有几个祖先也在范围里（0 是最靠根的）
    pub depth: u32,
    /// 它是不是记到过的进程（这一遍记下的也算）：只有这样的进程能当见证
    pub recorded: bool,
}

/// 一遍求出来的成员，按从叶子到根排好（深的在前，同深度按进程号）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Members {
    pub list: Vec<Member>,
    /// 这一遍被别的提交超过去了（只加不删地并进了范围）：不拿它下结论（「空了」「都停住了」「只剩见证了」）
    pub stale: bool,
    /// 提交时还有别的扫描在飞（取了快照、还没提交也没放弃）：不拿它判「清空了」（方案 5.4a 第 4a 条）
    pub busy: bool,
}

impl Members {
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn pids(&self) -> BTreeSet<Pid> {
        self.list.iter().map(|m| m.pid).collect()
    }
}

/// 一遍扫描算出来、还没提交的结果（[`Scope::compute`]）。
#[derive(Debug, Clone)]
pub struct Pass {
    /// 这一遍开始时的范围（快照）
    snapshot: Scope,
    table: Table,
    recheck: Recheck,
    /// 照完整提交算出来的范围
    next: Scope,
    members: Vec<Member>,
}

impl Pass {
    /// 测试用：这一遍算出来的成员（还没提交）。
    #[cfg(test)]
    pub fn members_for_test(&self) -> &[Member] {
        &self.members
    }
}

/// 清理范围。由 `ProcessTree` 一处持有。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// 根组号；桥接被回收之后为 `None`
    root: Option<Pid>,
    /// 根会话（桥接所在、也是后台所在的会话）：永远不当受管会话
    root_sid: Pid,
    /// 记到过的进程：身份（进程号，出生时间）→ 记它时的会话号。同一个进程号可以有两条不同出生时间的身份
    /// （只加不删并进来的，没了的那条等下一遍完整提交删）
    recorded: BTreeMap<(Pid, Birth), Pid>,
    /// 还有效的受管会话
    sessions: BTreeSet<Pid>,
    /// 作废的会话号：以后不再按它认领
    dropped: BTreeSet<Pid>,
    /// 代次：范围每改一次加一（扫描提交、会话作废、根组作废）。一遍扫描提交时拿它判断中间有没有别的改动
    generation: u64,
    /// 在飞的扫描有几遍（取了快照、还没提交也没放弃）。不属于范围的内容：完整提交时保留当前值，不取快照里的
    in_flight: usize,
}

impl Scope {
    /// `root`：桥接的进程号（也是根组号）；`root_birth`：它的出生时间（起它之后立刻读，读到了就记成记到过的进程：
    /// 一遍扫描都没做成时范围里也有它，审查 S6-5 1/F-3）；`root_sid`：桥接所在的会话。
    pub fn new(root: Pid, root_birth: Option<Birth>, root_sid: Pid) -> Self {
        let recorded = root_birth
            .map(|birth| ((root, birth), root_sid))
            .into_iter()
            .collect();
        Self {
            root: Some(root),
            root_sid,
            recorded,
            sessions: BTreeSet::new(),
            dropped: BTreeSet::new(),
            generation: 0,
            in_flight: 0,
        }
    }

    /// 桥接被回收了（或者已经被别处回收）：根组从此不算数。
    pub fn mark_reaped(&mut self) {
        self.root = None;
        self.generation += 1;
    }

    pub fn root_valid(&self) -> bool {
        self.root.is_some()
    }

    /// 读完进程表之后要复核的进程号：受管会话里记到过的成员。
    pub fn witness_pids(&self) -> Vec<Pid> {
        let pids: BTreeSet<Pid> = self
            .recorded
            .iter()
            .filter(|(_, sid)| self.sessions.contains(sid))
            .map(|((pid, _), _)| *pid)
            .collect();
        pids.into_iter().collect()
    }

    /// 一遍扫描开始：在飞的数加一，返回快照。调用方在提交或放弃时调 [`Scope::end_scan`]（用离开作用域就减的守卫）。
    pub fn begin_scan(&mut self) -> Scope {
        self.in_flight += 1;
        self.clone()
    }

    /// 一遍扫描结束（提交了、报错了、崩了）。
    pub fn end_scan(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// 在飞的扫描有几遍（测试用）。
    #[cfg(test)]
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// 这个身份是不是记到过的进程。
    pub fn is_recorded(&self, pid: Pid, birth: Birth) -> bool {
        self.recorded.contains_key(&(pid, birth))
    }

    /// 记一个身份；同一个身份已经记着别的会话号时，取等于它自己进程号的那个（进程换会话只有自己另立会话一条路，
    /// 立完会话号就是它自己的进程号，之后不再变，所以那个是后来的；审查 S6-5 3/F-1）。
    fn record(&mut self, pid: Pid, birth: Birth, sid: Pid) {
        let e = self.recorded.entry((pid, birth)).or_insert(sid);
        if *e != sid && sid == pid {
            *e = sid;
        }
    }

    /// 只加不删：把成员按身份并进来，它们带来的会话也并进来（作废过的不恢复）。不删任何记到过的进程，不作废会话。
    fn merge(&mut self, members: &[Member]) {
        for m in members {
            let (Some(birth), Some(sid)) = (m.birth, m.sid) else {
                continue;
            };
            self.record(m.pid, birth, sid);
            if sid != self.root_sid && !self.dropped.contains(&sid) {
                self.sessions.insert(sid);
            }
        }
        self.generation += 1;
    }

    /// 让一个会话作废（清理时杀掉了它最后的见证）。
    pub fn drop_session(&mut self, sid: Pid) {
        if self.sessions.remove(&sid) {
            self.dropped.insert(sid);
            self.generation += 1;
        }
    }

    /// 一遍扫描（审查 S6-5 2/F-1，方案 5.4a「扫描怎么提交到范围」）分四步：在锁里取快照（[`Scope::clone`]，连同
    /// 代次）→ 放开锁读进程表 → 复核**快照里的**见证（[`Scope::witness_pids`]）→ 用（快照，表，复核）算结果
    /// （本函数，纯）→ 拿锁提交（[`Scope::commit`]）。
    pub fn compute(snapshot: &Scope, table: &Table, recheck: &Recheck) -> Pass {
        let mut next = snapshot.clone();
        let members = next.apply(table, recheck);
        Pass {
            snapshot: snapshot.clone(),
            table: table.clone(),
            recheck: recheck.clone(),
            next,
            members,
        }
    }

    /// 提交一遍扫描。开始之后没有别的改动 → 完整提交（可删、可作废会话）；有 → 只加不删（[`Scope::merge`]），返回的
    /// 成员标成旧的。根组不归扫描管，哪一遍都碰不到它的有效期。返回的成员带上「提交时还有没有别的扫描在飞」（调用方
    /// 这时还算在在飞的数里，提交之后再 [`Scope::end_scan`]）。
    pub fn commit(&mut self, pass: Pass) -> Members {
        let mut members = self.commit_inner(pass);
        members.busy = self.in_flight > 1;
        members
    }

    fn commit_inner(&mut self, pass: Pass) -> Members {
        // 3a（审查 S6-5 3/F-2）：快照里根组有效、现在已经作废（桥接在这期间被回收）。这一遍原来的结果是按「根组有效」
        // 算的，读表时那个组号可能已经换了人：照「根组无效」用同一份快照、同一张表、同一次复核重算，只加不删。丢掉的只有
        // 凭根组号认下的（和只能顺它们的父链走到的）；凭身份、凭会话、顺它们的父链认下的照样留下
        if self.root.is_none() && pass.snapshot.root.is_some() {
            let mut again = pass.snapshot.clone();
            again.root = None;
            let members = again.apply(&pass.table, &pass.recheck);
            self.merge(&members);
            return Members {
                list: members,
                stale: true,
                busy: false,
            };
        }
        if pass.snapshot.generation == self.generation {
            let (root, in_flight) = (self.root, self.in_flight);
            *self = pass.next;
            self.root = root;
            self.in_flight = in_flight;
            self.generation = pass.snapshot.generation + 1;
            return Members {
                list: pass.members,
                stale: false,
                busy: false,
            };
        }
        self.merge(&pass.members);
        Members {
            list: pass.members,
            stale: true,
            busy: false,
        }
    }

    /// 测试用：一遍不和别人交错的扫描（取快照、算、提交连着做）。
    #[cfg(test)]
    pub fn observe(&mut self, table: &Table, recheck: &Recheck) -> Members {
        let pass = Scope::compute(self, table, recheck);
        self.commit(pass)
    }

    /// 照完整提交改自己：先按复核结果核受管会话，再求成员，最后把成员记下。返回从叶子到根排好的成员。
    fn apply(&mut self, table: &Table, recheck: &Recheck) -> Vec<Member> {
        // 记到过的进程没了就失效：表里没有这个号，或者有、读到的出生时间不一样（那是别的进程）。表里有这个号但
        // 这一遍没读到出生时间 → 不删，留给下一遍（这一遍它也不凭身份算成员、不发信号）
        self.recorded.retain(|(pid, birth), _| {
            table
                .0
                .get(pid)
                .is_some_and(|p| p.birth.is_none_or(|b| b == *birth))
        });
        // 受管会话：读完进程表之后，至少一个记到过的成员还活着（出生时间对得上）、还在这个会话里
        let witnessed: BTreeSet<Pid> = self
            .recorded
            .iter()
            .filter(|((pid, birth), sid)| recheck.get(pid) == Some(&Some((*birth, **sid))))
            .map(|(_, sid)| *sid)
            .collect();
        let gone: Vec<Pid> = self
            .sessions
            .iter()
            .filter(|s| !witnessed.contains(s))
            .copied()
            .collect();
        for sid in gone {
            self.drop_session(sid);
        }

        let base = |p: &Proc| -> bool {
            let recorded = p.birth.is_some_and(|b| self.is_recorded(p.pid, b));
            let in_root = self.root == Some(p.pgid);
            let in_session = p.sid.is_some_and(|s| self.sessions.contains(&s));
            recorded || in_root || in_session
        };
        let mut children: BTreeMap<Pid, Vec<Pid>> = BTreeMap::new();
        for p in table.0.values() {
            if p.ppid != p.pid {
                children.entry(p.ppid).or_default().push(p.pid);
            }
        }
        let mut members: BTreeSet<Pid> = table
            .0
            .values()
            .filter(|p| base(p))
            .map(|p| p.pid)
            .collect();
        let mut queue: Vec<Pid> = members.iter().copied().collect();
        while let Some(pid) = queue.pop() {
            for c in children.get(&pid).into_iter().flatten() {
                if members.insert(*c) {
                    queue.push(*c);
                }
            }
        }

        // 记下：每个读得到出生时间与会话号的成员；它带来的非根会话成为受管会话（作废过的不再恢复）
        for pid in &members {
            let p = &table.0[pid];
            if let (Some(birth), Some(sid)) = (p.birth, p.sid) {
                self.recorded.insert((*pid, birth), sid);
                if sid != self.root_sid && !self.dropped.contains(&sid) {
                    self.sessions.insert(sid);
                }
            }
        }

        let depth_of = |mut pid: Pid| -> u32 {
            let mut d = 0;
            // 父链上有环（进程号 0、1 这类）时最多走表的大小那么多步
            for _ in 0..table.0.len() {
                let ppid = table.0[&pid].ppid;
                if ppid == pid || !members.contains(&ppid) {
                    break;
                }
                d += 1;
                pid = ppid;
            }
            d
        };
        let mut out: Vec<Member> = members
            .iter()
            .map(|pid| {
                let p = &table.0[pid];
                Member {
                    pid: p.pid,
                    birth: p.birth,
                    ppid: p.ppid,
                    sid: p.sid,
                    run: p.run,
                    depth: depth_of(p.pid),
                    recorded: p.birth.is_some_and(|b| self.is_recorded(p.pid, b)),
                }
            })
            .collect();
        out.sort_by(|a, b| b.depth.cmp(&a.depth).then(a.pid.cmp(&b.pid)));
        out
    }

    /// 清理时每个受管会话留到最后的见证：停住了的、记到过的成员里离根最近的那个。
    pub fn witnesses(&self, members: &Members) -> BTreeMap<Pid, Pid> {
        let mut best: BTreeMap<Pid, (u32, Pid)> = BTreeMap::new();
        for m in &members.list {
            let Some(sid) = m.sid else { continue };
            if !m.recorded || m.run != Run::Stopped || !self.sessions.contains(&sid) {
                continue;
            }
            let e = best.entry(sid).or_insert((m.depth, m.pid));
            if (m.depth, m.pid) < *e {
                *e = (m.depth, m.pid);
            }
        }
        best.into_iter().map(|(sid, (_, pid))| (sid, pid)).collect()
    }
}

#[cfg(test)]
mod tests;
