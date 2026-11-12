//! 当前打开的对话的时间线（T09 方案 2.2「时间线：按事件改，行有登记表」）。
//!
//! 不变量：**时间线里的每一行，从进入时间线起，流上属于它的事件一条没漏。** 游标跨过一段事件（快照）时
//! 整个时间线按快照页重建；连接没断时的查询结果按身份合并。顺序只有一个真值：创建这一行的事件的 seq——
//! 实时取信封 seq，查询结果取 `*_seq` 字段；字段为 0 当协议错误，整条时间线标「数据不完整」。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use iwork_proto::pb;

/// 行在时间线上的位置：创建事件的 seq + 同一 seq 内的槽位（重建得出的说明行用 0，排在实体行前）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowKey {
    pub seq: u64,
    pub slot: u8,
}

impl RowKey {
    pub const fn entity(seq: u64) -> Self {
        Self { seq, slot: 1 }
    }
    pub const fn derived(seq: u64) -> Self {
        Self { seq, slot: 0 }
    }
    /// 给网页的稳定行号：由位置算出，行在重建前后不变，网页拿它排序、当高度缓存的键。
    pub fn ordinal(self) -> i64 {
        // seq 远小于 2^62（单机事件日志），乘 2 不会溢出；溢出就是数据坏了，饱和到上限不影响排序的单调性。
        i64::try_from(
            self.seq
                .saturating_mul(2)
                .saturating_add(u64::from(self.slot)),
        )
        .unwrap_or(i64::MAX)
    }
}

/// 当次说明的种类（档位切换、刚切的模型、刚切的项目）。重建时丢弃（方案 D5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    ApprovalMode,
    Model,
    DmProject,
}

/// 行登记表（方案 2.2 的表，一种行一个变体）。加一种行：这里一个变体 + `derive::row` 一臂 + 假后台场景一行。
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Message(pb::Message),
    Tool(pb::ToolCall),
    /// 装箱：审批带完整调用记录（T07），比其他行大得多。
    Approval(Box<pb::Approval>),
    UserAction(pb::UserActionCard),
    /// 回合结束行：改动摘要与印章，或失败、取消的原因。
    TurnEnd(pb::Turn),
    /// 继续 / 放弃卡片：只在待处理表里有这一项时才有这一行。
    Resume(pb::ResumeCard),
    /// 重建得出：相邻两个回合模型不同时，贴在后一个回合前。
    ModelSwitch {
        model: pb::ModelSelection,
    },
    /// 重建得出：相邻两条消息的项目不同时，贴在后一条前。`None` 是纯私聊。
    Divider {
        project_id: Option<String>,
    },
    /// 当次说明。
    Note {
        kind: NoteKind,
        text: NoteText,
    },
}

/// 当次说明的内容；文字在 derive 里拼。
#[derive(Debug, Clone, PartialEq)]
pub enum NoteText {
    ApprovalMode(Option<i32>),
    Model(pb::ModelSelection),
    DmProject(Option<String>),
}

impl Row {
    /// 行身份（方案 2.2 表的「身份」一列）。
    pub fn identity(&self, key: RowKey) -> String {
        match self {
            Row::Message(m) => m.id.clone(),
            Row::Tool(t) => tool_identity(&t.turn_id, &t.tool_call_id),
            Row::Approval(a) => a.id.clone(),
            Row::UserAction(c) => c.id.clone(),
            Row::TurnEnd(t) => format!("end:{}", t.id),
            Row::Resume(r) => format!("resume:{}", r.turn_id),
            Row::ModelSwitch { .. } | Row::Divider { .. } | Row::Note { .. } => {
                // 重建得出与当次说明的身份由插入方给（见 `Timeline::insert_derived`、`note`），这里不会被调用。
                format!("row:{}:{}", key.seq, key.slot)
            }
        }
    }

    fn is_derived(&self) -> bool {
        matches!(self, Row::ModelSwitch { .. } | Row::Divider { .. })
    }
}

pub fn tool_identity(turn_id: &str, tool_call_id: &str) -> String {
    format!("{turn_id}/{tool_call_id}")
}

/// 上限：往上翻超过它时从新的一端丢（方案 2.2 查询结果第 4 条）。
pub const TIMELINE_CAP: usize = 5000;

/// 时间线数据不完整的原因：某个实体的排序字段是 0。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incomplete {
    pub entity: &'static str,
    pub id: String,
}

/// 一次修改让哪些行变了，供帧引擎记脏。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct TimelineChange {
    /// 整个时间线换了（重建、关闭）。
    pub reset: bool,
    pub rows: BTreeSet<String>,
    /// 只追加了正文的行：行身份 → 追加前的长度。同一帧里这一行若也整行变了，只发整行。
    pub appended: BTreeMap<String, usize>,
    pub header: bool,
    /// 锚点窗口、当前位置、滚动指令变了（T09 方案 2.9）。
    pub anchors: bool,
    pub cursor: bool,
    pub scroll: bool,
}

