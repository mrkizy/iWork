# iWork 项目约定

> 给在本仓库工作的 agent 看（Claude Code 经 CLAUDE.md 的 `@AGENTS.md` 读到）。只写看代码看不出来的东西：依据在哪、约定、不能碰的线。

## 项目是什么

iWork 是本机运行的 ADE（Agent Development Environment）：用户以项目为单位，与 iWork 及自己定义的长期助手对话，由不同厂商的 Agent CLI（Claude Code、Codex、Antigravity CLI）组成团队完成开发。非商用个人工具，目标用户是重度个人开发者。

当前阶段：**M1**（单成员 + 后台服务 + 审批 + 助手私聊 + 附件 + 添加项目 + 本机额度，只接 Claude Code）。

## 依据文档（先读，按优先级）

1. `docs/requirements/iwork-requirements.md`：需求与架构基线（v1.74）。**已确认**的需求不得自行改动。
2. `docs/design/m1-tech-design.md`：M1 技术设计，写“怎么做”。与需求冲突时以需求为准。
3. `docs/design/t0-findings.md`：三家厂商 CLI 的实测事实。适配器实现以它为准，不要凭记忆或网上资料假设厂商行为。
4. `docs/tasks/M1/`：M1 任务卡（T01–T11）与未决事项清单（`README.md`），每张卡写明目标、范围、交付物、验收标准、依赖。
5. `docs/design/ui-m1.md` 与 `docs/design/ui/`：M1 界面说明与画布截图。

发现文档之间、文档与代码之间不一致，或某条设计在实现中行不通：**停下来交给 supervisor**，由 supervisor 与产品负责人确认后改文档，再改代码。不要在代码里悄悄偏离设计。

**欢迎质疑需求和方案。** 设计阶段难免有考虑不周的地方，实现时也可能找到更好的做法。发现需求或方案不合理，或者有更合适的替代方案，请提出来，经 supervisor 交给产品负责人讨论。提的时候写清三件事：现在的做法哪里有问题；建议怎么改；依据是什么（证据、代价、影响范围）。讨论出结论之前，仍按现有文档执行；产品负责人同意后，先改文档，再改代码。

## 技术栈与常用命令

- Rust stable（edition 2024），Cargo workspace；异步运行时统一 tokio。
- 客户端：Tauri v2；网页层 SolidJS + TypeScript（pnpm）。
- 协议：Protobuf，Rust 侧用 prost 生成，TypeScript 侧用 protobuf-es 生成；生成代码不手改、不入库（构建时生成）。
- 存储：SQLite（rusqlite，WAL）。版本控制读操作用 gitoxide（`gix`），写操作调本机 `git`。

工具链：Rust 版本由根目录 `rust-toolchain.toml` 固定（经 rustup 自动安装，不用系统自带或 Homebrew 的 rustc）；pnpm 版本由 `client/package.json` 的 `packageManager` 固定；脚本需要 bash 与 jq。CI 是 `.github/workflows/ci.yml`，三平台跑下面除基准外的全部检查。CI 的运行状态不登录也能查（`curl https://api.github.com/repos/mrkizy/iWork/actions/runs?branch=<分支>`），失败日志要登录 GitHub 后用 `gh run view <run-id> --log-failed` 看；Windows 行为本机验证不了，以 CI 为准，拿不到日志时交 supervisor 请产品负责人登录 `gh`，不要只凭推断改。不登录的 GitHub API 每小时只有 60 次，多个 worker 同时用 curl 轮询会很快用光（2026-11-12 实测），轮询 CI 改用 `gh run view`（产品负责人已登录）。ubuntu job 卡在「Tauri 系统依赖（Linux）」、日志是 apt 的 `Temporary failure resolving` 时是 GitHub runner 的网络问题，只重跑这个 job（`gh run rerun <run-id> --failed`），不改代码。Windows 上用 PowerShell 5.1 跑的 `.ps1` 脚本含中文时必须存成带 BOM 的 UTF-8，否则按 ANSI 读、整个脚本解析失败（T03 实测）。M1 不支持 Windows（T03）：只在 Windows CI 上冒出来的路径写法问题（`\\?\` 前缀、反斜杠、CRLF），在测试里统一写法，或把跟真实文件系统对拍的测试限定 `#[cfg(unix)]` 并注明原因；不为它改产品代码（T07 实测）。

