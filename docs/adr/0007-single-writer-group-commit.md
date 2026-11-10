# ADR-0007 存储层用单个 std 写线程做组提交，异步接口靠自写的无运行时一次性应答

- 状态：已采纳（2026-11-10，T02）
- 依据：方案 `docs/plans/T02.md` 2.4、第 12 节；技术设计 5.2；ADR-0002

## 背景

`iwork-store` 按 `scripts/deps-policy.toml` 不能依赖 tokio；端口要求 `impl Future + Send`；SQLite 只允许一个写者，写连接不跨线程共享。设计 5.2 要求一次命令一个事务、单写者、组提交，且断电不丢已确认事件。

## 决定

- 一个 std 线程独占写连接，请求经有界 `sync_channel` 进去；第一条到达后再等 2ms（最多 256 条）合成一批，一个 `BEGIN IMMEDIATE … COMMIT`，每条命令一个 `SAVEPOINT`，一次 fsync 摊给整批。回复在 `COMMIT` 返回之后才发。
- 应答用自写的 `Reply<T>`（`Arc<Mutex<Slot>> + Condvar + Waker`）：既是 `Future`，也能在普通线程里阻塞等待；发送端没送值就被丢弃时接收端得到点名的 `Io` 错误。
- `seq` 由写线程显式分配（`next_seq` 是下一个可分配的号）；单条失败且事务仍在时退回它占的号；事务已被 SQLite 自动回滚、savepoint 操作失败或 `COMMIT` 失败时整批作废，先确认回滚再从持久的 `MAX(seq)` 重新确定；回滚本身失败则写连接标为失效，之后的写一律立即报错，由 T04 整体重启。
- 读走 N 条只读连接的线程池；写队列、读队列满都返回 `Busy`（可重试）。
- macOS 默认开 `PRAGMA fullfsync`（方案 Q3 默认），`StoreConfig.fullfsync` 可关。

## 备选

- tokio `spawn_blocking` + `oneshot`：要改依赖策略，store 绑定运行时，测试必须起 runtime。
- 每条命令一个事务：fsync 次数 = 命令数，16 并发时写线程成瓶颈。
- `AUTOINCREMENT`：savepoint 回滚时序列表也回滚，但那是 SQLite 内部行为，不如自己保证并测试。

## 影响

- store 能用 `pollster` 测；换运行时只动 `reply.rs`、`writer.rs`，端口与表不变。
- 初版基准（M 系列 Mac，`cargo bench -p iwork-store --bench write_commit`）：单生产者 P95 约 9.9ms（fullfsync 开）/ 3.7ms（关）；16 并发约 1600 / 3300 条每秒，每批约 16 条。
