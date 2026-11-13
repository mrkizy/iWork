//! 从事件日志折叠出的内存表，以及用它回答 11 种视图查询与两种旁路查询。
//!
//! 数据形状按 T09 方案第 3 节补完字段后的协议造：`*_seq` 都填真值（创建事件的信封 seq），G1 标题、
//! G3 审批终态、G4 最近回合状态、G7 助手最近动态、G8 分页筛选都由这里算。规则照技术设计 9.0a、9.1 与
//! 方案第 3 节写；T10 的真后台汇合时两份录像对拍终态（方案第 11 步）。
//!
//! 只有 [`Tables::fold`] 改表（加上测试场景经 `FakeDaemon::mutate_tables` 的直接修改）；查询只读。

mod anchors;
mod paging;

use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use iwork_proto::pb;
use iwork_proto::pb::domain_event::Event as E;
use iwork_proto::pb::query::Kind as Q;
use iwork_proto::pb::query_result::Kind as R;

/// 会话快照与「不带 limit」的翻页取多少条消息；与 `iwork_app::subscription` 的快照页大小相同。
pub const DEFAULT_PAGE: u32 = iwork_app::subscription::CONVERSATION_SNAPSHOT_PAGE_SIZE;

/// 自动标题取首条消息的前多少个字符（技术设计 9.0a）。
pub const AUTO_TITLE_CHARS: usize = 20;

/// 一个对话：实体本身加上折叠时顺带记下的东西。
#[derive(Debug, Clone, Default)]
pub struct ConversationRow {
    /// `title`、`title_source` 存在这里；`unread`、`last_turn_state`、`last_message_first_line`
    /// 与 FIXED 标题在查询时算，存的值不用。
    pub conversation: pb::Conversation,
    /// 按 created_seq 递增。
    pub messages: Vec<pb::Message>,
    /// 最近一次「对话里发生了事」的 seq，「最近活动」排序用。已读不算活动。
    pub last_activity_seq: u64,
}

/// 折叠出的全部表。字段公开只为测试场景直接改（见 `FakeDaemon::mutate_tables`）。
#[derive(Debug, Clone, Default)]
pub struct Tables {
    pub projects: BTreeMap<String, pb::Project>,
    pub assistants: BTreeMap<String, pb::Assistant>,
    pub conversations: BTreeMap<String, ConversationRow>,
    /// 消息 id → 所在对话。
    pub message_conversation: HashMap<String, String>,
    pub turns: BTreeMap<String, pb::Turn>,
    /// 回合 id → 进入 INTERRUPTED 的事件 seq（ResumeCard.created_seq）。
    pub interrupted_at: HashMap<String, u64>,
    /// 键 `turn_id/tool_call_id`。
    pub tool_calls: BTreeMap<String, pb::ToolCall>,
    pub approvals: BTreeMap<String, pb::Approval>,
    pub user_actions: BTreeMap<String, pb::UserActionCard>,
    /// 草稿卡片与它被提出时的 seq。
    pub drafts: BTreeMap<String, (pb::AgentsMdDraftCard, u64)>,
    pub attachments: BTreeMap<String, pb::Attachment>,
    pub rules: BTreeMap<String, pb::Rule>,
    pub runtimes: BTreeMap<String, pb::RuntimeStatus>,
    pub quota: BTreeMap<String, pb::RuntimeQuota>,
    pub settings: pb::GlobalSettings,
    pub local_config: BTreeMap<String, Vec<pb::LocalConfigItem>>,
    /// 助手 id → 它最近发的一条消息（对话 id、消息 id）。G7 的 MESSAGE 用。
    pub last_message_by_assistant: HashMap<String, (String, String)>,
    /// 制品内容（sha256 → 字节），ReadArtifact 用。场景经 `mutate_tables` 放进来。
    pub artifacts: BTreeMap<String, Bytes>,
    /// 用户最近点过的那条系统通知（GetOpenTarget）；测试经 `FakeDaemon::set_open_target` 设。token 0 表示没有。
    pub open_target: pb::GetOpenTargetResult,
    /// 假的「盘」：InspectProjectPath、AddProject、InitRepository 按它判（假后台不碰真盘；T09 方案 15.2）。
    /// 键是选择框给的路径；场景经 `Step::Mutate` 登记，克隆完、新建项目时自己登记。
    pub disk: BTreeMap<String, FakeDir>,
    /// 克隆时后台拒的地址 → 拒绝的文字（错误码照真后台的地址白名单：INVALID_ARGUMENT）。假后台不另写一份
    /// 白名单规则（规则只在 iwork-vcs-git 一处），由场景登记哪个地址被拒。
    pub rejected_clone_urls: BTreeMap<String, String>,
    /// 初始化会失败的目录 → 原因（错误码照真后台没配提交身份时：VCS_IDENTITY_MISSING）。
    pub init_failures: BTreeMap<String, String>,
}

/// 假的「盘」上的一个目录。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FakeDir {
    /// 会被加进来的目录：选的是仓库的子目录时是仓库根。
    pub root_path: String,
    pub versioned: bool,
    pub has_agents_md: bool,
}

impl FakeDir {
    /// 受版本控制的仓库根（克隆完、新建的项目就是这样）。
    pub fn repo(root: &str) -> Self {
        FakeDir {
            root_path: root.to_owned(),
            versioned: true,
            has_agents_md: false,
        }
    }
}

pub fn tool_call_key(turn_id: &str, tool_call_id: &str) -> String {
    format!("{turn_id}/{tool_call_id}")
}

