# ADR-0002 端口用原生 async（RPITIT + Send）与静态分发

- 状态：已采纳（2026-11-09，T01）
- 依据：方案 `docs/plans/T01.md` 5.1；技术设计第 2 节「ports 不依赖任何框架」

## 背景

`iwork-ports` 不得依赖框架与 I/O 库（含 tokio），也不想引入宏 crate；后台服务跑在 tokio 多线程运行时上，端口返回的 future 必须是 `Send` 才能 `spawn`。原生 `async fn` 写在 trait 里时，调用方无法要求返回的 future 是 `Send`。

## 决定

- 异步方法统一写成 `fn f(&self, ..) -> impl Future<Output = T> + Send`（RPITIT），所有端口 trait 都是 `Send + Sync`。同步端口（`Clock`、`IdGenerator`）是普通方法。
- `iwork-core`、`iwork-app` 对端口类型泛型；`iwork-daemon` 组合时落实具体类型；测试用假实现。

## 备选

- `async-trait`：可以 `dyn`，但引入宏 crate，且每次调用一次堆分配。
- `trait_variant`：同样是宏 crate。
- 只写 `async fn`：future 不保证 `Send`，daemon 无法 `spawn`。

## 影响

- 端口不能当 `dyn` 用。确实需要动态分发时（如 M2 多运行时同时在线），在 daemon 加一层 `Boxed` 适配，不改 `iwork-ports`。
- `iwork-ports/tests/fakes.rs` 给每个端口一个假实现和编译期 `Send` 断言，证明按签名写得出实现。
