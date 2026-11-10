# T07 第 1 轮返工：B 块交付记录

任务：`2026-11-10-m1-t07`；分支：`cao/009e8660`；接手提交：`95f5ac6`。

状态：B 块代码与定向验收已整理，按 supervisor 要求先停在此处。全量检查与基准仍排队，本草稿不宣称最终通过。

本轮只修 `2026-11-10-m1-t07/1/F-13` 至 `F-25`。A 块保持交接结果；F-1、F-2、F-6、F-7 等方案复核，F-12 等产品负责人，不在本轮修改。

## 根因、实现与反例

以下路径以仓库根目录为起点。引擎路径前缀为 `crates/iwork-core/src/approval/engine/`；B 块测试文件为 `crates/iwork-core/tests/engine/review_round1_b.rs`，由 `tests/engine.rs` 加载。多数反例先运行到预期的失败，再修实现；F-22 的旧恢复签名无法表达回合身份，扩展签名后用双助手、双项目恢复测试验收。

| Finding | 根因与修改位置 | 自动化反例测试名 |
| --- | --- | --- |
| F-13 | 原实现先改审批、档位和等待表，再提交；Busy 或取消提交会留下未落盘状态。`changes.rs:4` 在克隆上生成计划，非法转换返回错误；`mod.rs:908`、`:991`、`:1108` 在成功提交或确认回调后更新内存、答复。项目默认、审查启动与结果、恢复放弃计划同样处理。T05 接缝见下文。 | `failed_mode_commit_keeps_previous_mode`、`failed_denial_can_be_retried_and_is_recorded`、`failed_project_default_commit_keeps_old_default`、`cancelled_mode_commit_does_not_publish_uncommitted_mode`、`void_plan_does_not_change_waiting_state_before_commit`；T05 的 `storage_failure_at_any_stage_stops_the_runtime` 另核对确认回调与成功存储事件完全相等。 |
| F-14 | 本地回执与订阅曾重复插入同一规则；拼装失败还会丢掉全部活规则。`changes.rs:119` 统一按 ID、事件序号幂等应用新增与撤销，迟到回执不能复活已撤销规则；`mod.rs:558` 传播拼装错误，权限入口拒绝且点名规则 ID。 | `remembered_rule_receipt_race_keeps_deny_rules`（存储回执闸门固定交错）、`invalid_ruleset_fails_closed_with_rule_id` |
| F-15 | 锁只通知首位等待者，等待者收到通知但未 poll 就取消会丢失移交。`lock.rs:65`、`:73` 用等待登记的析构继续通知下一位，不引入运行时依赖。 | `decision_lock.rs::cancelled_notified_waiter_hands_off_wakeup`（手工 poll 和独立 waker 计数） |
| F-16 | `release` 用缺失的 call 提前返回，连续接回合也丢失。`mod.rs:823` 分开生成 GrantOnce 与返回原 TurnId；旧审批只跳过一次性放行。 | `legacy_approval_resumes_without_grant_once` |
| F-17 | 删除助手时从普通待审批推导回合集合，漏掉只有卡片的回合。`mod.rs:211` 的卡片独立保存助手，`:1165` 把卡片回合也纳入删除计划。 | `assistant_deletion_voids_card_without_ordinary_approval` |
| F-18 | 引擎只接卡片终态，仍把执行中卡片看作 Pending。`changes.rs:72`、`mod.rs:1221` 消费全部卡片状态；作废计划只选 Pending。 | `active_and_unknown_cards_are_not_voided`（Starting、Executing、Stopping）；原 `user_action_card_round_trip` 保证终态回执仍可用。 |
| F-19 | 恢复忽略卡片事件。`recovery.rs:15` 重放提议、状态与输出引用，用原回合查询助手，回答通道为 detached。 | `recovered_cards_keep_owner_and_latest_state`（Pending 仍能作废，Executing 不得作废） |
| F-20 | 原计时器不覆盖建连，cancel/close 又可无限等待。`runtime.rs:87` 在建连前登记同一截止时间，覆盖 prompt、更新流与关闭；`:284` 在审批已转问用户后做最多三秒的收尾。 | `review_deadline_covers_open_cancel_and_close`（分别等到 open、prompt、close 阶段才触发定时器） |
| F-21 | 只在切手动时放弃审查，切全开后受拒绝记忆阻挡的审查永久停在 InProgress。`changes.rs:141` 的 `plan_reviews_leaving_auto` 让切档命令和私聊项目切换共用放弃逻辑，对离开 Auto 后未放行的审查统一放弃；领域触发和原因改为 `ModeChangedFromAuto`。 | `leaving_auto_abandons_review_blocked_by_denial_memory`；`dm_project_switch_out_of_auto_abandons_review_immediately` 还验证放弃提交失败返回错误、保留旧状态、重试成功；原 `to_manual_abandons_review` 保留手动路径。 |
| F-22 | ConversationId 不能唯一确定助手、私聊项目与快照。`recovery.rs:15` 的恢复查询改为 `keys_for(ConversationId, TurnId)`。 | `recovery_resolves_each_original_turn_line`（同对话不同助手、项目，永久记住仍归原助手） |
| F-23 | ReviewId 没有真实回合，运行时用量、额度和模型被丢弃。`mod.rs:97` 的 SessionKeys 加 `runtime_id`、`assistant_version`；`runtime.rs:42`、`:87` 写 SystemAiReview 的 TurnQueued、TurnStateChanged、UsageReported、QuotaObserved、TurnModelReported、TurnFinished。 | `system_review_records_turn_usage_quota_model_and_reason` |
| F-24 | 通过理由在状态触发和编码中丢失。`crates/iwork-domain/src/approval.rs:267`、`:335`、`:586` 保存并传递 reason；`crates/iwork-app/src/convert/model.rs:294`、`:334` 复用 proto 已有 reason，旧 Passed 缺字段时读为空串，不改 proto。 | 引擎的 `system_review_records_turn_usage_quota_model_and_reason`；`crates/iwork-app/tests/review_reason.rs` 的 `passed_and_failed_review_reasons_survive_wire_roundtrip`、`old_passed_review_without_reason_still_decodes` |
| F-25 | 审查输入始终为空上下文，恢复也不重建任务目标。`context.rs:27` 按对话中的项目线消费消息事件，助手输出只保留尾部 2000 个 Unicode 字符；在线和恢复共用同一函数。 | `review_context_is_bounded_isolated_and_replayed`、`artifact_messages_are_skipped_without_polluting_review_context`（在线与恢复各验证一次） |

