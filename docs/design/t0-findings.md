# T0 实测结论（2026-11-07）

本文汇总 M1 开工前对三家厂商 CLI 的全部实测，是适配器实现的事实依据。测试脚本在 `tools/t0/`，原始结果包由产品负责人保存。凡与本文冲突的假设，以本文为准；厂商升级后行为变化，须重跑对应脚本并更新本文。

实测分两轮：快速版与完整版（完整版含 12 分钟审批长等待，2026-11-07 晚完成）。

测试环境：macOS（arm64），Claude Code 2.1.319，Codex CLI 0.162.1，Antigravity CLI（agy）1.5.2，Node 26.5，Python 3.13。

## 1 结论速览

| 厂商 | 首版接法 | 审批交给 iWork | 边界（沙箱） | 额度 / 用量 | 登录检测 |
| --- | --- | --- | --- | --- | --- |
| Claude Code | iWork 自写适配器：`claude -p --input-format stream-json --output-format stream-json --verbose` + 经 `--settings` 注入的 PreToolUse hook | 可以，用户的 auto 模式与放行规则绕不过 hook | hook 返回 `updatedInput` 改写命令，把 Bash 命令包进 iWork 沙箱执行器 | stream-json 的 `rate_limit_event`（窗口类型、使用率、重置时间）；`result` 中的用量 | `claude auth status`（JSON） |
| Codex | iWork 自写适配器：本机 `codex app-server`（JSON-RPC over stdio） | 可以，`approvalPolicy: "untrusted"` 时每个动作都发审批请求 | Codex 自带沙箱（`sandbox: "workspace-write"`），也支持 `externalSandbox` | `account/rateLimits/read`、`account/rateLimits/updated`、`thread/tokenUsage/updated` | `codex login status` |
| Antigravity CLI（agy） | 原生 ACP：`agy acp --sandbox` | 可以，每个动作（含 echo）都发 `session/request_permission` | agy 沙箱挡住联网与工作区外写入；越界时带 `BypassSandbox: true` 再请求一次 | 每轮上报 token 用量；无额度窗口 | 待查（`agy acp --login` 可登录） |

## 2 Claude Code（M1 唯一运行时）

### 2.1 已验证

| 项 | 结论 |
| --- | --- |
| hook 不可绕过 | 用户全局权限模式为 auto（会自动放行）时，注入的 PreToolUse hook 仍被调用；返回 deny 后命令未执行，并计入 `result.permission_denials`。 |
| hook 改写命令 | hook 返回 `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"<新命令>"}}}` 后，实际执行的是新命令。 |
| 长等待 | hook 设 `"timeout": 7200` 时等待 12 分钟未被打断；不设 timeout 时等待 90 秒未被打断。 |
| **hook 超时 = 放行（安全缺陷）** | hook 设 `"timeout": 5`、30 秒后才回 deny：命令在超时后**被执行了**。完整版复测结果相同。 |
| 额度 | stream-json 输出 `rate_limit_event`：`rateLimitType`、`utilization`、`resetsAt`、`status`、`unifiedWindows`（本次只出现 seven_day）。只在回合运行时上报。 |
| 登录 | `claude auth status` 返回 JSON：`loggedIn`、`authMethod`、`subscriptionType`、`loginExpiresAt` 等，退出码 0。返回里有邮箱，**不得保存**。 |
| 用户配置渗入 | 用户自己的 hook（如 SessionStart）会在 iWork 会话中运行；未信任目录的项目放行规则会被忽略。 |

### 2.2 实现约束（由上表推出）

1. **绝不让 hook 自然超时。** hook 的 `timeout` 设为允许的最大值；桥接为每个待审批请求记时，在到时前主动中断回合并以拒绝返回；用户批准后用 `--resume <session_id>` 续接。为此写一条故障注入测试。
2. hook 程序连不上桥接时一律返回 deny（失败即关闭）。
3. hook 程序用 Rust 写成小体积单文件，启动开销计入性能基准。
4. 每个成员会话一个常驻 `claude -p` 进程，经 stream-json 输入多轮；进程退出后用 `--resume` 续接。
5. 只取 `claude auth status` 中的 `loggedIn`、`authMethod`、`subscriptionType`、`loginExpiresAt`。
6. 用户配置会渗入（上表），需求要求成员会话与用户全局配置隔离（8.4）。实测结论见 2.2a：采用参数屏蔽。

