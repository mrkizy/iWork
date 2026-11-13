-- 0002：回合宿主与行级游标要的列与索引（T10a 方案 2.2）。只加列与索引；回填从 events 算，和迁移同一个事务。
-- 投影重建时这些列由投影器重新写出，和这里的回填同一口径（migration_0002_backfills_from_events 对拍）。

-- 回合：排进来的 seq、结束的 seq（0 = 没结束）；一趟编排的起点、最近一条取消意图、最近一条继续意图。
-- 后三个迁移时留空：那时没有在跑的编排，旧库里也没有意图事件。
ALTER TABLE turns ADD COLUMN created_seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE turns ADD COLUMN ended_seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE turns ADD COLUMN run_started_seq INTEGER;
ALTER TABLE turns ADD COLUMN cancel_intent_seq INTEGER;
ALTER TABLE turns ADD COLUMN resume_intent_seq INTEGER;

UPDATE turns SET created_seq = COALESCE(
  (SELECT MIN(e.seq) FROM events e WHERE e.stream_kind = 'turn' AND e.stream_id = turns.id), 0);
UPDATE turns SET ended_seq = COALESCE(
  (SELECT MAX(e.seq) FROM events e
   WHERE e.stream_kind = 'turn' AND e.stream_id = turns.id AND e.type = 'turn_finished'), 0);

-- 工具调用归哪个对话（行级游标按对话取）
ALTER TABLE tool_calls ADD COLUMN conversation_id TEXT NOT NULL DEFAULT '';
UPDATE tool_calls SET conversation_id = COALESCE(
  (SELECT t.conversation_id FROM turns t WHERE t.id = tool_calls.turn_id), '');

-- approvals.requested_seq、user_actions.proposed_seq 在 0001 里已有，就是各自流第一条事件的 seq，不另加列
CREATE INDEX turns_by_conversation_created ON turns(conversation_id, created_seq);
CREATE INDEX turns_by_conversation_ended ON turns(conversation_id, ended_seq);
CREATE INDEX tool_calls_by_conversation ON tool_calls(conversation_id, started_seq);
CREATE INDEX approvals_by_conversation_seq ON approvals(conversation_id, requested_seq);
CREATE INDEX user_actions_by_conversation_seq ON user_actions(conversation_id, proposed_seq);
CREATE INDEX messages_by_conversation_seq ON messages(conversation_id, appended_seq);