命令（T01 建立后以此为准；改动须同步本节）：

- 全量测试：`cargo test --workspace`（含存储的崩溃注入测试 `cargo test -p iwork-store --test crash_injection`，随机 kill -9 子进程 200 轮，约 1 分钟；库建在系统临时目录）
- 静态检查：`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all --check`
- 许可与来源：`cargo deny check`（`deny.toml`）
- 依赖方向：`scripts/check-deps.sh`，真值是 `scripts/deps-policy.toml`（加 crate、放开外部依赖都改这个文件）；`scripts/check-deps.sh --selftest` 证明检查会红
- 领域层用词：`scripts/check-domain-terms.sh`（`iwork-domain` 里不许出现厂商名与 git 字样）
- 协议兼容：新加或改了 `.proto` 字段后跑 `cargo run -p iwork-proto --example update-contract -- --additive` 把新字段登记进 `proto/compat/v1/contract.binpb`；它拒绝改号、改类型、删字段；加了样本构造函数后跑 `cargo run -p iwork-proto --example dump-compat -- proto/compat/v1/samples` 写出新的金样本（只增不改，已有样本内容不同会拒绝）。规则见 `proto/compat/README.md`
- 前端：`pnpm -C client install --frozen-lockfile`、`pnpm -C client gen`（从 `proto/` 生成 TS 到 `client/src/gen/`，不入库）、`pnpm -C client typecheck`、`pnpm -C client test`（含开发服务器只绑 127.0.0.1、发布配置不含开发入口两项检查）；加了或删了素材文件（`import.meta.glob` 读的目录）后先删 `client/node_modules/.vitest-cache`，vitest 的持久缓存不会因此失效，会拿旧的文件列表跑（T09 2b 实测，CI 是干净环境不受影响）
- 客户端：Tauri 配置分三层，层清单只写在 `client/scripts/tauri-layers.ts`，包装脚本与 CI 检查共用它。开发用 `pnpm -C client tauri dev`（叠加 `tauri.dev.conf.json`）；发布构建用 `pnpm -C client tauri build`（叠加 `tauri.build.conf.json`）。tauri CLI 只支持 dev 和 build 两个子命令，且只能经这两条包装命令调用：包装脚本拒绝其他子命令（bundle、android、ios 等）、调用方自带的 `-c/--config` 和环境变量 `TAURI_CONFIG`；裸跑 `tauri build`、`tauri bundle` 会被拒；`client/src-tauri/` 下多出层清单没登记的 Tauri 配置文件时 `pnpm -C client test` 失败。`cargo build`/`cargo run -p iwork-client` 不需要 `dist/`，但 `cargo run` 起来的窗口是空的（没有开发服务器也没有嵌入资源），看界面要用 `tauri dev`。客户端只有发布构建（`pnpm -C client tauri build` 出的包）连不上时会拉起包里的后台；`dev` 构建（`--features dev`，含 `pnpm -C client tauri dev`、`dev:fake`、e2e）只连不拉：设了 `IWORK_DEV_ENDPOINT` 就连那个端点并走开发信任，没设就连当前用户的默认端点，连不上就停在诊断页（T04p，supervisor 定）。`pnpm -C client tauri build` 还会编后台、装配、核对：成品在 `<输出根>/release/dist/{unsigned,signed}/iWork.app`（Linux 是 `dist/linux/iwork/`）；设 `IWORK_SIGN_IDENTITY=<钥匙串里的证书名>`（可另给 `IWORK_SIGN_KEYCHAIN`）出整包签名的形态，`-` 和空值会被拒；`build` 不接受 `--bundles`、`--no-bundle`、`--target`、`--debug`、`--runner`、`--features` 与 `--` 之后的参数，例外是独立的 `--debug`（或 `-d`）与 `--no-bundle` 同时在的测试构建（`pnpm -C client e2e:webview` 用它）：不编后台、不装配、不出包，`--features` 照放（supervisor 2026-11-13 定）。Tauri 自己的 `release/bundle/` 是中间产物，里面没有后台
- 发布打包的配套（T04p）：出包前核对 `target/release/examples/check-install <包里放程序的目录> [--expect-leaf <叶证书 SHA-256>]`（打包命令里已经在跑）；装配 `scripts/assemble-release.sh` 只由打包命令调用，`<dist>` 里的 `.iwork-journal` 和锁归它管，别手改；真包演练 `scripts/drill-release-bundle.sh --packages <目录>` 只在专用的 macOS 账号里跑，装新版只用整包替换；测试用签名身份 `scripts/test-signing-identity.sh <目录> <证书名>` 在临时钥匙串里建自签代码签名证书，用完 `security delete-keychain`。仓库脚本和测试一律不改用户的钥匙串搜索列表（CI 在 `ci.yml` 里自己存、改、恢复）
- 性能基准：`cargo bench -p iwork-bench`（T01 建空 crate，T11 填实现）；存储写入基准初版 `cargo bench -p iwork-store --bench write_commit`（每组一行 JSON 到 stdout，macOS 上 fullfsync 开、关各一组；T11 收编后改这一条）；审批求值热路径单跑 `cargo bench -p iwork-bench --bench approval`（T07）；审批引擎（追平、启动折叠、并发放行，真实 SqliteStore）`cargo bench -p iwork-bench --bench approval_engine`（T07 方案 6.10.12）；审批引擎前后对照（747db6b 对当前树，同一时段交替多轮）`scripts/bench-approval-engine-ab.sh [轮数]`（脚本里每次 cargo bench 自己套 `lockf /tmp/iwork-heavy-test.lock`，外面不要再套，否则自己等自己；正式数在 mac120 空闲时段量，每轮前查有没有构建在跑）；ipc 热路径：`cargo bench -p iwork-bench --bench ipc`（`IWORK_BENCH_FANOUT_SECS` 调推送扇出与慢消费者对照的时长；慢消费者对照的前提不成立时只打 INVALID、不报数，`scripts/check-ipc-bench-invalid.sh` 验这一点，脚本自己套锁，外面不要再套）；回合编排流式热路径（假会话连发 10 万条正文过 `run_turn`，T06 S2）：`cargo bench -p iwork-bench --bench turn_stream`（`IWORK_BENCH_TURN_UPDATES`、`IWORK_BENCH_TURN_REPS` 调条数与轮数）；助手配置自动保存闸每轮主循环推一次（闸忙着、没变化时不重算不重发助手主页，T09 组 3）：`cargo bench -p iwork-bench --bench assistant_edits`；Claude hook 冷启动与体积：`cargo bench -p iwork-claude-hook --bench cold_start`（T06 S3，只 unix；S9 收进 `iwork-bench`）
- 客户端日志（2026-11-14，审计修复）：桌面客户端写 `<数据目录>/logs/client.log`（`iwork-log`，ADR-0013）。客户端日志字段不 Debug 整个请求、回包、意图、查询用途、错误枚举，不记后台的说明文字（`Error.message` 可能带路径）；只记种类、错误码、长度，经 `iwork-client-core` 的 `session::log_fields` 取名字。隐私回归：`cargo test -p iwork-client-core --features dev --test client_log_process`。后台的开发暂停点 `IWORK_DEV_PAUSE_AT` 多了 `endpoint`（第 1 步读完清单、装日志之前）。
- 客户端重开 1s（需求 14，T10a 组 B）只对优化构建量：`cargo build --profile perf-test -p iwork-daemon --features dev` 后跑 `cargo test --profile perf-test -p iwork-client-core --features dev --test launcher_process -- --ignored --exact reconnect_within_1s`（release 构建，套重测试锁；CI 在 release-check 跑）。调试版只验功能（`reconnect_reuses_running_daemon`）：每次建连客户端都对后台程序文件全量验签，耗时随体积涨，调试版在 macOS CI 上光这一步就超过 1s。`perf-test` 带 dev feature，产物只在 `target/perf-test/`，不许拿去打包或放进 `target/release`。
- 本机通信（T03）：
  - 不开 TCP/UDP：`scripts/check-no-tcp.sh`（源码层面；`--selftest` 证明检查会红；运行时监听检查归 T11）
  - 发布构建没有 `--dev-trust`：`scripts/check-no-dev-trust.sh`（feature 图 + release 产物 grep 含阳性对照 + 发布进程拒绝该参数；`--graph-only` 只查 feature 图，`--artifacts F...` 追加检查打包产物）
  - 安装清单：`scripts/write-install-manifest.sh <目录>`（无证书构建的打包步骤调用，写 `iwork-manifest.json`）
  - fuzz（nightly + cargo-fuzz，在 `crates/iwork-ipc` 下）：`cargo +nightly fuzz run frame_decode -- -max_total_time=60`，另两个目标 `connection_prehello`、`connection_established`
  - 续传性质测试完整一轮：`PROPTEST_CASES=2000 cargo test -p iwork-ipc --test stream_proptest`
  - 另一用户连接被拒（只 Linux，需要另一个系统用户）：见 `crates/iwork-ipc/tests/peer_other_user.rs` 文件头
  - 调试客户端（dev feature）：`cargo run -p iwork-ipc --features dev --example ipc-tool -- hello`；`-- --endpoint <sock> serve` 起一个玩具后台自测
  - 对端身份探针（dev feature，平台实测用）：`cargo run -p iwork-ipc --features dev --example peer-probe -- --help`
