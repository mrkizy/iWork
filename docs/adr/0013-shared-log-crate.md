# ADR-0013 后台与客户端共用一个日志 crate（iwork-log），数据目录的定位放进去

- 状态：已采纳（2026-11-13，任务 2026-11-13-startup-audit-loss；产品负责人定方案 A）
- 依据：T03 方案 2.12（`peer_rejected`、清单审计两端都记）；T04 方案 2.14（日志格式、滚动、脱敏）；技术设计 5.1、10.3；
  `AGENTS.md`「依赖只向下」；审查账本 2026-11-13-ipc-protocol-violation-flake/1/F-1、F-2

## 背景

- 桌面客户端从启动到退出都没有装 tracing 订阅者。客户端拒绝后台时的 `peer_rejected(role=client)`、启动时的
  `manifest_loaded` / `manifest_unavailable` 全部丢失（F-1）。T03 2.12 要求两端都记，可客户端的日志该写到哪，之前没有文档定过。
- 后台的日志实现（JSON 行、按大小滚动、字段名黑名单与截断、审计 target 不被级别过滤）写在 `iwork-daemon::logging` 里，
  客户端用不到。数据目录的定位（`DataDir::locate`、`logs/` 子目录）写在 `iwork-store` 里，客户端也不能依赖它：它带着
  rusqlite，客户端的依赖策略不许。
- 客户端照抄一份实现会变成两份脱敏规则，以后一边改了另一边不知道。

## 决定

- 新建工作区 crate `iwork-log`。直接依赖只有 tracing、tracing-subscriber（开 registry）、serde_json；unix 上另有 libc，
  打开日志文件时用 `O_NOFOLLOW`，后台原来就这么做。里面放：
  - 日志的通用部分，从 `iwork-daemon::logging` 原样搬来：`JsonLayer`、黑名单与截断、`RollingFile`（文件名改成参数）、
    `QueueSink`、`filter_from`、`LogGuard`、`flush_global`、panic 钩子。新加的 `init(LogConfig, extra)` 只装一次全局订阅者：
    `always_on` 指定永远放行的 target（审计），`extra` 留给后台叠审计旁路。
  - 日志文件名 `DAEMON_LOG = "daemon.log"`、`CLIENT_LOG = "client.log"`。
  - `data_dir` 模块：`DATA_DIR_ENV`、`locate_root()`（三平台位置和环境变量覆盖，从 `iwork-store` 搬来）、`logs_dir(root)`、
    `create_private_dir`（新建的目录设 0700）。`iwork-store::DataDir` 的 `locate`、`logs` 和建目录都改成调它，路径只算一处；
    其余子目录（制品区、`run/`、`backups/`、`imports/`）还是只有 `iwork-store` 知道。
- 数据目录的定位放 `iwork-log`，不放 `iwork-ipc` 旁边，理由有三：
  - 存储层依赖 ipc 会把对端身份、协议这些和存储无关的代码带进 `iwork-store` 的依赖闭包。`iwork-log` 是叶子 crate，谁依赖它都不多带东西。
  - 客户端要数据目录，只是为了找日志目录。两样放在一起，客户端只多依赖一个 crate。
  - `iwork-ipc` 的 run 目录（socket、锁）是另一套路径表（技术设计 4、T03 2.3），和数据目录没有关系，放一起反倒让人以为是一回事。
- 后台：`iwork-daemon::logging` 只留后台自己的东西：命令、回合、审批的 span，`AuditTap` 与 `AuditSignal`，还有 `init(dir)`
  （它调 `iwork_log::init`，叠上审计旁路）。日志位置不变，仍是 `<数据目录>/logs/daemon.log`。
- 客户端：`iwork-client-core::logging::init()` 写 `<数据目录>/logs/client.log`。格式、滚动（10 MiB × 5）、脱敏和 daemon.log
  用同一份代码，`iwork.ipc.audit` 永远放行。`client/src-tauri/src/main.rs` 第一行就装日志，在建拨号器、第一次取身份和拨号之前；
  事件循环结束（`RunEvent::Exit`）时排空队列。日志目录不在就按 0700 建；数据目录定位不了、目录写不了时只写 stderr，不挡启动。
- 后台启动时的清单审计（F-2）：`SelfIdentity::detect_deferred` 把读清单的结果存成快照（`StartupAudit`），后台装好日志后
  再 `record()`。不重读清单，信任根不变，文件日志仍在拿到单实例锁之后才装。

## 已知限制

- 多个客户端实例同写一个 `client.log`：每行一次追加写，各行会交错，但不会写坏；两个实例同时滚动时可能互相改名，丢掉一个文件。
  M1 客户端不做单实例，不按 pid 分文件（产品负责人 2026-11-13 定）。
- 开发构建的客户端（`tauri dev`、`dev:fake`）没设 `IWORK_DATA_DIR` 时，日志写进开发者真实的数据目录，和开发构建的后台一样。
  真实 WebView 测试（`e2e:webview`）给每条用例设了临时的 `IWORK_DATA_DIR`。

## 后果

- 加一种要写日志的进程：在 `iwork-log` 加一个文件名常量，在那个进程里调一次 `iwork_log::init`。
- 改脱敏规则、滚动参数，后台和客户端一起生效。
- `scripts/deps-policy.toml` 加了 `[crates.iwork-log]`，并把 iwork-log 加进 iwork-store、iwork-daemon、iwork-client-core、
  iwork-client 的 `internal`。