### 2.2a 配置隔离实测（T0f，`tools/t0/iwork-t0f.py`，2026-11-08）

| 变体 | 结果 |
| --- | --- |
| V0 不加参数 | 加载用户的 SessionStart hook、`permissionMode=auto`、4 个 MCP（含 claude.ai 连接器）、16 个插件、113 个技能、全局 CLAUDE.md |
| V1 `--setting-sources project,local --strict-mcp-config --mcp-config <空> --settings <iWork>` | 用户 hook 未运行，只运行 iWork 注入的 hook；`permissionMode=default`；MCP 0 个（claude.ai 连接器也不加载）；用户插件与技能都不加载；全局 CLAUDE.md 不进入上下文；**沿用原登录，正常回复** |
| V2 = V1 + `--mcp-config <测试 MCP>` + `--plugin-dir <测试插件>` | 测试 MCP（source=dynamic）、测试插件、插件内的技能都出现在本会话 |
| V3 空的 `CLAUDE_CONFIG_DIR` | `loggedIn=false`，回复 “Not logged in”：**专用配置目录需要重新登录** |

结论：M1 采用参数屏蔽（M1 技术设计 9.5），不用专用配置目录。

仍会出现的内容：Claude Code 自带的 4 个内置插件（`cc-plugin-*@builtin`）与约 19 个内置技能（如 `deep-research`、`schedule`、`loop`、`batch`、`code-review`），在空配置目录里同样存在，属于 CLI 本身而非用户配置。`--disable-slash-commands` 可一并关闭全部技能与命令（含 iWork 导入的），粒度较粗。已决定 M1 保留内置技能，作为已知缺口在 M2 评估（M1 技术设计第 1 节）。

`--setting-sources project,local` 仍会加载仓库内的 `.claude/settings.json` 与本地设置（含其中的 hook）；v1.44 曾决定照常加载，v1.63 改为一律不加载（排除写法见 O1 第 ③ 项）。

### 2.3 不采用的接法：claude-agent-acp

`@agentclientprotocol/claude-agent-acp` 0.101.0 实测：新会话**沿用用户的 auto 模式**，此时不发任何权限请求，客户端选“拒绝”无效；强制 `session/set_mode` 为 `default`（Manual）后，写文件与联网会请求且拒绝生效，但 echo 等被它视为只读的命令不请求；它不使用客户端终端与客户端文件读写，iWork 无法把命令包进沙箱。保留为可选接法，不用于 M1。

## 3 Codex（M2 接入，协议已核对）

### 3.1 已验证

| 项 | 结论 |
| --- | --- |
| 版本 | 用本机 Codex CLI（0.162.1）；`codex app-server` 默认 stdio，换行分隔 JSON-RPC（消息可省略 `jsonrpc` 字段）。 |
| 协议定义 | `codex app-server generate-json-schema --out <dir>` 与 `generate-ts --out <dir>` 生成完整定义。接口标为 experimental，**按版本生成并做兼容测试**。 |
| 握手 | `initialize {clientInfo:{name,title,version}}` → 结果含 `userAgent`、`codexHome`；随后发通知 `initialized`。 |
| 建会话 | `thread/start {cwd, approvalPolicy:"untrusted", approvalsReviewer:"user", sandbox:"workspace-write"}` → `result.thread.id`。 |
| 回合 | `turn/start {threadId, input:[{type:"text", text}]}`；结束以通知 `turn/completed` 为准。 |
| 审批 | 执行命令、写文件、联网都发服务端请求 `item/commandExecution/requestApproval`；回 `{"decision":"decline"}` 后未执行，回 `"accept"` 后执行。决定可选 `accept`、`acceptForSession`、`decline`、`cancel` 等。另有 `item/fileChange/requestApproval`、`item/permissions/requestApproval`。 |
| AI 审查 | `approvalsReviewer: "auto_review"` 由 Codex 子代理先审（对应 iWork 自动模式的 AI 审查）。 |
| 额度 | `account/rateLimits/read` 返回 `primary.usedPercent`（实测 38）、`windowDurationMins`（10080，即 7 天）、`resetsAt`；另有通知 `account/rateLimits/updated`。 |
| 用量 | 通知 `thread/tokenUsage/updated`。 |
| 登录 | `codex login status` 输出 “Logged in using ChatGPT”。 |
| 长等待 | 审批请求挂起 12 分钟（726 秒）后回 `decline`：未超时，命令未执行。 |
| 写文件审批 | 写文件时先后发 `item/commandExecution/requestApproval` 与 `item/fileChange/requestApproval`，拒绝后文件不存在。 |
| 额度推送 | 回合运行中持续推送 `account/rateLimits/updated`（5 个回合共 12 次），可直接用于额度显示。 |
| 用户配置渗入 | app-server 会运行用户自己的 Codex hook（`~/.codex/hooks.json`，通知 `hook/started` 的 `source: "user"`），并启动用户配置的 MCP 服务器。与 Claude Code 相同，iWork 的审批必须在其之上且不可被放宽。 |