impl Tables {
    /// 对话 id 的唯一来源：带对话 id 的事件直接取；回合、工具调用、审批、卡片事件经表查（G5 落地前事件载荷里没有）。
    pub fn conversation_of(&self, event: &E) -> Option<String> {
        let by_turn = |t: &str| self.turns.get(t).map(|t| t.conversation_id.clone());
        match event {
            E::ConversationCreated(e) => e.conversation.as_ref().map(|c| c.id.clone()),
            E::ConversationDeleting(e) => Some(e.conversation_id.clone()),
            E::ConversationDeleted(e) => Some(e.conversation_id.clone()),
            E::ConversationModelChanged(e) => Some(e.conversation_id.clone()),
            E::ApprovalModeChanged(e) => match e.target.as_ref().and_then(|t| t.kind.as_ref()) {
                Some(pb::approval_mode_target::Kind::ConversationId(id)) => Some(id.clone()),
                Some(pb::approval_mode_target::Kind::Member(m)) => Some(m.conversation_id.clone()),
                Some(pb::approval_mode_target::Kind::ProjectId(_)) | None => None,
            },
            E::DmProjectSwitched(e) => Some(e.conversation_id.clone()),
            E::ConversationRead(e) => Some(e.conversation_id.clone()),
            E::MessageAppended(e) => e.message.as_ref().map(|m| m.conversation_id.clone()),
            E::MessageCompleted(e) => Some(e.conversation_id.clone()),
            E::TurnQueued(e) => e.turn.as_ref().map(|t| t.conversation_id.clone()),
            E::TurnStateChanged(e) => by_turn(&e.turn_id),
            E::TurnFinished(e) => by_turn(&e.turn_id),
            E::TurnModelReported(e) => by_turn(&e.turn_id),
            E::TurnSessionBound(e) => by_turn(&e.turn_id),
            E::ApprovalDelivered(e) => by_turn(&e.turn_id),
            E::TurnIntentRecorded(e) => by_turn(&e.turn_id),
            E::ToolCallStarted(e) => by_turn(&e.turn_id),
            E::ToolCallFinished(e) => by_turn(&e.turn_id),
            E::ApprovalRequested(e) => e.approval.as_ref().map(|a| a.conversation_id.clone()),
            E::ApprovalReviewUpdated(e) => self.approval_conversation(&e.approval_id),
            E::ApprovalDecided(e) => self.approval_conversation(&e.approval_id),
            E::ApprovalVoided(e) => self.approval_conversation(&e.approval_id),
            E::UserActionProposed(e) => e.card.as_ref().map(|c| c.conversation_id.clone()),
            E::UserActionStateChanged(e) => self
                .user_actions
                .get(&e.card_id)
                .map(|c| c.conversation_id.clone()),
            E::AttachmentStored(e) => e.attachment.as_ref().map(|a| a.conversation_id.clone()),
            E::AttachmentConfirmed(e) => Some(e.conversation_id.clone()),
            E::ProjectAdded(_)
            | E::ProjectUpdated(_)
            | E::ProjectRelocated(_)
            | E::ProjectPathStatusChanged(_)
            | E::ProjectRemoved(_)
            | E::ProjectSettingsUpdated(_)
            | E::RepositoryInitialized(_)
            | E::CloneFinished(_)
            | E::AgentsMdDraftProposed(_)
            | E::AgentsMdDraftDecided(_)
            | E::AssistantCreated(_)
            | E::AssistantUpdated(_)
            | E::AssistantDeleted(_)
            | E::AssistantReset(_)
            | E::ApprovalEvaluated(_)
            | E::RuntimeSessionForgetFailed(_)
            | E::RuleRemembered(_)
            | E::RuleRevoked(_)
            | E::RulesDeleted(_)
            | E::ApprovalStaleResultDropped(_)
            | E::SecurityListEntryAdded(_)
            | E::SecurityListEntryRemoved(_)
            | E::RuntimeUpdated(_)
            | E::QuotaObserved(_)
            | E::UsageReported(_)
            | E::LocalImportPrompted(_)
            | E::LocalConfigImported(_)
            | E::McpSecretSet(_)
            | E::GlobalSettingsChanged(_)
            | E::RecoveryCompleted(_) => None,
        }
    }

    fn approval_conversation(&self, id: &str) -> Option<String> {
        self.approvals.get(id).map(|a| a.conversation_id.clone())
    }

