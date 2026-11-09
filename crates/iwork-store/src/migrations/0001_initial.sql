-- 迁移 1：初始表（方案 2.5）。三组表：日志与幂等（永不清）、非投影的持久表（重建不清）、
-- 投影表（重建清掉重算）。每张表的归属登记在 src/tables.rs。
--
-- 投影表每张都有 updated_seq：最后一次改变这一行任何字段的事件 seq（供 T03 按实体版本应用事件；
-- 本卡不暴露到读模型）。投影器凡是写到这一行就写入当前 seq。

-- 日志与幂等 ------------------------------------------------------------
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;

CREATE TABLE events (
  seq         INTEGER PRIMARY KEY,   -- 写线程显式分配，连续无洞；不用 AUTOINCREMENT
  stream_kind TEXT    NOT NULL,      -- StreamKind 的蛇形名
  stream_id   TEXT,                  -- NULL = 整类一条流（settings、system、rules_deleted）
  type        TEXT    NOT NULL,      -- DomainEvent::kind()
  payload     BLOB    NOT NULL,      -- EventCodec 编出的字节
  command_id  TEXT    NOT NULL,
  actor       TEXT    NOT NULL,
  at          INTEGER NOT NULL       -- UTC 毫秒
) STRICT;
CREATE INDEX events_by_stream ON events(stream_kind, stream_id, seq);

CREATE TABLE commands (
  command_id TEXT PRIMARY KEY,
  first_seq  INTEGER NOT NULL,
  last_seq   INTEGER NOT NULL,
  at         INTEGER NOT NULL
) STRICT;

