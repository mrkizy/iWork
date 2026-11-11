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

## 附：T06 S0 实测运行说明（给产品负责人；2026-11-11，developer 按 Claude Code 2.1.322 校对）

T06 方案 16.2、16.3 要在真机上重跑的东西，按下面的顺序跑。脚本都在 `tools/t0/`，在仓库根目录下跑。跑出来的结论写进本文 2.4 节（新加），一项一行，注明 CLI 版本。

### 先说清楚几件事

- **会花订阅额度**。每次 `claude -p` 起一个实例就是一次真实的模型请求。下面每一步都写了大约起几个实例。数法：脚本给每个实例留一份 `<用例名>-stream.jsonl`（G7h 被杀掉的那几个不留），结果包里数这类文件就是这一步实际起了几个。
- **`iwork-t0g.py` 只动自己建的目录**：`~/iwork-t0g-<随机>/`，跑完自己删；结果包是 `~/iwork-t0g-results-<时间>-<随机>.tar.gz`，不覆盖已有文件。两处例外：`~/.npmrc` 被写进沙箱的禁读名单（不读内容）；G4 ⑧ 的最小构建会写 `~/.npm`、`~/.cargo` 这些工具链缓存，不想动就加 `--skip-builds`。
- **另外三个老脚本会清空并重写固定目录**：`~/iwork-t0-results/`、`~/iwork-t0b-results/`、`~/iwork-t0f-results/` 和同名的 `.tar.gz`。跑之前看一眼，有要留的先挪走。
- **登录状态只留四个字段**：`iwork-t0.sh`、`iwork-t0c.py`、`iwork-t0f.py` 落盘前只留 `claude auth status` 的 `loggedIn`、`authMethod`、`subscriptionType`、`loginExpiresAt`，不是 JSON 时只记字节数；`codex login status` 只记退出码与字节数（AGENTS.md 安全不变量：邮箱等不落盘）。`iwork-t0.sh` 因此要 `jq`。
- **G2、G3、G5、G9d 的设置按产品设置写**（方案 3.2）：`disableAllHooks: false`、`permissions.disableBypassPermissionsMode: "disable"`、PreToolUse hook `timeout: 7200`（只有「超时后才拒绝 / 允许」两格是 5 秒，那是被测的东西；G3 测的就是各种 timeout 取值）、SessionStart 报到 hook（`timeout: 30`）。外层保护的预检与故障格子用同一份设置，只差被测的 hook。有意和产品不同的三处：`sandbox.allowUnsandboxedCommands` 写 `false`（外层保护：故障格子里坏掉的 hook 拦不住沙箱外重试；产品手动、自动档是 `true`）；不写 `sandbox.network.allowedDomains`（`["*"]` 本身要 G4 ② 实测，写进去万一校验不过，`-p` 模式会把整个设置文件静默忽略）；不写 `sandbox.credentials.envVars`（G9b 单独测）。文件系统的禁读名单用外层保护那一套，比产品的底线清单宽。
- **结论只认四种证据**：流里的结构化工具调用与结果、哨兵 / 观测文件、hook 探针的调用记录、常驻会话活着时的生命周期事件。不看模型怎么说。每行是 `PASS` / `FAIL` / `INCONCLUSIVE`（没测成）/ `INFO`（只记录）。`summary.txt` 最后一行是合计；有 `FAIL` 或 `INCONCLUSIVE` 时脚本退出码是 1，这是正常的，看具体哪几行。

### 跑之前要准备的

| 要什么 | 为什么 |
| --- | --- |
| 日常账号已经登录 Claude Code（`claude` 能正常回话） | 所有组都要 |
| `python3`（3.9 以上，只用标准库）；macOS 上要有 `lsof`（系统自带） | 脚本本身；进程清理靠 `lsof` 找工作目录在根目录里的进程，找不到就不开跑 |
| `cargo`、`npm`、`curl`、`nc`、`gh` 在 `PATH` 上 | G4 的 ②、⑦、⑧；没有的那几行会是 `FAIL`（命令自己退出码非零），记下就行，不影响别的行 |
| G4 ④ 的语料 `--corpus corpus.json` | 来自 `cargo run -p iwork-core --example dump-credential-corpus > corpus.json`。**main 上现在还没有这个 example**（T07 交付），没有就不加 `--corpus`，G4 ④ 记 `INCONCLUSIVE`（等 T07 合入再单独补跑 `--only G4 --corpus ...`） |
| 一次性环境（临时 macOS 用户或虚拟机） | G2 的故障格子、G5、G9d 的完整结论只在那里拿得到，见第 4 步 |
| 一台 Linux（能临时改名 `bwrap`） | 16.2 第 3 项 |
| 一块能合盖的 MacBook，人在旁边 | 16.2 第 1 项 |

