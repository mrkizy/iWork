//! 金样本的手写构造函数：每个关键消息至少一条，字段尽量填满（T01 方案 3.7）。
//!
//! 样本只增不改：已经生成过文件的函数不许再改它的取值（`tests/compat.rs` 会比对字节）。
//! 新加了字段想保护它的取值，就写一个新函数、在 `mod.rs` 的登记表末尾追加，生成带新编号的样本。
//! 取值刻意都不是默认值，否则 proto3 不编码它，样本就保护不到这个字段。
//!
//! 结构体字面量刻意写全字段、不用 `..Default::default()`：给已有样本的消息加字段时编译会停在这里，
//! 逼加字段的人看一眼样本。旧构造函数里补默认值（编码结果不变，样本不动），想保护新值就另加构造函数。

use bytes::Bytes;
use iwork_proto::pb::*;
use prost_types::Timestamp;

// ID 都是合法 ULID（Crockford base32，没有 I L O U）：iwork-app 会把事件样本解成领域类型，ID 解析不宽容。
const PROJECT: &str = "01JC0P0000000000000000PRJ1";
const PROJECT_2: &str = "01JC0P0000000000000000PRJ2";
const ASSISTANT: &str = "01JC0A0000000000000000AST1";
const CONVERSATION: &str = "01JC0C0000000000000000CNV1";
const MESSAGE: &str = "01JC0M0000000000000000MSG1";
const TURN: &str = "01JC0T0000000000000000TRN1";
const APPROVAL: &str = "01JC0V0000000000000000APR1";
const CARD: &str = "01JC0D0000000000000000CRD1";
const RULE: &str = "01JC0R0000000000000000RXE1";
const ENTRY: &str = "01JC0E0000000000000000ENT1";
const ATTACHMENT: &str = "01JC0F0000000000000000ATT1";
const JOB: &str = "01JC0J0000000000000000JB01";
const RUNTIME: &str = "01JC0X0000000000000000RTM1";
/// 提议哈希是 SHA-256，领域类型要求正好 32 字节。
const PROPOSAL_HASH: [u8; 32] = [
    0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
    0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c,
];
const REVIEW: &str = "01JC0V0000000000000000RVW1";

fn s(v: &str) -> String {
    v.to_owned()
}

/// 毫秒精度的时间：Rust 与 TS 的 JSON 都写成三位小数，避免两边小数位数不同。
fn ts(seconds: i64) -> Option<Timestamp> {
    Some(Timestamp {
        seconds,
        nanos: 123_000_000,
    })
}

fn model_sel() -> Option<ModelSelection> {
    Some(ModelSelection {
        model_id: s("opus"),
        effort: Some(s("high")),
    })
}

fn artifact(area: ArtifactArea) -> Option<ArtifactRef> {
    Some(ArtifactRef {
        sha256: s("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"),
        area: area.into(),
        size: 4096,
    })
}

fn usage() -> Option<Usage> {
    Some(Usage {
        input_tokens: 1200,
        output_tokens: 340,
        cache_read_tokens: 5000,
        cache_write_tokens: 60,
        cost_usd: Some(0.25),
    })
}

fn error_full() -> Error {
    Error {
        code: ErrorCode::ProtocolVersionMismatch.into(),
        message: s("协议主版本不一致"),
        retryable: true,
        details: [(s("client_major"), s("1")), (s("server_major"), s("2"))].into(),
    }
}

fn project_settings() -> ProjectSettings {
    ProjectSettings {
        default_approval_mode: ApprovalMode::Auto.into(),
        default_model: model_sel(),
        ai_review: Some(AiReviewSettings {
            enabled: true,
            runtime_id: Some(s(RUNTIME)),
            model: model_sel(),
            timeout_secs: 60,
        }),
    }
}

fn project() -> Project {
    Project {
        id: s(PROJECT),
        root_path: s("/Users/me/code/iwork"),
        vcs_kind: s("vcs-a"),
        name: s("iwork"),
        pinned: true,
        last_active_at: ts(1_762_000_000),
        path_status: PathStatus::Ok.into(),
        settings: Some(project_settings()),
        unread: 3,
    }
}

fn assistant_config() -> AssistantConfig {
    AssistantConfig {
        name: s("审查员"),
        avatar: Some(Avatar {
            initial: s("审"),
            color: s("#4f46e5"),
        }),
        prompt: s("你负责代码审查。"),
        runtime_id: s(RUNTIME),
        model: model_sel(),
        disabled_groups: vec![ToolGroup::Web.into(), ToolGroup::RunCommands.into()],
    }
}

fn assistant() -> Assistant {
    Assistant {
        id: s(ASSISTANT),
        builtin: true,
        version: 4,
        config: Some(assistant_config()),
        updated_at: ts(1_762_000_100),
    }
}