    /// 应用一条持久事件。`seq` 与 `at` 取自信封。
    pub fn fold(&mut self, seq: u64, at: &prost_types::Timestamp, event: &E) {
        if let Some(conv) = self.conversation_of(event)
            && !matches!(event, E::ConversationRead(_))
            && let Some(row) = self.conversations.get_mut(&conv)
        {
            row.last_activity_seq = seq;
            row.conversation.updated_at = Some(*at);
        }
        match event {
            E::ProjectAdded(e) => {
                if let Some(p) = &e.project {
                    self.projects.insert(p.id.clone(), p.clone());
                }
            }
            E::ProjectUpdated(e) => {
                if let Some(p) = self.projects.get_mut(&e.project_id) {
                    if let Some(n) = &e.name {
                        p.name = n.clone();
                    }
                    if let Some(pin) = e.pinned {
                        p.pinned = pin;
                    }
                    if e.last_active_at.is_some() {
                        p.last_active_at = e.last_active_at;
                    }
                }
            }
            E::ProjectRelocated(e) => {
                if let Some(p) = self.projects.get_mut(&e.project_id) {
                    p.root_path = e.new_path.clone();
                    p.path_status = pb::PathStatus::Ok.into();
                }
            }
            E::ProjectPathStatusChanged(e) => {
                if let Some(p) = self.projects.get_mut(&e.project_id) {
                    p.path_status = e.path_status;
                }
            }
            E::ProjectRemoved(e) => {
                self.projects.remove(&e.project_id);
            }
            E::ProjectSettingsUpdated(e) => {
                if let Some(p) = self.projects.get_mut(&e.project_id) {
                    p.settings = e.settings.clone();
                }
            }
            E::RepositoryInitialized(e) => {
                if let Some(d) = self.disk.get_mut(&e.path) {
                    d.versioned = true;
                }
            }
            E::CloneFinished(e) => {
                if let Some(path) = e.path.as_ref().filter(|_| e.ok) {
                    self.disk.insert(path.clone(), FakeDir::repo(path));
                }
            }
            E::AgentsMdDraftProposed(e) => {
                let card = pb::AgentsMdDraftCard {
                    card_id: e.card_id.clone(),
                    project_id: e.project_id.clone(),
                    content: e.content_artifact.clone(),
                    proposed_at: Some(*at),
                };
                self.drafts.insert(e.card_id.clone(), (card, seq));
            }
            E::AgentsMdDraftDecided(e) => {
                self.drafts.remove(&e.card_id);
            }
            E::AssistantCreated(e) => {
                if let Some(a) = &e.assistant {
                    self.assistants.insert(a.id.clone(), a.clone());
                }
            }
            E::AssistantUpdated(e) => {
                if let Some(a) = self.assistants.get_mut(&e.assistant_id) {
                    a.version = e.version;
                    a.config = e.config.clone();
                    a.updated_at = Some(*at);
                }
            }
            E::AssistantReset(e) => {
                if let Some(a) = self.assistants.get_mut(&e.assistant_id) {
                    a.version = e.version;
                    a.config = e.config.clone();
                    a.updated_at = Some(*at);
                }
            }
            E::AssistantDeleted(e) => {
                self.assistants.remove(&e.assistant_id);
            }
            E::ConversationCreated(e) => {
                if let Some(c) = &e.conversation {
                    let mut c = c.clone();
                    c.title_source = match c.kind {
                        Some(pb::conversation::Kind::AssistantDm(_)) => pb::TitleSource::Fixed,
                        _ => pb::TitleSource::Auto,
                    }
                    .into();
                    c.updated_at = Some(*at);
                    self.conversations.insert(
                        c.id.clone(),
                        ConversationRow {
                            conversation: c,
                            messages: Vec::new(),
                            last_activity_seq: seq,
                        },
                    );
                }
            }
            E::ConversationDeleting(e) => {
                if let Some(row) = self.conversations.get_mut(&e.conversation_id) {
                    row.conversation.state = pb::ConversationState::Deleting.into();
                }
            }
            E::ConversationDeleted(e) => self.remove_conversation(&e.conversation_id),
            E::ConversationModelChanged(e) => {
                if let Some(row) = self.conversations.get_mut(&e.conversation_id) {
                    row.conversation.model_override = e.model.clone();
                }
            }
            E::ApprovalModeChanged(e) => {
                if let Some(pb::approval_mode_target::Kind::ConversationId(id)) =
                    e.target.as_ref().and_then(|t| t.kind.as_ref())
                    && let Some(row) = self.conversations.get_mut(id)
                {
                    row.conversation.approval_mode = e.mode;
                }
            }
            E::DmProjectSwitched(e) => {
                if let Some(row) = self.conversations.get_mut(&e.conversation_id)
                    && let Some(pb::conversation::Kind::AssistantDm(dm)) =
                        &mut row.conversation.kind
                {
                    dm.context_project_id = e.project_id.clone();
                }
            }
            E::ConversationRead(e) => {
                if let Some(row) = self.conversations.get_mut(&e.conversation_id) {
                    row.conversation.last_read_seq = e.last_read_seq;
                }
            }
            E::MessageAppended(e) => {
                if let Some(m) = &e.message {
                    self.append_message(seq, at, m);
                }
            }
            E::MessageCompleted(e) => {
                if let Some(m) = self.message_mut(&e.conversation_id, &e.message_id) {
                    m.body = e.body.clone();
                    m.status = e.status;
                }
            }
            E::TurnQueued(e) => {
                if let Some(t) = &e.turn {
                    let mut t = t.clone();
                    t.created_seq = seq;
                    t.ended_seq = 0;
                    self.turns.insert(t.id.clone(), t);
                }
            }
            E::TurnStateChanged(e) => {
                if e.to == i32::from(pb::TurnState::Interrupted) {
                    self.interrupted_at.insert(e.turn_id.clone(), seq);
                }
                if let Some(t) = self.turns.get_mut(&e.turn_id) {
                    t.state = e.to;
                    t.state_reason = e.reason.clone();
                    t.interrupt_reason = e.interrupt_reason;
                }
            }
            E::TurnFinished(e) => {
                if let Some(t) = self.turns.get_mut(&e.turn_id) {
                    t.state = e.state;
                    t.ended_seq = seq;
                    t.ended_at = Some(*at);
                    t.usage = e.usage;
                    t.celebrate = e.celebrate;
                    t.change_summary = e.change_summary.clone();
                    t.interrupt_reason = None;
                }
            }
            E::TurnModelReported(e) => {
                if let Some(t) = self.turns.get_mut(&e.turn_id) {
                    t.model = e.model.clone();
                }
            }
            E::ToolCallStarted(e) => {
                self.tool_calls.insert(
                    tool_call_key(&e.turn_id, &e.tool_call_id),
                    pb::ToolCall {
                        turn_id: e.turn_id.clone(),
                        tool_call_id: e.tool_call_id.clone(),
                        tool_name: e.tool_name.clone(),
                        action: e.action.clone(),
                        finished: false,
                        ok: false,
                        output_artifact: None,
                        created_seq: seq,
                    },
                );
            }
            E::ToolCallFinished(e) => {
                if let Some(c) = self
                    .tool_calls
                    .get_mut(&tool_call_key(&e.turn_id, &e.tool_call_id))
                {
                    c.finished = true;
                    c.ok = e.ok;
                    c.output_artifact = e.output_artifact.clone();
                }
            }
            E::ApprovalRequested(e) => {
                if let Some(a) = &e.approval {
                    let mut a = a.clone();
                    a.created_seq = seq;
                    a.decider = None;
                    a.void_reason = None;
                    a.decided_at = None;
                    self.approvals.insert(a.id.clone(), a);
                }
            }
            E::ApprovalReviewUpdated(e) => {
                if let Some(a) = self.approvals.get_mut(&e.approval_id) {
                    a.review = e.review;
                    a.review_id = Some(e.review_id.clone());
                    a.review_reason = e.reason.clone();
                }
            }
            // 终态契约（方案第 3 节 G3）：APPROVED 带 decider；DENIED 两者都没有；VOIDED 带 void_reason；都带 decided_at。
            E::ApprovalDecided(e) => {
                if let Some(a) = self.approvals.get_mut(&e.approval_id) {
                    a.state = e.state;
                    a.decided_at = Some(*at);
                    a.void_reason = None;
                    a.decider =
                        (e.state == i32::from(pb::ApprovalState::Approved)).then_some(e.decider);
                }
            }
            E::ApprovalVoided(e) => {
                if let Some(a) = self.approvals.get_mut(&e.approval_id) {
                    a.state = pb::ApprovalState::Voided.into();
                    a.decided_at = Some(*at);
                    a.decider = None;
                    a.void_reason = Some(e.reason);
                }
            }
            E::ApprovalEvaluated(_) | E::ApprovalStaleResultDropped(_) => {}
            // 交付记录只给编排用（T07），不改任何视图。
            E::ApprovalDelivered(_) => {}
            // 回合意图：真后台用它推派发与续接，界面不画它（T10a 第 12 节给 T09 的交接）
            E::TurnIntentRecorded(_) => {}
            E::TurnSessionBound(e) => {
                if let Some(t) = self.turns.get_mut(&e.turn_id) {
                    t.runtime_session_id = Some(e.runtime_session_id.clone());
                }
            }
            E::RuleRemembered(e) => {
                if let Some(r) = &e.rule {
                    self.rules.insert(r.id.clone(), r.clone());
                }
            }
            E::RuleRevoked(e) => {
                self.rules.remove(&e.rule_id);
            }
            E::RulesDeleted(e) => {
                for id in &e.rule_ids {
                    self.rules.remove(id);
                }
            }
            E::SecurityListEntryAdded(e) => {
                if let Some(r) = &e.rule {
                    self.rules.insert(r.id.clone(), r.clone());
                }
            }
            E::SecurityListEntryRemoved(e) => {
                self.rules.remove(&e.entry_id);
            }
            E::UserActionProposed(e) => {
                if let Some(c) = &e.card {
                    let mut c = c.clone();
                    c.created_seq = seq;
                    self.user_actions.insert(c.id.clone(), c);
                }
            }
            E::UserActionStateChanged(e) => {
                if let Some(c) = self.user_actions.get_mut(&e.card_id) {
                    c.state = e.state;
                    if e.exit.is_some() {
                        c.exit = e.exit;
                    }
                    if e.error.is_some() {
                        c.error = e.error.clone();
                    }
                    if e.output_artifact.is_some() {
                        c.output_artifact = e.output_artifact.clone();
                    }
                    c.void_reason = e.void_reason;
                }
            }
            E::AttachmentStored(e) => {
                if let Some(a) = &e.attachment {
                    let mut a = a.clone();
                    a.created_at = Some(*at);
                    self.attachments.insert(a.id.clone(), a);
                }
            }
            E::AttachmentConfirmed(e) => {
                if let Some(a) = self.attachments.get_mut(&e.attachment_id) {
                    a.needs_confirmation = false;
                }
            }
            E::RuntimeUpdated(e) => {
                if let Some(r) = &e.runtime {
                    self.runtimes.insert(r.runtime_id.clone(), r.clone());
                }
            }
            E::QuotaObserved(e) => {
                self.quota.insert(
                    e.runtime_id.clone(),
                    pb::RuntimeQuota {
                        runtime_id: e.runtime_id.clone(),
                        windows: e.windows.clone(),
                        observed_at: e.observed_at,
                    },
                );
            }
            E::UsageReported(_)
            | E::RuntimeSessionForgetFailed(_)
            | E::LocalImportPrompted(_)
            | E::McpSecretSet(_) => {}
            E::LocalConfigImported(e) => {
                let items = self.local_config.entry(e.runtime_id.clone()).or_default();
                for imported in &e.items {
                    match items.iter_mut().find(|i| i.id == imported.id) {
                        Some(i) => i.imported = true,
                        None => items.push(pb::LocalConfigItem {
                            imported: true,
                            ..imported.clone()
                        }),
                    }
                }
            }
            E::GlobalSettingsChanged(e) => {
                if let Some(v) = e.playful_ui {
                    self.settings.playful_ui = v;
                }
                if let Some(v) = e.autostart {
                    self.settings.autostart = v;
                }
                if let Some(v) = &e.projects_root {
                    self.settings.projects_root = v.clone();
                }
            }
            E::RecoveryCompleted(_) => {}
        }
    }