CREATE TABLE outbox (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,   -- 删过行也不复用号
  command_id TEXT    NOT NULL,
  target     TEXT    NOT NULL,
  payload    BLOB    NOT NULL,
  state      TEXT    NOT NULL,       -- queued|dispatched|accepted_by_runtime|failed|expired|cancelled
  attempts   INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX outbox_by_state ON outbox(state, id);

-- 非投影的持久表 --------------------------------------------------------
CREATE TABLE message_checkpoints (   -- 设计 3.5：流式正文检查点，不进事件日志
  message_id TEXT PRIMARY KEY,
  offset     INTEGER NOT NULL,
  text       TEXT    NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE artifact_objects (      -- 设计 5.4：制品对象登记；引用关系在投影表 artifact_refs
  area       TEXT    NOT NULL,       -- artifacts|attachments|private
  sha256     TEXT    NOT NULL,       -- 64 位小写十六进制
  size       INTEGER NOT NULL,
  pins       INTEGER NOT NULL DEFAULT 0,   -- retain/release 的显式计数
  created_at INTEGER NOT NULL,
  PRIMARY KEY (area, sha256)
) STRICT;

-- 投影表（可从 events 重建）------------------------------------------------
CREATE TABLE projects (
  id                    TEXT PRIMARY KEY,
  root_path             TEXT    NOT NULL,
  name                  TEXT    NOT NULL,
  pinned                INTEGER NOT NULL,
  last_active_at        INTEGER,
  path_status           TEXT    NOT NULL,          -- ok|missing
  default_approval_mode TEXT    NOT NULL,          -- manual|auto（ProjectDefaultMode）
  snapshot              BLOB    NOT NULL,          -- ProjectAdded{project} 的载荷
  updated_seq           INTEGER NOT NULL
) STRICT;

CREATE TABLE assistants (
  id              TEXT PRIMARY KEY,
  name            TEXT    NOT NULL,
  builtin         INTEGER NOT NULL,
  current_version INTEGER NOT NULL,
  snapshot        BLOB    NOT NULL,                -- AssistantCreated{assistant} 的载荷（含当前版本）
  updated_seq     INTEGER NOT NULL
) STRICT;

CREATE TABLE assistant_versions (
  assistant_id TEXT    NOT NULL,
  version      INTEGER NOT NULL,
  created_at   INTEGER NOT NULL,
  snapshot     BLOB    NOT NULL,                   -- AssistantUpdated{assistant_id, version, config} 的载荷
  updated_seq  INTEGER NOT NULL,
  PRIMARY KEY (assistant_id, version)
) STRICT;

CREATE TABLE conversations (
  id                 TEXT PRIMARY KEY,
  kind               TEXT    NOT NULL,             -- project|assistant_dm
  project_id         TEXT,                         -- 项目对话的项目
  assistant_id       TEXT,                         -- 私聊的助手
  context_project_id TEXT,                         -- 私聊当前所选项目
  state              TEXT    NOT NULL,             -- active|deleting（deleted 的行直接删掉）
  last_read_seq      INTEGER NOT NULL,
  unread             INTEGER NOT NULL DEFAULT 0,
  approval_mode      TEXT,                         -- 对话级档位；私聊即自己的覆盖
  updated_at         INTEGER NOT NULL,
  snapshot           BLOB    NOT NULL,             -- ConversationCreated{conversation} 的载荷
  updated_seq        INTEGER NOT NULL
) STRICT;
CREATE INDEX conversations_by_project ON conversations(project_id, updated_at);
CREATE INDEX conversations_by_assistant ON conversations(assistant_id);

CREATE TABLE conversation_member_modes (           -- 成员单独设置（FR11）
  conversation_id TEXT    NOT NULL,
  assistant_id    TEXT    NOT NULL,
  mode            TEXT    NOT NULL,                -- manual|auto|full_access
  updated_seq     INTEGER NOT NULL,
  PRIMARY KEY (conversation_id, assistant_id)
) STRICT;

CREATE TABLE messages (
  id              TEXT PRIMARY KEY,                -- ULID，按它分页
  conversation_id TEXT    NOT NULL,
  author_kind     TEXT    NOT NULL,                -- user|assistant|system
  status          TEXT    NOT NULL,                -- sent|streaming|completed|interrupted|failed
  turn_id         TEXT,
  appended_seq    INTEGER NOT NULL,                -- 未读计数用
  created_at      INTEGER NOT NULL,
  snapshot        BLOB    NOT NULL,                -- MessageAppended{message} 的载荷（结束后正文已填）
  updated_seq     INTEGER NOT NULL
) STRICT;
CREATE INDEX messages_by_conversation ON messages(conversation_id, id);

CREATE TABLE turns (
  id              TEXT PRIMARY KEY,
  conversation_id TEXT    NOT NULL,
  state           TEXT    NOT NULL,                -- queued|running|…|interrupted_crash|interrupted_approval_wait_limit|…
  started_at      INTEGER NOT NULL,
  ended_at        INTEGER,
  interrupted_seq INTEGER,                         -- 进入 Interrupted 的事件 seq，未读计数用；离开时清 NULL
  snapshot        BLOB    NOT NULL,                -- TurnQueued{turn} 的载荷
  updated_seq     INTEGER NOT NULL
) STRICT;
CREATE INDEX turns_by_conversation ON turns(conversation_id, id);
CREATE INDEX turns_by_state ON turns(state);        -- 崩溃恢复找持有进程的回合（T04）

CREATE TABLE turn_usage (                          -- 「本对话用量」（设计 9.4）
  turn_id            TEXT PRIMARY KEY,
  conversation_id    TEXT    NOT NULL,
  input_tokens       INTEGER NOT NULL,
  output_tokens      INTEGER NOT NULL,
  cache_read_tokens  INTEGER NOT NULL,
  cache_write_tokens INTEGER NOT NULL,
  cost_usd           REAL,
  updated_seq        INTEGER NOT NULL
) STRICT;

CREATE TABLE tool_calls (
  turn_id        TEXT    NOT NULL,
  tool_call_id   TEXT    NOT NULL,
  tool_name      TEXT    NOT NULL,
  ok             INTEGER,                          -- NULL = 还没结束
  output_sha256  TEXT,
  output_area    TEXT,
  output_size    INTEGER,
  started_seq    INTEGER NOT NULL,
  updated_seq    INTEGER NOT NULL,
  PRIMARY KEY (turn_id, tool_call_id)
) STRICT;

CREATE TABLE approvals (
  id              TEXT PRIMARY KEY,
  turn_id         TEXT    NOT NULL,
  conversation_id TEXT    NOT NULL,
  state           TEXT    NOT NULL,                -- pending|approved|denied|voided（主状态的真值在这三列，不在快照里）
  decider         TEXT,                            -- approved 时：user|mode_switch|ai_review
  void_reason     TEXT,                            -- voided 时：turn_cancelled|conversation_deleted|turn_abandoned_after_interrupt|assistant_deleted
  review_phase    TEXT    NOT NULL,                -- not_reviewed|in_progress|passed|failed|errored|timed_out|abandoned
  awaits_user     INTEGER NOT NULL,                -- Approval::awaits_user()
  requested_seq   INTEGER NOT NULL,
  requested_at    INTEGER NOT NULL,
  decided_at      INTEGER,                         -- ApprovalDecided / ApprovalVoided 的信封 at
  snapshot        BLOB    NOT NULL,                -- ApprovalRequested{approval} 的载荷，state 固定写成 Pending（基底）
  updated_seq     INTEGER NOT NULL
) STRICT;
CREATE INDEX approvals_pending ON approvals(awaits_user, id);
CREATE INDEX approvals_by_conversation ON approvals(conversation_id, id);
CREATE INDEX approvals_by_turn ON approvals(turn_id);

CREATE TABLE user_actions (
  id              TEXT PRIMARY KEY,
  turn_id         TEXT    NOT NULL,
  conversation_id TEXT    NOT NULL,
  state_kind      TEXT    NOT NULL,                -- UserActionStateKind 蛇形名
  proposed_seq    INTEGER NOT NULL,
  snapshot        BLOB    NOT NULL,                -- UserActionProposed{card} 的载荷
  updated_seq     INTEGER NOT NULL
) STRICT;
CREATE INDEX user_actions_by_state ON user_actions(state_kind, id);
CREATE INDEX user_actions_by_conversation ON user_actions(conversation_id, id);

CREATE TABLE agents_md_drafts (
  card_id        TEXT PRIMARY KEY,
  project_id     TEXT    NOT NULL,
  content_sha256 TEXT    NOT NULL,
  content_area   TEXT    NOT NULL,
  content_size   INTEGER NOT NULL,
  proposed_at    INTEGER NOT NULL,
  decision       TEXT,                             -- NULL = 待处理；accept|discard
  updated_seq    INTEGER NOT NULL
) STRICT;

CREATE TABLE approval_rules (                      -- 记住的规则 + 安全清单条目，都是 RuleEntry
  id          TEXT PRIMARY KEY,
  layer       TEXT    NOT NULL,
  scope_kind  TEXT    NOT NULL,                    -- global|project|assistant|conversation
  scope_id    TEXT,
  created_at  INTEGER NOT NULL,
  snapshot    BLOB    NOT NULL,                    -- RuleRemembered{rule} 的载荷
  updated_seq INTEGER NOT NULL
) STRICT;
CREATE INDEX approval_rules_by_scope ON approval_rules(scope_kind, scope_id);
CREATE INDEX approval_rules_by_layer ON approval_rules(layer);

CREATE TABLE attachments (
  id                 TEXT PRIMARY KEY,
  conversation_id    TEXT    NOT NULL,
  sha256             TEXT    NOT NULL,
  size               INTEGER NOT NULL,
  mime               TEXT    NOT NULL,
  name               TEXT    NOT NULL,
  origin_kind        TEXT    NOT NULL,             -- repo_path|copied|pasted|credential_inline
  needs_confirmation INTEGER NOT NULL,
  confirmed          INTEGER NOT NULL DEFAULT 0,
  created_at         INTEGER NOT NULL,
  snapshot           BLOB    NOT NULL,             -- AttachmentStored{attachment} 的载荷
  updated_seq        INTEGER NOT NULL
) STRICT;
CREATE INDEX attachments_by_conversation ON attachments(conversation_id, id);

CREATE TABLE runtimes (
  id              TEXT PRIMARY KEY,
  kind            TEXT    NOT NULL,
  state           TEXT    NOT NULL,
  import_prompted INTEGER NOT NULL DEFAULT 0,      -- LocalImportPrompted
  snapshot        BLOB    NOT NULL,                -- RuntimeUpdated{runtime} 的载荷（只含 4 个登录字段）
  updated_seq     INTEGER NOT NULL
) STRICT;

CREATE TABLE quota_snapshots (                     -- 每个窗口只留最近一次
  runtime_id  TEXT    NOT NULL,
  window      TEXT    NOT NULL,
  used_pct    INTEGER NOT NULL,
  resets_at   INTEGER,
  observed_at INTEGER NOT NULL,
  updated_seq INTEGER NOT NULL,
  PRIMARY KEY (runtime_id, window)
) STRICT;

CREATE TABLE local_config_items (
  runtime_id  TEXT    NOT NULL,
  id          TEXT    NOT NULL,
  kind        TEXT    NOT NULL,                    -- mcp|skill|plugin
  name        TEXT    NOT NULL,
  description TEXT    NOT NULL,
  source      TEXT    NOT NULL,
  imported    INTEGER NOT NULL,
  updated_seq INTEGER NOT NULL,
  PRIMARY KEY (runtime_id, id)
) STRICT;

CREATE TABLE global_settings (                     -- 单行；NULL = 用户从没设过，读模型在读时补默认值
  id            INTEGER PRIMARY KEY CHECK (id = 1),
  playful_ui    INTEGER,
  autostart     INTEGER,
  projects_root TEXT,
  updated_seq   INTEGER NOT NULL
) STRICT;

CREATE TABLE artifact_refs (                       -- 事件里出现的每个 ArtifactRef 一行
  area        TEXT    NOT NULL,
  sha256      TEXT    NOT NULL,
  ref_kind    TEXT    NOT NULL,                    -- message|tool_call|user_action|agents_md_draft|attachment
  ref_id      TEXT    NOT NULL,
  updated_seq INTEGER NOT NULL,
  PRIMARY KEY (area, sha256, ref_kind, ref_id)
) STRICT;
CREATE INDEX artifact_refs_by_ref ON artifact_refs(ref_kind, ref_id);
