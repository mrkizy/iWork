//! 助手配置自动保存闸（`store::assistant_edits`，步 7c）的交错属性测试。
//!
//! 后台建模成「版本号 + 配置」：命令按乐观并发受理（版本对不上回 CONFLICT，名字为空回校验错误），按 command id
//! 去重（断线重发同一条只生效一次）。随机混合：用户改一项、别的写入方并发改任意一项、命令送达后台、回包送达
//! 客户端、摘要重查（读到的是那一刻的后台）、断线后重发在途命令。每一步之后、以及全部落定之后查不变量：
//! - I1 同一个助手同时在途的用户命令不超过一条；
//! - I2 没有别的写入方时，从不因版本冲突被拒；
//! - I3 用户的命令只改用户改过的字段（别的写入方改的其他字段不会被盖掉）；
//! - I4 全部落定后，用户改过的每一项都落到了后台；后台的值是用户最后一次的值，除非别的写入方在用户那一次落地
//!   之后又改了它（同一字段两边都改时，用户还没落定的改动胜出：它会以「最新配置 + 补丁」重放）；
//!   用户的非法改动（空名字，后台校验拒）不落地、不重试、不把同一条命令里合法的改动拖下去（闸拆开逐个重放），
//!   之后没有别的改动时页面拿到错误；
//! - I5 页面上挂着「名字不能为空」时，用户最后一次改名字确实是改成空（被新的合法名字替代的旧拒绝不挂错误，
//!   审查 g3 1/F-3）。
//!
//! 「已落定的字段被别的写入方改过、用户之后只改另一项，不把旧值盖回去」随机序列里 I3/I4 合起来证不到（I3 只看
//! 用户从没碰过的字段，I4 碰到落定后的外部写就跳过），单独固定一条交错（`landed_field_changed_by_other_is_not_reverted`）。
//!
//! 对照（证明测试有牙）：不加闸的写法（每次改动立刻拿摘要的版本与整份配置发出去）必须被抓到。

use std::collections::{BTreeMap, VecDeque};

use iwork_client_core::store::assistant_edits::{AssistantEdits, Base, Field, FieldKey, Send};
use iwork_proto::pb;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestError, TestRunner};

const ID: &str = "a";

// ---------- 两种保存法 ----------