    fn append_message(&mut self, seq: u64, at: &prost_types::Timestamp, m: &pb::Message) {
        let Some(row) = self.conversations.get_mut(&m.conversation_id) else {
            return;
        };
        let mut m = m.clone();
        m.created_seq = seq;
        m.created_at = Some(*at);
        // G1：项目对话的自动标题在首条用户消息时定，之后不再变（技术设计 9.0a）。
        let is_user = matches!(
            m.author.as_ref().and_then(|a| a.kind.as_ref()),
            Some(pb::author::Kind::User(_))
        );
        let c = &mut row.conversation;
        if is_user && c.title_source == i32::from(pb::TitleSource::Auto) && c.title.is_empty() {
            let first_attachment = m
                .attachment_ids
                .first()
                .and_then(|id| self.attachments.get(id))
                .map(|a| a.name.clone());
            c.title = auto_title(&body_text(m.body.as_ref()), first_attachment.as_deref());
        }
        if let Some(pb::author::Kind::Assistant(a)) =
            m.author.as_ref().and_then(|a| a.kind.as_ref())
        {
            self.last_message_by_assistant.insert(
                a.assistant_id.clone(),
                (m.conversation_id.clone(), m.id.clone()),
            );
        }
        self.message_conversation
            .insert(m.id.clone(), m.conversation_id.clone());
        row.messages.push(m);
    }

    fn message_mut(&mut self, conversation_id: &str, message_id: &str) -> Option<&mut pb::Message> {
        let row = self.conversations.get_mut(conversation_id)?;
        // 流式完成的多半是最后几条：从后往前找。
        row.messages.iter_mut().rev().find(|m| m.id == message_id)
    }

    pub fn message(&self, message_id: &str) -> Option<&pb::Message> {
        let conv = self.message_conversation.get(message_id)?;
        let row = self.conversations.get(conv)?;
        row.messages.iter().rev().find(|m| m.id == message_id)
    }

    fn remove_conversation(&mut self, id: &str) {
        let Some(row) = self.conversations.remove(id) else {
            return;
        };
        for m in &row.messages {
            self.message_conversation.remove(&m.id);
        }
        let turns: Vec<String> = self
            .turns
            .values()
            .filter(|t| t.conversation_id == id)
            .map(|t| t.id.clone())
            .collect();
        for t in &turns {
            self.turns.remove(t);
            self.interrupted_at.remove(t);
        }
        self.tool_calls.retain(|_, c| !turns.contains(&c.turn_id));
        self.approvals.retain(|_, a| a.conversation_id != id);
        self.user_actions.retain(|_, c| c.conversation_id != id);
        self.attachments.retain(|_, a| a.conversation_id != id);
        self.last_message_by_assistant.retain(|_, (c, _)| c != id);
    }

    // ---------- 查询 ----------