fn conversation_project() -> Conversation {
    Conversation {
        id: s(CONVERSATION),
        kind: Some(conversation::Kind::Project(ProjectConversation {
            project_id: s(PROJECT),
        })),
        state: ConversationState::Active.into(),
        last_read_seq: 42,
        unread: 2,
        model_override: model_sel(),
        approval_mode: Some(ApprovalMode::FullAccess.into()),
        updated_at: ts(1_762_000_200),
    }
}

fn conversation_dm() -> Conversation {
    Conversation {
        id: s(CONVERSATION),
        kind: Some(conversation::Kind::AssistantDm(AssistantDm {
            assistant_id: s(ASSISTANT),
            context_project_id: Some(s(PROJECT)),
        })),
        state: ConversationState::Deleting.into(),
        last_read_seq: 7,
        unread: 1,
        model_override: None,
        approval_mode: Some(ApprovalMode::Manual.into()),
        updated_at: ts(1_762_000_300),
    }
}

fn message_assistant() -> Message {
    Message {
        id: s(MESSAGE),
        conversation_id: s(CONVERSATION),
        author: Some(Author {
            kind: Some(author::Kind::Assistant(AssistantAuthor {
                assistant_id: s(ASSISTANT),
                name_snapshot: s("审查员"),
            })),
        }),
        context_project_id: Some(s(PROJECT)),
        status: MessageStatus::Streaming.into(),
        body: Some(MessageBody {
            content: Some(message_body::Content::InlineText(s("正在看……"))),
        }),
        attachment_ids: vec![s(ATTACHMENT)],
        created_at: ts(1_762_000_400),
        turn_id: Some(s(TURN)),
    }
}

fn turn() -> Turn {
    Turn {
        id: s(TURN),
        conversation_id: s(CONVERSATION),
        assistant_id: s(ASSISTANT),
        assistant_version: 4,
        runtime_id: s(RUNTIME),
        model: model_sel(),
        runtime_session_id: Some(s("sess-1")),
        state: TurnState::Interrupted.into(),
        state_reason: Some(s("审批等待到上限")),
        started_at: ts(1_762_000_500),
        ended_at: ts(1_762_000_600),
        kind: TurnKind::Assistant.into(),
        interrupt_reason: Some(InterruptReason::ApprovalWaitLimit.into()),
    }
}

fn action_command() -> ActionSummary {
    ActionSummary {
        kind: ActionKind::OutsideSandbox.into(),
        target: Some(ActionTarget {
            kind: Some(action_target::Kind::Command(CommandTarget {
                program: s("npm"),
                subcommand: Some(s("publish")),
                argv: vec![s("npm"), s("publish"), s("--access=public")],
            })),
        }),
        tool_name: s("Bash"),
        raw_summary: s("npm publish --access=public"),
        out_of_bounds: true,
        bypass_sandbox_requested: true,
        opaque_script: true,
    }
}

fn action_paths() -> ActionSummary {
    ActionSummary {
        kind: ActionKind::Write.into(),
        target: Some(ActionTarget {
            kind: Some(action_target::Kind::Paths(PathList {
                paths: vec![s("/tmp/a.txt"), s("/tmp/b.txt")],
            })),
        }),
        tool_name: s("Edit"),
        raw_summary: s("改两个文件"),
        out_of_bounds: false,
        bypass_sandbox_requested: false,
        opaque_script: false,
    }
}

fn approval() -> Approval {
    Approval {
        id: s(APPROVAL),
        turn_id: s(TURN),
        conversation_id: s(CONVERSATION),
        kind: ApprovalKind::OutsideSandbox.into(),
        action: Some(action_command()),
        matched_rule_ids: vec![s(RULE)],
        ruleset_version: s("2026.11.1"),
        state: ApprovalState::Pending.into(),
        review: ReviewState::InProgress.into(),
        review_reason: Some(s("等待审查")),
        offered_remember: vec![RememberScope::Once.into(), RememberScope::Project.into()],
        remember_label: s("本项目内 `npm publish …` 不再问"),
        requested_at: ts(1_762_000_700),
        decided_at: ts(1_762_000_800),
        review_id: Some(s(REVIEW)),
    }
}

fn user_action_card() -> UserActionCard {
    UserActionCard {
        id: s(CARD),
        turn_id: s(TURN),
        conversation_id: s(CONVERSATION),
        content: Some(user_action_card::Content::Command(CommandProposal {
            command: s("ssh-add -l"),
            cwd: s("/Users/me"),
        })),
        reason: s("列出已加载的密钥"),
        proposal_hash: Bytes::from_static(&PROPOSAL_HASH),
        state: UserActionState::Unknown.into(),
        exit: Some(ExitStatus {
            kind: Some(exit_status::Kind::Code(2)),
        }),
        output_artifact: artifact(ArtifactArea::Artifacts),
        error: Some(s("后台服务中断")),
        void_reason: Some(VoidReason::ConversationDeleted.into()),
    }
}