- 审批引擎交错性质测试完整一轮（T07 方案 6.10.13；默认 256 例随 `cargo test` 跑）：`PROPTEST_CASES=5000 lockf /tmp/iwork-heavy-test.lock cargo test -p iwork-core --test engine_interleavings --test engine_fold`；复现某个种子加 `PROPTEST_RNG_SEED=<种子>`
- 审批对拍语料：`cargo run -p iwork-core --example dump-credential-corpus` 导出需凭据命令的匹配语料（JSON），供 T06 对固定版本的 Claude Code 实测回填；内置规则集快照更新用 `IWORK_UPDATE_SNAPSHOTS=1 cargo test -p iwork-core --test builtin_rules`（改内置条目必须同时升 `RULESET_VERSION`）；成员会话永久禁用的工具名单（从登记表导出，T06 实测脚本的 `--disabled-tools` 用它）：`cargo run -p iwork-core --example dump-disabled-tools`
- 重测试排队（产品负责人 2026-11-10 定：同一时间只跑一张卡的重测试）：`cargo test --workspace`、`cargo clippy --workspace`、崩溃注入、`PROPTEST_CASES` 调大的属性测试、fuzz、`cargo bench`、release 构建、docker 里的测试，一律套 `lockf /tmp/iwork-heavy-test.lock <命令>` 跑（macOS 自带 `lockf`，拿不到锁就排队等）。例外：`pnpm -C client e2e:webview`、`scripts/bench-approval-engine-ab.sh` 和 `scripts/check-ipc-bench-invalid.sh` 在内部自己套锁，外面再套就会自己等自己、把所有卡的重测试一起卡死（2026-11-12 实测，卡了十几分钟）；单个 crate 的少量定向测试不用套。临时验证副本里跑也一样套（`e2e:webview` 跑的时候测试窗口别被挡住：macOS 会把被挡住的窗口判成不可见，WebKit 随即停掉页面计时器，`forced_fallback` 这类等页面计时的用例会超时，看着像产品出错；T09 组 3 实测）