    /// 回答一个查询。`head` 是这次读看到的最高 seq，填进 `QueryResult.seq`。
    pub fn query(&self, head: u64, q: &pb::Query) -> Result<pb::QueryResult, pb::Error> {
        let kind = q
            .kind
            .as_ref()
            .ok_or_else(|| invalid("query", "查询为空"))?;
        let result = match kind {
            Q::GetConversation(g) => R::GetConversation(self.get_conversation(g)?),
            Q::ListAnchors(a) => R::ListAnchors(self.list_anchors(a)?),
            Q::ListConversations(l) => R::ListConversations(self.list_conversations(l)?),
            Q::ListProjects(_) => R::ListProjects(pb::ListProjectsResult {
                projects: self
                    .projects
                    .values()
                    .map(|p| self.project_view(p))
                    .collect(),
            }),
            Q::ListAssistants(_) => R::ListAssistants(pb::ListAssistantsResult {
                assistants: self.assistants.values().cloned().collect(),
                activities: self.activities(),
            }),
            Q::ListPendingItems(_) => R::ListPendingItems(pb::ListPendingItemsResult {
                items: self
                    .pending_items(None)
                    .into_iter()
                    .map(|(_, i)| i)
                    .collect(),
            }),
            Q::GetQuota(_) => R::GetQuota(pb::GetQuotaResult {
                runtimes: self.quota.values().cloned().collect(),
            }),
            Q::GetRuntimeStatus(_) => R::GetRuntimeStatus(pb::GetRuntimeStatusResult {
                runtimes: self.runtimes.values().cloned().collect(),
            }),
            Q::GetOpenTarget(_) => R::GetOpenTarget(self.open_target.clone()),
            Q::InspectProjectPath(i) => R::InspectProjectPath(self.inspect_project_path(&i.path)?),
            Q::ListModels(_) => R::ListModels(pb::ListModelsResult {
                models: vec![
                    pb::ModelOption {
                        model_id: "fake-large".into(),
                        display_name: "Fake Large".into(),
                    },
                    pb::ModelOption {
                        model_id: "fake-small".into(),
                        display_name: "Fake Small".into(),
                    },
                ],
                effort_levels: vec!["low".into(), "medium".into(), "high".into()],
            }),
            Q::ListRules(l) => R::ListRules(pb::ListRulesResult {
                rules: self
                    .rules
                    .values()
                    .filter(|r| !is_security_list(r))
                    .filter(|r| match &l.project_id {
                        None => true,
                        Some(p) => matches!(
                            r.scope.as_ref().and_then(|s| s.kind.as_ref()),
                            Some(pb::scope::Kind::ProjectId(id)) if id == p
                        ),
                    })
                    .cloned()
                    .collect(),
            }),
            Q::ListLocalConfig(l) => R::ListLocalConfig(pb::ListLocalConfigResult {
                items: self
                    .local_config
                    .get(&l.runtime_id)
                    .cloned()
                    .unwrap_or_default(),
            }),
            Q::ListSecurityLists(_) => R::ListSecurityLists(pb::ListSecurityListsResult {
                rules: self
                    .rules
                    .values()
                    .filter(|r| is_security_list(r))
                    .cloned()
                    .collect(),
            }),
            Q::GetSettings(g) => R::GetSettings(pb::GetSettingsResult {
                global: Some(self.settings.clone()),
                project: g
                    .project_id
                    .as_ref()
                    .and_then(|p| self.projects.get(p))
                    .and_then(|p| p.settings.clone()),
            }),
            Q::ReadArtifact(r) => R::ReadArtifact(self.read_artifact(r)?),
        };
        Ok(pb::QueryResult {
            seq: head,
            kind: Some(result),
        })
    }

    fn read_artifact(&self, r: &pb::ReadArtifact) -> Result<pb::ArtifactChunk, pb::Error> {
        let sha = r.artifact.as_ref().map(|a| a.sha256.as_str()).unwrap_or("");
        let data = self.artifacts.get(sha).ok_or_else(|| pb::Error {
            code: pb::ErrorCode::ArtifactNotFound.into(),
            message: format!("制品不存在：{sha}"),
            retryable: false,
            details: Default::default(),
        })?;
        let total = data.len() as u64;
        if r.offset > total {
            return Err(pb::Error {
                code: pb::ErrorCode::ArtifactRangeInvalid.into(),
                message: format!("offset {} 超过总长 {total}", r.offset),
                retryable: false,
                details: Default::default(),
            });
        }
        let limit = u64::from(r.limit.clamp(1, 1024 * 1024));
        let end = (r.offset + limit).min(total);
        Ok(pb::ArtifactChunk {
            data: data.slice(r.offset as usize..end as usize),
            offset: r.offset,
            total_size: total,
            eof: end == total,
        })
    }

    fn project_view(&self, p: &pb::Project) -> pb::Project {
        let unread = self
            .conversations
            .values()
            .filter(|row| project_of(&row.conversation) == Some(p.id.as_str()))
            .map(|row| self.unread(row))
            .sum();
        pb::Project {
            unread,
            ..p.clone()
        }
    }

    /// 对话的查询视图：算出未读、FIXED 标题、最近回合状态、最近一条消息首行（G1、G4、G8）。
    pub fn conversation_view(&self, row: &ConversationRow) -> pb::Conversation {
        let c = &row.conversation;
        let title = match (pb::TitleSource::try_from(c.title_source), &c.kind) {
            (Ok(pb::TitleSource::Fixed), Some(pb::conversation::Kind::AssistantDm(dm))) => self
                .assistants
                .get(&dm.assistant_id)
                .and_then(|a| a.config.as_ref())
                .map(|cfg| cfg.name.clone())
                .unwrap_or_default(),
            _ => c.title.clone(),
        };
        let last_turn_state = self
            .turns
            .values()
            .filter(|t| t.conversation_id == c.id)
            .max_by_key(|t| t.created_seq)
            .map(|t| t.state);
        pb::Conversation {
            title,
            unread: self.unread(row),
            last_turn_state,
            last_message_first_line: row
                .messages
                .last()
                .map(|m| first_line(&body_text(m.body.as_ref())))
                .unwrap_or_default(),
            ..c.clone()
        }
    }

    /// 未读（技术设计 9.1）：只算「发给用户的消息」（助手消息）与「待处理卡片」，在 last_read_seq 之后的。
    fn unread(&self, row: &ConversationRow) -> u32 {
        let after = row.conversation.last_read_seq;
        let start = row.messages.partition_point(|m| m.created_seq <= after);
        let messages = row.messages[start..]
            .iter()
            .filter(|m| {
                matches!(
                    m.author.as_ref().and_then(|a| a.kind.as_ref()),
                    Some(pb::author::Kind::Assistant(_))
                )
            })
            .count();
        let cards = self
            .pending_items(Some(&row.conversation.id))
            .into_iter()
            .filter(|(seq, _)| *seq > after)
            .count();
        u32::try_from(messages + cards).unwrap_or(u32::MAX)
    }