fn rule() -> Rule {
    Rule {
        id: s(RULE),
        layer: RuleLayer::Remembered.into(),
        matcher: Some(RuleMatcher {
            kind: Some(rule_matcher::Kind::ProgramSubcommand(ProgramSubcommand {
                program: s("npm"),
                subcommand: Some(s("test")),
            })),
        }),
        effect: RuleEffect::Allow.into(),
        source: Some(RuleSource {
            kind: Some(rule_source::Kind::Remembered(RememberedRuleSource {
                approval_id: s(APPROVAL),
                by: s("user"),
            })),
        }),
        scope: Some(Scope {
            kind: Some(scope::Kind::ProjectId(s(PROJECT))),
        }),
        created_at: ts(1_762_000_900),
        label: s("本项目内 `npm test …` 不再问"),
    }
}

/// 用户加进「需凭据的命令」清单的一条：清单条目就是规则条目。
fn security_list_rule() -> Rule {
    Rule {
        id: s(ENTRY),
        layer: RuleLayer::CredentialCommands.into(),
        matcher: Some(RuleMatcher {
            kind: Some(rule_matcher::Kind::ProgramSubcommand(ProgramSubcommand {
                program: s("terraform"),
                subcommand: Some(s("apply")),
            })),
        }),
        effect: RuleEffect::OutsideSandbox.into(),
        source: Some(RuleSource {
            kind: Some(rule_source::Kind::User(UserRuleSource {})),
        }),
        scope: Some(Scope {
            kind: Some(scope::Kind::Global(GlobalScope {})),
        }),
        created_at: ts(1_762_001_000),
        label: s("terraform apply"),
    }
}

fn attachment() -> Attachment {
    Attachment {
        id: s(ATTACHMENT),
        conversation_id: s(CONVERSATION),
        sha256: s("2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae"),
        size: 1024,
        mime: s("image/png"),
        name: s("截图.png"),
        origin: Some(AttachmentOrigin {
            kind: Some(attachment_origin::Kind::Pasted(ArtifactRef {
                sha256: s("2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae"),
                area: ArtifactArea::Attachments.into(),
                size: 1024,
            })),
        }),
        needs_confirmation: true,
        created_at: ts(1_762_001_100),
    }
}

fn runtime_status() -> RuntimeStatus {
    RuntimeStatus {
        runtime_id: s(RUNTIME),
        kind: s("runtime-a"),
        display_name: s("Runtime A"),
        cli_path: Some(s("/usr/local/bin/runtime-a")),
        cli_version: Some(s("2.1.0")),
        adapter_version: s("0.1.0"),
        state: RuntimeState::ReadyVerified.into(),
        login: Some(LoginInfo {
            logged_in: true,
            auth_method: s("oauth"),
            subscription_type: s("max"),
            login_expires_at: ts(1_763_000_000),
        }),
    }
}

fn quota_window() -> QuotaWindow {
    QuotaWindow {
        window: s("five_hour"),
        used_pct: 38,
        resets_at: ts(1_762_010_000),
        observed_at: ts(1_762_001_200),
    }
}

fn local_config_item() -> LocalConfigItem {
    LocalConfigItem {
        id: s("mcp:github"),
        kind: LocalConfigKind::Mcp.into(),
        name: s("github"),
        description: s("GitHub MCP"),
        source: s("~/.config/runtime-a/mcp.json"),
        imported: true,
    }
}

fn verdict_deny() -> Verdict {
    Verdict {
        kind: Some(verdict::Kind::Deny(DenyReason {
            kind: Some(deny_reason::Kind::DenialMemory(ActionFingerprint {
                kind: ActionKind::Delete.into(),
                target: Some(ActionTarget {
                    kind: Some(action_target::Kind::Path(s("/Users/me/code/iwork/build"))),
                }),
            })),
        })),
    }
}

// ---------- 帧 ----------

pub fn hello() -> Hello {
    Hello {
        protocol_major: 1,
        protocol_minor: 3,
        client_kind: ClientKind::Desktop.into(),
        client_version: s("0.1.0"),
        resume_token: Some(Bytes::from_static(b"resume-token")),
    }
}

pub fn welcome() -> Welcome {
    Welcome {
        session_id: s("session-1"),
        server_version: s("0.1.0"),
        capabilities: vec![s("resume"), s("chunk-upload")],
        head_seq: 9_007_199_254_740_993,
        protocol_major: 1,
        protocol_minor: 2,
    }
}