## 架构约定

- 进程：客户端（Tauri）、后台服务（daemon，常驻单实例）、每个运行中成员一个适配器进程，下面是厂商 CLI。编排核心是 daemon 里的一层，不是进程。
- crate 与依赖方向见 `docs/design/m1-tech-design.md` 第 2 节。**依赖只向下**：`iwork-domain`、`iwork-ports` 不依赖任何框架与 I/O；`iwork-core` 不知道 SQLite、git、ACP 的存在；只有 `iwork-daemon` 同时知道所有实现。违反即 `important`。
- 后台服务只通过 AgentPort 说 ACP。各厂商差异全部收在适配器里，核心不写厂商特例。版本控制差异收在 VcsProvider 里，核心不写 git 特例。审批按命令文字识别 git 等程序的子命令（如 git push 必问、git clean 算删除）属于审批规则，可以出现在 core 的规则登记表里；版本控制操作本身的差异仍收在 VcsProvider（产品负责人 2026-11-10 确认）。
- 状态变化一律经事件：命令只返回受理结果与事件 `seq`；客户端只有一条更新路径（订阅事件）。
- 事件只追加不修改；一次命令 = 追加事件 + 更新投影 + 写 outbox + 记录 `command_id`，同一事务；单写者线程，组提交。
- 网页层只渲染：不解析协议全集、不做业务判断；Rust 侧每 16ms 合并一次 `ViewPatch` 推给网页。
- 状态机与审批求值是纯函数，必须有属性测试。

