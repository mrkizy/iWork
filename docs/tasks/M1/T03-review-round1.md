# T03 第 1 轮审查返工回报

- 审查账：`2026-11-10-m1-t03`，第 1 轮 F-1～F-10（`agent-scaffold findings list --task 2026-11-10-m1-t03`）
- 分支：`cao/2e7bcbe5`；返工起点 `c3e8cda`，返工提交范围 `git log c3e8cda..8fcf6f0`（含两次合并 main：`34afe8c`、`4992805`）
- 写法：「前任已做」= 机器重启前上一位 developer 跑过并报告的结果，本次没有重跑；「本次」= 接手后实际跑的命令与结果
- 行号都是 `8fcf6f0` 上的

## 逐条

### F-1（blocking）`Cf::adopt` 空指针分支会 `CFRelease(NULL)`

- 根因：`(!ptr.is_null()).then_some(Cf(ptr))` 先建出 `Cf(NULL)`，条件不成立时丢弃它，`Drop` 里 `CFRelease(NULL)` 直接崩溃。
- 改动：`af95668`。`crates/iwork-ipc/src/peer/macos.rs:480` `Cf::adopt` 改成 `then(|| Cf(ptr))` 惰性构造，注释写明为什么不能 `then_some`。
- 新增测试（同文件 `tests`）：`adopt_null_is_none_and_never_releases`（:516）、`adopt_non_null_releases_exactly_once`（:523，用 `CFGetRetainCount` 核对释放恰好一次）。
- 变异：前任没有报告单独的撤销实验。测试注释说明：撤回成 `then_some` 时第一条测试会因 `CFRelease(NULL)` 崩溃。

### F-2（blocking）超前 `from_seq` 走快照后仍保留超前游标

- 根因：空流上第一次登记就把客户端声明的 `from_seq` 写进游标；续传判定失败、改走快照时，游标没有回到 `snapshot_seq`，快照之后、超前游标之前的事件被当成「已送出」吞掉。
- 改动：`15b1176`。`crates/iwork-ipc/src/server/stream.rs:162` `add_scopes` 只**返回**续传起点、不改游标；新增 `adopt_cursor`（:183），只在确定续传时由 `server/connection.rs` 调用；快照路径游标 = `snapshot_seq`。
- 新增测试：`crates/iwork-ipc/tests/stream.rs:592` `ahead_from_seq_snapshot_then_live_events_arrive`（超前游标 → 快照 → 之后的实时事件必须到）。
- 变异（前任已做）：撤销修复后这条测试变红，恢复后通过。

### F-3（blocking）写 socket 阻塞时 `fail_connection` 关不掉连接

- 根因：写任务 `write_all(...).await` 不和连接取消令牌一起等。客户端停读、socket 写不动时，`fail_connection` 发出的取消没人响应，连接一直挂着；握手阶段的写也没有期限。
- 改动：`9ef7368`，`crates/iwork-ipc/src/server/connection.rs`：
  - 写任务每次写都 `select!` 连接的 `cancel`。被取消时记 `torn = true`：流里留着半帧，之后不再写任何东西，最后一帧也不补写。`shutdown` 加 `FINAL_FRAME_WRITE` 期限。
  - 握手阶段的拒绝响应与 `Welcome` 都套 `handshake_timeout`。
- 新增测试：`crates/iwork-ipc/tests/slow_consumer.rs:289` `stalled_client_is_closed_without_resuming_reads`、`:311` `shutdown_completes_while_client_stalls`。
- 变异（前任已做）：撤销修复后变红，恢复后通过。

### F-4（blocking）增量合并与缓冲转队列都能绕过连接预算

- 根因：① 合并相邻 `MessageDelta` 时没算净增长，合并后的帧可以超出实时上限；② 交接缓冲按事件存，转进发送队列时重新编码（带上更多 `subscription_ids`），预留的长度和入队的长度不一致。
- 改动：`a0b15c8`。
  - `crates/iwork-ipc/src/server/budget.rs:221` `try_merge`：先算净增长，超过实时上限就不合并。
  - `crates/iwork-ipc/src/server/stream.rs:352` `take_buffer`、`:362` `move_from_buffer`：交接缓冲直接存完整的待发帧，预留与入队用同一个完整编码长度。
- 新增测试：`budget.rs:458` `merge_never_exceeds_live_limit`、`:480` `buffered_frame_moves_with_its_full_length`（自己编的帧与 `encode_frame` 逐字节相等）；`stream.rs:412` `buffered_events_with_many_scope_ids_stay_within_budget`。
- 变异（前任已做）：撤销修复后变红，恢复后通过。