pub fn request_command() -> Request {
    Request {
        request_id: 7,
        body: Some(request::Body::Command(command_send_message())),
    }
}

pub fn response_error() -> Response {
    Response {
        request_id: 0,
        body: Some(response::Body::Error(error_full())),
    }
}

pub fn event_persisted() -> Event {
    Event {
        subscription_ids: vec![1, 2],
        body: Some(event::Body::Persisted(PersistedEvent {
            seq: 1001,
            at: ts(1_762_002_000),
            actor: s("user"),
            event: Some(ev_approval_requested()),
        })),
    }
}

pub fn chunk() -> Chunk {
    Chunk {
        upload_id: s("upload-1"),
        offset: 1_048_576,
        data: Bytes::from_static(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a]),
        last: true,
    }
}

pub fn subscribe() -> Subscribe {
    Subscribe {
        subscription_id: 3,
        scope: Some(SubscriptionScope {
            kind: Some(subscription_scope::Kind::Conversation(ConversationScope {
                conversation_id: s(CONVERSATION),
            })),
        }),
        from_seq: 900,
    }
}

// ---------- 命令 ----------

pub fn command_send_message() -> Command {
    Command {
        command_id: s("01JC0K0000000000000000CMD1"),
        kind: Some(command::Kind::SendMessage(SendMessage {
            conversation_id: s(CONVERSATION),
            text: s("帮我看看这个改动"),
            attachment_ids: vec![s(ATTACHMENT)],
        })),
    }
}

pub fn command_decide_approval() -> Command {
    Command {
        command_id: s("01JC0K0000000000000000CMD2"),
        kind: Some(command::Kind::DecideApproval(DecideApproval {
            approval_id: s(APPROVAL),
            decision: ApprovalDecision::Allow.into(),
            remember: RememberScope::Project.into(),
        })),
    }
}

pub fn command_set_approval_mode() -> Command {
    Command {
        command_id: s("01JC0K0000000000000000CMD3"),
        kind: Some(command::Kind::SetApprovalMode(SetApprovalMode {
            target: Some(ApprovalModeTarget {
                kind: Some(approval_mode_target::Kind::Member(MemberTarget {
                    conversation_id: s(CONVERSATION),
                    assistant_id: s(ASSISTANT),
                })),
            }),
            mode: ApprovalMode::Manual.into(),
            follow_supervisor: true,
        })),
    }
}

pub fn command_update_project_settings() -> Command {
    let settings = project_settings();
    Command {
        command_id: s("01JC0K0000000000000000CMD4"),
        kind: Some(command::Kind::UpdateProjectSettings(
            UpdateProjectSettings {
                project_id: s(PROJECT),
                default_approval_mode: Some(ApprovalMode::Auto.into()),
                default_model: settings.default_model,
                ai_review: settings.ai_review,
            },
        )),
    }
}

pub fn command_upload_attachment() -> Command {
    Command {
        command_id: s("01JC0K0000000000000000CMD5"),
        kind: Some(command::Kind::UploadAttachment(UploadAttachment {
            conversation_id: s(CONVERSATION),
            name: s("日志.txt"),
            mime: s("text/plain"),
            source: Some(upload_attachment::Source::UploadId(s("upload-1"))),
        })),
    }
}

pub fn command_execute_user_action() -> Command {
    Command {
        command_id: s("01JC0K0000000000000000CMD6"),
        kind: Some(command::Kind::ExecuteUserAction(ExecuteUserAction {
            card_id: s(CARD),
            proposal_hash: Bytes::from_static(&PROPOSAL_HASH),
        })),
    }
}

pub fn command_add_security_list_entry() -> Command {
    Command {
        command_id: s("01JC0K0000000000000000CMD7"),
        kind: Some(command::Kind::AddSecurityListEntry(AddSecurityListEntry {
            list: SecurityList::SensitiveDirs.into(),
            value: s("~/secrets"),
            scope: Some(Scope {
                kind: Some(scope::Kind::ProjectId(s(PROJECT))),
            }),
        })),
    }
}

pub fn command_create_assistant() -> Command {
    Command {
        command_id: s("01JC0K0000000000000000CMD8"),
        kind: Some(command::Kind::CreateAssistant(CreateAssistant {
            config: Some(assistant_config()),
        })),
    }
}

pub fn command_accepted() -> CommandAccepted {
    CommandAccepted {
        first_seq: 1001,
        last_seq: 1003,
        replayed: true,
    }
}

// ---------- 查询 ----------