### 3.2 不采用的接法：codex-acp

- `@zed-industries/codex-acp` 0.16.0 内置 0.98.0 的旧 Codex 内核，读不懂新模型元数据（`unknown variant max`），默认模型与 gpt-5.5 都报 “not supported when using Codex with a ChatGPT account”。淘汰。
- `@agentclientprotocol/codex-acp` 2.5.0 可用新模型，但默认 “Auto review” 模式不发权限请求，联网被其沙箱静默拦下且不询问；也不使用客户端终端。仅作可选。

## 4 Antigravity CLI（agy，M2 接入）

| 项 | 结论 |
| --- | --- |
| 启动 | `agy acp`（子命令，不是参数）；常用参数 `--sandbox`、`--add-dir`、`--disable-telemetry`、`--process-per-session`。 |
| 会话 | `session/new` 返回 `models`（如 gemini-3.8-flash、claude-sonnet-4-6 等）、`modes`（`default`、`accept-edits`、`plan`），默认 `default`（每次询问）。 |
| 审批 | 每个动作（含 echo、写文件、联网）都发 `session/request_permission`，选项为 `allow`（allow_once）与 `deny`（reject_once）；拒绝后未执行。 |
| 沙箱 | `--sandbox` 下联网无输出、写工作区外报 `Operation not permitted`；agy 随后带 `rawInput.BypassSandbox: true` 再发一次权限请求，批准后在沙箱外执行。**iWork 必须把带 `BypassSandbox` 的请求按“越界”判定。** |
| 写文件 | 用自己的 `write_file` 工具，`tool_call_update` 带 diff。 |
| 客户端能力 | 不使用客户端终端与客户端文件读写。 |
| 用量 | 每轮 `result.usage` 上报 token；无额度窗口。 |
| 长等待 | 审批请求挂起 12 分钟（720 秒）后拒绝：未超时，命令未执行，回合以 `end_turn` 结束。 |
| 越界后批准 | 联网与写工作区外：第一次在沙箱内执行（联网无输出），随后带 `BypassSandbox: true` 再请求；批准后在沙箱外执行成功（联网返回 HTTP 200、工作区外文件被创建）。证实“批准越界 = 真正出边界”，必须经 iWork 的越界判定。 |

## 4a 联网不受控带来的待测项（M2 接入前）

需求 v1.38 起联网不受控，沙箱须放开网络。Codex 需在 `workspace-write` 下开启网络访问；agy 的 `--sandbox` 实测会挡住联网并转为 `BypassSandbox` 请求，需查明能否只放开网络。若不能，则无法从 `BypassSandbox` 请求区分“联网”与“写工作区外”，须另定处理方式。

## 5 共同结论

1. 四个被测的 ACP 适配器都**不使用**客户端终端与客户端文件读写，命令都在它们自己的进程里执行。经原生 ACP 接入的厂商，边界只能靠启用厂商自带沙箱；“由 iWork 把命令包进沙箱”只在自写适配器中可行（Claude 的 hook 改写；Codex 的 `externalSandbox`）。
2. 适配器版本与 CLI 版本可能不一致（codex-acp 内置旧内核）。运行时登记须同时记录 CLI 版本、适配器版本、协议版本。
3. 不依赖适配器的默认模式与默认模型：建会话后由 iWork 显式设置。
4. 厂商 CLI 会读取用户自己的配置（hook、权限模式、技能），iWork 的判定必须在其之上且不可被其放宽。

## 6 T03 本机通信实测（2026-11-10）

为 `iwork-ipc` 的对端校验（T03 方案 2.4b、第 8 节 N1–N3）补的实测：每个平台要证明「核验到的是对端**正在运行的映像**，不是路径上的文件」，证明不了的平台全部分支返回 `Unavailable`、M1 暂不支持（方案 Q1）。脚本在 `tools/t0/iwork-t03-*`，原始输出在 `tools/t0/out/t03-*`（家目录与临时目录已脱敏）。