## 验证

所有 Cargo 命令使用 rustup 代理与仓库固定的 Rust 1.99.1。环境默认的 Homebrew Cargo 1.83 无法解析 edition 2024，最初一次调用在解析 manifest 时失败；切换到 rustup 后验证正常。下表的 `cargo` 指 rustup 代理。

| 命令 | 已观察到的结果 |
| --- | --- |
| `cargo test -p iwork-core --test engine review_round1_b` | 18 条 B 块反例通过。 |
| `cargo test -p iwork-core --test engine dm_project_switch_out_of_auto_abandons_review_immediately` | 扩展提交失败与重试断言后，单独复跑通过。 |
| `cargo test -p iwork-core --test engine --test decision_lock --test turn_runner` | 引擎 33 条、决策锁 1 条、T05 接缝 7 条通过；这次运行早于最后新增的私聊项目切换反例，后者单独验收如上。 |
| `cargo test -p iwork-app --test review_reason` | 通过理由、不通过理由与旧 Passed 兼容：2 条通过。 |
| `scripts/check-deps.sh`（PATH 优先 rustup） | 通过。 |
| `scripts/check-domain-terms.sh` | 通过。 |
| `cargo fmt --all --check`、`git diff --check` | 通过。 |
| `lockf /tmp/iwork-heavy-test.lock cargo clippy --workspace --all-targets -- -D warnings` | 首次执行发现 8 处条件合并、多余 clone 与可变引用提示，已修正；第二次排队，尚无最终结果。 |
| `lockf /tmp/iwork-heavy-test.lock cargo test --workspace` | 排队，尚无结果。 |
| `lockf /tmp/iwork-heavy-test.lock <基准前后验证脚本>` | 排队，尚无结果。脚本用 git archive 导出 95f5ac6 到系统临时目录，以 env -i 与临时 HOME 构建；先跑 `cargo bench --locked -p iwork-bench --bench approval`，叠加本轮补丁后再跑一次，并核对原工作区状态不变。 |

重测试未绕锁。supervisor 确认 T03 的 mutations.sh 长时间持锁、机器负载很高，本轮继续排队。基准只覆盖现有的取事实、归一、求值与规则拼装，不覆盖存储延迟或审查运行时耗时；数字出来后再评价性能，不能先写成无回归。

定向测试在指定 worktree 内运行，开工 `git status --short` 为空；运行期间没有混入别人的改动。最终提交后再记一次状态。

## 偏离与风险

- **T05 的加法接缝**：`crates/iwork-core/src/turn/permission.rs:41` 加默认空实现的 `PermissionDecider::committed(events)`；`runner.rs:431` 的提交接缝只在成功回执后调用。`void_pending` 只生成计划，不提前移除等待项或回复。`ApprovalEngine::void_for_event` 与恢复返回的计划也要求 T10 / 恢复事务成功提交后调用 `ApprovalEngine::committed`。回合状态仍只由编排写，没有让审批引擎写助手回合状态。决定与理由已写入 `docs/adr/T07-c-denial-memory-and-engine-seam.md`。
- **制品消息**：正文落成 `MessageBody::Artifact` 的消息不进审查上下文；跳过后保留已有目标与摘要，不读取引用、不崩溃。T10 接制品消息时要向引擎供给有界摘要。本轮不加 ArtifactStore 泛型，按 supervisor 2026-11-10 的确认执行；ADR 同步记载，反例测试固定这个行为。
- **事件错误接线**：`on_event` 与 `recover` 现在返回 Result，事件消费者须处理错误并重试，不能静默丢掉私聊项目切换引起的审查放弃。
- **会话身份接线**：调用方须让 `SessionKeys.runtime_id` 对应传入的审查端口，给出助手配置版本；恢复查询须按原 TurnId 返回助手、项目与原快照。项目指定审查运行时时，事件使用 `ai_review.runtime_id`。
- 代码与验证仅覆盖本机；Windows 结果仍由 CI 验证。本轮不推送、不合入 main，也不自行关闭账本条目。

## 维护说明

沿用的样板是 T05 的 PermissionDecider 接缝、系统回合事件与用量字段，以及现有引擎端口假实现。规则条目与领域状态机表仍是唯一真值，没有新增审批特例或第二套规则事实。

再加一种审批规则仍只加条目；新增状态载荷由领域类型与编解码的穷举匹配提示所有消费处。修改本轮数据会牵动 SessionKeys 的构造方、T10 / 恢复的确认回调、事件编解码及审查输入；这些接线点在上文列明。新逻辑拆进 changes、context、recovery、runtime 小模块，原 engine/mod.rs 缩小。