impl TimelineChange {
    pub fn is_empty(&self) -> bool {
        !self.reset
            && self.rows.is_empty()
            && self.appended.is_empty()
            && !self.header
            && !self.anchors
            && !self.cursor
            && !self.scroll
    }
    pub fn merge(&mut self, other: TimelineChange) {
        self.reset |= other.reset;
        self.rows.extend(other.rows);
        for (k, v) in other.appended {
            self.appended.entry(k).or_insert(v);
        }
        self.header |= other.header;
        self.anchors |= other.anchors;
        self.cursor |= other.cursor;
        self.scroll |= other.scroll;
    }
    /// 锚点状态的一次变化记成脏。
    pub fn from_anchor(e: super::anchors::AnchorEffect) -> Self {
        Self {
            anchors: e.anchors_dirty,
            cursor: e.cursor_dirty,
            scroll: e.scroll_dirty,
            ..Self::default()
        }
    }
    fn row(id: impl Into<String>) -> Self {
        let mut c = Self::default();
        c.rows.insert(id.into());
        c
    }
}

#[derive(Debug, Clone)]
pub struct Timeline {
    pub conversation_id: String,
    pub conversation: Option<pb::Conversation>,
    rows: BTreeMap<RowKey, Row>,
    by_id: HashMap<String, RowKey>,
    /// 键 → 行身份（by_id 的反向；截掉一端时按键找身份，不扫全表）。
    ids: HashMap<RowKey, String>,
    /// 行数上限（默认 [`TIMELINE_CAP`]）。
    cap: usize,
    /// 流式正文与工具输出，按行身份（消息 id、`turn/tool`）。长度就是下一个期望的 offset。
    text: HashMap<String, String>,
    /// 工具调用 id → 行身份（`ToolOutputDelta` 只带 tool_call_id）。
    tool_by_call: HashMap<String, String>,
    pub turns: BTreeMap<String, pb::Turn>,
    pub attachments: BTreeMap<String, pb::Attachment>,
    pub usage_total: Option<pb::Usage>,
    pub has_older: bool,
    pub at_tail: bool,
    pub new_below: u32,
    pub incomplete: Option<Incomplete>,
    /// 有缺口在等检查点（最新页查询）时为真；结果回来就清。
    pub gap_pending: bool,
    /// 消息锚点（T09 方案 2.9）：随时间线一起，换对话就丢；时间线重建时保留（居中加载要靠它落位）。
    pub anchors: super::anchors::Anchors,
}

/// 查询结果进时间线的方式（方案 2.2「查询结果怎么进时间线」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageMode {
    /// 订阅快照里的最新页：游标跨过了一段事件，整个重建。
    Snapshot,
    /// 连接没断时收到的最新页：at_tail 时按身份合并，否则重建。
    LiveLatest,
    /// 带 before_message_id 的翻页结果：往前接。
    Older,
    /// 带 after_message_id 的翻页结果：往后接；接到最新（has_newer 为假）就回到 at_tail（T09 方案 G11）。
    Newer,
    /// 带 around_seq 的居中一段：和已加载的行不保证接得上，整个重建成这一段（跳到没加载的锚点）。
    /// `center` 是那个 around_seq：超过上限时保留它附近连续的一段。
    Around { center: u64 },
}

impl Timeline {
    pub fn new(conversation_id: impl Into<String>) -> Self {
        Self::with_cap(conversation_id, TIMELINE_CAP)
    }

    pub fn with_cap(conversation_id: impl Into<String>, cap: usize) -> Self {
        assert!(cap > 0, "时间线上限必须大于 0");
        Self {
            conversation_id: conversation_id.into(),
            conversation: None,
            rows: BTreeMap::new(),
            by_id: HashMap::new(),
            ids: HashMap::new(),
            cap,
            text: HashMap::new(),
            tool_by_call: HashMap::new(),
            turns: BTreeMap::new(),
            attachments: BTreeMap::new(),
            usage_total: None,
            has_older: false,
            at_tail: true,
            new_below: 0,
            incomplete: None,
            gap_pending: false,
            anchors: super::anchors::Anchors::default(),
        }
    }