### 6.1 结论速览

| 平台 | 映像绑定 | 取法 | M1 |
| --- | --- | --- | --- |
| Linux | 成立（N3） | `SO_PEERCRED` uid/pid；打开 `/proc/<pid>/exe` 得 fd，从 fd 读全文 SHA-256；路径从同一个 fd 反查 | 支持 |
| macOS | 成立（N1） | `LOCAL_PEERTOKEN` 审计令牌 → 动态 `SecCode` → 读签名信息 → 把读到的身份钉成 requirement → 全量 `SecStaticCodeCheckValidity` → `SecCodeCheckValidity` | 支持 |
| Windows | **不成立**（N2，6.4）：改名运行中进程的 exe 后，`QueryFullProcessImageNameW` 仍返回旧路径 | 代码已按方案写好（两次查名 + 按句柄 `WinVerifyTrust`），由常量 `IMAGE_BINDING_VERIFIED = false` 统一返回 `Unavailable` | **暂不支持**：`SelfIdentity::detect` 在 Windows 上失败 |
| Linux、macOS | **fork+exec 冒充能绕过**（6.5） | — | 已登记需求第 9 章已知限制（v1.73） |

### 6.2 Linux（N3）

环境：Docker（OrbStack）`rust:1`，内核 Linux 7.0.11 aarch64。脚本 `tools/t0/iwork-t03-linux-procexe.sh`。做法：把 `/bin/sleep` 复制出来运行，换掉文件之后再打开 `/proc/<pid>/exe` 算哈希。

| 情形 | 操作 | `/proc/<pid>/exe` 读到的哈希 | readlink |
| --- | --- | --- | --- |
| 改名替换 | `mv peer peer.old`，原路径放别的程序 | 不变 | 跟着 inode 的新名字 |
| mv 覆盖 | 别的程序 `mv -f` 到原路径 | 不变 | 原路径 + ` (deleted)` |
| 原地改字节 / 截断重写 | `dd conv=notrunc`、`cp` | 不变：写入被拒（`ETXTBSY`） | 原路径 |
| 删除 | `rm` | 不变，仍可读 | 原路径 + ` (deleted)` |
| 进程已退出 | — | 打不开 → `Unavailable` | — |

改名到别的目录会被「同安装目录」检查拦下。

### 6.3 macOS（N1）

环境：macOS 27.2（26B5086k）arm64。脚本 `tools/t0/iwork-t03-macos-codesign.sh`，自签证书放临时钥匙串、跑完删除。不需要 `security-framework` crate，手写 FFI。

- `getsockopt(SOL_LOCAL, LOCAL_PEERTOKEN)` 返回 0、32 字节；交给 `SecCodeCopyGuestWithAttributes(kSecGuestAttributeAudit)` 返回 0。
- 各种二进制：链接器 ad-hoc（cargo 原样产物）与 `codesign -s -` → `Image(CdHash)`，与 `codesign -dvvv` 一致；同一张自签证书签的两个二进制 → `Certificate`，叶证书 SHA-256 相同；另一张证书 → 不同；签名后改字节 → `Unavailable`（-67061）；未签名 arm64 内核拒绝执行；未签名 x86_64（Rosetta）→ `Unavailable`。
- requirement 语言的 `certificate leaf = H"…"` 只收 SHA-1，所以钉身份用 SHA-1，对外报的仍是同一份 DER 的 SHA-256。

换文件反例（对端连着时动它的可执行文件）：

| 反例 | 结果 |
| --- | --- |
| mv 走再放别的文件 | 始终是原映像 |
| rm 再放别的文件 | 原映像 → `Unavailable`（-67034）；从未报成新文件 |
| 原地 dd 改一字节 | `Unavailable`（-67061） |
| 改一个还没调入的代码页再让它执行 | 只做动态校验时一直通过（进程执行的仍是原始指令）；加全量静态校验后 `Unavailable` |
| 读签名信息与钉身份校验之间停 4 秒换文件 | 钉住的校验通过，读到的始终是运行中的证书 |