### F-5（blocking）在飞请求和查询任务没有容量上限，停读挡不住内存

- 根因：停读只看发送预算。命令、交接 FIFO、等名额的查询都不计数，慢 handler 下可以无限堆积。
- 改动：`c41bcfd`。
  - 入站分道从 `connection.rs` 搬到新文件 `crates/iwork-ipc/src/server/lanes.rs`。
  - 新增每连接准入上限 `ServerConfig::max_pending_requests`（`server/mod.rs:56`，默认 64），统计命令、交接 FIFO 与等名额的查询；用完就停读（`lanes.rs:81` `can_admit`、`:96` `admission_freed`；`connection.rs` 读循环里 `can_read = !budget_paused && lanes.can_admit()`）。
  - 查询道先拿名额再 `spawn`（`:130` `spawn_side_query`），及时收割完成的任务（`:117` `reap`）。
- 新增测试：`crates/iwork-ipc/tests/lanes.rs:53` `pending_requests_are_bounded_under_slow_handler`、`:97` `admission_frees_as_requests_complete`；夹具补在 `tests/support/mod.rs`。
- 变异：前任没有报告单独的撤销实验。

### F-6（important）交接取消只丢 `JoinHandle`，handler 任务继续跑

- 根因：丢掉 tokio 的 `JoinHandle` 不会中止任务。交接超时或连接关闭后，handler 还在跑，还占着资源。
- 改动：`c41bcfd`（与 F-5 同一提交）。handler 任务的句柄包进 `AbortOnDrop`（`lanes.rs:278`），等它的 future 被丢掉时任务随之中止；连接关闭时 `abort_all`（`lanes.rs:130`）。
- 新增测试：`tests/lanes.rs:122` `handover_timeout_aborts_never_returning_handler`、`:139` `handover_timeout_aborts_late_handler_before_it_finishes`、`:158` `connection_close_aborts_in_flight_command_and_side_query`。
- 变异：前任没有报告单独的撤销实验。

### F-7（blocking）零载荷 `Subscribed` 不占 `Incoming` 配额，可以无限入队

- 根因：客户端 `Incoming` 按载荷字节计费，零载荷项计 0，可以无限入队。
- 改动：`417f9fe`，`crates/iwork-ipc/src/client.rs`：
  - `incoming_charge`（:227）按完整线上帧长度计费，每项至少 1，上限是整个配额。
  - 超上限的单项等 `Incoming` 清空后独占配额。
- 新增测试（`client.rs` 内）：`zero_payload_items_still_take_quota`（:722）、`charge_is_full_wire_length_at_least_one_capped_at_limit`（:736）、`oversize_item_waits_for_empty_incoming_then_takes_whole_quota`（:743）。
- 变异：前任没有报告单独的撤销实验。

### F-8（important）`Incoming` 被丢弃后，等配额的读任务不退出

- 根因：读任务停在 `acquire_many` 上；消费端丢掉 `Incoming` 后没人释放配额，也没人关信号量，读任务永远等。
- 改动：`417f9fe`（与 F-7 同一提交）。`Incoming` 的 `Drop` 关闭配额信号量（`client.rs:219`）；`IncomingTx::push` 拿配额失败返回 false，读任务退出并结束所有在飞请求；`push_closed`（:275）不占配额。
- 新增测试：`client.rs:758` `dropping_incoming_releases_a_waiting_producer`；`tests/slow_consumer.rs:333` `dropping_full_incoming_ends_in_flight_requests`、`:358` `dropping_client_and_full_incoming_leaves_no_tasks`。
- 变异：前任没有报告单独的撤销实验。

### F-9（important）属性测试从服务端真值补客户端快照，验证不了实际交付