    /// 待处理项与它在时间线上的 seq，按 seq 递增。`conversation` 给定时只取这个对话的（草稿卡片没有对话）。
    pub fn pending_items(&self, conversation: Option<&str>) -> Vec<(u64, pb::PendingItem)> {
        use pb::pending_item::Item;
        let in_conv = |c: &str| conversation.is_none_or(|want| want == c);
        let mut items: Vec<(u64, pb::PendingItem)> = Vec::new();
        for a in self.approvals.values() {
            if a.state == i32::from(pb::ApprovalState::Pending) && in_conv(&a.conversation_id) {
                items.push((a.created_seq, item(Item::Approval(a.clone()))));
            }
        }
        for c in self.user_actions.values() {
            if user_action_open(c.state) && in_conv(&c.conversation_id) {
                items.push((c.created_seq, item(Item::UserAction(c.clone()))));
            }
        }
        for t in self.turns.values() {
            if t.state == i32::from(pb::TurnState::Interrupted) && in_conv(&t.conversation_id) {
                let seq = self
                    .interrupted_at
                    .get(&t.id)
                    .copied()
                    .unwrap_or(t.created_seq);
                items.push((
                    seq,
                    item(Item::Resume(pb::ResumeCard {
                        turn_id: t.id.clone(),
                        conversation_id: t.conversation_id.clone(),
                        reason: t.interrupt_reason.unwrap_or_default(),
                        created_seq: seq,
                    })),
                ));
            }
        }
        if conversation.is_none() {
            for (card, seq) in self.drafts.values() {
                items.push((*seq, item(Item::AgentsMdDraft(card.clone()))));
            }
        }
        items.sort_by_key(|(seq, _)| *seq);
        items
    }

    /// G7：每个助手至多一条，跨它参与的全部对话按 等你处理 > 运行中 > 最近一条消息 取。
    fn activities(&self) -> Vec<pb::AssistantActivity> {
        use pb::AssistantActivityKind as K;
        let mut out = Vec::new();
        for id in self.assistants.keys() {
            let convs: Vec<&str> = self
                .conversations
                .values()
                .filter(|row| self.participates(id, row))
                .map(|row| row.conversation.id.as_str())
                .collect();
            let waiting = convs
                .iter()
                .flat_map(|c| self.pending_items(Some(c)))
                .max_by_key(|(seq, _)| *seq);
            if let Some((_, it)) = waiting {
                let (conv, item_id) = pending_identity(&it);
                out.push(pb::AssistantActivity {
                    assistant_id: id.clone(),
                    kind: K::Waiting.into(),
                    conversation_id: conv,
                    pending_item_id: Some(item_id),
                    message_first_line: None,
                });
                continue;
            }
            let running = self
                .turns
                .values()
                .filter(|t| &t.assistant_id == id && turn_running(t.state))
                .max_by_key(|t| t.created_seq);
            if let Some(t) = running {
                out.push(pb::AssistantActivity {
                    assistant_id: id.clone(),
                    kind: K::Running.into(),
                    conversation_id: t.conversation_id.clone(),
                    pending_item_id: None,
                    message_first_line: None,
                });
                continue;
            }
            if let Some((conv, msg)) = self.last_message_by_assistant.get(id) {
                let line = self
                    .message(msg)
                    .map(|m| first_line(&body_text(m.body.as_ref())))
                    .unwrap_or_default();
                out.push(pb::AssistantActivity {
                    assistant_id: id.clone(),
                    kind: K::Message.into(),
                    conversation_id: conv.clone(),
                    pending_item_id: None,
                    message_first_line: Some(line),
                });
            }
        }
        out
    }

    fn participates(&self, assistant_id: &str, row: &ConversationRow) -> bool {
        match &row.conversation.kind {
            Some(pb::conversation::Kind::AssistantDm(dm)) => dm.assistant_id == assistant_id,
            _ => self.turns.values().any(|t| {
                t.conversation_id == row.conversation.id && t.assistant_id == assistant_id
            }),
        }
    }