与方案的两处差别：读身份后钉成 requirement 再验（头文件说动态对象的签名信息「有一部分来自磁盘」，只有 `SecCodeCheckValidity` 承诺不受磁盘改动影响）；加一步全量静态校验（否则「改未调入的页」验不出来），代价是每次建连哈希一遍对端映像，量级与 Linux 相同。`kSecCodeInfoMainExecutable` 的路径在 rm+替换后会指向新文件，只用于审计与「同安装目录」检查，此时签名身份本来就是 `Unavailable`。

### 6.4 Windows（N2）

环境：GitHub Actions `windows-latest`（Windows 10.0.26100），PowerShell 5.1。脚本 `tools/t0/iwork-t03-windows-authenticode.ps1`，CI job `windows-n2` 在 runner 上现建自签代码签名证书（`CurrentUser\My`，跑完删除），不用任何仓库密钥。记录：run 42762534066（另有 42758184421 结果相同），artifact `t03-windows-n2`（`transcript.txt`）。被测程序是拷出来的 `cmd.exe`（本身只有系统目录签名，没有内嵌签名）。

| 项 | 操作 | 结果 |
| --- | --- | --- |
| ① 自签 | `Set-AuthenticodeSignature` 签过的拷贝，按方案参数调 `WinVerifyTrust` | `0x800B0109`（`CERT_E_UNTRUSTEDROOT`），符合预期 |
| ② 篡改 | 签名后改一个字节 | `0x80096010`（`TRUST_E_BAD_DIGEST`），符合预期 |
| ③ 未签名 | 原样拷贝 | `0x800B0100`（`TRUST_E_NOSIGNATURE`），符合预期：`WinVerifyTrust` 按文件验时不查系统目录 |
| ④ 叶证书 SHA-256 | 证书本身的 DER 与从文件内嵌签名取回的 DER 各算一次 | 相同 |
| ⑤ 管道两端 pid | 同一进程里建管道两端，`GetNamedPipeClientProcessId` / `GetNamedPipeServerProcessId` | 都返回本进程 pid，调用都成功 |
| ⑥(a) 原地写 | 进程运行中打开它的 exe 写 | 被拒（文件被另一进程占用） |
| ⑥(b) 改名 | 运行中把 `live.exe` 改名为 `live.old.exe`，原路径放入别的文件，再查 `QueryFullProcessImageNameW(PROCESS_NAME_NATIVE)` | **仍返回旧路径 `…\live.exe`** |

④ 必须读文件内嵌的签名（`X509Certificate.CreateFromSignedFile`），不能用 `Get-AuthenticodeSignature`：拷来的 `cmd.exe` 签名后，Authenticode 哈希不含签名区，仍能匹配系统目录，`Get-AuthenticodeSignature` 优先返回微软的目录签名（run 42758068951 实测 `SignatureType=Catalog`、Subject 为 Microsoft Windows），两行哈希因此不同。Rust 实现按同一个 `WinVerifyTrust` 状态取签名者（方案 2.4），走的是内嵌签名，不受影响。

结论：⑥(b) 说明进程映像名不反映改名后的当前名。改名后，原路径上的文件已经不是正在运行的映像，「两次查名 + 按句柄验」验到的会是这个新文件，**按句柄绑定不成立**。Windows 在 M1 暂不支持，`IMAGE_BINDING_VERIFIED` 保持 `false`。命名管道 DACL、两端 pid、按句柄验映像等 `cfg(windows)` 测试已在 CI `windows-latest` 上跑通（同一 run：筛 `windows` 的 lib 测试实际执行 12 条，其中 9 条 `cfg(windows)` 测试逐名核对；另 3 条是 `peer::windows_image` 里各平台都编译的纯规则测试）。

### 6.5 fork+exec 冒充（Linux、macOS 都成立，已登记需求第 9 章已知限制（v1.73））

「核验运行中的映像」证明了「pid 正在跑的是哪份映像」，但方案还隐含了一条没写出来的假设：**校验时那个 pid 就是之后在这条连接上收发数据的进程**。这条假设不成立。