## 安全不变量（任何改动都不能破坏；违反即 blocking）

- 不修改厂商 CLI，不禁用其登录方式；登录走厂商自己的流程。例外：删除对话、删除助手时，由适配层按 iWork 记录的会话编号删除对应的运行时会话记录文件（需求 v1.78；定位规则见 T06 方案 5.5：只删名字恰好是「编号.jsonl」的普通文件，不读内容、不碰别处），不动 CLI 的程序、设置与登录。**不读取、不保存、不转发任何凭据或 token。** `claude auth status` 只取 `loggedIn`、`authMethod`、`subscriptionType`、`loginExpiresAt`，邮箱等不落盘、不进日志。
- iWork 的审批判定优先于厂商 CLI 自身的权限模式与放行规则，不能被它们放宽。另有一条单列的已知例外：技能正文里的内联命令（`!命令`）在技能加载时直接执行、不经 hook（产品负责人 2026-11-12 定 PD-5，不设 `disableSkillShellExecution`；仓库里的技能照常加载，P16，所以不可信仓库的技能也能借此执行命令，产品负责人接受）。其余例外只有一条（产品负责人 2026-11-12 定，T06 S0 实测，Claude Code 2.1.322）：PreToolUse hook 没给出决定（没被调用、正常退出却没写出决定）时，CLI 自己不问的动作（工作目录内的读，含 Read、Grep、Glob 与只读命令如 `cat`；起子代理）照 CLI 自己的规则执行，记为已知限制（技能声明的 `allowed-tools` 会把这一范围撑大，产品负责人 2026-11-12 接受为已知限制，P17）；CLI 要问的动作（其他命令、写文件、MCP、工作目录外的读）仍然不执行（不注入 hook 时已实测；hook 无输出或乱码时、子代理里、清单内只读命令出不出沙箱，待 T06 R3）；Read 读工作目录内指向目录外的文件链接也被拒，目录链接与经链接的搜索、只读命令还没测（2026-11-12 补测，T06 方案 3.4）。只读命令一项产品负责人另行确认接受（2026-11-12），最坏情况按沙箱算（待 R3 的底线格、沙箱外重试格证实；读得到就回产品负责人重定）。项目目录本身包住凭据目录或数据目录时（如把家目录加成项目），那一刻文件工具连系统底线也挡不住，产品负责人定记为已知限制、不在添加项目时拒绝（2026-11-12，T06 方案 P12）。配套三条写死：hook 失败一律拦（`onFailure: "block"`）、权限模式用 `dontAsk`、hook 程序只有写出完整决定才以 0 退出（T06 方案 3.4）。
- 成员会话与用户自己的厂商 CLI 全局配置隔离（产品负责人 2026-11-12 定 V3 选 b：以后改为每家一个 iWork 专用配置目录，用户在里面装定制、由 CLI 自己加载；用户亲手写进专用目录的设置本就是用户的决定，iWork 不拦，只保证成员写不进这个目录；方案定稿并实施之前，下面这条照旧）：不运行用户的 hook、不沿用用户的权限模式、不加载未经导入的 MCP、技能与插件（需求 8.4、FR20）。仓库内的 `.claude/settings.json`、`settings.local.json` 也一律不加载（v1.63），不得为了让仓库自带 hook 生效而放开。Claude Code 的自动记忆一律关闭（`CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`，实测不关会读进用户自己的记忆）。M1 里 Claude Code 的后台任务也关闭（`CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`，Monitor 在永久禁用名单里；产品负责人 2026-11-12 定 D5：方向是尽量保留厂商原本的做法，M2 和多成员并行一起设计后开放）。成员会话的环境变量原样继承后台的环境（产品负责人 2026-11-12 定，T06 方案 P14）：从终端或 Claude Code 里直接运行 iWork、或把带定制的变量设成全局时，用户的插件等定制会经环境带进会话，记为已知限制；会话里出现非内置、非 iWork 给的插件时停会话并停用这个运行时（T06 方案 3.1b）。
- Claude Code 的 PreToolUse hook：连不上桥接一律 deny（失败即关闭）；**绝不让 hook 自然超时**（实测超时即放行），桥接必须在到时前主动中断回合并以拒绝返回。
- 系统底线写死、不可配置、不可提权（唯一例外：用户在 iWork 专用配置目录里自己写的沙箱排除命令与额外可读写路径，视同用户的决定，产品负责人 2026-11-12 接受，包括恶意仓库的技能 hook 或内联命令借此持久化写入的风险；见需求 8.4）：读写用户凭据目录、读写 iWork 数据目录（只读区 `attachments/`、`imports/` 可读不可写；`artifacts/` 不可读）、写 `.git/` 内部（只拦直接写文件，git 命令照常）、删除关键路径、非流程产生的受保护分支变更（这一条是 M1 已知缺口，M1 不实现监控，见技术设计第 1 节，审查时不按缺失报 blocking）；由审批与沙箱两道闸同时执行，三档相同；需要凭据的程序走“沙箱外执行”。凭据目录与关键路径清单见需求第 9 章。安全设计原则（需求 v1.50、v1.69）：首版尽量完善且合理，只覆盖常见场景；不为堵一个缺口引入一串新机制；新增安全规则不得改动已有规则与求值流程；遇到安全缺口先找现有机制能否覆盖，但也不能硬套现有机制（合理且正确地利用现有机制），覆盖不了就记为已知限制，交产品负责人决定，不要自行加新组件。底线拦的是 Agent，不是用户：需要改动底线范围内的内容时生成“亲手执行”卡片（精确命令或补丁 + 理由），由用户点“执行”（iWork 以用户身份执行，审计记为用户）或复制自行执行；卡片不可记住，全开档也出现，不得做成 Agent 可以自己批准的通道；执行后 Agent 只得知是否执行与退出码，输出只给用户。判定在编排核心，用户确认在应用服务，执行与密钥存取在基础设施（`iwork-host`），适配层只转发不判断。用户附加的凭据文件提示后可附加，私钥一律拒绝。
- 用户拒绝过的动作（按“动作类型 + 目标”识别，与工具无关），在本对话内、直到用户发出下一条消息之前不得以任何方式绕过：跨回合、中断后续接、换成员或运行时都自动拒绝（需求 AC05）。
- agy 的权限请求带 `rawInput.BypassSandbox: true` 时，按“越界”判定（联网不受控后，如何区分由联网引起的 BypassSandbox 留待 M2 接入 agy 时实测）。
- 本机通信只走 Unix domain socket / Windows 命名管道，建连先校验对端同用户、同签名；不开 TCP 端口。hook 与 Claude 桥接之间另加一次性令牌与对端进程号校验（M1 技术设计 6.3）。「不开 TCP 端口」管的是 iWork 自己的进程（后台服务、客户端、桥接、hook）在任何构建（含调试构建与 `--dev-trust`）里的通信；开发期前端工具链（`tauri dev` 时的 Vite 开发服务器）不在此列，但必须只绑 `127.0.0.1`、固定端口、限制可读的文件范围，且发布构建的配置里不得出现开发服务器地址（CI 检查）（产品负责人 2026-11-09 确认）。