pub fn query_get_conversation() -> Query {
    Query {
        kind: Some(query::Kind::GetConversation(GetConversation {
            conversation_id: s(CONVERSATION),
            before_message_id: Some(s(MESSAGE)),
            limit: 50,
        })),
    }
}

pub fn query_result_get_conversation() -> QueryResult {
    QueryResult {
        seq: 1003,
        kind: Some(query_result::Kind::GetConversation(GetConversationResult {
            conversation: Some(conversation_project()),
            messages: vec![message_assistant()],
            turns: vec![turn()],
            has_more: true,
        })),
    }
}

pub fn query_result_read_artifact() -> QueryResult {
    QueryResult {
        seq: 1003,
        kind: Some(query_result::Kind::ReadArtifact(ArtifactChunk {
            data: Bytes::from_static(b"hello artifact"),
            offset: 4082,
            total_size: 4096,
            eof: true,
        })),
    }
}

pub fn query_result_pending() -> QueryResult {
    QueryResult {
        seq: 1004,
        kind: Some(query_result::Kind::ListPendingItems(
            ListPendingItemsResult {
                items: vec![
                    PendingItem {
                        item: Some(pending_item::Item::Approval(approval())),
                    },
                    PendingItem {
                        item: Some(pending_item::Item::UserAction(user_action_card())),
                    },
                    PendingItem {
                        item: Some(pending_item::Item::AgentsMdDraft(AgentsMdDraftCard {
                            card_id: s(CARD),
                            project_id: s(PROJECT),
                            content: artifact(ArtifactArea::Artifacts),
                            proposed_at: ts(1_762_003_000),
                        })),
                    },
                    PendingItem {
                        item: Some(pending_item::Item::Resume(ResumeCard {
                            turn_id: s(TURN),
                            conversation_id: s(CONVERSATION),
                            reason: InterruptReason::Crash.into(),
                        })),
                    },
                ],
            },
        )),
    }
}

// ---------- 实体（3.4 列出的关键消息） ----------

pub fn approval_full() -> Approval {
    approval()
}

pub fn turn_full() -> Turn {
    turn()
}

pub fn user_action_card_full() -> UserActionCard {
    user_action_card()
}

pub fn rule_full() -> Rule {
    rule()
}

pub fn runtime_status_full() -> RuntimeStatus {
    runtime_status()
}

pub fn error() -> Error {
    error_full()
}

// ---------- 随会话下发给桥接 ----------

pub fn session_constraints() -> SessionConstraints {
    SessionConstraints {
        deny_read_write: vec![s("~/.ssh"), s("~/Library/Application Support/iWork")],
        read_only: vec![s("~/Library/Application Support/iWork/attachments")],
        allow_write: Some(AllowWrite {
            kind: Some(allow_write::Kind::Whitelist(WritablePaths {
                paths: vec![s("/Users/me/code/iwork"), s("$TMPDIR")],
            })),
        }),
        excluded_commands: vec![CommandPattern {
            program: s("gh"),
            subcommand: Some(s("pr")),
        }],
        allow_unsandboxed_retry: true,
        network: NetworkPolicy::Open.into(),
    }
}

pub fn session_tool_set() -> SessionToolSet {
    SessionToolSet {
        enabled_groups: vec![ToolGroup::ReadAndSearch.into(), ToolGroup::EditFiles.into()],
        include_always_on: true,
    }
}

pub fn session_model() -> SessionModel {
    SessionModel { model: model_sel() }
}

pub fn session_setup() -> SessionSetup {
    SessionSetup {
        model: model_sel(),
        constraints: Some(session_constraints()),
        tool_set: Some(session_tool_set()),
    }
}

// ---------- 不落盘的增量 ----------

pub fn transient_message_delta() -> TransientEvent {
    TransientEvent {
        event: Some(transient_event::Event::MessageDelta(MessageDelta {
            message_id: s(MESSAGE),
            offset: 128,
            text: s("继续输出"),
        })),
    }
}

pub fn transient_tool_output_delta() -> TransientEvent {
    TransientEvent {
        event: Some(transient_event::Event::ToolOutputDelta(ToolOutputDelta {
            tool_call_id: s("tool-1"),
            offset: 64,
            text: s("ok\n"),
        })),
    }
}

pub fn transient_clone_progress() -> TransientEvent {
    TransientEvent {
        event: Some(transient_event::Event::CloneProgress(CloneProgress {
            job_id: s(JOB),
            phase: s("receiving"),
            percent: 42,
            text: s("Receiving objects: 42%"),
        })),
    }
}

pub fn transient_subscription_lagging() -> TransientEvent {
    TransientEvent {
        event: Some(transient_event::Event::SubscriptionLagging(
            SubscriptionLagging {
                subscription_id: 3,
                last_seq: 1002,
            },
        )),
    }
}