- **Linux**（`tools/t0/iwork-t03-linux-forkexec.py`）：攻击进程 connect → fork，子进程留着 socket → 父进程 exec 原版二进制。`SO_PEERCRED` 的 pid 是 connect 那一刻的进程，`/proc/<pid>/exe` 此时正是原版，哈希匹配；收到的数据来自攻击者子进程。实测输出：`hash==legit True`、`received: b'from attacker child pid 8'`。同用户 ptrace 也能做到类似效果。
- **macOS**（`tools/t0/iwork-t03-macos-forkexec.sh`）：`LOCAL_PEERPID` / `LOCAL_PEERTOKEN` 给的是「最后在这条连接上读写过的进程」，令牌带 pidversion，exec 后旧令牌作废（-67065），所以单靠 exec 骗不过；但如果被信任程序 exec 之后自己往这条连接写过字节（例如它的 stdout/stderr 被接到这个 socket），服务端在 accept 后判定时就会看到被信任映像，而实际控制连接的是攻击者子进程。攻击者能在服务端 accept 之前完成 fork、exec 与这一次写入。
- 两个方向都存在：冒充客户端（exec 被信任的客户端）、冒充后台（exec 出来的后台往继承的 fd 写日志）。
- 前提：攻击者是同用户、能在本机运行代码。这比方案已知限制 L2（能在后台启动前改安装目录）门槛低，不需要改任何文件。
- 实现里没有为它加任何机制（安全设计原则：不擅自加机制）。可选方向（未实现、未评估）：macOS 记下判定通过时的完整审计令牌，之后每帧前再读一次比对，变了就断开；Linux 没有等价的「最后读写者」信息。产品负责人已决定记为已知限制、不另加机制（需求第 9 章，v1.73）。

## 7 T04 生命周期实测（2026-11-11）

为 T04 方案第 8 节 N1–N5 补的实测。脚本在 `tools/t0/iwork-t04-*`，原始输出在 `tools/t0/out/t04-*`（临时目录与家目录已脱敏）。方案要求 N5 最先做、不过就停：**N5 有一项不过（7.1 A1），N1–N4 未做**，等 supervisor 定修法后再继续。

### 7.1 N5 真实包上完成依据与签名能不能同时成立

环境：macOS 27.2（26B5086k）arm64，测时 loadavg 30–53（提交的输出那一轮）；`pnpm -C client tauri build --bundles app` 的真实产物 + `cargo build --release -p iwork-daemon`。脚本 `tools/t0/iwork-t04-bundle-probe.sh`（macOS）与 `tools/t0/iwork-t04-linux-bundle.sh`（Linux，Docker `rust:1`，Linux 7.0.11 aarch64）。

后台现在还是空壳、客户端还不会连后台，所以「握手」与「运行映像身份」两件事用探针冒充两个角色，放在真实包的同一位置、按同样方式签名或登记清单：`iwork-t04-ipcprobe.rs`（依赖 `iwork-ipc`、**不开 dev feature**，走发布构建的 `RequireSigner`，`SelfIdentity::detect` 与对端判定都是 T03 已合入的代码）；`iwork-t04-imgprobe.c`（与 `peer/macos.rs` 同一串 Security 调用）。客户端主程序、资源、`Info.plist` 都是 tauri 的原样产物。两种包形态里「带清单那一行」「整包签名那一行」的判定按方案 2.9 的定义用 shell 写了一遍。

**第 0 项：tauri build 没配签名身份时不做整包签名。** 包里没有 `Contents/_CodeSignature`，主程序只有链接器的 ad-hoc 签名（`flags=0x20002(adhoc,linker-signed)`），与 `target/release/iwork-client` 逐字节相同。所以无证书形态不用额外去掉整包签名。