- 根因：旧的 `stream_proptest` 客户端模型从服务端的 World 读数据补快照，等于拿答案对答案；服务端少发、错发都测不出来。
- 改动分四个提交：
  - `e3d6d5b`：服务端订阅快照交接按方案 2.8 跳过缓冲里 `seq ≤ snapshot_seq` 的事件（`crates/iwork-ipc/src/server/stream.rs:325` `insert_snapshot_at`）。这是测试重写过程中暴露的服务端缺陷，一起修了。
  - `0034388`：测试夹具发真实数据：事件载荷带操作，快照与视图查询结果带实体，实体可以多路由（`tests/support/mod.rs`）；新增测试 `tests/stream.rs:611` `snapshot_handover_skips_buffered_events_covered_by_snapshot`。
  - `d45eaea`：`Client::view_query_nowait`（`client.rs:466`），返回 `request_id`，让属性测试把结果和请求对上号。
  - `d4dd764`：`tests/stream_proptest.rs` 重写，客户端模型拆到 `tests/stream_proptest/model.rs`。`View` 只从 `Incoming` 收到的载荷写入，不持有服务端 World；服务端日志只给 `Audit` 做断言用。`Audit::check_segment`（model.rs:110）逐段核对「区间内应收的完整持久事件序列」，`View::apply` 另核对 `MessageDelta` 字节偏移连续（「增量字节偏移连续」）。
  - `8fcf6f0`：续传模型修正：写端已关闭时先读完 `Incoming` 再重连。proptest 找到的突发关闭反例种子留在 `tests/stream_proptest.proptest-regressions`。
- 测试：`stream_never_loses_or_duplicates_across_reconnects`（`tests/stream_proptest.rs:168`）。
- 变异（前任已做，`PROPTEST_CASES=2000`）：
  - `subscribed()` 清空 snapshot → 断言失败「缺少快照部分」；
  - deliver 丢掉 seq=9 → 断言失败「区间(7,10] 实收 [8,10] 期望 [8,9,10]」；
  - 两种变异都变红，还原后通过。2000 例完整一轮在 `8fcf6f0` 上通过（72.89s，含首轮找到的回归种子）。

### F-10（important）两处 `expect` 压掉 `large_enum_variant`，项目没登记例外

- 改动：`d4dd764`。去掉两处 `#[expect(clippy::large_enum_variant)]`，大变体装箱：
  - `crates/iwork-ipc/src/frame.rs:23` `Frame::Event(Box<pb::Event>)`，解码处 :154；
  - `crates/iwork-ipc/src/client.rs:195` `IncomingItem::Event(Box<pb::Event>)`；
  - 调用点同步：`server/budget.rs` 测试、`tests/stream.rs`、`tests/support/mod.rs`、`crates/iwork-bench/benches/ipc.rs`（两处模式匹配改成先取 `e.body`）。
- 验证：`grep -rn large_enum_variant crates/iwork-ipc` 无结果；clippy 四组见下。装箱在推送热路径上，所以附基准前后对比（见下）。

## 全量检查

前任已做（`8fcf6f0`，合并 main 后）：

- `cargo test --workspace`：378 passed，2 ignored
- `PROPTEST_CASES=2000 cargo test -p iwork-ipc --test stream_proptest`：通过，72.89s
- `cargo fmt --all --check`、`cargo deny check`、`scripts/check-deps.sh`（含 `--selftest`）、`scripts/check-domain-terms.sh`、`scripts/check-no-tcp.sh`（含 `--selftest`）：通过
- `cargo clippy -p iwork-ipc --all-targets -- -D warnings`：通过，没有 `expect` 压制

本次（2026-11-10，`8fcf6f0`，都套 `lockf /tmp/iwork-heavy-test.lock`）：

