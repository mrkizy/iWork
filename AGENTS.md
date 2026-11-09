# iWork 项目约定

> 给在本仓库工作的 agent 看（Claude Code 经 CLAUDE.md 的 `@AGENTS.md` 读到）。只写看代码看不出来的东西：依据在哪、约定、不能碰的线。

## 项目是什么

iWork 是本机运行的 ADE（Agent Development Environment）：用户以项目为单位，与 iWork 及自己定义的长期助手对话，由不同厂商的 Agent CLI（Claude Code、Codex、Antigravity CLI）组成团队完成开发。非商用个人工具，目标用户是重度个人开发者。

当前阶段：**M1**（单成员 + 后台服务 + 审批 + 助手私聊 + 附件 + 添加项目 + 本机额度，只接 Claude Code）。

## 依据文档（先读，按优先级）

1. `docs/requirements/iwork-requirements.md`：需求与架构基线（v1.71）。**已确认**的需求不得自行改动。
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

工具链：Rust 版本由根目录 `rust-toolchain.toml` 固定（经 rustup 自动安装，不用系统自带或 Homebrew 的 rustc）；pnpm 版本由 `client/package.json` 的 `packageManager` 固定；脚本需要 bash 与 jq。CI 是 `.github/workflows/ci.yml`，三平台跑下面除基准外的全部检查。

命令（T01 建立后以此为准；改动须同步本节）：

- 全量测试：`cargo test --workspace`
- 静态检查：`cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all --check`
- 许可与来源：`cargo deny check`（`deny.toml`）
- 依赖方向：`scripts/check-deps.sh`，真值是 `scripts/deps-policy.toml`（加 crate、放开外部依赖都改这个文件）；`scripts/check-deps.sh --selftest` 证明检查会红
- 领域层用词：`scripts/check-domain-terms.sh`（`iwork-domain` 里不许出现厂商名与 git 字样）
- 协议兼容：新加或改了 `.proto` 字段后跑 `cargo run -p iwork-proto --example update-contract -- --additive` 把新字段登记进 `proto/compat/v1/contract.binpb`；它拒绝改号、改类型、删字段；加了样本构造函数后跑 `cargo run -p iwork-proto --example dump-compat -- proto/compat/v1/samples` 写出新的金样本（只增不改，已有样本内容不同会拒绝）。规则见 `proto/compat/README.md`
- 前端：`pnpm -C client install --frozen-lockfile`、`pnpm -C client gen`（从 `proto/` 生成 TS 到 `client/src/gen/`，不入库）、`pnpm -C client typecheck`、`pnpm -C client test`（含开发服务器只绑 127.0.0.1、发布配置不含开发入口两项检查）
- 客户端：Tauri 配置分三层，层清单只写在 `client/scripts/tauri-layers.ts`，包装脚本与 CI 检查共用它。开发用 `pnpm -C client tauri dev`（叠加 `tauri.dev.conf.json`）；发布构建用 `pnpm -C client tauri build`（叠加 `tauri.build.conf.json`）。tauri CLI 只支持 dev 和 build 两个子命令，且只能经这两条包装命令调用：包装脚本拒绝其他子命令（bundle、android、ios 等）、调用方自带的 `-c/--config` 和环境变量 `TAURI_CONFIG`；裸跑 `tauri build`、`tauri bundle` 会被拒；`client/src-tauri/` 下多出层清单没登记的 Tauri 配置文件时 `pnpm -C client test` 失败。`cargo build`/`cargo run -p iwork-client` 不需要 `dist/`，但 `cargo run` 起来的窗口是空的（没有开发服务器也没有嵌入资源），看界面要用 `tauri dev`
- 性能基准：`cargo bench -p iwork-bench`（T01 建空 crate，T11 填实现）；ipc 热路径：`cargo bench -p iwork-bench --bench ipc`（`IWORK_BENCH_FANOUT_SECS` 调推送扇出时长）
- 本机通信（T03）：
  - 不开 TCP/UDP：`scripts/check-no-tcp.sh`（源码层面；`--selftest` 证明检查会红；运行时监听检查归 T11）
  - 发布构建没有 `--dev-trust`：`scripts/check-no-dev-trust.sh`（feature 图 + release 产物 grep 含阳性对照 + 发布进程拒绝该参数；`--graph-only` 只查 feature 图，`--artifacts F...` 追加检查打包产物）
  - 安装清单：`scripts/write-install-manifest.sh <目录>`（无证书构建的打包步骤调用，写 `iwork-manifest.json`）
  - fuzz（nightly + cargo-fuzz，在 `crates/iwork-ipc` 下）：`cargo +nightly fuzz run frame_decode -- -max_total_time=60`，另两个目标 `connection_prehello`、`connection_established`
  - 续传性质测试完整一轮：`PROPTEST_CASES=2000 cargo test -p iwork-ipc --test stream_proptest`
  - 另一用户连接被拒（只 Linux，需要另一个系统用户）：见 `crates/iwork-ipc/tests/peer_other_user.rs` 文件头
  - 调试客户端（dev feature）：`cargo run -p iwork-ipc --features dev --example ipc-tool -- hello`；`-- --endpoint <sock> serve` 起一个玩具后台自测
  - 对端身份探针（dev feature，平台实测用）：`cargo run -p iwork-ipc --features dev --example peer-probe -- --help`

## 架构约定