### 顺序与估算

时间按每个实例 20～60 秒估（G3、G7 里有故意等的，单算）。实例数是从脚本里数出来的上限，实际会少一些（运行门没过的格子不起实例）。

| 步 | 在哪跑 | 命令 | 大约多久 | 大约起几个实例 | 看什么 |
| --- | --- | --- | --- | --- | --- |
| 1 | 日常账号 | `python3 tools/t0/iwork-t0g.py --selftest` | 15 秒 | 0 | 最后一行 `selftest ok`；跑完家目录下没有 `iwork-t0g-*` 残留 |
| 2 | 日常账号 | `python3 tools/t0/iwork-t0g.py --only G2 --quick` | 1～2 小时 | 不超过 129：探查 1 + 8 个候选配置 ×（外层保护预检 5 + 普通 Bash 一行 11） | `外层保护 [...]` 那 8 行：哪几样保护在这台机器上成立。`G2 硬前提` 这一行一定是 `INCONCLUSIVE`（`--quick` 不许进已验证表），正常 |
| 3 | 日常账号 | `python3 tools/t0/iwork-t0g.py`（全量，不带 `--disposable-env`） | 半天（G2 占大头） | 约 170～780，见下面的细账 | G1、G3、G4、G6、G7、G8、G9a～c 的结论；G2 的格子哪些因为外层保护没成立而没跑；G5、G9d 一定是 `INCONCLUSIVE`（要一次性环境），正常；G2 的「沙箱外重试」11 格和 `G2 硬前提` 总行也一定是 `INCONCLUSIVE`（这一种只在一次性环境跑，不起实例），正常 |
| 4 | 一次性环境 | `python3 tools/t0/iwork-t0g.py --only G2,G5,G9 --disposable-env` | 3.5～9 小时 | G2 最多 704（8 × 88），被淘汰的候选只跑 11；G5 7 个；G9 2 个 | `G2 硬前提` 一行 `PASS` 且没有 `INCONCLUSIVE` 的格子（88 格，含「沙箱外重试」11 格，其中「允许」一格要看到工作区外的哨兵）；`G5 A --setting-sources 空值` 一行 `PASS`；`G9d` 一行 `PASS`。**G2 的 PASS 只有这一步拿得到** |
| 5 | 日常账号，人在旁边 | `python3 tools/t0/iwork-t0g.py --only G3 --long` | 30～45 分钟 | 13（探查 1 + 预检 5 + 六个取值 6 + 睡眠时钟 1） | 前面六个取值各睡 90 秒，要先跑完（十几分钟）才出提示；看到「看到这行后 30 秒内合盖」就合盖，睡满 5 分钟再打开；看 `G3 睡眠后命令没执行` 一行和它的墙钟用时。步 2 里外层保护的 Bash 两项没成立的话，这一步整组不跑（运行门），改到一次性环境里加 `--disposable-env` 跑 |
| 6 | 日常账号 | 16.2 第 2 项（没有终端时的登录）：照方案里那一行命令跑 | 1 分钟 | 0（只起 `claude auth login`，用空的配置目录，不动现有登录） | 有没有打印登录网址、有没有自己开浏览器、有没有因为没终端报错退出 |
| 7 | Linux | `bwrap` 临时改名后：`--only G4`，再 `--only G4 --no-fail-if-unavailable` | 各 20～30 分钟 | 各约 27 | 第一次：CLI 起不来、G4 全是 `INCONCLUSIVE`，记下 stderr 的退出码与文字；第二次：`G4 ①b 写工作区外` 那一行，和 `g4-1b-stream.jsonl` 里 `system/init` 有没有沙箱状态字段。跑完把 `bwrap` 改回来 |
| 8 | 日常账号 | `bash tools/t0/iwork-t0.sh` | 1～2 分钟 | 3 | 原 2.1 的 Q3～Q5；结果在 `~/iwork-t0-results.tar.gz`（登录状态只有四个字段） |
| 9 | 日常账号 | `bash tools/t0/iwork-t0b.sh` | 约 14 分钟 | 2 | 原 2.1 的 Q6（hook 超时 = 放行）；结果在 `~/iwork-t0b-results.tar.gz` |
| 10 | 日常账号 | `python3 tools/t0/iwork-t0f.py` | 5 分钟左右 | 约 5 | 原 2.2a 的 V0～V3；结果在 `~/iwork-t0f-results.tar.gz` |

