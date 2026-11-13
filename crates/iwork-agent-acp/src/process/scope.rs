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

/// 一个记到过的进程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Recorded {
    birth: Birth,
    /// 最近一次看到它时的会话号
    sid: Pid,
}

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
    /// 这一遍开始时范围的代次
    since: u64,
    /// 照完整提交算出来的范围
    next: Scope,
    members: Vec<Member>,
}

/// 清理范围。由 `ProcessTree` 一处持有。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// 根组号；桥接被回收之后为 `None`
    root: Option<Pid>,
    /// 根会话（桥接所在、也是后台所在的会话）：永远不当受管会话
    root_sid: Pid,
    recorded: BTreeMap<Pid, Recorded>,
    /// 还有效的受管会话
    sessions: BTreeSet<Pid>,
    /// 作废的会话号：以后不再按它认领
    dropped: BTreeSet<Pid>,
    /// 代次：范围每改一次加一（扫描提交、会话作废、根组作废）。一遍扫描提交时拿它判断中间有没有别的改动
    generation: u64,
}

impl Scope {
    /// `root`：桥接的进程号（也是根组号）；`root_birth`：它的出生时间（起它之后立刻读，读到了就记成记到过的进程：
    /// 一遍扫描都没做成时范围里也有它，审查 S6-5 1/F-3）；`root_sid`：桥接所在的会话。
    pub fn new(root: Pid, root_birth: Option<Birth>, root_sid: Pid) -> Self {
        let recorded = root_birth
            .map(|birth| {
                (
                    root,
                    Recorded {
                        birth,
                        sid: root_sid,
                    },
                )
            })
            .into_iter()
            .collect();
        Self {
            root: Some(root),
            root_sid,
            recorded,
            sessions: BTreeSet::new(),
            dropped: BTreeSet::new(),
            generation: 0,
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
        self.recorded
            .iter()
            .filter(|(_, r)| self.sessions.contains(&r.sid))
            .map(|(pid, _)| *pid)
            .collect()
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
            since: snapshot.generation,
            next,
            members,
        }
    }

    /// 提交一遍扫描。开始之后没有别的改动 → 完整提交（可删、可作废会话）；有 → 只加不删：这一遍的新成员与它们
    /// 带来的会话并进来（作废过的不恢复），不删记到过的、不作废会话，返回的成员标成旧的。根组不归扫描管，哪一遍都
    /// 碰不到它；快照之后根组作废了的那一遍整个丢掉。
    pub fn commit(&mut self, pass: Pass) -> Members {
        // 快照之后根组作废了（桥接被回收）：这一遍是按「根组有效」算的，读表时那个组号可能已经换了人，凭根组认下的
        // 成员不可信。整遍丢掉，什么都不并进来，当没做成（回收只在一次成功的清理之后，那时范围里已没有活成员）
        if self.root.is_none() && pass.next.root.is_some() {
            return Members {
                list: Vec::new(),
                stale: true,
            };
        }
        if pass.since == self.generation {
            let root = self.root;
            *self = pass.next;
            self.root = root;
            self.generation = pass.since + 1;
            return Members {
                list: pass.members,
                stale: false,
            };
        }
        for m in &pass.members {
            let (Some(birth), Some(sid)) = (m.birth, m.sid) else {
                continue;
            };
            self.recorded
                .entry(m.pid)
                .or_insert(Recorded { birth, sid });
            if sid != self.root_sid && !self.dropped.contains(&sid) {
                self.sessions.insert(sid);
            }
        }
        self.generation += 1;
        Members {
            list: pass.members,
            stale: true,
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
        self.recorded.retain(|pid, r| {
            table
                .0
                .get(pid)
                .is_some_and(|p| p.birth.is_none_or(|b| b == r.birth))
        });
        // 受管会话：读完进程表之后，至少一个记到过的成员还活着（出生时间对得上）、还在这个会话里
        let witnessed: BTreeSet<Pid> = self
            .recorded
            .iter()
            .filter(|(pid, r)| recheck.get(pid) == Some(&Some((r.birth, r.sid))))
            .map(|(_, r)| r.sid)
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
            let recorded =
                p.birth.is_some() && self.recorded.get(&p.pid).map(|r| r.birth) == p.birth;
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
                self.recorded.insert(*pid, Recorded { birth, sid });
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
                    recorded: self.recorded.contains_key(pid),
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