trait Saver {
    fn edit(&mut self, f: Field, base: Base<'_>) -> Option<Send>;
    fn observed(&mut self, base: Base<'_>) -> Option<Send>;
    fn accepted(&mut self, base: Base<'_>) -> Option<Send>;
    fn rejected(&mut self, conflict: bool, base: Base<'_>) -> Option<Send>;
    fn error(&self) -> bool;
}

struct Gate(AssistantEdits);

impl Saver for Gate {
    fn edit(&mut self, f: Field, base: Base<'_>) -> Option<Send> {
        self.0.edit(ID, f, Some(base))
    }
    fn observed(&mut self, base: Base<'_>) -> Option<Send> {
        self.0.observed(ID, Some(base))
    }
    fn accepted(&mut self, base: Base<'_>) -> Option<Send> {
        self.0.accepted(ID, Some(base))
    }
    fn rejected(&mut self, conflict: bool, base: Base<'_>) -> Option<Send> {
        self.0.rejected(ID, conflict, "名字不能为空", Some(base))
    }
    fn error(&self) -> bool {
        self.0.error(ID).is_some()
    }
}

/// 不加闸：每次改动立刻拿摘要里的版本与整份配置 + 这一项发出去。
struct Naive;

impl Saver for Naive {
    fn edit(&mut self, f: Field, base: Base<'_>) -> Option<Send> {
        let mut config = base.config.clone();
        f.apply(&mut config);
        Some(Send {
            assistant_id: ID.into(),
            config,
            expected_version: base.version,
        })
    }
    fn observed(&mut self, _: Base<'_>) -> Option<Send> {
        None
    }
    fn accepted(&mut self, _: Base<'_>) -> Option<Send> {
        None
    }
    fn rejected(&mut self, _: bool, _: Base<'_>) -> Option<Send> {
        None
    }
    fn error(&self) -> bool {
        true
    }
}

// ---------- 世界 ----------

#[derive(Debug, Clone)]
enum Op {
    /// 用户改一项（第几项、取第几个值）。
    User(usize, usize),
    /// 用户把名字改成空（后台校验拒）。
    BadName,
    /// 别的写入方读后台最新配置、改一项、按当前版本写回（守规矩的写入方）。
    Other(usize, usize),
    /// 后台处理队首的命令。
    Process,
    /// 客户端收到队首的回包。
    Reply,
    /// 摘要重查：客户端读到此刻的后台。
    Refresh,
    /// 断线重连：在途（没收到回包）的命令按原 id 重发。
    Resend,
}

fn op(with_other: bool) -> BoxedStrategy<Op> {
    let base = prop_oneof![
        4 => (0usize..KEYS.len(), 0usize..3).prop_map(|(f, v)| Op::User(f, v)),
        1 => Just(Op::BadName),
        4 => Just(Op::Process),
        4 => Just(Op::Reply),
        3 => Just(Op::Refresh),
        1 => Just(Op::Resend),
    ];
    if with_other {
        prop_oneof![5 => base, 1 => (0usize..KEYS.len(), 0usize..3).prop_map(|(f, v)| Op::Other(f, v))]
            .boxed()
    } else {
        base.boxed()
    }
}

/// 第 `f` 项取第 `v` 个值。七项：名字、提示词、模型、三个工具分组、头像配色。
fn field(f: usize, v: usize) -> Field {
    let names = ["阿森", "林岚", "老周"];
    let models = ["opus", "sonnet", "haiku"];
    match f {
        0 => Field::Name(names[v].into()),
        1 => Field::Prompt(format!("提示词 {v}")),
        2 => Field::Model(pb::ModelSelection {
            model_id: models[v].into(),
            effort: None,
        }),
        3 => Field::Tool {
            group: pb::ToolGroup::EditFiles as i32,
            enabled: v.is_multiple_of(2),
        },
        4 => Field::Tool {
            group: pb::ToolGroup::RunCommands as i32,
            enabled: v.is_multiple_of(2),
        },
        5 => Field::Tool {
            group: pb::ToolGroup::Web as i32,
            enabled: !v.is_multiple_of(2),
        },
        _ => Field::AvatarColor(["ch1", "ch6", "ch9"][v].into()),
    }
}

const KEYS: [FieldKey; 7] = [
    FieldKey::Name,
    FieldKey::Prompt,
    FieldKey::Model,
    FieldKey::Tool(pb::ToolGroup::EditFiles as i32),
    FieldKey::Tool(pb::ToolGroup::RunCommands as i32),
    FieldKey::Tool(pb::ToolGroup::Web as i32),
    FieldKey::AvatarColor,
];

/// 一份配置里某一项的值（拿 Field 表示，好比较）。
fn read(c: &pb::AssistantConfig, k: FieldKey) -> Field {
    match k {
        FieldKey::Name => Field::Name(c.name.clone()),
        FieldKey::Prompt => Field::Prompt(c.prompt.clone()),
        FieldKey::Model => Field::Model(c.model.clone().unwrap_or_default()),
        FieldKey::Tool(g) => Field::Tool {
            group: g,
            enabled: !c.disabled_groups.contains(&g),
        },
        FieldKey::AvatarColor => Field::AvatarColor(c.avatar.clone().unwrap_or_default().color),
    }
}

#[derive(Debug, Clone)]
struct Cmd {
    id: u64,
    send: Send,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Reply {
    Accepted { events: bool },
    Conflict,
    Invalid,
}

struct World {
    version: u32,
    config: pb::AssistantConfig,
    /// 客户端看到的摘要（重查时才更新）。
    seen: (u32, pb::AssistantConfig),
    to_backend: VecDeque<Cmd>,
    to_client: VecDeque<(u64, Reply)>,
    /// 后台已处理过的命令（按 id 去重，重发拿同一个回包）。
    done: BTreeMap<u64, Reply>,
    /// 发出去、还没收到回包的命令（重发用）。
    unreplied: BTreeMap<u64, Cmd>,
    next_id: u64,
    time: u64,
    /// 用户每一项最后一次改成的值、改的时刻、这个值被后台受理（落地）的时刻（None = 改了以后没有再受理过；
    /// 后台本来就是这个值时不需要再发命令，见 I4）。
    user_last: BTreeMap<FieldKey, (Field, u64, Option<u64>)>,
    /// 别的写入方每一项最后一次写的时刻。
    other_last: BTreeMap<FieldKey, u64>,
    user_edited: Vec<FieldKey>,
    conflicts: usize,
    bad_name_pending: bool,
    /// 用户最后一次改名字是改成空。
    name_bad: bool,
}

impl World {
    fn new() -> Self {
        let config = pb::AssistantConfig {
            name: "初始".into(),
            prompt: "初始提示词".into(),
            runtime_id: "claude-code".into(),
            model: Some(pb::ModelSelection {
                model_id: "opus".into(),
                effort: None,
            }),
            ..Default::default()
        };
        World {
            version: 1,
            seen: (1, config.clone()),
            config,
            to_backend: VecDeque::new(),
            to_client: VecDeque::new(),
            done: BTreeMap::new(),
            unreplied: BTreeMap::new(),
            next_id: 0,
            time: 0,
            user_last: BTreeMap::new(),
            other_last: BTreeMap::new(),
            user_edited: Vec::new(),
            conflicts: 0,
            bad_name_pending: false,
            name_bad: false,
        }
    }

    fn base(&self) -> Base<'_> {
        Base {
            version: self.seen.0,
            config: &self.seen.1,
        }
    }

    fn send(&mut self, s: Option<Send>) {
        if let Some(send) = s {
            self.next_id += 1;
            let cmd = Cmd {
                id: self.next_id,
                send,
            };
            self.unreplied.insert(cmd.id, cmd.clone());
            self.to_backend.push_back(cmd);
        }
    }

    /// 后台处理一条命令。
    fn process(&mut self) -> Result<(), TestCaseError> {
        let Some(cmd) = self.to_backend.pop_front() else {
            return Ok(());
        };
        self.time += 1;
        let reply = if let Some(r) = self.done.get(&cmd.id) {
            *r
        } else if cmd.send.config.name.trim().is_empty() {
            Reply::Invalid
        } else if cmd.send.expected_version != self.version {
            self.conflicts += 1;
            Reply::Conflict
        } else if cmd.send.config == self.config {
            // 配置没变：受理、不产生事件；用户要的值已经在后台了，同样算落地
            self.mark_landed();
            Reply::Accepted { events: false }
        } else {
            // I3：用户的命令只改用户改过的字段
            for k in KEYS {
                if read(&cmd.send.config, k) != read(&self.config, k) {
                    prop_assert!(
                        self.user_edited.contains(&k),
                        "用户的命令改了用户没碰过的 {:?}：{:?} → {:?}",
                        k,
                        read(&self.config, k),
                        read(&cmd.send.config, k)
                    );
                }
            }
            self.config = cmd.send.config.clone();
            self.version += 1;
            self.mark_landed();
            Reply::Accepted { events: true }
        };
        self.done.insert(cmd.id, reply);
        self.to_client.push_back((cmd.id, reply));
        Ok(())
    }

    /// 用户的命令被后台受理：后台此刻和用户最后一次的值相同的那几项记为这一刻落地（已经落过地的不改时刻）。
    fn mark_landed(&mut self) {
        for (k, (v, _, at)) in &mut self.user_last {
            if at.is_none() && read(&self.config, *k) == *v {
                *at = Some(self.time);
            }
        }
    }

    fn other(&mut self, f: Field) {
        self.time += 1;
        f.apply(&mut self.config);
        self.version += 1;
        self.other_last.insert(f.key(), self.time);
    }
}

fn step(w: &mut World, s: &mut dyn Saver, op: &Op) -> Result<(), TestCaseError> {
    match op {
        Op::User(f, v) => {
            let f = field(*f, *v);
            w.user_edited.push(f.key());
            w.time += 1;
            w.user_last.insert(f.key(), (f.clone(), w.time, None));
            // 任何一次合法改动都清掉上一次的错误
            w.bad_name_pending = false;
            if f.key() == FieldKey::Name {
                w.name_bad = false;
            }
            let out = s.edit(f, w.base());
            w.send(out);
        }
        Op::BadName => {
            // 空名字把之前没落定的名字改动也替换掉了：名字这一项不再有期望值
            w.bad_name_pending = true;
            w.name_bad = true;
            w.user_edited.push(FieldKey::Name);
            w.user_last.remove(&FieldKey::Name);
            let out = s.edit(Field::Name("  ".into()), w.base());
            w.send(out);
        }
        Op::Other(f, v) => w.other(field(*f, *v)),
        Op::Process => w.process()?,
        Op::Reply => {
            if let Some((id, r)) = w.to_client.pop_front() {
                // 同一条命令的回包只认第一次（重发得到的第二个回包，会话层的在飞表里已经没有它了）
                if w.unreplied.remove(&id).is_some() {
                    let out = match r {
                        // 回包分不出有没有产生事件（真后台没有事件时 first_seq = last_seq = head），闸不看它
                        Reply::Accepted { .. } => s.accepted(w.base()),
                        Reply::Conflict => s.rejected(true, w.base()),
                        Reply::Invalid => s.rejected(false, w.base()),
                    };
                    w.send(out);
                }
            }
        }
        Op::Refresh => {
            w.seen = (w.version, w.config.clone());
            let out = s.observed(w.base());
            w.send(out);
        }
        Op::Resend => {
            let again: Vec<Cmd> = w.unreplied.values().cloned().collect();
            w.to_backend.extend(again);
        }
    }
    // I1：同时在途（发出、没收到回包）的用户命令不超过一条
    prop_assert!(w.unreplied.len() <= 1, "同时在途 {} 条", w.unreplied.len());
    Ok(())
}

fn run(s: &mut dyn Saver, ops: &[Op], with_other: bool) -> Result<(), TestCaseError> {
    let mut w = World::new();
    for op in ops {
        step(&mut w, s, op)?;
    }
    // 落定：命令、回包、摘要轮流走到没东西可走（有上限：闸不能一直重发）
    for round in 0.. {
        prop_assert!(round < 200, "落不定：一直有命令在发");
        let quiet = w.to_backend.is_empty() && w.to_client.is_empty();
        for op in [Op::Process, Op::Reply, Op::Refresh] {
            step(&mut w, s, &op)?;
        }
        if quiet && w.to_backend.is_empty() && w.to_client.is_empty() && w.unreplied.is_empty() {
            break;
        }
    }
    if !with_other {
        prop_assert_eq!(w.conflicts, 0, "没有别的写入方，却因版本冲突被拒");
    }
    // I4：参照时刻 = 用户最后一次的值被后台受理的时刻；改了以后没再受理过（后台本来就是这个值，闸不必再发），
    // 就取改的时刻。参照时刻之后别的写入方没再改这一项，后台的值就得是用户的。
    for k in KEYS {
        let Some((want, edited, landed)) = w.user_last.get(&k) else {
            continue;
        };
        let since = landed.unwrap_or(*edited);
        let other_after = w.other_last.get(&k).is_some_and(|t| *t > since);
        if !other_after {
            prop_assert_eq!(&read(&w.config, k), want, "{:?} 不是用户最后一次的值", k);
        }
    }
    if w.bad_name_pending {
        prop_assert!(s.error(), "空名字被后台拒了，页面却没拿到错误");
    }
    // I5
    if s.error() {
        prop_assert!(w.name_bad, "空名字已被合法名字替代，页面还挂着旧错误");
    }
    prop_assert!(!w.config.name.trim().is_empty(), "空名字写进了后台");
    Ok(())
}

fn ops(with_other: bool) -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(op(with_other), 1..60)
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]

    /// 只有用户在改：从不冲突，全部落定后每一项都是用户最后一次的值。
    #[test]
    fn gate_alone(ops in ops(false)) {
        run(&mut Gate(AssistantEdits::default()), &ops, false)?;
    }

    /// 别的写入方并发改：用户命令只动用户改过的字段，别人改的其他字段留着；同一字段用户没落定的改动胜出。
    #[test]
    fn gate_with_other_writer(ops in ops(true)) {
        run(&mut Gate(AssistantEdits::default()), &ops, true)?;
    }
}

/// 对照：不加闸的写法必须被抓到（只有用户在改时就会撞冲突或丢改动）。
#[test]
fn naive_saver_is_caught() {
    let mut runner = TestRunner::new(Config {
        cases: 256,
        failure_persistence: None,
        ..Config::default()
    });
    let r = runner.run(&ops(false), |ops| run(&mut Naive, &ops, false));
    match r {
        Err(TestError::Fail(reason, ops)) => {
            eprintln!("不加闸被抓到：{reason}；最小序列 {ops:?}");
        }
        other => panic!("不加闸的写法没被抓到：{other:?}"),
    }
}

/// 缩出来的反例，固定下来。
fn pinned(ops: Vec<Op>, with_other: bool) {
    if let Err(e) = run(&mut Gate(AssistantEdits::default()), &ops, with_other) {
        panic!("{e}");
    }
}

/// 空名字和合法的提示词并进同一条命令被后台拒：闸拆开逐个重放，提示词照常落地，空名字丢掉并报错。
#[test]
fn invalid_name_does_not_drag_valid_edits_down() {
    use Op::*;
    pinned(vec![User(0, 0), BadName, User(1, 0)], false);
}

/// 命令在途时用户把模型改回同一个值，期间别的写入方改成了别的：这次改动要再发一次（按编号落定，不按值）。
#[test]
fn same_value_re_edit_after_other_writer_is_resent() {
    use Op::*;
    pinned(
        vec![
            User(0, 0),
            User(0, 0),
            Process,
            User(2, 0),
            Reply,
            Refresh,
            Process,
            Other(2, 1),
            User(2, 0),
        ],
        true,
    );
}

// ---------- 主循环推闸不白重算 ----------

/// 会话层每轮主循环都对忙着的助手推一次闸（`Store::step_assistant_edit`）。闸没变就不标助手主页脏
/// （热路径：标了就每帧重算、重发一遍主页）；闸变了（发出、落定）才标。
#[test]
fn idle_step_does_not_mark_assistant_page_dirty() {
    use iwork_client_core::store::{Store, ViewKey};
    let mut store = Store::new();
    store.summary.assistants.push(pb::Assistant {
        id: ID.into(),
        version: 1,
        config: Some(pb::AssistantConfig {
            name: "阿森".into(),
            ..Default::default()
        }),
        ..Default::default()
    });
    store.dirty = Default::default();
    let sent = store.step_assistant_edit(ID, |e, b| e.edit(ID, Field::Prompt("新".into()), b));
    assert!(sent.is_some(), "对照：改动发出去了");
    assert!(
        store.dirty.views.contains(&ViewKey::Assistant),
        "闸变了：标脏"
    );

    store.dirty = Default::default();
    assert!(
        store
            .step_assistant_edit(ID, |e, b| e.observed(ID, b))
            .is_none()
    );
    assert!(
        !store.dirty.views.contains(&ViewKey::Assistant),
        "闸忙着、摘要没变：不标脏"
    );

    // 后台受理、摘要追上：落定，闸变了
    let sent = sent.unwrap();
    store.step_assistant_edit(ID, |e, b| e.accepted(ID, b));
    let a = store
        .summary
        .assistants
        .iter_mut()
        .find(|a| a.id == ID)
        .unwrap();
    a.version = 2;
    a.config = Some(sent.config.clone());
    store.dirty = Default::default();
    store.step_assistant_edit(ID, |e, b| e.observed(ID, b));
    assert!(
        store.dirty.views.contains(&ViewKey::Assistant),
        "落定：标脏"
    );
}

/// 审查 g3 1/F-3（复现原样）：空名字在途时改成合法的 B；空名字被拒、B 保存成功后，页面不挂旧错误。
#[test]
fn superseded_invalid_name_leaves_no_error() {
    use Op::*;
    pinned(vec![BadName, User(0, 1), Process, Reply], false);
    let mut gate = AssistantEdits::default();
    let old = pb::AssistantConfig {
        name: "A".into(),
        ..Default::default()
    };
    let base = Base {
        version: 1,
        config: &old,
    };
    assert!(gate.edit(ID, Field::Name("".into()), Some(base)).is_some());
    assert!(gate.edit(ID, Field::Name("B".into()), Some(base)).is_none());
    let next = gate
        .rejected(ID, false, "名字不能为空", Some(base))
        .unwrap();
    assert_eq!(next.config.name, "B");
    assert!(gate.accepted(ID, Some(base)).is_none());
    let saved = Base {
        version: 2,
        config: &next.config,
    };
    assert!(gate.observed(ID, Some(saved)).is_none());
    assert_eq!(gate.busy().count(), 0);
    assert_eq!(gate.error(ID), None, "B 已保存：不挂空名字的错误");
}

/// 审查 g3 附注：提示词落定后别的写入方改了它；用户随后只改模型（摘要还没追上，发出去版本冲突）。重放用后台最新的
/// 配置叠模型这一项，别人的提示词留着，不被盖回用户旧的值。
#[test]
fn landed_field_changed_by_other_is_not_reverted() {
    use Op::*;
    let mut w = World::new();
    let mut s = Gate(AssistantEdits::default());
    for op in [
        User(1, 0),
        Process,
        Reply,
        Refresh,
        Other(1, 1),
        User(2, 1),
        Process,
        Reply,
        Refresh,
        Process,
        Reply,
        Refresh,
    ] {
        step(&mut w, &mut s, &op).unwrap();
    }
    assert_eq!(w.conflicts, 1, "对照：中间确实冲突过一次");
    assert!(w.unreplied.is_empty() && w.to_backend.is_empty());
    assert_eq!(
        read(&w.config, FieldKey::Prompt),
        field(1, 1),
        "别人的提示词留着"
    );
    assert_eq!(
        read(&w.config, FieldKey::Model),
        field(2, 1),
        "用户的模型落地了"
    );
}