步 2 可以省掉、直接跑步 3，它只是让你先花一两个小时看清外层保护成不成立，再决定要不要花半天跑全量。

全量（步 3）的细账，每组起几个实例：

| 组 | 实例数 | 说明 |
| --- | --- | --- |
| 开头的探查 | 1～2 | `--setting-sources ""` 跑不通才多一次 |
| G1 | 10 | 9 种工具探针各 1，cd 之后的 cwd 1 |
| G2 | 8 × 5 = 40 次预检，加格子 | 候选配置 = `--modes` 里的模式（默认 `default,dontAsk`）× 4 种（`+block`、`+block+prompts-none`、`+block+denyInsteadOfPrompting`、不设 onFailure 的对照）= 8 个。每个先跑普通 Bash 一行 11 格，11 格全 `PASS` 才跑剩下的 77 格（含排在最后的「沙箱外重试」11 格；每个留下的候选因此比原来多 11 次短调用）。日常账号下外层保护没成立的格子不起实例，「沙箱外重试」11 格在日常账号下一律不起实例 |
| G3 | 11 | 预检 5 + 六个 timeout 取值，每个 hook 睡 90 秒 |
| G4 | 26 + 语料行数 | 基础 15、最小构建 3（`--skip-builds` 就没有）、⑥ 重试 5、⑨ 全开档 3 |
| G5 | 7 | 只在 `--disposable-env` 下跑 |
| G6 | 10 | 思考强度 6、不存在的模型 1、换模型续接 2、换系统提示 1 |
| G7 | 7 + 2 × `--repeat` | 常驻会话 3、换配置续接 4、杀进程续接每次 2（默认 `--repeat 3`） |
| G8 | 7 | 工具清单 4、MCP 3 |
| G9 | 1～2 | G9d 只在 `--disposable-env` 下跑 |

### 一次性环境怎么搭（16.2 第 5 项）

macOS 上：`sudo sysadminctl -addUser t0gtmp -password -`（按提示输密码），快速切换到这个用户，登录 Claude Code，`git clone` 一份仓库（或者拷过去），跑步 4 的命令。跑完把 `~/iwork-t0g-results-*.tar.gz` 拷出来（比如放到 `/Users/Shared/`），切回自己的账号，`sudo sysadminctl -deleteUser t0gtmp`。这个用户里不要放任何要紧的东西：故障格子会故意弄坏 hook，靠的只是沙箱和这个用户本身的隔离。

### 结果不对时怎么办

**`FAIL`**：说明拿到了正面证据，证明行为不对。

- `G2 硬前提` 一行是 `FAIL`（每个候选配置都有格子执行了不许执行的命令）：**整张卡停**，结果包交 supervisor，由产品负责人决定（方案 P5 选项 c）。别的组先不用看。
- 单个 G2 格子 `FAIL` 而总行不是 `FAIL`：说明那个候选配置不行，别的候选过了就用别的，记下来。
- `进程清理` 或 `进程组` 出现 `FAIL`：脚本自己会停（清不干净不跑后面的）。看 `ps` 里还有没有工作目录在 `~/iwork-t0g-*` 下的进程，手动结束后再跑。
- `运行前后原有文件核对` 是 `FAIL`：家目录下原有的 `iwork-t0g*` 文件或 `~/.npmrc` 变了，结果包和 `summary.txt` 里有变了哪几项，交 supervisor。
- 别的组的 `FAIL`：就是实测结论，照实写进 2.4 节，交 supervisor 看对方案哪一节有影响（每组影响哪里见方案 16.1 的表）。

**`INCONCLUSIVE`**：没测成，不是测出了问题。按 `summary.txt` 那一行后面写的原因处理：