// ---------- 持久事件：每个变体一条 ----------

fn ev(event: domain_event::Event) -> DomainEvent {
    DomainEvent { event: Some(event) }
}

use domain_event::Event as E;

pub fn ev_project_added() -> DomainEvent {
    ev(E::ProjectAdded(ProjectAdded {
        project: Some(project()),
    }))
}

pub fn ev_project_updated() -> DomainEvent {
    ev(E::ProjectUpdated(ProjectUpdated {
        project_id: s(PROJECT),
        name: Some(s("iwork-2")),
        pinned: Some(false),
        last_active_at: ts(1_762_004_000),
    }))
}

pub fn ev_project_relocated() -> DomainEvent {
    ev(E::ProjectRelocated(ProjectRelocated {
        project_id: s(PROJECT),
        new_path: s("/Users/me/work/iwork"),
    }))
}

pub fn ev_project_path_status_changed() -> DomainEvent {
    ev(E::ProjectPathStatusChanged(ProjectPathStatusChanged {
        project_id: s(PROJECT),
        path_status: PathStatus::Missing.into(),
    }))
}

pub fn ev_project_removed() -> DomainEvent {
    ev(E::ProjectRemoved(ProjectRemoved {
        project_id: s(PROJECT),
    }))
}

pub fn ev_project_settings_updated() -> DomainEvent {
    ev(E::ProjectSettingsUpdated(ProjectSettingsUpdated {
        project_id: s(PROJECT),
        settings: Some(project_settings()),
    }))
}

pub fn ev_repository_initialized() -> DomainEvent {
    ev(E::RepositoryInitialized(RepositoryInitialized {
        path: s("/Users/me/code/new"),
    }))
}

pub fn ev_clone_finished() -> DomainEvent {
    ev(E::CloneFinished(CloneFinished {
        job_id: s(JOB),
        ok: false,
        error: Some(Error {
            code: ErrorCode::CloneFailed.into(),
            message: s("克隆失败"),
            retryable: true,
            details: [(s("path"), s("/Users/me/code/x"))].into(),
        }),
        path: Some(s("/Users/me/code/x")),
    }))
}

pub fn ev_agents_md_draft_proposed() -> DomainEvent {
    ev(E::AgentsMdDraftProposed(AgentsMdDraftProposed {
        card_id: s(CARD),
        project_id: s(PROJECT),
        content_artifact: artifact(ArtifactArea::Artifacts),
    }))
}

pub fn ev_agents_md_draft_decided() -> DomainEvent {
    ev(E::AgentsMdDraftDecided(AgentsMdDraftDecided {
        card_id: s(CARD),
        project_id: s(PROJECT),
        decision: DraftDecision::Discard.into(),
    }))
}

pub fn ev_assistant_created() -> DomainEvent {
    ev(E::AssistantCreated(AssistantCreated {
        assistant: Some(assistant()),
    }))
}

pub fn ev_assistant_updated() -> DomainEvent {
    ev(E::AssistantUpdated(AssistantUpdated {
        assistant_id: s(ASSISTANT),
        version: 5,
        config: Some(assistant_config()),
    }))
}

pub fn ev_assistant_deleted() -> DomainEvent {
    ev(E::AssistantDeleted(AssistantDeleted {
        assistant_id: s(ASSISTANT),
        name_snapshot: s("审查员"),
    }))
}

pub fn ev_assistant_reset() -> DomainEvent {
    ev(E::AssistantReset(AssistantReset {
        assistant_id: s(ASSISTANT),
        version: 6,
        config: Some(assistant_config()),
    }))
}

pub fn ev_conversation_created() -> DomainEvent {
    ev(E::ConversationCreated(ConversationCreated {
        conversation: Some(conversation_dm()),
    }))
}

pub fn ev_conversation_deleting() -> DomainEvent {
    ev(E::ConversationDeleting(ConversationDeleting {
        conversation_id: s(CONVERSATION),
    }))
}

pub fn ev_conversation_deleted() -> DomainEvent {
    ev(E::ConversationDeleted(ConversationDeleted {
        conversation_id: s(CONVERSATION),
    }))
}

pub fn ev_conversation_model_changed() -> DomainEvent {
    ev(E::ConversationModelChanged(ConversationModelChanged {
        conversation_id: s(CONVERSATION),
        model: model_sel(),
    }))
}

pub fn ev_approval_mode_changed() -> DomainEvent {
    ev(E::ApprovalModeChanged(ApprovalModeChanged {
        target: Some(ApprovalModeTarget {
            kind: Some(approval_mode_target::Kind::ConversationId(s(CONVERSATION))),
        }),
        mode: Some(ApprovalMode::FullAccess.into()),
        cause: ApprovalModeChangeCause::ProjectSwitch.into(),
    }))
}