    /// G8：按最近活动倒序；`before` 是上一批最后一条的 id；`limit == 0` 不分页。
    fn list_conversations(
        &self,
        l: &pb::ListConversations,
    ) -> Result<pb::ListConversationsResult, pb::Error> {
        let needle = l.title_contains.as_deref().map(str::to_lowercase);
        let mut ranked: Vec<(u64, pb::Conversation)> = self
            .conversations
            .values()
            .filter(|row| match &l.project_id {
                None => true,
                Some(p) => project_of(&row.conversation) == Some(p.as_str()),
            })
            .map(|row| (row.last_activity_seq, self.conversation_view(row)))
            .filter(|(_, c)| {
                needle
                    .as_ref()
                    .is_none_or(|n| c.title.to_lowercase().contains(n.as_str()))
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
        let mut rows: Vec<pb::Conversation> = ranked.into_iter().map(|(_, c)| c).collect();
        if let Some(before) = &l.before {
            let pos = rows
                .iter()
                .position(|c| &c.id == before)
                .ok_or_else(|| invalid("before", &format!("游标对话不在列表里：{before}")))?;
            rows.drain(..=pos);
        }
        let mut has_more = false;
        if l.limit > 0 && rows.len() > l.limit as usize {
            rows.truncate(l.limit as usize);
            has_more = true;
        }
        Ok(pb::ListConversationsResult {
            conversations: rows,
            has_more,
        })
    }

    /// 键集分页：不带 before 取最新一页；页内各种行取 seq 落在这一页范围里的（方案 2.2 行登记表）。
    fn get_conversation(
        &self,
        g: &pb::GetConversation,
    ) -> Result<pb::GetConversationResult, pb::Error> {
        let row = self.conversation_row(&g.conversation_id)?;
        if let paging::Mode::Rows(cursor) = paging::mode(g)? {
            return Ok(self.row_page(row, cursor));
        }
        let limit = if g.limit == 0 { DEFAULT_PAGE } else { g.limit } as usize;
        let (start, end) = paging::page_range(&row.messages, g, limit)?;
        let page = &row.messages[start..end];
        let has_more = start > 0;
        let has_newer = end < row.messages.len();
        // 页的 seq 范围 [lo, hi)：最旧一页从 0 起，最新一页到无穷。
        let lo = if has_more {
            page.first().map_or(0, |m| m.created_seq)
        } else {
            0
        };
        let hi = if end < row.messages.len() {
            row.messages[end].created_seq
        } else {
            u64::MAX
        };
        let in_page = |seq: u64| seq >= lo && seq < hi;
        let page_turns: std::collections::BTreeSet<&str> =
            page.iter().filter_map(|m| m.turn_id.as_deref()).collect();
        let turns: Vec<pb::Turn> = self
            .turns
            .values()
            .filter(|t| t.conversation_id == row.conversation.id)
            .filter(|t| {
                in_page(t.created_seq)
                    || (t.ended_seq != 0 && in_page(t.ended_seq))
                    || page_turns.contains(t.id.as_str())
            })
            .cloned()
            .collect();
        let conv_turns: std::collections::BTreeSet<&str> = self
            .turns
            .values()
            .filter(|t| t.conversation_id == row.conversation.id)
            .map(|t| t.id.as_str())
            .collect();
        let tool_calls = self
            .tool_calls
            .values()
            .filter(|c| conv_turns.contains(c.turn_id.as_str()) && in_page(c.created_seq))
            .cloned()
            .collect();
        let approvals = self
            .approvals
            .values()
            .filter(|a| a.conversation_id == row.conversation.id && in_page(a.created_seq))
            .cloned()
            .collect();
        let user_actions = self
            .user_actions
            .values()
            .filter(|c| c.conversation_id == row.conversation.id && in_page(c.created_seq))
            .cloned()
            .collect();
        let attachments = page
            .iter()
            .flat_map(|m| &m.attachment_ids)
            .filter_map(|id| self.attachments.get(id))
            .cloned()
            .collect();
        let usage_total = self.usage_total(&row.conversation.id);
        let mut out = pb::GetConversationResult {
            conversation: Some(self.conversation_view(row)),
            messages: page.to_vec(),
            turns,
            has_more,
            tool_calls,
            approvals,
            user_actions,
            attachments,
            usage_total,
            has_newer,
        };
        self.add_page_turns(&row.conversation.id, &mut out);
        Ok(out)
    }

    /// 一页的 `turns` 再补两类：页里工具、审批、卡片所属的回合；这个对话所有还没到终态的回合（之后它们的事件
    /// 到了，客户端要在 `turns` 里找得到）。两种取法共用，和真后台 `iwork-store` 的 `assemble` 同一条规则。
    fn add_page_turns(&self, conversation_id: &str, out: &mut pb::GetConversationResult) {
        use pb::TurnState as S;
        let mut ids: std::collections::BTreeSet<String> =
            out.turns.iter().map(|t| t.id.clone()).collect();
        ids.extend(out.tool_calls.iter().map(|c| c.turn_id.clone()));
        ids.extend(out.approvals.iter().map(|a| a.turn_id.clone()));
        ids.extend(out.user_actions.iter().map(|c| c.turn_id.clone()));
        ids.extend(
            self.turns
                .values()
                .filter(|t| t.conversation_id == conversation_id)
                .filter(|t| {
                    !matches!(
                        S::try_from(t.state),
                        Ok(S::Completed | S::Cancelled | S::Failed)
                    )
                })
                .map(|t| t.id.clone()),
        );
        out.turns = ids
            .iter()
            .filter_map(|id| self.turns.get(id))
            .cloned()
            .collect();
    }

    /// 整个对话的累计用量（标题栏「本对话用量」）。
    fn usage_total(&self, conversation_id: &str) -> Option<pb::Usage> {
        self.turns
            .values()
            .filter(|t| t.conversation_id == conversation_id)
            .filter_map(|t| t.usage.as_ref())
            .fold(None::<pb::Usage>, |acc, u| {
                let mut acc = acc.unwrap_or_default();
                acc.input_tokens += u.input_tokens;
                acc.output_tokens += u.output_tokens;
                acc.cache_read_tokens += u.cache_read_tokens;
                acc.cache_write_tokens += u.cache_write_tokens;
                if let Some(c) = u.cost_usd {
                    acc.cost_usd = Some(acc.cost_usd.unwrap_or(0.0) + c);
                }
                Some(acc)
            })
    }

    /// 行级游标的一页（方案 2.9「行级游标的语义」）：消息、工具调用、审批、亲手执行卡片按 created_seq，回合结束按
    /// ended_seq，五路合在一起排好，取紧挨游标的 limit 行。结果里放选中行对应的实体、它们所属的回合（以及结束行被
    /// 选中的回合；回合自己的 created_seq、ended_seq 可以在范围外）、选中消息的附件。
    fn row_page(
        &self,
        row: &ConversationRow,
        cursor: paging::RowCursor,
    ) -> pb::GetConversationResult {
        enum R<'a> {
            Message(&'a pb::Message),
            Tool(&'a pb::ToolCall),
            Approval(&'a pb::Approval),
            UserAction(&'a pb::UserActionCard),
            TurnEnd(&'a pb::Turn),
        }
        let cid = row.conversation.id.as_str();
        let conv_turns: std::collections::BTreeSet<&str> = self
            .turns
            .values()
            .filter(|t| t.conversation_id == cid)
            .map(|t| t.id.as_str())
            .collect();
        let mut all: Vec<(u64, R<'_>)> = Vec::new();
        all.extend(row.messages.iter().map(|m| (m.created_seq, R::Message(m))));
        all.extend(
            self.tool_calls
                .values()
                .filter(|c| conv_turns.contains(c.turn_id.as_str()))
                .map(|c| (c.created_seq, R::Tool(c))),
        );
        all.extend(
            self.approvals
                .values()
                .filter(|a| a.conversation_id == cid)
                .map(|a| (a.created_seq, R::Approval(a))),
        );
        all.extend(
            self.user_actions
                .values()
                .filter(|c| c.conversation_id == cid)
                .map(|c| (c.created_seq, R::UserAction(c))),
        );
        // 回合结束行：和客户端建这一行的条件一样（iwork-client-core store::timeline 的 merge_page：结束了、不在跑、
        // 不是中断——中断的回合显示继续卡片，不显示结束行）。两边对「哪些算一行」必须一致，否则按行数取的页对不上。
        all.extend(
            self.turns
                .values()
                .filter(|t| t.conversation_id == cid && t.ended_seq != 0)
                .filter(|t| !turn_active(t.state) && t.state != pb::TurnState::Interrupted as i32)
                .map(|t| (t.ended_seq, R::TurnEnd(t))),
        );
        all.sort_by_key(|(seq, _)| *seq);
        let seqs: Vec<u64> = all.iter().map(|(seq, _)| *seq).collect();
        let ((start, end), has_more, has_newer) = cursor.select(&seqs);

        let mut out = pb::GetConversationResult {
            conversation: Some(self.conversation_view(row)),
            has_more,
            has_newer,
            usage_total: self.usage_total(cid),
            ..Default::default()
        };
        let mut turn_ids: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for (_, r) in &all[start..end] {
            match r {
                R::Message(m) => {
                    turn_ids.extend(m.turn_id.as_deref());
                    out.attachments.extend(
                        m.attachment_ids
                            .iter()
                            .filter_map(|id| self.attachments.get(id))
                            .cloned(),
                    );
                    out.messages.push((*m).clone());
                }
                R::Tool(c) => {
                    turn_ids.insert(&c.turn_id);
                    out.tool_calls.push((*c).clone());
                }
                R::Approval(a) => {
                    turn_ids.insert(&a.turn_id);
                    out.approvals.push((*a).clone());
                }
                R::UserAction(c) => {
                    turn_ids.insert(&c.turn_id);
                    out.user_actions.push((*c).clone());
                }
                R::TurnEnd(t) => {
                    turn_ids.insert(&t.id);
                }
            }
        }
        out.turns = turn_ids
            .into_iter()
            .filter_map(|id| self.turns.get(id))
            .cloned()
            .collect();
        self.add_page_turns(cid, &mut out);
        out
    }

    fn conversation_row(&self, id: &str) -> Result<&ConversationRow, pb::Error> {
        self.conversations.get(id).ok_or_else(|| pb::Error {
            code: pb::ErrorCode::NotFound.into(),
            message: format!("对话不存在：{id}"),
            retryable: false,
            details: Default::default(),
        })
    }
}

fn item(i: pb::pending_item::Item) -> pb::PendingItem {
    pb::PendingItem { item: Some(i) }
}

/// 待处理项属于哪个对话、它的 id（审批 id、卡片 id、回合 id）。
pub fn pending_identity(it: &pb::PendingItem) -> (String, String) {
    use pb::pending_item::Item;
    match &it.item {
        Some(Item::Approval(a)) => (a.conversation_id.clone(), a.id.clone()),
        Some(Item::UserAction(c)) => (c.conversation_id.clone(), c.id.clone()),
        Some(Item::Resume(r)) => (r.conversation_id.clone(), r.turn_id.clone()),
        Some(Item::AgentsMdDraft(d)) => (String::new(), d.card_id.clone()),
        None => (String::new(), String::new()),
    }
}

/// 卡片没到终态（UNKNOWN 也不是终态，等用户确认）。
pub fn user_action_open(state: i32) -> bool {
    use pb::UserActionState as S;
    matches!(
        S::try_from(state),
        Ok(S::Pending | S::Starting | S::Executing | S::Stopping | S::Unknown)
    )
}

pub fn turn_running(state: i32) -> bool {
    use pb::TurnState as T;
    matches!(
        T::try_from(state),
        Ok(T::Queued | T::Running | T::WaitingApproval | T::Cancelling)
    )
}

fn is_security_list(r: &pb::Rule) -> bool {
    use pb::RuleLayer as L;
    matches!(
        L::try_from(r.layer),
        Ok(L::CredentialCommands | L::SensitiveDirs | L::WritableDirs)
    )
}

pub fn project_of(c: &pb::Conversation) -> Option<&str> {
    match &c.kind {
        Some(pb::conversation::Kind::Project(p)) => Some(&p.project_id),
        _ => None,
    }
}

pub fn body_text(b: Option<&pb::MessageBody>) -> String {
    match b.and_then(|b| b.content.as_ref()) {
        Some(pb::message_body::Content::InlineText(t)) => t.clone(),
        Some(pb::message_body::Content::Artifact(_)) | None => String::new(),
    }
}

pub fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// 技术设计 9.0a：换行与连续空白折叠成一个空格，取前 20 个字符，超出加「…」；正文为空取第一个附件名。
/// 设计写的是按字素簇截；假后台按 char 截，只用于造数据，差异在含组合字符的标题上，T10 的真实现按设计做。
pub fn auto_title(text: &str, first_attachment: Option<&str>) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return first_attachment.unwrap_or("").to_owned();
    }
    let mut chars = collapsed.chars();
    let head: String = chars.by_ref().take(AUTO_TITLE_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

impl Tables {
    /// 检查一个目录（照假的「盘」）：已经是项目 → KNOWN；受版本控制 → ADDABLE；否则 NOT_VERSION_CONTROLLED。
    /// 不在「盘」上 → PROJECT_PATH_MISSING（真后台路径不存在时的错误码）。
    pub fn inspect_project_path(
        &self,
        path: &str,
    ) -> Result<pb::InspectProjectPathResult, pb::Error> {
        // 键是选择框给的路径；AddProject 发的是检查回来的仓库根，也认（按 root_path 找）
        let d = self
            .disk
            .get(path)
            .or_else(|| self.disk.values().find(|d| d.root_path == path))
            .ok_or_else(|| pb::Error {
                code: pb::ErrorCode::ProjectPathMissing.into(),
                message: format!("找不到这个文件夹：{path}"),
                retryable: false,
                ..Default::default()
            })?;
        let known = self
            .projects
            .values()
            .find(|p| p.root_path == d.root_path)
            .map(|p| p.id.clone());
        let state = match (&known, d.versioned) {
            (Some(_), _) => pb::ProjectPathState::Known,
            (None, true) => pb::ProjectPathState::Addable,
            (None, false) => pb::ProjectPathState::NotVersionControlled,
        };
        Ok(pb::InspectProjectPathResult {
            state: state.into(),
            name: last_segment(&d.root_path).to_owned(),
            root_path: d.root_path.clone(),
            project_id: known,
            has_agents_md: d.has_agents_md,
        })
    }
}

/// 路径的最后一段（项目名的默认取法）。
pub fn last_segment(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
}

fn invalid(field: &str, message: &str) -> pb::Error {
    pb::Error {
        code: pb::ErrorCode::InvalidArgument.into(),
        message: message.to_owned(),
        retryable: false,
        details: [("field".to_owned(), field.to_owned())].into(),
    }
}

/// 回合还在跑（排队、运行、等审批、取消中）：和 iwork-client-core `store::timeline::turn_is_active` 同一个条件
/// （假后台不依赖 client-core，照抄一份；两处对「哪些回合有结束行」必须一致，见 `row_page`）。
fn turn_active(state: i32) -> bool {
    use pb::TurnState as S;
    matches!(
        S::try_from(state),
        Ok(S::Queued | S::Running | S::WaitingApproval | S::Cancelling)
    )
}