| 原因 | 怎么办 |
| --- | --- |
| 「外层保护没验证过（[...]），这个格子不在这台机器上跑」 | 运行门没过：日常账号下这几样保护不成立，故障格子不许跑。去一次性环境加 `--disposable-env` 跑（步 4）。同时把 `外层保护 [...]` 那几行原样记下来：产品里的本机验证就靠这几样 |
| 「只在一次性环境……里加 --disposable-env 跑」 | G5、G9d 在日常账号下本来就不跑，去步 4 |
| G2「bash-沙箱外重试」那 11 格「外层保护没验证过（['unsandboxed']）」 | 这一种没有预检，日常账号下本来就不跑，去步 4。G2 总行因此在日常账号下恒为 `INCONCLUSIVE` |
| 「缺正面证据：……」「运行无效（没有 init、超时或退出码不对）」「没有归属对得上的目标调用，或不止一次」「目标调用没有按 id 对上的结果」「探针没有在这次调用上被调起的记录」 | 这次运行本身不算数：模型没按要求发调用、发了两次、超时，或 CLI 没起来。先看这个用例的 `<名>-stream.jsonl` 和 `<名>-stderr.txt`；偶发的（模型没照做、网络抖）就用 `--only <组>` 单独重跑这一组；每次都这样的，交 supervisor |
| 「权限模式没生效：init 报 ...」 | 设置文件没被 CLI 收下（`-p` 模式下设置文件校验不过会被整个静默忽略）。看 `<名>-stderr.txt` 有没有设置校验的告警，交 supervisor，别重跑 |
| 对照没成立（`G5 阳性对照`、`G8 MCP 阳性对照`、G9b 对照变量看不到、外层保护里要成功的那一笔没成功） | 测的办法在这台机器上不灵，负向结论说明不了问题。看对照那次的 stream 和 stderr，交 supervisor |
| G4 ④「没给 --corpus」或「语料读不了」 | 等 T07 的 `dump-credential-corpus` 合入，再 `--only G4 --corpus corpus.json` 补跑 |
| 「登记之外的调用（探针一律拒绝）」一行次数很多 | 这行是 `INFO`，不算结论；但次数多说明模型老跑偏，相应用例容易变成没测成。看各用例目录 `hook-calls.jsonl` 里带 `refused` 的行。偶发就重跑那一组 |
| 「脚本自己出错：...」 | 脚本的错，交 developer，附上这一行和结果包 |

`--quick` 下 `G2 硬前提` 恒为 `INCONCLUSIVE`，这是设计如此，不用处理。

### 跑之前已知的风险点（S0 校对时从 2.1.322 打包代码里看不出结论，要靠这次实测）

- G6 的思考强度读回：hook 输入里的 `effort` 字段只在当前模型支持思考强度时才带。脚本默认 `--model haiku`（2.1.322 里指 `claude-haiku-5-5`），它支不支持看不出来。要是 G6 五个取值都是 `FAIL` 且读回是 `None`，换 `--only G6 --model sonnet` 重跑一次再下结论；`xhigh`、`max` 也可能只有部分模型支持。
- G6 续接时换追加系统提示：2.1.322 新增 `--system-prompt-snapshot`，默认开，会话第一次的系统提示会被记下来，续接时原样再用，后来传的不同文本不生效。那一行只是 `INFO`，读结果时知道这一点。
- G5 的阳性对照：仓库设置里同时写了 `disableAllHooks: true`。命令行设置写的是 `false`，按设置层的优先级应当压过它，但从代码里没法确认合并后取哪个；要是对照没成立，G5 三行都是 `INCONCLUSIVE`，交 supervisor。
- G8a `--tools ""`：要是这个版本有不受 `--tools` 管的常驻工具，`init.tools` 不会是空的，G8a 记 `FAIL`。照实记。
- G4 ②：`allowedDomains: ["*"]` 能不能放行全部，文档只写了 `*.example.com` 这种通配，要实测。
- G4 ⑦a `security list-keychains` 只有 macOS 有；在 Linux 上跑 G4（步 7）这一行一定是 `FAIL`，不算问题。
- 16.2 第 1 项说「再用 G2 选定配置重复一次」：`--only G3` 用的是 `--modes` 第一个模式加 `onFailure: block`，表达不了 `--permission-prompts none` 或 `denyInsteadOfPrompting` 这两种候选。G2 选出来的要是这两种之一，第二次睡眠时钟要改脚本才能跑，交 developer。
- G2「沙箱外重试」：提示里要求模型给 Bash 带上 `dangerouslyDisableSandbox: true`，模型照不照做没实测过。不照做时目标调用（命令逐字相同且带这个参数）对不上，这一行记 `INCONCLUSIVE`，不会误判成 `PASS` 或 `FAIL`。在步 4 里大量出现时，看这些格子的 `<名>-stream.jsonl` 里 Bash 调用带没带这个参数，交 supervisor（可能要换提示或换模型）。
- `iwork-t0.sh` 按 `--help` 文字查参数在不在：2.1.322 的 `--help` 里没有 `--permission-prompt-tool`，日志会写 `flag --permission-prompt-tool: no`，但打包代码里这个参数还在（只是不在帮助里列出）；`iwork-t0f.py` 用的 `--max-turns` 也一样。不是退化。