pub fn ev_dm_project_switched() -> DomainEvent {
    ev(E::DmProjectSwitched(DmProjectSwitched {
        conversation_id: s(CONVERSATION),
        project_id: Some(s(PROJECT_2)),
    }))
}

pub fn ev_conversation_read() -> DomainEvent {
    ev(E::ConversationRead(ConversationRead {
        conversation_id: s(CONVERSATION),
        last_read_seq: 1002,
    }))
}

pub fn ev_message_appended() -> DomainEvent {
    ev(E::MessageAppended(MessageAppended {
        message: Some(message_assistant()),
    }))
}

pub fn ev_message_completed() -> DomainEvent {
    ev(E::MessageCompleted(MessageCompleted {
        conversation_id: s(CONVERSATION),
        message_id: s(MESSAGE),
        body: Some(MessageBody {
            content: Some(message_body::Content::Artifact(ArtifactRef {
                sha256: s("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"),
                area: ArtifactArea::Artifacts.into(),
                size: 70_000,
            })),
        }),
        status: MessageStatus::Interrupted.into(),
    }))
}

pub fn ev_turn_queued() -> DomainEvent {
    ev(E::TurnQueued(TurnQueued { turn: Some(turn()) }))
}

pub fn ev_turn_state_changed() -> DomainEvent {
    ev(E::TurnStateChanged(TurnStateChanged {
        turn_id: s(TURN),
        from: TurnState::WaitingApproval.into(),
        to: TurnState::Interrupted.into(),
        reason: Some(s("审批等待到上限")),
        interrupt_reason: Some(InterruptReason::ApprovalWaitLimit.into()),
        from_interrupt_reason: Some(InterruptReason::Crash.into()),
    }))
}

pub fn ev_turn_finished() -> DomainEvent {
    ev(E::TurnFinished(TurnFinished {
        turn_id: s(TURN),
        state: TurnState::Completed.into(),
        usage: usage(),
        celebrate: true,
        change_summary: Some(ChangeSummary {
            files_changed: 2,
            insertions: 30,
            deletions: 4,
            paths: vec![s("src/a.rs"), s("src/b.rs")],
        }),
    }))
}

pub fn ev_turn_model_reported() -> DomainEvent {
    ev(E::TurnModelReported(TurnModelReported {
        turn_id: s(TURN),
        model: model_sel(),
    }))
}

pub fn ev_tool_call_started() -> DomainEvent {
    ev(E::ToolCallStarted(ToolCallStarted {
        turn_id: s(TURN),
        tool_call_id: s("tool-1"),
        tool_name: s("Edit"),
        action: Some(action_paths()),
    }))
}

pub fn ev_tool_call_finished() -> DomainEvent {
    ev(E::ToolCallFinished(ToolCallFinished {
        turn_id: s(TURN),
        tool_call_id: s("tool-1"),
        ok: true,
        output_artifact: artifact(ArtifactArea::Artifacts),
    }))
}

pub fn ev_approval_requested() -> DomainEvent {
    ev(E::ApprovalRequested(ApprovalRequested {
        approval: Some(approval()),
    }))
}

pub fn ev_approval_review_updated() -> DomainEvent {
    ev(E::ApprovalReviewUpdated(ApprovalReviewUpdated {
        approval_id: s(APPROVAL),
        review_id: s(REVIEW),
        review: ReviewState::Abandoned.into(),
        reason: Some(s("切回手动档")),
    }))
}

pub fn ev_approval_decided() -> DomainEvent {
    ev(E::ApprovalDecided(ApprovalDecided {
        approval_id: s(APPROVAL),
        state: ApprovalState::Approved.into(),
        decider: ApprovalDecider::User.into(),
        remember: Some(RememberScope::Forever.into()),
    }))
}

pub fn ev_approval_voided() -> DomainEvent {
    ev(E::ApprovalVoided(ApprovalVoided {
        approval_id: s(APPROVAL),
        reason: VoidReason::TurnAbandonedAfterInterrupt.into(),
    }))
}

pub fn ev_approval_voided_turn_failed() -> DomainEvent {
    ev(E::ApprovalVoided(ApprovalVoided {
        approval_id: s(APPROVAL),
        reason: VoidReason::TurnFailed.into(),
    }))
}

pub fn ev_approval_evaluated() -> DomainEvent {
    ev(E::ApprovalEvaluated(ApprovalEvaluated {
        turn_id: s(TURN),
        action: Some(action_command()),
        matched_rule_ids: vec![s(RULE), s("01JC0R0000000000000000BSE1")],
        ruleset_version: s("2026.11.1"),
        verdict: Some(verdict_deny()),
        layer_hit: RuleLayer::DenialMemory.into(),
        duration_ms: 3,
    }))
}