    pub fn rows(&self) -> impl DoubleEndedIterator<Item = (&RowKey, &Row)> {
        self.rows.iter()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 旁表的条目数：(回合, 附件, 工具调用索引)。截断时它们要跟着变小（内存上界）。
    pub fn side_table_sizes(&self) -> (usize, usize, usize) {
        (
            self.turns.len(),
            self.attachments.len(),
            self.tool_by_call.len(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn key_of(&self, identity: &str) -> Option<RowKey> {
        self.by_id.get(identity).copied()
    }

    pub fn row(&self, identity: &str) -> Option<&Row> {
        self.by_id.get(identity).and_then(|k| self.rows.get(k))
    }

    pub fn text(&self, identity: &str) -> Option<&str> {
        self.text.get(identity).map(String::as_str)
    }

    /// 最旧一条消息的 id（往上翻页的游标）。
    pub fn oldest_message_id(&self) -> Option<&str> {
        self.rows.values().find_map(|r| match r {
            Row::Message(m) => Some(m.id.as_str()),
            _ => None,
        })
    }

    /// 正在跑的回合（排队、运行、等审批、取消中）。
    pub fn running_turn(&self) -> Option<&pb::Turn> {
        self.turns.values().rev().find(|t| turn_is_active(t.state))
    }

    // ---------- 写入（只由 Store::apply 调） ----------

    fn insert(&mut self, key: RowKey, identity: String, row: Row) -> TimelineChange {
        if let Some(old) = self.by_id.get(&identity).copied()
            && old != key
        {
            self.rows.remove(&old);
            self.ids.remove(&old);
        }
        if let Some(prev) = self.ids.insert(key, identity.clone())
            && prev != identity
        {
            // 同一个位置上换了一行（不该发生：位置由创建事件的 seq 唯一确定）：旧行的身份一起去掉。
            self.by_id.remove(&prev);
        }
        self.by_id.insert(identity.clone(), key);
        self.rows.insert(key, row);
        TimelineChange::row(identity)
    }

    fn remove(&mut self, identity: &str) -> Option<RowKey> {
        let key = self.by_id.remove(identity)?;
        self.rows.remove(&key);
        self.ids.remove(&key);
        Some(key)
    }

    /// 按键去掉一行（截掉一端用），连同它的正文。`from_old_end` 表示从老的一端截。
    fn remove_key(&mut self, key: RowKey, from_old_end: bool) -> Option<String> {
        let row = self.rows.remove(&key)?;
        let id = self.ids.remove(&key)?;
        self.by_id.remove(&id);
        self.text.remove(&id);
        // 随行一起截掉的旁表（审查 2 附注）：截掉一端时它们不能留着无限长。
        match &row {
            Row::Tool(t) => {
                self.tool_by_call.remove(&t.tool_call_id);
            }
            Row::Message(m) => {
                for a in &m.attachment_ids {
                    self.attachments.remove(a);
                }
            }
            // 回合条目只在从老的一端截时删：从新的一端截（往上翻超过上限）时，回合结束行先被截掉，可这个回合
            // 更早的消息与工具行还在，它们的表情、前面的模型切换行都要读这个回合（审查 3/F-1）。
            Row::TurnEnd(t) if from_old_end && !turn_is_active(t.state) => {
                self.turns.remove(&t.id);
            }
            _ => {}
        }
        Some(id)
    }

    /// 改一行（按身份找）。找不到返回 `None`：调用方丢掉事件并记 debug 日志（方案 2.2）。
    fn update(&mut self, identity: &str, f: impl FnOnce(&mut Row)) -> Option<TimelineChange> {
        let key = self.by_id.get(identity)?;
        let row = self.rows.get_mut(key)?;
        f(row);
        Some(TimelineChange::row(identity))
    }

    /// 新行要进时间线：`at_tail == false` 时不进，只计数（方案 2.2 第 4 条）。
    fn admit_new(&mut self, identity: &str, is_message: bool) -> bool {
        if self.at_tail || self.by_id.contains_key(identity) {
            return true;
        }
        if is_message {
            self.new_below = self.new_below.saturating_add(1);
        }
        false
    }

    // ---------- 持久事件 ----------

    /// 应用一条持久事件。返回 `None` 表示事件不属于这条时间线（或目标行不在），调用方丢掉。
    pub fn apply_event(
        &mut self,
        seq: u64,
        at: Option<&prost_types::Timestamp>,
        event: &pb::domain_event::Event,
    ) -> Option<TimelineChange> {
        use pb::domain_event::Event as E;
        let cid = self.conversation_id.clone();
        let change = match event {
            E::MessageAppended(e) => {
                let mut m = e.message.clone()?;
                if m.conversation_id != cid {
                    return None;
                }
                m.created_seq = seq;
                let id = m.id.clone();
                if !self.admit_new(&id, true) {
                    return Some(TimelineChange::default().with_header());
                }
                if let Some(text) = body_text(m.body.as_ref()) {
                    self.text.insert(id.clone(), text.to_owned());
                }
                let project = m.context_project_id.clone();
                let mut c = self.insert(RowKey::entity(seq), id.clone(), Row::Message(m));
                c.merge(self.derive_after_message(seq, &id, project));
                c
            }
            E::MessageCompleted(e) => {
                if e.conversation_id != cid {
                    return None;
                }
                let text = body_text(e.body.as_ref()).map(str::to_owned);
                let mut c = self.update(&e.message_id, |r| {
                    if let Row::Message(m) = r {
                        m.body.clone_from(&e.body);
                        m.status = e.status;
                    }
                })?;
                if let Some(t) = text {
                    self.text.insert(e.message_id.clone(), t);
                }
                // 回合结束行读同回合消息的 status（derive::row::has_failed_message）：status 变了它也得重发。
                if let Some(Row::Message(m)) = self.row(&e.message_id)
                    && let Some(turn) = m.turn_id.as_deref()
                {
                    let end = format!("end:{turn}");
                    if self.by_id.contains_key(&end) {
                        c.rows.insert(end);
                    }
                }
                c
            }
            E::ConversationModelChanged(e) if e.conversation_id == cid => self.note(
                seq,
                NoteKind::Model,
                NoteText::Model(e.model.clone().unwrap_or_default()),
            ),
            E::ApprovalModeChanged(e) => {
                let target = e.target.as_ref()?.kind.as_ref()?;
                let on_this = match target {
                    pb::approval_mode_target::Kind::ConversationId(id) => *id == cid,
                    pb::approval_mode_target::Kind::Member(m) => m.conversation_id == cid,
                    pb::approval_mode_target::Kind::ProjectId(_) => false,
                };
                if !on_this {
                    return None;
                }
                if let (Some(conv), pb::approval_mode_target::Kind::ConversationId(_)) =
                    (self.conversation.as_mut(), target)
                {
                    conv.approval_mode = e.mode;
                }
                let mut c = self.note(seq, NoteKind::ApprovalMode, NoteText::ApprovalMode(e.mode));
                c.header = true;
                c
            }
            E::DmProjectSwitched(e) if e.conversation_id == cid => {
                if let Some(pb::Conversation {
                    kind: Some(pb::conversation::Kind::AssistantDm(dm)),
                    ..
                }) = self.conversation.as_mut()
                {
                    dm.context_project_id.clone_from(&e.project_id);
                }
                let mut c = self.note(
                    seq,
                    NoteKind::DmProject,
                    NoteText::DmProject(e.project_id.clone()),
                );
                c.header = true;
                c
            }
            E::AttachmentStored(e) => {
                let a = e.attachment.clone()?;
                if a.conversation_id != cid {
                    return None;
                }
                self.attachments.insert(a.id.clone(), a);
                self.rows_mentioning_attachment()
            }
            E::AttachmentConfirmed(e) if e.conversation_id == cid => {
                let a = self.attachments.get_mut(&e.attachment_id)?;
                a.needs_confirmation = false;
                self.rows_mentioning_attachment()
            }
            E::TurnQueued(e) => {
                let mut t = e.turn.clone()?;
                if t.conversation_id != cid {
                    return None;
                }
                t.created_seq = seq;
                // 时间线不接着最新时，新回合的行不进来，贴在它前面的模型切换行也不进来。
                let mut c = if self.at_tail {
                    self.derive_after_turn(&t)
                } else {
                    TimelineChange::default()
                };
                self.turns.insert(t.id.clone(), t);
                c.header = true;
                c
            }
            E::TurnStateChanged(e) => {
                let t = self.turns.get_mut(&e.turn_id)?;
                t.state = e.to;
                t.state_reason.clone_from(&e.reason);
                t.interrupt_reason = e.interrupt_reason;
                self.messages_of_turn(&e.turn_id).with_header()
            }
            E::TurnModelReported(e) => {
                let t = self.turns.get_mut(&e.turn_id)?;
                t.model.clone_from(&e.model);
                let mut c = self.rebuild_derived();
                c.header = true;
                c
            }
            E::TurnFinished(e) => {
                let t = self.turns.get_mut(&e.turn_id)?;
                t.state = e.state;
                t.usage.clone_from(&e.usage);
                t.celebrate = e.celebrate;
                t.change_summary.clone_from(&e.change_summary);
                t.ended_seq = seq;
                t.ended_at = at.cloned();
                let turn = t.clone();
                if let Some(u) = &e.usage {
                    add_usage(self.usage_total.get_or_insert_with(Default::default), u);
                }
                let id = format!("end:{}", turn.id);
                let mut c = self.messages_of_turn(&turn.id);
                if self.admit_new(&id, false) {
                    c.merge(self.insert(RowKey::entity(seq), id, Row::TurnEnd(turn)));
                }
                c.header = true;
                c
            }
            E::ToolCallStarted(e) => {
                if !self.turns.contains_key(&e.turn_id) {
                    return None;
                }
                let id = tool_identity(&e.turn_id, &e.tool_call_id);
                if !self.admit_new(&id, false) {
                    return Some(TimelineChange::default());
                }
                self.tool_by_call.insert(e.tool_call_id.clone(), id.clone());
                let call = pb::ToolCall {
                    turn_id: e.turn_id.clone(),
                    tool_call_id: e.tool_call_id.clone(),
                    tool_name: e.tool_name.clone(),
                    action: e.action.clone(),
                    finished: false,
                    ok: false,
                    output_artifact: None,
                    created_seq: seq,
                };
                self.insert(RowKey::entity(seq), id, Row::Tool(call))
            }
            E::ToolCallFinished(e) => {
                let id = tool_identity(&e.turn_id, &e.tool_call_id);
                self.update(&id, |r| {
                    if let Row::Tool(t) = r {
                        t.finished = true;
                        t.ok = e.ok;
                        t.output_artifact.clone_from(&e.output_artifact);
                    }
                })?
            }
            E::ApprovalRequested(e) => {
                let mut a = e.approval.clone()?;
                if a.conversation_id != cid {
                    return None;
                }
                a.created_seq = seq;
                let id = a.id.clone();
                if !self.admit_new(&id, false) {
                    return Some(TimelineChange::default());
                }
                self.insert(RowKey::entity(seq), id, Row::Approval(Box::new(a)))
            }
            E::ApprovalReviewUpdated(e) => self.update(&e.approval_id, |r| {
                if let Row::Approval(a) = r {
                    a.review = e.review;
                    a.review_id = Some(e.review_id.clone());
                    a.review_reason.clone_from(&e.reason);
                }
            })?,
            E::ApprovalDecided(e) => self.update(&e.approval_id, |r| {
                if let Row::Approval(a) = r {
                    a.state = e.state;
                    // 终态契约（方案 G3）：只有 APPROVED 带批准者。
                    a.decider =
                        (e.state == pb::ApprovalState::Approved as i32).then_some(e.decider);
                    a.void_reason = None;
                    a.decided_at = at.cloned();
                }
            })?,
            E::ApprovalVoided(e) => self.update(&e.approval_id, |r| {
                if let Row::Approval(a) = r {
                    a.state = pb::ApprovalState::Voided as i32;
                    a.decider = None;
                    a.void_reason = Some(e.reason);
                    a.decided_at = at.cloned();
                }
            })?,
            E::UserActionProposed(e) => {
                let mut card = e.card.clone()?;
                if card.conversation_id != cid {
                    return None;
                }
                card.created_seq = seq;
                let id = card.id.clone();
                if !self.admit_new(&id, false) {
                    return Some(TimelineChange::default());
                }
                self.insert(RowKey::entity(seq), id, Row::UserAction(card))
            }
            E::UserActionStateChanged(e) => self.update(&e.card_id, |r| {
                if let Row::UserAction(c) = r {
                    c.state = e.state;
                    if e.exit.is_some() {
                        c.exit.clone_from(&e.exit);
                    }
                    c.error.clone_from(&e.error);
                    if e.output_artifact.is_some() {
                        c.output_artifact.clone_from(&e.output_artifact);
                    }
                    c.void_reason = e.void_reason;
                }
            })?,
            E::ConversationRead(e) if e.conversation_id == cid => {
                if let Some(conv) = self.conversation.as_mut() {
                    conv.last_read_seq = e.last_read_seq;
                }
                return None;
            }
            E::ConversationDeleting(e) if e.conversation_id == cid => {
                if let Some(conv) = self.conversation.as_mut() {
                    conv.state = pb::ConversationState::Deleting as i32;
                }
                // 亲手执行卡片的说明读「对话是否在删除中」（「有一张卡片的执行结果无法核对，请确认后继续删除」）。
                let mut c = TimelineChange::default().with_header();
                c.rows
                    .extend(self.identities_where(|r| matches!(r, Row::UserAction(_))));
                c
            }
            _ => return None,
        };
        let mut change = change;
        change.merge(self.trim_oldest());
        Some(change)
    }

    /// 当次说明：上一个锚点之后已有同类说明时，重建得出的那一行不再贴（`rebuild_derived` 里判断）。
    fn note(&mut self, seq: u64, kind: NoteKind, text: NoteText) -> TimelineChange {
        let id = format!("note:{seq}");
        if !self.admit_new(&id, false) {
            return TimelineChange::default();
        }
        // 当次说明总是先于它说明的那个锚点到达，锚点进来时再判断要不要贴重建得出的行。
        self.insert(RowKey::entity(seq), id, Row::Note { kind, text })
    }

    /// 实时的新消息：只和前一条消息比（事件按 seq 递增到达，新消息总在最后），不全量重算——
    /// 这是流式热路径，全量重算会让一万条消息的对话每条都扫一遍。
    fn derive_after_message(
        &mut self,
        seq: u64,
        id: &str,
        project: Option<String>,
    ) -> TimelineChange {
        let prev = self
            .rows
            .range(..RowKey::derived(seq))
            .rev()
            .find_map(|(k, r)| match r {
                Row::Message(p) => Some((k.seq, p.context_project_id.clone())),
                _ => None,
            });
        match prev {
            Some((ps, pp))
                if pp != project && !self.has_note_between(ps, seq, NoteKind::DmProject) =>
            {
                self.insert(
                    RowKey::derived(seq),
                    format!("divider:{id}"),
                    Row::Divider {
                        project_id: project,
                    },
                )
            }
            _ => TimelineChange::default(),
        }
    }

    /// 实时的新回合：只和前一个回合比。
    fn derive_after_turn(&mut self, t: &pb::Turn) -> TimelineChange {
        let prev = self
            .turns
            .values()
            .filter(|p| p.created_seq != 0 && p.created_seq < t.created_seq)
            .max_by_key(|p| p.created_seq);
        match prev {
            Some(p)
                if model_id(p) != model_id(t)
                    && !self.has_note_between(p.created_seq, t.created_seq, NoteKind::Model) =>
            {
                let model = t.model.clone().unwrap_or_default();
                self.insert(
                    RowKey::derived(t.created_seq),
                    format!("model:{}", t.id),
                    Row::ModelSwitch { model },
                )
            }
            _ => TimelineChange::default(),
        }
    }

    fn rows_mentioning_attachment(&self) -> TimelineChange {
        let mut c = TimelineChange::default();
        for row in self.rows.values() {
            if let Row::Message(m) = row
                && m.attachment_ids
                    .iter()
                    .any(|a| self.attachments.contains_key(a))
            {
                c.rows.insert(m.id.clone());
            }
        }
        c
    }

    // ---------- 流式增量 ----------

    /// `MessageDelta` / `ToolOutputDelta`（方案 2.2）：offset 等于已有长度追加；小于丢掉重叠部分；
    /// 大于（缺了一段）停止追加，返回 `Gap` 让会话层发一次检查点查询。
    pub fn apply_delta(&mut self, delta: Delta<'_>) -> DeltaOutcome {
        let identity = match delta.target {
            DeltaTarget::Message(id) => {
                if !self.by_id.contains_key(id) {
                    return DeltaOutcome::Ignored;
                }
                id.to_owned()
            }
            DeltaTarget::ToolOutput(call) => match self.tool_by_call.get(call) {
                Some(id) => id.clone(),
                None => return DeltaOutcome::Ignored,
            },
        };
        let text = self.text.entry(identity.clone()).or_default();
        let have = text.len() as u64;
        let end = delta.offset.saturating_add(delta.text.len() as u64);
        if delta.offset > have {
            self.gap_pending = true;
            return DeltaOutcome::Gap;
        }
        if end <= have {
            return DeltaOutcome::Ignored;
        }
        let skip = usize::try_from(have - delta.offset).unwrap_or(usize::MAX);
        // offset 是 UTF-8 字节；重叠恰好落在字符中间说明对端坏了，按缺口处理拿检查点。
        let Some(tail) = delta.text.get(skip..) else {
            self.gap_pending = true;
            return DeltaOutcome::Gap;
        };
        let before = text.len();
        text.push_str(tail);
        let mut c = TimelineChange::default();
        c.appended.insert(identity, before);
        DeltaOutcome::Appended(c)
    }

    // ---------- 查询结果 ----------

    /// 对话查询结果进时间线。结果里有排序字段为 0 的实体时记下 `incomplete`（协议错误），界面整页说明。
    pub fn apply_page(
        &mut self,
        page: &pb::GetConversationResult,
        mode: PageMode,
    ) -> TimelineChange {
        self.gap_pending = false;
        if let Some(conv) = &page.conversation {
            self.conversation = Some(conv.clone());
        }
        let rebuild = match mode {
            PageMode::Snapshot | PageMode::Around { .. } => true,
            PageMode::LiveLatest => !self.at_tail,
            PageMode::Older | PageMode::Newer => false,
        };
        if rebuild {
            let id = self.conversation_id.clone();
            let conv = self.conversation.take();
            let anchors = std::mem::take(&mut self.anchors);
            *self = Timeline::with_cap(id, self.cap);
            self.conversation = conv;
            self.anchors = anchors;
        }
        if mode != PageMode::Older {
            self.usage_total = page.usage_total;
        }
        if let Err(bad) = self.merge_page(page) {
            tracing::warn!(target: "iwork_client", entity = bad.entity, id = %bad.id, "查询结果的排序字段为 0，时间线不显示");
            self.incomplete = Some(bad);
        }
        match mode {
            PageMode::Older => self.has_older = page.has_more,
            PageMode::Newer => {
                // 接到最新：这个结果在全序里有确定位置，之后的新消息事件都排在它后面，不变量成立（方案 2.2 第 3 条）。
                if !page.has_newer {
                    self.at_tail = true;
                    self.new_below = 0;
                }
            }
            PageMode::Around { .. } => {
                self.has_older = page.has_more;
                self.at_tail = !page.has_newer;
                self.new_below = 0;
            }
            PageMode::Snapshot | PageMode::LiveLatest => {
                if rebuild {
                    self.has_older = page.has_more;
                }
                self.at_tail = true;
                self.new_below = 0;
            }
        }
        let mut c = self.rebuild_derived();
        c.reset = rebuild;
        c.header = true;
        // 合并模式下逐行标脏太碎：查询页本来就是一批行，按整份重发处理。
        if !rebuild {
            c.rows.extend(self.by_id.keys().cloned());
        }
        // 上限：从离可见窗口远的那一端丢（方案 2.2 第 4 条）——往上翻丢新的一端，往下翻丢旧的一端。
        match mode {
            PageMode::Older => self.enforce_cap(),
            PageMode::Newer => c.merge(self.drop_oldest_over_cap()),
            // 居中那一段不接着最新，trim_oldest 管不到它（审查 4/F-8）：两端都可能超，按离目标远近裁
            PageMode::Around { center } => self.trim_around(center),
            _ => c.merge(self.trim_oldest()),
        }
        c
    }

    fn merge_page(&mut self, page: &pb::GetConversationResult) -> Result<(), Incomplete> {
        fn need(entity: &'static str, id: &str, seq: u64) -> Result<u64, Incomplete> {
            if seq == 0 {
                Err(Incomplete {
                    entity,
                    id: id.to_owned(),
                })
            } else {
                Ok(seq)
            }
        }
        for a in &page.attachments {
            self.attachments.insert(a.id.clone(), a.clone());
        }
        for m in &page.messages {
            let seq = need("Message", &m.id, m.created_seq)?;
            if let Some(t) = body_text(m.body.as_ref()) {
                let cur = self.text.entry(m.id.clone()).or_default();
                // 流式中的检查点正文可能比已收到的增量短：只取更长的那份。
                if t.len() >= cur.len() {
                    *cur = t.to_owned();
                }
            }
            self.insert(RowKey::entity(seq), m.id.clone(), Row::Message(m.clone()));
        }
        for t in &page.turns {
            need("Turn", &t.id, t.created_seq)?;
            self.turns.insert(t.id.clone(), t.clone());
            if t.ended_seq != 0
                && !turn_is_active(t.state)
                && t.state != pb::TurnState::Interrupted as i32
            {
                self.insert(
                    RowKey::entity(t.ended_seq),
                    format!("end:{}", t.id),
                    Row::TurnEnd(t.clone()),
                );
            }
        }
        for t in &page.tool_calls {
            let seq = need("ToolCall", &t.tool_call_id, t.created_seq)?;
            let id = tool_identity(&t.turn_id, &t.tool_call_id);
            self.tool_by_call.insert(t.tool_call_id.clone(), id.clone());
            self.insert(RowKey::entity(seq), id, Row::Tool(t.clone()));
        }
        for a in &page.approvals {
            let seq = need("Approval", &a.id, a.created_seq)?;
            self.insert(
                RowKey::entity(seq),
                a.id.clone(),
                Row::Approval(Box::new(a.clone())),
            );
        }
        for c in &page.user_actions {
            let seq = need("UserActionCard", &c.id, c.created_seq)?;
            self.insert(
                RowKey::entity(seq),
                c.id.clone(),
                Row::UserAction(c.clone()),
            );
        }
        Ok(())
    }

    /// 待处理表换了：同步这个对话的继续 / 放弃卡片行。
    pub fn sync_resume_cards<'a>(
        &mut self,
        cards: impl Iterator<Item = &'a pb::ResumeCard>,
    ) -> TimelineChange {
        let mut want: BTreeMap<String, &pb::ResumeCard> = BTreeMap::new();
        for c in cards.filter(|c| c.conversation_id == self.conversation_id) {
            want.insert(format!("resume:{}", c.turn_id), c);
        }
        let mut change = TimelineChange::default();
        let have: Vec<String> = self
            .by_id
            .keys()
            .filter(|k| k.starts_with("resume:"))
            .cloned()
            .collect();
        for id in have {
            if !want.contains_key(&id) {
                self.remove(&id);
                change.rows.insert(id);
            }
        }
        for (id, card) in want {
            if card.created_seq == 0 {
                self.incomplete.get_or_insert(Incomplete {
                    entity: "ResumeCard",
                    id: card.turn_id.clone(),
                });
                continue;
            }
            if self.row(&id) != Some(&Row::Resume(card.clone())) {
                change.merge(self.insert(
                    RowKey::entity(card.created_seq),
                    id,
                    Row::Resume(card.clone()),
                ));
            }
        }
        change
    }

    /// 重算重建得出的两种行（模型切换、项目分隔线）。
    fn rebuild_derived(&mut self) -> TimelineChange {
        let mut change = TimelineChange::default();
        let old: Vec<String> = self
            .by_id
            .iter()
            .filter(|(_, k)| self.rows.get(k).is_some_and(Row::is_derived))
            .map(|(id, _)| id.clone())
            .collect();
        let mut want: BTreeMap<String, (RowKey, Row)> = BTreeMap::new();

        // 模型切换：按回合的 created_seq 排序，相邻不同模型贴在后一个前；两者之间已有模型当次说明就不贴。
        let mut turns: Vec<&pb::Turn> =
            self.turns.values().filter(|t| t.created_seq != 0).collect();
        turns.sort_by_key(|t| t.created_seq);
        for pair in turns.windows(2) {
            let (prev, next) = (pair[0], pair[1]);
            if model_id(prev) == model_id(next) {
                continue;
            }
            if self.has_note_between(prev.created_seq, next.created_seq, NoteKind::Model) {
                continue;
            }
            want.insert(
                format!("model:{}", next.id),
                (
                    RowKey::derived(next.created_seq),
                    Row::ModelSwitch {
                        model: next.model.clone().unwrap_or_default(),
                    },
                ),
            );
        }

        // 项目分隔线：相邻两条消息的 context_project_id 不同。
        let messages: Vec<(u64, &pb::Message)> = self
            .rows
            .iter()
            .filter_map(|(k, r)| match r {
                Row::Message(m) => Some((k.seq, m)),
                _ => None,
            })
            .collect();
        for pair in messages.windows(2) {
            let ((ps, prev), (ns, next)) = (pair[0], pair[1]);
            if prev.context_project_id == next.context_project_id {
                continue;
            }
            if self.has_note_between(ps, ns, NoteKind::DmProject) {
                continue;
            }
            want.insert(
                format!("divider:{}", next.id),
                (
                    RowKey::derived(ns),
                    Row::Divider {
                        project_id: next.context_project_id.clone(),
                    },
                ),
            );
        }

        for id in old {
            if !want.contains_key(&id) {
                self.remove(&id);
                change.rows.insert(id);
            }
        }
        for (id, (key, row)) in want {
            if self.key_of(&id) != Some(key) || self.row(&id) != Some(&row) {
                change.merge(self.insert(key, id, row));
            }
        }
        change
    }

    fn has_note_between(&self, from: u64, to: u64, kind: NoteKind) -> bool {
        self.rows
            .range(RowKey::entity(from)..RowKey::derived(to))
            .any(|(_, r)| matches!(r, Row::Note { kind: k, .. } if *k == kind))
    }

    /// 往上翻超过上限时从新的一端丢，之后新消息只计数（方案 2.2 查询结果第 4 条）。
    fn enforce_cap(&mut self) {
        while self.rows.len() > self.cap {
            let Some(&key) = self.rows.keys().next_back() else {
                break;
            };
            self.remove_key(key, false);
            self.at_tail = false;
        }
    }

    /// 居中一段超过上限：每次丢离 `center` 远的那一端，留下目标附近连续的一段。丢了旧的一端标成还能往上翻，
    /// 丢了新的一端标成不接着最新（往下翻取回）。整段是重建出来的，网页随 reset 整份重画，不用逐行通知删除。
    fn trim_around(&mut self, center: u64) {
        while self.rows.len() > self.cap {
            let (Some(&old), Some(&new)) = (self.rows.keys().next(), self.rows.keys().next_back())
            else {
                break;
            };
            if center.saturating_sub(old.seq) >= new.seq.saturating_sub(center) {
                self.remove_key(old, true);
                self.has_older = true;
            } else {
                self.remove_key(new, false);
                self.at_tail = false;
            }
        }
    }

    /// 停在最新时超过上限：从老的一端丢，标成还能往上翻取回（supervisor 2026-11-11 定：长时间挂着的对话
    /// 内存要有上界）。返回被丢的行，帧引擎据此让网页删掉。
    fn trim_oldest(&mut self) -> TimelineChange {
        if !self.at_tail {
            return TimelineChange::default();
        }
        self.drop_oldest_over_cap()
    }

    /// 超过上限的部分从老的一端丢，标成还能往上翻取回。
    fn drop_oldest_over_cap(&mut self) -> TimelineChange {
        let mut c = TimelineChange::default();
        while self.rows.len() > self.cap {
            let Some(&key) = self.rows.keys().next() else {
                break;
            };
            if let Some(id) = self.remove_key(key, true) {
                c.rows.insert(id);
            }
            self.has_older = true;
        }
        c
    }

    /// 满足条件的行的身份。行的视图模型读了行外的状态（回合状态、连接、摘要表、设置）时，
    /// 那份状态一变，持有它的一方用这个把受影响的行标脏（`derive::row` 里每一处行外读取都要有对应的标脏）。
    pub fn identities_where(&self, pred: impl Fn(&Row) -> bool) -> Vec<String> {
        self.by_id
            .iter()
            .filter(|(_, k)| self.rows.get(k).is_some_and(&pred))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 这个回合的消息行：它们的表情读回合状态（`derive::row::message`）。
    fn messages_of_turn(&self, turn_id: &str) -> TimelineChange {
        let mut c = TimelineChange::default();
        c.rows.extend(self.identities_where(
            |r| matches!(r, Row::Message(m) if m.turn_id.as_deref() == Some(turn_id)),
        ));
        c
    }

    /// 锚点对应时间线里的哪一行：用户消息是那条消息；等你处理是那张卡片（审批、亲手执行）或继续卡片；
    /// 失败的回合优先落在失败的那条助手消息上（说明行在那里，2a 第 10 条），没有就落在回合结束行。
    /// 行不在时间线里返回 `None`。
    pub fn anchor_row(&self, a: &pb::Anchor) -> Option<String> {
        use pb::anchor::Kind as K;
        let candidates: Vec<String> = match a.kind.as_ref()? {
            K::UserMessage(m) => vec![m.message_id.clone()],
            K::Waiting(w) => vec![
                w.pending_item_id.clone(),
                format!("resume:{}", w.pending_item_id),
            ],
            K::FailedTurn(f) => {
                let mut v = self.identities_where(|r| {
                    matches!(r, Row::Message(m) if m.turn_id.as_deref() == Some(f.turn_id.as_str())
                        && m.status == pb::MessageStatus::Failed as i32)
                });
                v.push(format!("end:{}", f.turn_id));
                v
            }
        };
        candidates
            .into_iter()
            .find(|id| self.by_id.contains_key(id))
    }

    /// 最新一条消息的 id（往后翻页的游标）。
    pub fn newest_message_id(&self) -> Option<&str> {
        self.rows.values().rev().find_map(|r| match r {
            Row::Message(m) => Some(m.id.as_str()),
            _ => None,
        })
    }

    /// 行身份 → 键的全部映射，按位置排序（帧引擎算窗口用）。
    pub fn identities(&self) -> Vec<(RowKey, String)> {
        let mut v: Vec<(RowKey, String)> =
            self.by_id.iter().map(|(id, k)| (*k, id.clone())).collect();
        v.sort();
        v
    }
}

impl TimelineChange {
    fn with_header(mut self) -> Self {
        self.header = true;
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DeltaTarget<'a> {
    Message(&'a str),
    ToolOutput(&'a str),
}

#[derive(Debug, Clone, Copy)]
pub struct Delta<'a> {
    pub target: DeltaTarget<'a>,
    pub offset: u64,
    pub text: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeltaOutcome {
    Appended(TimelineChange),
    Ignored,
    /// 中间缺了一段：要一次检查点（最新页查询）。
    Gap,
}

pub fn turn_is_active(state: i32) -> bool {
    use pb::TurnState as S;
    matches!(
        S::try_from(state),
        Ok(S::Queued | S::Running | S::WaitingApproval | S::Cancelling)
    )
}

fn model_id(t: &pb::Turn) -> &str {
    t.model.as_ref().map_or("", |m| m.model_id.as_str())
}

fn body_text(body: Option<&pb::MessageBody>) -> Option<&str> {
    match body?.content.as_ref()? {
        pb::message_body::Content::InlineText(t) => Some(t),
        pb::message_body::Content::Artifact(_) => None,
    }
}

pub fn add_usage(total: &mut pb::Usage, u: &pb::Usage) {
    total.input_tokens += u.input_tokens;
    total.output_tokens += u.output_tokens;
    total.cache_read_tokens += u.cache_read_tokens;
    total.cache_write_tokens += u.cache_write_tokens;
    total.cost_usd = match (total.cost_usd, u.cost_usd) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    };
}