| 组 | 命令 | 结果 |
| --- | --- | --- |
| 1 | `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| 2 | `cargo clippy -p iwork-ipc --all-targets --features dev -- -D warnings` | 通过 |
| 3 | `cargo clippy --workspace --exclude iwork-client --all-targets --target x86_64-pc-windows-msvc -- -D warnings` | **本机做不完**：`libsqlite3-sys`（bundled，经 `iwork-store`、`iwork-daemon` 进来）交叉编 C 时找不到 Windows 的 `stdlib.h`，是本机缺 MSVC 头文件，跟代码无关。另外排除 `iwork-store`、`iwork-daemon` 后重跑：通过（含 `iwork-ipc`）。这两个 crate 的 Windows clippy 以 CI 的 windows-latest 为准 |
| 4 | `cargo clippy -p iwork-ipc --all-targets --features dev --target x86_64-pc-windows-msvc -- -D warnings` | 通过 |

## 基准前后对比（F-10 装箱、F-4 合并、F-3 写任务都在推送热路径上）

- 修改前 = `c3e8cda` 的 `git archive` 临时副本（独立 target 目录）；修改后 = `8fcf6f0`
- 命令：`cargo bench -p iwork-bench --bench ipc`（默认扇出 10 秒），整段套 `lockf /tmp/iwork-heavy-test.lock`，两个版本交替各跑 3 轮，每项取 3 轮的中位数
- 机器：macOS（Apple Silicon），重启后约 38 分钟。负载 `uptime`：开始时 1 分钟 8.85（5 分钟 24.06），结束时 7.36（17.75）。重启后杀毒软件在扫盘，负载并不低，单轮波动大（见最后一列），结论只看中位数
- 两个版本的基准代码只差 F-10 的两处模式匹配（`IncomingItem::Event` 装箱），测的东西一样

| 基准 | 修改前 P50 | 修改后 P50 | 修改前 P95 | 修改后 P95 | P95 变化 | 修改后 P95 三轮 |
| --- | --- | --- | --- | --- | --- | --- |
| connect_handshake_latency（内存传输） | 11.3µs | 13.9µs | 35.1µs | 30.1µs | -14.2% | 前 35.1µs / 36.4µs / 22.2µs；后 36.3µs / 30.1µs / 26.1µs |
| ipc_publish_fanout（1 条连接，1600/s × 10s） | 11.6µs | 10.4µs | 52.8µs | 32.5µs | -38.4% | 前 115.0µs / 45.9µs / 52.8µs；后 87.8µs / 29.4µs / 32.5µs |
| ipc_publish_fanout（4 条连接，1600/s × 10s） | 14.1µs | 12.5µs | 49.8µs | 38.3µs | -23.1% | 前 35.3µs / 50.9µs / 49.8µs；后 1.10ms / 32.3µs / 38.3µs |
| ipc_replay_10k（续传 9 999 条，每条约 200B） | 7.60ms | 6.20ms | 11.20ms | 9.20ms | -17.9% | 前 11.20ms / 8.30ms / 13.10ms；后 9.20ms / 11.50ms / 6.30ms |
| ipc_scope_change_snapshot（Subscribe→Subscribed） | 12.5µs | 11.5µs | 25.3µs | 21.1µs | -16.6% | 前 21.7µs / 25.3µs / 31.4µs；后 47.3µs / 21.1µs / 20.5µs |
| ipc_view_query_handover（查询 5ms 在飞） | 10.6µs | 11.4µs | 2.30ms | 2.30ms | +0.0% | 前 1.20ms / 2.30ms / 2.30ms；后 2.30ms / 2.30ms / 2.30ms |
| ipc_view_query_handover（查询 50ms 在飞） | 15.90ms | 16.00ms | 46.80ms | 46.80ms | +0.0% | 前 46.60ms / 47.30ms / 46.80ms；后 46.80ms / 46.80ms / 47.00ms |

结论：推送扇出（1 条、4 条连接）的 P95 没有回退，中位数反而更低，在这个负载下差异属于噪声范围，不能说改快了。其余各项 P95 持平或更低，没有超过 10% 的回退。修改后 4 条连接第 1 轮 P95 1.10ms 是单轮离群值，另两轮 32.3µs、38.3µs。

另记一件事：本次第一遍对比（负载 1 分钟 17～23）的 6 次运行里有 3 次（修改前第 2 轮、修改后第 2、3 轮）只打出前 4 项，就没有后 3 项了；那一遍丢了 stderr，看不到原因。修改前、修改后都出现过，所以不是这次返工引入的。之后直接跑修改后的二进制 3 次（`IWORK_BENCH_FANOUT_SECS=1`），重跑上表 6 次并保留 stderr，都完整输出、退出码 0，没有复现。上表用的是第二遍的数据。

### 第 2 轮 2/F-1 修复前后（高负载，待 T11 空载复测）

2/F-1 改了客户端读任务（`read_loop` 每帧多 poll 一次 `incoming.closed()`），它在推送热路径上，所以再做一次对比。

- 修改前 = `5e59dc8` 的 ipc 代码，修改后 = `0027b32`；两边都用 `38e47de` 修好的基准夹具（见下）。
- `cargo bench -p iwork-bench --bench ipc`，整段套锁，交替跑 6 轮（前 3 轮先跑修改前，后 3 轮先跑修改后），每项取 6 轮中位数，12 次退出码都是 0。
- **负载高**：`uptime` 1 分钟 17.9～28.6（5 分钟 21.6～24.5），10 核机器。主要来源是杀毒软件 kavd（约 230% CPU），还有别的卡在编译。单轮波动大，这组数据只能看方向。

| 基准 | 修改前 P50 | 修改后 P50 | 修改前 P95 | 修改后 P95 | P95 变化 | 各轮 P95（前 6 轮；后 6 轮） |
| --- | --- | --- | --- | --- | --- | --- |
| connect_handshake_latency（内存传输） | 47.2µs | 34.3µs | 233.8µs | 177.8µs | -24.0% | 前 285.0µs / 170.5µs / 241.4µs / 882.0µs / 89.0µs / 226.2µs；后 82.5µs / 449.5µs / 83.1µs / 213.5µs / 526.3µs / 142.1µs |
| ipc_publish_fanout（1 条连接，1600/s × 10s） | 52.5µs | 51.8µs | 773.2µs | 572.8µs | -25.9% | 前 1.10ms / 935.2µs / 1.00ms / 611.2µs / 528.3µs / 565.1µs；后 543.6µs / 924.5µs / 679.2µs / 530.0µs / 512.5µs / 602.0µs |
| ipc_publish_fanout（4 条连接，1600/s × 10s） | 47.0µs | 57.2µs | 505.7µs | 594.0µs | +17.5% | 前 469.3µs / 573.3µs / 490.1µs / 521.2µs / 531.7µs / 449.0µs；后 958.7µs / 766.4µs / 480.1µs / 442.7µs / 643.2µs / 544.9µs |
| ipc_replay_10k（续传 9 999 条，每条约 200B） | 9.10ms | 10.30ms | 11.70ms | 13.50ms | +15.4% | 前 11.00ms / 12.20ms / 11.20ms / 15.80ms / 25.00ms / 10.60ms；后 17.10ms / 14.90ms / 11.80ms / 12.10ms / 12.10ms / 15.20ms |
| ipc_scope_change_snapshot（Subscribe→Subscribed） | 28.3µs | 32.4µs | 283.2µs | 138.4µs | -51.1% | 前 156.7µs / 98.3µs / 409.7µs / 436.3µs / 655.9µs / 55.8µs；后 1.90ms / 193.8µs / 74.5µs / 396.8µs / 80.6µs / 83.0µs |
| ipc_view_query_handover（查询 5ms 在飞） | 46.0µs | 56.1µs | 2.30ms | 1.95ms | -15.2% | 前 2.30ms / 2.50ms / 2.30ms / 1.90ms / 1.50ms / 2.30ms；后 2.30ms / 2.20ms / 2.30ms / 1.60ms / 1.70ms / 1.30ms |
| ipc_view_query_handover（查询 50ms 在飞） | 11.75ms | 10.75ms | 46.55ms | 46.60ms | +0.1% | 前 46.80ms / 46.30ms / 46.70ms / 46.40ms / 46.40ms / 47.50ms；后 46.80ms / 46.50ms / 47.10ms / 46.60ms / 46.60ms / 46.40ms |

结论：4 条连接推送 P95 +17.5%、续传 +15%，超过 10%；但 1 条连接推送 -26%、换范围快照 -51%，方向相反，同一版本内单轮能差 2 倍。4 条连接后 4 轮修改前后基本重合（修改后 480/443/643/545µs，修改前 490/521/532/449µs），超线主要来自修改后前两轮。改动本身每帧只多一次 poll，不分配内存。按 `AGENTS.md`「性能」节，记为回归、在审查中标出、不阻止合入（supervisor 2026-11-10 决定）。**待 T11 空载复测**。

基准夹具修复（`38e47de`）：前两次对比里偶发的「只输出前 4 项」这次拿到了 stderr：`ipc_scope_change_snapshot` 里 `subscribe().unwrap()` 得到 `Closed`。根因：夹具只提交不发布，而 `serve` 在后台任务里读 head 当发布水位基线；它若先读到 0，快照 S=1000 永远等不到发布，10 秒后按 PublishStalled 关连接。在 `Bench::start` 之后插 100ms sleep 可以稳定复现；改成先提交后发布（`commit_published`，与 daemon 写线程一致）后不再出现。ipc 代码没问题。

## 未解决与风险

- F-1、F-5～F-8 前任没有报告撤销修复的变异实验，只有正向测试；测试注释与提交说明写了每条测试守的是哪个根因。
- clippy 第 3 组本机只能排除 `iwork-store`、`iwork-daemon` 跑，完整结果看 CI。
- ipc 基准第一遍有 3 次输出不完整：第 2 轮已查明是基准夹具只提交不发布，`38e47de` 已修（见基准一节）。
- 2/F-1 修复后的基准是在高负载下测的，4 条连接推送 P95 +17.5%、续传 +15% 记为回归，待 T11 空载复测。
