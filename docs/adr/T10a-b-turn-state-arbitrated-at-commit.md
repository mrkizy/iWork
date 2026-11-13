# ADR-T10a-b 回合状态的写者不止一个，由存储在提交时裁决

- 状态：提议（2026-11-13，T10a 步 1；编号合入时由 supervisor 统一）
- 依据：`docs/plans/T10a.md` 0a.1 第 3 条、0a.3 的 A3、A4、X5、2.2、第 13 节；T02 方案 2.6「状态前提」「归属存续」；ADR-T07-e

## 背景

在跑的回合由编排在 `run_turn` 里写状态；排着的、已中断的、编排出错留下的回合没有编排，要由受理函数和回合宿主写。两类写者可能同时出手（撤回与派发、放弃与续接）。T10a 之前存储不校验回合状态，`projector/turns.rs` 照事件直接写，两个写者同时出手会把状态写乱。

## 决定

1. `TurnStateChanged` 落盘的前提：库里现在的状态等于事件的 `from`，并且 `from → to` 是转换表里某个触发给出的边（`iwork_domain::turn::check_turn_committed`）。
2. 回合意图 `TurnIntentRecorded`（取消 / 继续）落盘的前提：取消只落在 `Running`、`WaitingApproval` 上，继续只落在已中断上（`check_intent_committed`）。
3. 排新回合 `TurnQueued` 过归属存续（对话在、不在删，助手在；`requires_owner` 加这一种，判断仍是 `owner_alive`）。
4. 级联删除（`ConversationDeleted`、连带删私聊的 `AssistantDeleted`）的前提：被删对话名下回合都到终态（`iwork_domain::conversation::check_cascade`，卡片那一半由 T10e 加参数）。
5. 判断函数都在 `iwork-domain`；写线程（`SqliteProjector`）与参照投影（`iwork-ports/tests/reference_projection`，iwork-core 的假存储用它）各调一处。不通过报 `Projection`，整笔回滚。被拒的一方重读再定。
6. 存储另给一个排在写队列里的查回执 `StoragePort::command_receipt`，并把「`commit` 调用返回时已经入队」写进端口契约：同一个 `command_id` 前一份受理被中止、提交还在路上时，后一份靠它查到结果；后台的封口闸靠这条契约在锁里决定入不入队。

## 放弃的做法

- 所有回合状态经一个任务串行提交：在跑的回合里编排要等这个任务，T06 定的「提交顺序裁决审批的交付」就不成立了。
- 用进程内的锁排队：进程没了不留痕迹，重启后的写者不受约束；受理函数拿着锁被中止的窗口要另外处理。
- 查回执走读连接：看不到还在写队列里的那一笔。

## 影响

- 动 T02 已合入的投影器：每次回合状态变化、每条意图多读一行；级联删除多读名下回合。基准见 T10a 步 1 的实现报告。
- 测试布景不能再写表外的回合转换（比如 `Queued → WaitingApproval`），删对话之前回合要先到终态。
- 判断函数判错的后果是回合卡住（T10a 方案第 5 节 S3），靠 domain 的穷举测试与 `precondition_parity` 对拍挡。