## 性能

阈值见需求文档第 14 章，只统计 iWork 自身进程。基准进 CI，超阈值记为回归并在审查中标出，不阻止合入。动到热路径（事件推送、帧合并、存储写入、hook）必须附基准前后对比。本机负载高时（loadavg 明显高于核数，或杀毒软件在扫编译产物）基准绝对值不可信：前后两个版本在同一时段交替跑多轮、取中位数，只报相对值并写明负载。正式性能验收在 mac120 的夜间空闲时段测（见 `docs/tasks/M1/T11.md`）。

改了存储或审批引擎「接受什么」（提交前提、状态转换表、事件校验）的，要把用到这些写入路径的基准真跑一遍（缩小负载的冒烟就行）：`cargo test` 不跑基准的函数体，基准布景走的老捷径会被新校验悄悄拒掉（T10a 1b 实测，approval_engine 因此起不来）。冒烟命令里的规模上限要核一遍，别把改动到的组过滤掉（如 approval_engine 的 `IWORK_BENCH_FOLD_MAX` 小于 10000 时折叠组一档都不跑）。

## 不要擅自推翻的决定

以下都已由产品负责人确认（详见需求文档附录 B 决策记录），实现中有异议请交给 supervisor，不要自行改：

- 联网不受控：不询问、不设域名清单，沙箱放开网络（需求 v1.38）。
- 能力与界面分离（需求 7.2，v1.66）：所有用户能力都以应用服务的命令与查询提供，业务规则与校验不放在界面层；界面待定的能力（如安全清单）照常实现，经命令验收。
- 权限四概念：系统底线 / 能力边界 / 动作审批 / 流程确认点；不设能力档位；审批分手动 / 自动 / 全开三档（全开只在当前对话生效、不可设为默认），项目默认手动，成员单独设置优先；**沙箱底线部分三档相同**（只写白名单在全开档放开，全开档不提供沙箱外重跑），档位只决定问不问；越出沙箱只有“沙箱外执行”一个出口；判定结果固定五种（放行、询问、拒绝、亲手执行、沙箱外执行），不得新增；安全规则以带 `id` 的规则条目实现（需求 v1.52）。
- 接入选择规则：原生 ACP 优先；无原生 ACP 的由 iWork 自写适配器驱动用户本机 CLI；第三方翻译适配器只作可选。M1 的 Claude Code 用自写适配器（`-p` + stream-json + hook）。
- 首版厂商：Claude Code、Codex、Antigravity CLI（agy）。Gemini CLI 已停止账号登录，不再支持。
- M1 成员直接写用户的工作副本，不由 iWork 提交或合入。
- 被助手工具配置禁用的工具不能在审批卡片上临时启用，要用须到助手配置中启用。
- 界面上不出现“主管”二字（对用户叫 iWork），角色在界面上统称“助手”。

## 语言与风格

- 文档、任务汇报用中文；git 提交信息（标题与正文）用英文（产品负责人 2026-11-11 定）；代码标识符、日志字段、协议字段用英文。
- 错误不得静默吞掉；对外错误统一 `{code, message, retryable, details}`，错误码定义在 Protobuf 里。