pub fn ev_rule_remembered() -> DomainEvent {
    ev(E::RuleRemembered(RuleRemembered { rule: Some(rule()) }))
}

pub fn ev_rule_revoked() -> DomainEvent {
    ev(E::RuleRevoked(RuleRevoked { rule_id: s(RULE) }))
}

pub fn ev_rules_deleted() -> DomainEvent {
    ev(E::RulesDeleted(RulesDeleted {
        rule_ids: vec![s(RULE), s("01JC0R0000000000000000RXE2")],
        cause: RulesDeletedCause::AssistantDeleted.into(),
    }))
}

pub fn ev_approval_stale_result_dropped() -> DomainEvent {
    ev(E::ApprovalStaleResultDropped(ApprovalStaleResultDropped {
        approval_id: s(APPROVAL),
        review_id: s(REVIEW),
        result: ReviewState::Passed.into(),
        why: StaleReason::ModeNotAuto.into(),
    }))
}

pub fn ev_user_action_proposed() -> DomainEvent {
    let mut card = user_action_card();
    card.content = Some(user_action_card::Content::Patch(PatchProposal {
        path: s("/Users/me/.ssh/config"),
        old_text: s("Host a\n"),
        new_text: s("Host a\nHost b\n"),
    }));
    ev(E::UserActionProposed(UserActionProposed {
        card: Some(card),
    }))
}

pub fn ev_user_action_state_changed() -> DomainEvent {
    ev(E::UserActionStateChanged(UserActionStateChanged {
        card_id: s(CARD),
        state: UserActionState::Done.into(),
        exit: Some(ExitStatus {
            kind: Some(exit_status::Kind::Signal(15)),
        }),
        error: Some(s("被停止")),
        output_artifact: artifact(ArtifactArea::Artifacts),
        void_reason: Some(VoidReason::TurnCancelled.into()),
    }))
}

pub fn ev_security_list_entry_added() -> DomainEvent {
    ev(E::SecurityListEntryAdded(SecurityListEntryAdded {
        rule: Some(security_list_rule()),
    }))
}

pub fn ev_security_list_entry_removed() -> DomainEvent {
    ev(E::SecurityListEntryRemoved(SecurityListEntryRemoved {
        entry_id: s(ENTRY),
    }))
}

pub fn ev_attachment_stored() -> DomainEvent {
    ev(E::AttachmentStored(AttachmentStored {
        attachment: Some(attachment()),
    }))
}

pub fn ev_attachment_confirmed() -> DomainEvent {
    ev(E::AttachmentConfirmed(AttachmentConfirmed {
        conversation_id: s(CONVERSATION),
        attachment_id: s(ATTACHMENT),
    }))
}

pub fn ev_runtime_updated() -> DomainEvent {
    ev(E::RuntimeUpdated(RuntimeUpdated {
        runtime: Some(runtime_status()),
    }))
}

pub fn ev_quota_observed() -> DomainEvent {
    ev(E::QuotaObserved(QuotaObserved {
        runtime_id: s(RUNTIME),
        windows: vec![quota_window()],
        observed_at: ts(1_762_001_200),
    }))
}

pub fn ev_usage_reported() -> DomainEvent {
    ev(E::UsageReported(UsageReported {
        runtime_id: s(RUNTIME),
        turn_id: s(TURN),
        usage: usage(),
    }))
}

pub fn ev_local_import_prompted() -> DomainEvent {
    ev(E::LocalImportPrompted(LocalImportPrompted {
        runtime_id: s(RUNTIME),
    }))
}

pub fn ev_local_config_imported() -> DomainEvent {
    ev(E::LocalConfigImported(LocalConfigImported {
        runtime_id: s(RUNTIME),
        items: vec![local_config_item()],
    }))
}

pub fn ev_mcp_secret_set() -> DomainEvent {
    ev(E::McpSecretSet(McpSecretSet {
        mcp_server_id: s("mcp:github"),
        key_name: s("GITHUB_TOKEN"),
    }))
}

pub fn ev_global_settings_changed() -> DomainEvent {
    ev(E::GlobalSettingsChanged(GlobalSettingsChanged {
        playful_ui: Some(false),
        autostart: Some(true),
        projects_root: Some(s("/Users/me/code")),
    }))
}

pub fn ev_recovery_completed() -> DomainEvent {
    ev(E::RecoveryCompleted(RecoveryCompleted {
        interrupted_turn_ids: vec![s(TURN)],
    }))
}