| 项 | 结果 | 说明 |
| --- | --- | --- |
| A1 启动 | 通过 | 真实客户端命令行起、`open` 起，5 秒后都在 |
| A1 两端互认 | **不通过** | 后台位置 `manifest_loaded{self_listed=true}`；**客户端位置（包的主程序）`SelfIdentity::detect` 失败**：`SecStaticCodeCheckValidity 失败（OSStatus -67056）`，握手无从谈起 |
| A2 | 通过 | 真实客户端运行中的 cdhash（`codesign -dvvv <pid>`）= 清单 `cdhash:`；后台位置运行映像 cdhash = 清单 `cdhash:`；两个 `sha256:` = 文件全文哈希 |
| A3 | 通过 | 完整 → `Complete`；删清单 → `NotThisForm`；后台截断一半 → `Incomplete`；整包签名那一行 → `NotThisForm` |
| A4（记录） | — | `codesign --verify --deep --strict` 与 `spctl` 都报 `code has no resources but signature indicates they must be present`；带隔离标记时命令行直接起主程序照常运行；双击走 Gatekeeper 弹窗，无人值守没做 |
| B1 | 通过 | 自签证书整包签名、包里不放清单：`codesign --verify --deep --strict` 与 `SecStaticCodeCheckValidity`（严格 + 嵌套 + 全部架构）都过；整包签名那一行 `Complete`、带清单那一行 `NotThisForm`；对照：两个角色（证书签名）握手成功 |
| B2 | 通过 | 删资源、截断后台、截断客户端、改 `Info.plist` 一字节、多放一个文件：五种都验签不过、判 `Incomplete` |
| B3 | 通过 | 后台位置磁盘静态 cdhash = 运行中 cdhash；换构建标记重签后两者都变；真实客户端（证书签名）运行中 = 静态 |
| B4（记数） | — | 整包验签 20–24 ms（包 10 MB，5 次，高负载） |
| B5 | 通过 | 签名后加清单 → 验签不过；签名前把清单放进 `Contents/MacOS` 再签 → codesign 直接失败（`In subcomponent: …/iwork-manifest.json`，它把 `MacOS` 下的文件当代码）。两条路都走不通，「有证书的包不放清单」不是可选项 |
| B6 | 通过 | 拿走一个资源 → `Incomplete`；放回 → `Complete`；三个时刻后台程序与封印文件的 size/mtime/inode 都相同 |
| C（Linux） | 通过 | 两个二进制 + 清单打 tar 再解开：带清单那一行 `Complete`、截断 → `Incomplete`、删清单 → `NotThisForm`；两端按清单握手成功；启动时的映像身份 = `/proc/self/exe` 的 SHA-256，路径上换文件后每次重取仍是原值；T03 `peer_manifest` 6 条通过 |
| D1–D3 | 通过 | A、B 两种包、mv 与 rm 两种换法：运行映像 cdhash 都取得到、等于自己路径的静态 cdhash；换文件后重取只会是原值（mv）或 -67034（rm），从不报成新文件；同一张证书签的两个构建取到的值不同；读静态 cdhash 不读全文（文件中部改坏一字节照样取到原值，10 MB 文件 2.5–4.3 ms，与 50 KB 的文件同一量级）；通用二进制取的是本机架构那一片 |
| E1 | 通过 | A 目录整包换成 B → 清单那一行 `NotThisForm`、签名那一行 `Complete`；反方向对称 |
| E2（记录） | — | B 合并拷进 A（旧清单残留）→ 两行都 `Incomplete`，验签 -67054；删掉残留清单后签名那一行 `Complete` |

**A1 不通过的根因**（`A1g`）：同一个探针放在三处，对自己的运行映像做 `peer/macos.rs` `check_image_file` 那一步（`SecCodeCopyStaticCode` → `SecStaticCodeCheckValidity`）：

| 位置 | 默认参数 | `kSecCSDoNotValidateResources` | 动态校验 |
| --- | --- | --- | --- |
| 未封印 .app 的主程序（`CFBundleExecutable`） | **-67056**（`errSecCSResourcesNotFound`） | 0 | 0 |
| 未封印 .app 的 `Contents/MacOS` 里另一个程序 | 0 | 0 | 0 |
| 散放的二进制（T03 实测的形态） | 0 | 0 | 0 |

包的主程序的静态代码对象代表整个包：链接器签名没有资源封印，按默认参数就判「签名说有资源、却找不到」。T03 的实测（6.3）只测过散放的二进制，没碰到。影响两个方向：客户端算不出自己的身份、拒绝启动；后台判客户端这个对端时走同一个 `check_image_file`，同样会得到 `Unavailable` 并拒绝。后台（不是主程序）不受影响。

**候选修法的验证**（`A1h`，只在临时副本里改，仓库代码没动）：`check_image_file` 的静态校验带上 `kSecCSDoNotValidateResources`（只跳过包资源，代码页哈希照验；T03 加这一步本来就是为了验「还没调入的页」）。结果：形态 A 两个角色握手成功；形态 B 同样成功；把客户端换成清单外的映像（换签名标识重签，cdhash 必变）→ 后台以 `ImageNotTrusted` 拒绝。注意「在文件末尾追加一字节」造不出清单外的映像：签名之外的尾部不在代码目录覆盖范围内，cdhash 不变。

### 7.2 N1–N4

未做（N5 不过，按方案停下）。脚本已写好（`tools/t0/iwork-t04-macos-smappservice.sh`、`iwork-t04-smprobe.m`），等 N5 的修法定下后再跑。