- 进程：客户端（Tauri）、后台服务（daemon，常驻单实例）、每个运行中成员一个适配器进程，下面是厂商 CLI。编排核心是 daemon 里的一层，不是进程。
- crate 与依赖方向见 `docs/design/m1-tech-design.md` 第 2 节。**依赖只向下**：`iwork-domain`、`iwork-ports` 不依赖任何框架与 I/O；`iwork-core` 不知道 SQLite、git、ACP 的存在；只有 `iwork-daemon` 同时知道所有实现。违反即 `important`。
- 后台服务只通过 AgentPort 说 ACP。各厂商差异全部收在适配器里，核心不写厂商特例。版本控制差异收在 VcsProvider 里，核心不写 git 特例。
- 状态变化一律经事件：命令只返回受理结果与事件 `seq`；客户端只有一条更新路径（订阅事件）。
- 事件只追加不修改；一次命令 = 追加事件 + 更新投影 + 写 outbox + 记录 `command_id`，同一事务；单写者线程，组提交。
- 网页层只渲染：不解析协议全集、不做业务判断；Rust 侧每 16ms 合并一次 `ViewPatch` 推给网页。
- 状态机与审批求值是纯函数，必须有属性测试。

## 安全不变量（任何改动都不能破坏；违反即 blocking）

- 不修改厂商 CLI，不禁用其登录方式；登录走厂商自己的流程。**不读取、不保存、不转发任何凭据或 token。** `claude auth status` 只取 `loggedIn`、`authMethod`、`subscriptionType`、`loginExpiresAt`，邮箱等不落盘、不进日志。
- iWork 的审批判定优先于厂商 CLI 自身的权限模式与放行规则，不能被它们放宽。
- 成员会话与用户自己的厂商 CLI 全局配置隔离：不运行用户的 hook、不沿用用户的权限模式、不加载未经导入的 MCP、技能与插件（需求 8.4、FR20）。仓库内的 `.claude/settings.json`、`settings.local.json` 也一律不加载（v1.63），不得为了让仓库自带 hook 生效而放开。
- Claude Code 的 PreToolUse hook：连不上桥接一律 deny（失败即关闭）；**绝不让 hook 自然超时**（实测超时即放行），桥接必须在到时前主动中断回合并以拒绝返回。
- 系统底线写死、不可配置、不可提权：读写用户凭据目录、读写 iWork 数据目录（只读区 `attachments/`、`imports/` 可读不可写；`artifacts/` 不可读）、写 `.git/` 内部（只拦直接写文件，git 命令照常）、删除关键路径、非流程产生的受保护分支变更（这一条是 M1 已知缺口，M1 不实现监控，见技术设计第 1 节，审查时不按缺失报 blocking）；由审批与沙箱两道闸同时执行，三档相同；需要凭据的程序走“沙箱外执行”。凭据目录与关键路径清单见需求第 9 章。安全设计原则（需求 v1.50、v1.69）：首版尽量完善且合理，只覆盖常见场景；不为堵一个缺口引入一串新机制；新增安全规则不得改动已有规则与求值流程；遇到安全缺口先找现有机制能否覆盖，但也不能硬套现有机制（合理且正确地利用现有机制），覆盖不了就记为已知限制，交产品负责人决定，不要自行加新组件。底线拦的是 Agent，不是用户：需要改动底线范围内的内容时生成“亲手执行”卡片（精确命令或补丁 + 理由），由用户点“执行”（iWork 以用户身份执行，审计记为用户）或复制自行执行；卡片不可记住，全开档也出现，不得做成 Agent 可以自己批准的通道；执行后 Agent 只得知是否执行与退出码，输出只给用户。判定在编排核心，用户确认在应用服务，执行与密钥存取在基础设施（`iwork-host`），适配层只转发不判断。用户附加的凭据文件提示后可附加，私钥一律拒绝。
- 用户拒绝过的动作（按“动作类型 + 目标”识别，与工具无关），在本对话内、直到用户发出下一条消息之前不得以任何方式绕过：跨回合、中断后续接、换成员或运行时都自动拒绝（需求 AC05）。
- agy 的权限请求带 `rawInput.BypassSandbox: true` 时，按“越界”判定（联网不受控后，如何区分由联网引起的 BypassSandbox 留待 M2 接入 agy 时实测）。
- 本机通信只走 Unix domain socket / Windows 命名管道，建连先校验对端同用户、同签名；不开 TCP 端口。hook 与 Claude 桥接之间另加一次性令牌与对端进程号校验（M1 技术设计 6.3）。「不开 TCP 端口」管的是 iWork 自己的进程（后台服务、客户端、桥接、hook）在任何构建（含调试构建与 `--dev-trust`）里的通信；开发期前端工具链（`tauri dev` 时的 Vite 开发服务器）不在此列，但必须只绑 `127.0.0.1`、固定端口、限制可读的文件范围，且发布构建的配置里不得出现开发服务器地址（CI 检查）（产品负责人 2026-11-09 确认）。

## 性能

阈值见需求文档第 14 章，只统计 iWork 自身进程。基准进 CI，超阈值记为回归并在审查中标出，不阻止合入。动到热路径（事件推送、帧合并、存储写入、hook）必须附基准前后对比。

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

- 文档、提交说明、任务汇报用中文；代码标识符、日志字段、协议字段用英文。
- 错误不得静默吞掉；对外错误统一 `{code, message, retryable, details}`，错误码定义在 Protobuf 里。
