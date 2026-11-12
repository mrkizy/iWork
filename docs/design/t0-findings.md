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

### 2.4 T06 S0 补测（`tools/t0/iwork-t0g.py`，Claude Code **2.1.322**，macOS arm64，日常账号，模型 `claude-haiku-5-5`，2026-11-11 23:08 – 11-12 00:41）

> 来源三份，都在 2.1.322 上、都是日常账号：① S0 第一轮全量 `iwork-t0g.py --corpus --skip-builds`（结果包 `iwork-t0g-results-20261112-004107-942c1c`，约 766 个实例）；② R1 的五个手工探针（2026-11-12，产品负责人同意）；③ R1x 的七个手工探针（同日）。只收有结构化证据的结论（流里的工具调用与结果、哨兵与观测文件、探针记录）；没测成的项只写现状，不写结论。「R2」指脚本改完后在日常账号的重测，「R3」指一次性环境里的全量 G2 / G5 / G9（T06 方案 16.1、16.2）。

S0 合计 PASS 506、FAIL 46、INCONCLUSIVE 217、INFO 19。FAIL 里 34 个是 G2 的 read 行（真行为，见下）；G1f、G7e、G7f 是用例设计问题；G4 四条与「进程组」四行是真行为。INCONCLUSIVE 里 90 个本来就该在一次性环境跑，另 127 个是脚本问题（子代理工具名、模型重发、MCP 延迟加载、模型并行发调用），脚本已在改，R2 重测。G5、G9d、G4 ⑧、G3 的睡眠时钟、T06 方案 16.2 的手动项、G10 未跑。原 2.1、2.2a 各条在 2.1.322 上的重测（`iwork-t0.sh`、`iwork-t0b.sh`、`iwork-t0f.py`，方案 16.3）不在这一节里。

**G1 工具与 hook 输入**

| 项 | 结论 |
| --- | --- |
| G1a 内置工具 | `system/init.tools` 默认 25 个：Task、Bash、CronCreate、CronDelete、CronList、DesignSync、Edit、EnterWorktree、ExitWorktree、ListAgents、Monitor、NotebookEdit、PushNotification、Read、RemoteTrigger、ReportFindings、ScheduleWakeup、SendMessage、Skill、TaskStop、ToolSearch、WebFetch、WebSearch、Workflow、Write。默认没有 Grep、Glob：它们只在没有 Bash 时出现（G8b、G8c） |
| 子代理工具的名字 | `init.tools` 与 `result.permission_denials` 里叫 `Task`；模型发出的 `tool_use.name` 与 hook 输入的 `tool_name` 叫 `Agent` |
| 技术设计 7.1 没归类的 | DesignSync、EnterWorktree、ExitWorktree、ListAgents、Monitor、PushNotification、ReportFindings、ScheduleWakeup、SendMessage、ToolSearch、Workflow、CronList。7.1 提到而这个版本没有的：TodoWrite、计划模式工具、单独的「读取后台命令输出」工具（后台输出写进文件，用 Read 读）。差异表交产品负责人（任务卡 5c） |
| G1b hook 输入顶层字段 | `cwd`、`effort`、`hook_event_name`、`permission_mode`、`prompt_id`、`session_id`、`tool_input`、`tool_name`、`tool_use_id`、`transcript_path` |
| G1e `tool_input` 字段名 | Bash：`command`、`description`、`run_in_background`（沙箱外重试另带 `dangerouslyDisableSandbox`）；Read：`file_path`；Write：`content`、`file_path`；Edit：`file_path`、`new_string`、`old_string`、`replace_all`；ToolSearch：`max_results`、`query`；WebFetch：`prompt`、`url`；Agent：`description`、`prompt`、`subagent_type`；TaskStop：`task_id` |
| G1c cd 之后的 cwd | 没测成（模型并行发了调用）。原始记录：cd 跨 Bash 调用保留；cd 之后那条调用的 hook `cwd` 是新目录 |
| G1d 子代理进不进 hook | 没测成（脚本按 `Task` 登记，把 `Agent` 拒了）。子代理工具本身过 PreToolUse |
| G1f 结果是否都过 hook | 成功的工具结果都对得上一次 hook 请求。CLI 自己就判了失败的调用不过 hook、直接回报错结果：调用不存在的工具（`No such tool available`）、Bash 内置拦截（前台 `sleep N; …`，`Blocked:`） |
| 延迟加载 | 工具多时，WebFetch 与 MCP 工具要先经 `ToolSearch` 取定义；`ToolSearch` 过 PreToolUse |

**G2 hook 出事时执不执行（日常账号，8 个候选配置：`default` / `dontAsk` × `onFailure: block`、加 `--permission-prompts none`、加 `denyInsteadOfPrompting`、不设 `onFailure`）**

| 项 | 结论 |
| --- | --- |
| hook 明确允许 / 拒绝 | 每类工具都照办。拒绝的结果 `is_error=true`、文字 `PreToolUse:<工具> hook error: <原因>`，计入 `result.permission_denials` |
| `onFailure: "block"` | hook 非零退出（1、2）、被信号杀掉、超时（`timeout: 5`，之后答允许或拒绝都不算）、程序不存在：调用被拦，文字 `PreToolUse:<工具> hook error: […]: failed; blocking because onFailure is "block"`。Bash、Write、Read、MCP 都如此 |
| hook 不表态（退出码 0 没输出、输出不是 JSON、没注入 PreToolUse） | CLI 用自己的权限层接着判。Bash（沙箱内、清单内沙箱外）、Write、MCP：拒绝，流里有 `system/permission_denied`；`dontAsk` 下文字是 `Permission to use <工具> has been denied because Claude Code is running in don't ask mode`。**工作目录内的 Read：执行**。**子代理工具：执行**（子代理里的 Bash 被拒，理由是 `Permission prompts are not available in this context`）。S0 的 Bash 各行用的命令是 `touch`；只读命令见下面 R1x 的 x2 |
| 不设 `onFailure` | hook 失败与超时不拦，同样落到 CLI 权限层：Bash（`touch`）、Write、MCP 拒绝，工作目录内的 Read 执行。这里量的是 `timeout: 5`、带隔离参数、`default` / `dontAsk` 两种模式；原 2.1「hook 超时 = 放行」那一条是在用户自己的配置下量的，要等 `iwork-t0b.sh` 在 2.1.322 上重跑才能改 |
| 硬前提（T06 方案 3.4） | 「hook 没明确允许时每类工具都不执行」在 2.1.322 上不成立，8 个候选都一样：hook 不表态时，CLI 自己不问的动作照 CLI 的规则执行。产品负责人 2026-11-12 定：硬前提分两类写。**CLI 要问的动作**（只读命令之外的命令、写文件、MCP、工作目录外的读、子代理里的这些调用）仍然不执行，这一句在不注入 hook 时已实测，hook 无输出或乱码时的越界、子代理里的越界、清单内只读命令出不出沙箱待 R3。**CLI 不问的动作**（工作目录内的读，含 Read、Grep、Glob 与只读命令如 `cat`；起子代理）记为已知限制；只读命令一项最坏情况按沙箱算，待 R3 的底线格、沙箱外重试格证实；项目目录包住凭据目录或数据目录时文件工具连系统底线也读得到，记为已知限制（P12）。见 AGENTS.md 安全不变量第 2 条、技术设计第 1 节已知缺口表、方案 3.4。选定配置写死：`dontAsk` + `onFailure: "block"`，hook 程序只有写出完整决定才以 0 退出（方案 3.4a） |
| 候选配置之间的差别 | 带 `onFailure: "block"` 的六个候选（两种模式 × 不加 / 加 `--permission-prompts none` / 加 `denyInsteadOfPrompting`）逐格结果相同。`default` 与 `dontAsk` 的差别只在拒绝的文字和 `system/init.permissionMode` 报的值（`default` / `dontAsk`）。设置里没给 `defaultMode` 的运行，init 报的都是 `default` |
| 外层保护 | `permissions.blockReadsOutsideWorkingDirectories` / `blockWritesOutsideWorkingDirectories` 拦住文件工具读写工作目录之外，hook 答允许也照拦。沙箱拦住 Bash 读写工作区外。`--tools ""` 加 `--mcp-config` 时只剩 MCP 工具。六项在 8 个配置下都成立 |
| 子代理一行、沙箱外重试一行 | 没测成（前者脚本问题，后者只在一次性环境跑） |
| 模型重发 | hook 报错后模型常把同一个调用原样重发一次（33 个故障格），两次都被拦。判据因此改成：负向结论允许重发、每一次都要有证据；正向结论与读回仍要恰好一次（方案 16.1） |

**R1 读类补测（手工探针，2026-11-12，日常账号，`--tools Read`，`--setting-sources ""`，每条 1 个实例；记录 `r1-out/r1a…r1e.tools.jsonl`）**

| 项 | 设置 | 读什么 | 结论 |
| --- | --- | --- | --- |
| r1e（对照） | `dontAsk`，不注入 hook | 工作目录内的文件 | 读到了。复现 S0 的 read 行，探针测得出东西 |
| r1a | `dontAsk`，不注入 hook | 工作目录**外**的文件 | **没读到**。结果 `is_error=true`：`Permission to use Read has been denied because Claude Code is running in don't ask mode`，计入 `permission_denials` |
| r1b | `default`，不注入 hook | 工作目录外的文件 | **没读到**。`Claude requested permissions to read from <路径>, but you haven't granted it yet.` |
| r1c | `dontAsk` + `permissions.ask: ["Read"]`，hook 答允许 | 工作目录内的文件 | **没读到**，结果文字与 r1a 相同。hook 答了允许也过不了 `ask` 规则（探针没记 hook 有没有被调起，这一点靠 r1d 与 r1e 对照：同样的文件，不加 `ask` 读得到，加了读不到，有没有 hook 都一样）。给只读工具加 `ask` 规则这条路走不通 |
| r1d | `dontAsk` + `permissions.ask: ["Read"]`，不注入 hook | 工作目录内的文件 | 没读到（同上的文字） |

**R1x 补测（手工探针，2026-11-12，日常账号，`dontAsk`、不配 hook、沙箱开、`autoAllowBashIfSandboxed: false`、`allowUnsandboxedCommands: false`，每条只开一个工具、1 个实例；记录 `r1x-out/x1…x7.jsonl`）**

| 项 | 工具与目标 | 结论 |
| --- | --- | --- |
| x1 | Bash `cat` 工作目录外的文件 | 拒。`Permission to use Bash has been denied because Claude Code is running in don't ask mode`，流里有 `system/permission_denied` |
| x2 | Bash `cat` 工作目录内的文件 | **执行了，读到内容**。CLI 对它判为只读、目标在工作目录内的命令不问，`autoAllowBashIfSandboxed` 关着也一样 |
| x3 | Bash `ls` 工作目录外的目录 | 拒 |
| x4 | Grep 搜工作目录外 | 拒 |
| x5 | Glob 列工作目录外 | 拒 |
| x6 | Write 写工作目录外 | 拒，文件没创建 |
| x7 | Read 读工作目录内一个指向目录外文件的符号链接 | 拒。CLI 按链接的目标算 |

合起来：没有 hook 时，CLI 对读的规则是「工作目录内放行、工作目录外拒绝」，文件工具与只读命令都这样；Read 在 `default` 与 `dontAsk` 两种模式下都量过，别的只量了 `dontAsk`。

这两批探针**没量到**的（都排进了方案 16.1，括号里是哪一轮）：

- x2 那条 `cat` 是不是在沙箱里跑的：读的是目录内的普通文件，沙箱内外都读得到，证明不了（R3 的底线格）。
- 两批探针的 `allowUnsandboxedCommands` 都是 `false`，产品手动、自动档是 `true`：只读命令带 `dangerouslyDisableSandbox` 时 CLI 还问不问（R3 的沙箱外重试格）。
- 需凭据命令清单里的只读命令（如 `git remote -v`）在没有 hook 时执不执行（R3 的出沙箱格，每个条目配对照）。
- `cat`、`ls` 以外还有哪些命令被 CLI 当成只读：矩阵测不全，方案按「最坏情况按沙箱算」处理。
- hook 退出码 0 不表态（没输出、乱码）时读工作目录外会怎样（R3 的「工作目录外的读」一类与越界格）。
- 链接：指向目录外**目录**的链接；Grep、Glob、只读命令经链接读；hook 无输出时的 Read 文件链接（R3 的链接格）。越界目标只用过绝对路径，`..`、`~`、相对路径没量。
- 子代理里的读与只读命令（R3 的子代理里三格）。
- Grep、Glob、只读命令、子代理工具在 hook 明确拒绝时（R2 的拒绝格）。S0 里有顺带的证据：探针拒掉没登记的调用时，子代理工具 72 次都没起来，`ls -l`、`find`、`grep -rn` 各一次也没执行；Grep、Glob 一次都没有。
- 别的内置工具（ToolSearch、TaskStop 等）在没有 hook 时执不执行（R3 的「不问工具」记录行）。
- hook 允许时文件工具读写工作目录外成不成功（R2）。

**G3 hook timeout 取值**：7200、86400、604800、2147483、2147484、4294968（秒）下，hook 睡 90 秒都没被提前打断，命令没执行。没探到溢出。睡眠时钟没测。

**G4 沙箱固定配置**

| 项 | 结论 |
| --- | --- |
| ①a ①b ①g 写 | 工作区内可写；工作区外不可写；`allowWrite` 追加的目录可写 |
| ①c ①d 假凭据目录 | `denyRead` + `denyWrite` + `credentials.files`（deny）下，读、写都失败 |
| ①e ①f 只读区 | 写失败（符合预期）。**读也失败**：`denyRead` 目录下用 `allowRead` 放开的子目录，在同一目录又进了 `credentials.files` 时读不到。原因待重测 |
| ①h | 写 `.git/hooks` 失败（CLI 自带写保护） |
| ② 联网 | `allowedDomains: ["*"]` 下 HTTPS 通。**非 HTTP 的 TCP（github.com:22）不通；绑定并连接本机端口不通**（同一台机器沙箱外都通） |
| ⑤ | 禁读 `~/.npmrc` 后 `npm config get registry` 正常 |
| ⑥ 沙箱外重试 | 写工作区外被挡后，5 次里 3 次模型用同一条命令加 `dangerouslyDisableSandbox: true` 重试（Haiku） |
| ⑦ | `security list-keychains` 在沙箱内正常；`gh auth status` 在沙箱内失败（沙箱外正常）。`gh` 在需凭据命令清单里，产品里走沙箱外 |
| ⑧ 最小构建 | 未跑（`--skip-builds`） |
| ⑨ 全开档 | `allowWrite: ["/"]` 下工作区外可写；假凭据目录仍读不到、写不进；`allowUnsandboxedCommands: false` 下三个用例都没发起沙箱外重试 |
| ④ 对拍 | 44 行里实测成 17 行，都是 CLI 与我们同判沙箱外（含 `git pull && git push`、`git fetch; git pull`、`(git pull)`、`git pull 2>&1`）。没测成的 27 行里有我们判沙箱内的全部 16 行与判不确定的全部 10 行，**对拍结论不能用**。没测成的原因是每一行替身都多记到一笔沙箱外的调起，推测是 CLI 自己在沙箱外调了 `git`（观测文件没进结果包，没法确认；脚本修正里改成按参数与调用方区分，R2 重测）。语料各行的排除清单不是同一份：44 行里有 3 种，内置的 34 条每行都有 |
| init 的沙箱状态 | `system/init` 里没有沙箱状态字段 |

**G6 模型与思考强度**

| 项 | 结论 |
| --- | --- |
| `--effort` | `low`、`medium`、`high`、`xhigh`、`max` 都能从 hook 输入的 `effort.level` 读回同一个值（Haiku 5.5） |
| 非法值 | stderr 一行警告 `Unknown --effort value … ignoring it and using the default effort`，照默认值（读回 `medium`）跑，退出码 0 |
| 不存在的模型 | 进程退出码 1；`result.subtype` 是 `success` 但 `is_error=true`、`api_error_status=404`、`terminal_reason=api_error`；assistant 消息 `model="<synthetic>"`、`error="model_not_found"`；stderr 有 `[claude-code:unrecognized_model]` |
| 带新模型续接 | `--resume <id> --model sonnet`：init 报新模型，`session_id` 不变，记得上文 |
| 续接时换 `--append-system-prompt` | 新文本没生效（模型转述，只作参考） |

**G7 流式输入、中断、续接、进程**

| 项 | 结论 |
| --- | --- |
| a、a′ | 同一进程多轮，每轮一个 `result`；`--include-partial-messages` 有 `stream_event` |
| `system/init` 的时机 | 第一条输入之后才出；**每轮开头都有一个**；`set_model` 之后也再出一个 |
| b′、b″ | SessionStart hook 在第一条输入之前触发，新会话与 `--resume` 都是 |
| c `initialize` | 响应里有 `models`（14 项，含 `default`、`opus`、`fable`、`sonnet`、`haiku` 等别名与若干具体 ID）、`current_permission_mode`、`hook_on_failure_supported` 等键；**另有 `account` 键，不得保存**（S0 的结果包里 `g7-live-stream.jsonl` 存了这条响应的全文，这份包不要外传；脚本已改成落盘前删掉，2026-11-12 合入 main `655449e`）。S0 只看了键名：`current_permission_mode` 的取值、只发 `initialize` 不发用户消息时有没有响应，待 R2（方案 3.5 的 G7c′） |
| d 用量与额度 | **`total_cost_usd` 与 `modelUsage` 是会话累计值，跨 `--resume` 保留；单回合的用量与费用取 `model_usage_since_last_result`（上一个 `result` 之后新增，按模型分），T06 S5 录制样本核过（2026-11-12，T06 方案 6.3）。** `result` 键含 `usage`、`modelUsage`、`total_cost_usd`、`permission_denials`、`terminal_reason`、`api_error_status`、`stop_reason`、`num_turns` 等。`rate_limit_event.rate_limit_info`：`utilization` 是 0–1 的小数（0.63），`resetsAt` 是秒级时间戳，另有 `rateLimitType`（`seven_day`）、`status`（`allowed_warning`）、`unifiedWindows` |
| f interrupt | `control_request{subtype:"interrupt"}` 立刻生效：`control_response` success，流里一条 user 文本 `[Request interrupted by user]`，`result` 为 `subtype=error_during_execution`、`is_error=true`、`terminal_reason=aborted_streaming`；同一进程能接下一轮。对正在跑的前台命令没测到；**后台命令不会被它停掉** |
| g `set_model` | 生效；流里多一条 `user` 事件，`message.content` 是字符串（`<local-command-stdout>…`），不是内容块数组 |
| h | `--session-id` 指定的 id 生效；等审批时杀掉整个进程组后 `--resume` 接得上，id 不变 |
| i | 续接后让模型原样重发，`tool_input` 逐字段相同 2/3 |
| j | 换配置续接两个方向都跟着这一次启动的设置走，新 hook 生效 |
| e 进程组 | **hook 进程每次都自成进程组**（1310 次调用，`pgid` 都等于自己的 pid）。**后台 Bash 任务自成进程组**。前台命令没测到。结束 CLI 的进程组清不到这两种 |
| 前台 sleep | 第一段是裸 `sleep N` 的前台命令被 CLI 拦下（`Blocked: sleep N followed by: …`），不过 hook |
| 子代理 | 默认在后台跑：主回合先出一个 `result`，子代理结束后 CLI 自己再起一轮（同一进程第二个 init 与 result）。只在没有 hook 的格子里观察到，待重测 |

**G8 工具开关**

| 项 | 结论 |
| --- | --- |
| a | `--tools ""`：`init.tools` 为空 |
| b | `--tools "Read,Grep,Glob"`：只剩这三个 |
| c | `--disallowedTools` 生效；写了不存在的名字不报错；禁掉 Bash 后清单里多出 Glob、Grep |
| d | 同用时禁用优先 |
| MCP（e、f 与阳性对照） | **三行都没测成**：默认工具集下 MCP 工具是延迟加载的，模型先调 `ToolSearch`，探针没登记它，阳性对照没成立。那几次运行里看到的现象，只作线索、不算结论，待 R2：`--tools Read` 白名单下 MCP 工具还在 `init.tools` 里、也调成了；`--disallowedTools <MCP 工具名>` 后它从 `init.tools` 消失 |

**G9 环境变量**：CLI 把自己的环境变量传给 hook；`sandbox.credentials.envVars` 的 `deny` 让沙箱内的命令看不到这个变量（对照变量看得到）；hook 的父进程是 CLI，进程组是它自己。

**G5、G9d**：未跑（要一次性环境）。

**运行前后核对**：家目录下原有 1 项，没变。登记之外的调用 86 次，都被探针拒了。

#### R2（2026-11-12 13:51–14:10，Claude Code 2.1.322，日常账号，普通终端起，模型 `claude-haiku-5-5`；六个结果包 `…135202-09df2e`（G2 先行五格）、`…140247-6b03d0`（G4）、`…140747-55cf18`（G7）、`…140921-65e950`（G1）、`…141011-8ff1ae`（G8）、`…141030-50150f`（G5），109 个实例）

只收有结构化证据的。分析全文在 architect 的报告 `r2-analysis.md`；据此改的方案见 T06 方案 21 节「R2 的结果落进方案」。

**环境**：六个包的环境变量名记录里只有 `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS`，没有插件变量。109 个实例里有 `init` 的 107 个（另两个只发 `initialize`）。`init.plugins` 全是 `@builtin`（G5 阳性格多一个测试插件）：105 个实例是 4 个，G2 包的隔离探查那一次是 5 个，多一个 `cc-plugin-easel`。上文 S0 那一轮是从带着用户插件的环境里跑的，两轮不一样的地方以这一轮为准。

**先看这一条：哪些格子没带产品设置。** 脚本的权限模式和沙箱都只从设置文件来。`init` 报 `default` 的格子，设置文件里没有权限模式、也没有沙箱（只有 hook 与记忆改道），**命令不在沙箱里跑**。

| 包 | `init` 报的权限模式（有 `init` 的实例数） | 说明 |
| --- | --- | --- |
| G2 先行五格 | `dontAsk` 5，`default` 1（隔离探查） | 五格带了设置；沙箱用的是矩阵那一套外层保护，不是产品原样（T06 方案 3.4） |
| G4 | `dontAsk` 65，`default` 1（隔离探查） | 带了，沙箱照产品的翻译表 |
| G7 | `default` 8（含隔离探查），`dontAsk` 4（`g7-cfg-*`） | **常驻会话、中断、后台命令、续接各格没带产品设置**；两格只发 `initialize`，没有 `init` |
| G1 | `default` 11（含隔离探查） | **全部没带产品设置** |
| G8 | `default` 9（含隔离探查） | **全部没带产品设置** |
| G5 | `dontAsk` 2，`default` 1（隔离探查） | 环境行两格 |

所以**下面 G7 的进程事实、G1 的子代理事实、G8 的工具清单都是没开沙箱、权限模式 `default` 下量的**。工具清单后来在带产品设置的格子里见到的也是同样的 24 个（R2 补测）；进程关系在产品设置下还没量。

**G2 先行五格**（硬格子，全 PASS）

| 格子 | 结果 |
| --- | --- |
| Read 读工作目录外，hook 答允许 | 执行了，恰好一次调用，结果是文件内容 |
| Grep、Glob、只读命令（`cat` 工作目录内的文件）、子代理工具，hook 答拒绝 | 四格都没执行：工具结果是 `PreToolUse:<工具> hook error`，`result.permission_denials` 里有这一条 |

**G4 沙箱**（`dontAsk`）

- PASS：①a–①d、①f–①h、②（HTTPS 通）、⑤、⑦a、⑨a–⑨d。全开档下假凭据目录仍然读不到、写不了，不发起沙箱外重试。
- **⑩ 文件工具写工作目录外，hook 答允许 → 写得进**。
- 四条 FAIL，都是 CLI 的真实行为：

| 项 | 命令报了什么 | 对照 |
| --- | --- | --- |
| ①e 读只读区 | `cat: …/cred/readonly/ok.txt: Operation not permitted`，退出码 1 | ①e′ 去掉 `sandbox.credentials.files`：读到内容，退出码 0。`credentials.files` 的 `deny` 盖住了 `allowRead` |
| ②b `nc` 连 TCP 22 | `nc: getaddrinfo: nodename nor servname provided, or not known` | 沙箱外对照退出码 0。不认代理的程序在沙箱里连域名都解析不了 |
| ②c 绑本机端口 | `bind(('127.0.0.1',0))` 报 `PermissionError: Operation not permitted` | ②c′ 加 `sandbox.network.allowLocalBinding: true`：退出码 0 |
| ⑦b `gh auth status` | 输出没进包，只留摘要：270 字节，没提 TLS、连接、没登录 | 沙箱外对照退出码 0。原因不明 |

- ⑥ 被挡后请求沙箱外重试：5 次里模型发起了 1 次（输入带 `dangerouslyDisableSandbox: true`，探针拒了）。
- **④ `excludedCommands` 对拍**：语料 44 行，脚本测成 34 行，违例 0（CLI 判在沙箱外的 16 行，iWork 的规则也都判在沙箱外）。另有 4 行调起记录里有结果、脚本没认出来（`ls -la` 被用户 shell 的别名跑成 `ls -G -la`；`{ git pull; }`；`if true; then git pull; fi`；裸重定向 `> a.txt` 被 zsh 补成 `cat`），都在沙箱内。iWork 判「说不清」而 CLI 实测在沙箱内的 6 行：`git -c core.sshCommand=x pull`、`git -C sub pull`、`git --no-pager fetch`、`docker --context x push img`、`git pull > log.txt`、`git pull > log && git push`。`git pull 2>&1` 在沙箱外。没测成的 6 行：1 行模型改了命令，5 行命令里有带路径的词、替身顶替不了。
- **调起记录的祖先链**（39 个调起记录文件）：CLI 自己调的程序是它的直接子进程，374 笔（`git` 338、`npm root -g` 33、`gh pr view` 3），都在沙箱外；Bash 工具的命令隔着一层 shell，沙箱外的 20 笔直接父进程是 `zsh` 18、`gh` 2；**沙箱内的 26 笔祖先链全是「?」**，只拿得到直接父进程号。
- 命令是在带着用户 shell 别名的环境里执行的（上面 `ls` 那一行）。

**G7 流式输入、中断、进程**（除 `g7-cfg-*` 外没带产品设置）

- **前台 Bash 命令不在 CLI 的进程组里**：`sleep` 的进程组号等于它父 shell 的进程号，不是 CLI 的；调用的输入里没有 `run_in_background`。脚本是从 CLI 的进程号顺着父进程号找到它的。
- **`interrupt` 控制请求停得掉前台命令**：这一轮立刻以 `error_during_execution`、`terminal_reason=aborted_tools` 结束，`sleep` 没了，后面的 `touch` 没执行，同一进程接了下一轮。
- **后台 Bash 命令**同样自成进程组；`interrupt` 停不掉它（5 秒后还活着）；关掉输入后 CLI 30 秒内自己退出，那之后它也没了（这是 CLI 空闲、只有后台命令时量的）。
- **hook 进程自成进程组**：CLI 的进程组被 SIGKILL 之后 hook 进程还活着，3 次都是。
- **G7c′**：只发 `initialize`、不发用户消息，20 秒内有响应，`current_permission_mode` 是 `dontAsk`。给一份校验不过的设置文件时，取值是 `default`，会话还活着，流里没有 SessionStart 的 hook 事件。响应里另有 `pid`、`hook_on_failure_supported: true`。
- G7a、G7a′、G7b′、G7b″、G7h、G7j PASS；G7i 续接后原样重发 3 次结果相同；G7g 换模型前后 `assistant` 报的模型从 haiku 变成 sonnet。

**G1 工具与 hook 输入**（没带产品设置）

- G1a、G1b、G1c、G1e、G1f PASS；G1f 有 9 个探针运行有效，没有哪个工具结果没过 hook。
- **`init.tools` 是 24 个，没有 `ToolSearch`**：`Task`、`Bash`、`CronCreate`、`CronDelete`、`CronList`、`DesignSync`、`Edit`、`EnterWorktree`、`ExitWorktree`、`ListAgents`、`Monitor`、`NotebookEdit`、`PushNotification`、`Read`、`RemoteTrigger`、`ReportFindings`、`ScheduleWakeup`、`SendMessage`、`Skill`、`TaskStop`、`WebFetch`、`WebSearch`、`Workflow`、`Write`。有 `init` 的 107 个实例（含 3 个带 MCP 的）没有一个出现 `ToolSearch`；S0 的 25 个里有它。
- **G1d 子代理里的工具调用进 hook**：子代理发的 Bash 调用到了探针，hook 输入带子代理 id，流里那条调用带 `parent_tool_use_id`。
- **子代理默认在后台跑**：模型调子代理工具时输入里没有 `run_in_background`，CLI 报 `is_backgrounded: true`，工具结果当场返回「Async agent launched successfully」；子代理跑完后 CLI 自己又起一轮。一个 prompt 出了两个 `system/init`、两个 `result`（这一格是 `-p` 一次性运行，第一个 `result` 排在第二个 `init` 之后；常驻的流式输入下的次序见 R2 补测 n1）。
- **子代理工具的名字**：`init.tools` 与 `result.permission_denials` 里是 `Task`，模型发的 `tool_use` 与 hook 输入里是 `Agent`。

**G8 工具开关**（没带产品设置）

- G8a–G8d、G8f PASS，MCP 阳性对照成立。
- **G8e**：`--tools Read` 时 `init.tools` 是 `Read` 加 MCP 工具，MCP 工具调用成功。白名单不影响 MCP 工具。
- **G8g**：`--tools` 里写了不存在的工具名不报错，退出码 0，`init.tools` 只剩 `Read`。

**G5**

- 环境行 PASS：带着指向测试插件的 `CLAUDE_CODE_PLUGIN_DIRS` 时 `init.plugins` 里有它，干净时没有。
- **记忆开关在 `init` 里看得出来**：设了 `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` 的 105 个实例，`init` 里没有 `memory_paths` 这个键；没设的 2 个有，值是 `{"auto": "~/.claude/projects/<编码后的工作目录>/memory/"}`。
- **CLI 给工作目录编码时把 `/` 和 `_` 都换成 `-`**（根目录 `iwork-t0g-34_8u6x0` 对应 `…-iwork-t0g-34-8u6x0-g5-work`）。记忆读、记忆写、子代理记忆三行这一轮因此没测成（脚本拿原样的目录名去比）；重跑结果见 R2 补测。

**这一轮没量到的**（补测量了前三样）：前台命令跑着时硬杀 CLI 的进程组，命令的去向；只关输入、发 SIGTERM 时 CLI 收不收拾前台命令；`CLAUDE_CODE_DISABLE_BACKGROUND_TASKS` 的效果；⑦b 失败的原因；⑧c（`npm install` 真实依赖，被 `--skip-builds` 跳过）。

#### R2 补测（2026-11-12 下午，Claude Code 2.1.322，日常账号，普通终端起，模型 `claude-haiku-5-5`；四个结果包 `…153126-79f269`、`…161127-fe1169`、`…161240-a5754c`、`…161328-5de10e`，26 个实例加 4 个探查）

只收有结构化证据的。分析全文在 architect 的报告 `r2x-analysis.md`；据此定的 D2、D5、P3、P15 见 T06 方案 18 节（D5：M1 关掉后台任务、Monitor 进永久禁用名单，方向是 M2 开放；产品负责人 2026-11-12）。

**先看这一条：哪些格子是在什么设置下量的。** 脚本的权限模式和沙箱都只从设置文件来。下表「没带产品设置」的格子，设置文件里只有 hook 和记忆改道：`init` 报的权限模式是 `default`，**命令不在沙箱里跑**。R2 的 G7 各格（上文「G7 流式输入、中断、续接、进程」里 R2 那一轮的进程事实）也是这样。所以**下面的进程关系与信号行为都是没开沙箱时的**；产品里命令在沙箱里跑，CLI 到命令之间可能多一层，SIGTERM 时 CLI 收不收拾沙箱里的命令没量过。脚本已改成这几格带产品设置（main `af0b6a5`）；k3 带产品设置重跑过了，结果相同（见本节末「`--part skill` 与 k3 重跑」）。

| 格子 | 设置 |
| --- | --- |
| k1–k3（含 k4）、b1、b2、n1、工具清单五格 | **没带产品设置**：`default`、没开沙箱 |
| ①c′、①d′、p1、p2、p4 两格 | 带了：`dontAsk`，沙箱照产品那样 |
| G5 各格 | `dontAsk`；这几格不跑命令，沙箱设置没核对 |

**停 CLI 的三种办法**（前台命令 `/bin/sleep 61; touch <哨兵>` 跑着时；没开沙箱）

| 格 | 做法 | 结果 |
| --- | --- | --- |
| k1 | SIGKILL CLI 的进程组 | `sleep` 活着；70 秒后哨兵出现，命令接着跑完了 |
| k2 | 只关输入 | CLI 30 秒内没退，`sleep` 还在；流里有一条 30 秒的心跳，没有 `result`。命令跑完之后退不退没量 |
| k3 | 给 CLI 发 SIGTERM | 5 秒内 `sleep` 没了，那次调用的工具结果是 `Exit code 137`；CLI 退出，这一轮不出 `result` |

- **进程关系（k4，三格一致，没开沙箱）**：命令的 shell 是 CLI 的直接子进程，自成一个会话（会话号 = 进程组号 = shell 自己的进程号）；`sleep` 的父进程是 shell，进程组号、会话号跟 shell。hook 进程是 CLI 的直接子进程，进程组号是它自己。
- R2 里「关输入后 CLI 30 秒内自己退出」量的是 CLI 空闲、只有后台命令时；有前台命令在跑时不成立（k2）。

**`CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`**（常驻的流式输入；没带产品设置）

| 格 | 变量 | 结果 |
| --- | --- | --- |
| b1 | 设 | 子代理前台跑：`task_started` 带 `is_backgrounded: false`，子代理工具的结果当场带回子代理的最终报告；一个 `init`、一个 `result`；子代理里的 Bash 调用进了 hook，hook 输入带子代理 id。（这一格脚本记的是 INCONCLUSIVE：提示词里命令后面紧跟一个句点，模型抄进了命令，探针照规矩拒了；上面几样都有正面证据） |
| b2 | 设 | 带 `run_in_background: true` 的 Bash 调用被 CLI 拒掉，工具结果是 `InputValidationError: … An unexpected parameter run_in_background`，**这一次不进 hook**；模型改成不带这个参数重发，进 hook、执行 |
| n1 | 不设 | 子代理工具当场返回「Async agent launched」，`task_started` 带 `is_backgrounded: true`，流里有 `background_tasks_changed`。次序：`init` → `result` → 子代理的 Bash 调用（进 hook、执行）→ `task_notification` → `init` → `result`。主回合的 `result` 出来时子代理还没开始干活 |

- 设了变量，`init.tools` 仍是 24 个，`Monitor` 还在。
- 厂商文档说这个变量还会停掉「超时的命令自动挪到后台」，没实测。

**沙箱翻译**（带产品设置）：去掉 `sandbox.credentials.files` 之后，读、写假凭据目录里只读区之外的文件仍被挡（①c′ `cat: … Operation not permitted`，①d′ `touch: … Operation not permitted`，退出码都是 1，没发起沙箱外重试）。所以数据目录只进 `denyRead` + `denyWrite`、不进 `credentials.files`，别的部分照样挡得住。

**沙箱联网**（带产品设置：`allowedDomains: ["*"]`）

| 格 | 结果 |
| --- | --- |
| p1 沙箱里命令拿到的代理类环境变量（只记名字，不读值） | 23 个：`HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`、`NO_PROXY`、`FTP_PROXY`、`GRPC_PROXY`（各有小写版）、`RSYNC_PROXY`、`DOCKER_HTTP_PROXY`、`DOCKER_HTTPS_PROXY`、`CLOUDSDK_PROXY_ADDRESS` / `PORT` / `TYPE` / `USERNAME` / `PASSWORD`、`GIT_SSH_COMMAND`、`SRT_SSH_PROXY_COMMAND`、`SANDBOX_RUNTIME` |
| p2 用 `SRT_SSH_PROXY_COMMAND` 连 `github.com 22` | 读到 `SSH-2.0-`，退出码 0。会走代理的程序，非 HTTP 协议、非 HTTP 端口也出得去 |
| p4 连脚本在沙箱外起的本机监听（只绑 127.0.0.1） | 带 `sandbox.network.allowLocalBinding: true`：读到问候，监听端接到 1 次连接。不带：连接失败，退出码 1，监听端 0 次 |

**工具名与工具清单**（没带产品设置；带产品设置的 6 个实例 `init.tools` 也是同样的 24 个）

| 参数 | `init.tools` |
| --- | --- |
| `--tools Read,Task` | `Task`、`Read` |
| `--tools Read,Agent` | `Task`、`Read` |
| `--disallowedTools Task` | 23 个，没有 `Task` |
| `--disallowedTools Agent` | 23 个，没有 `Task` |
| `--disallowedTools Bash` | 24 个：少了 `Bash` **和 `Monitor`**，多了 `Glob`、`Grep` |

子代理工具的两个名字 CLI 都认；`init.tools` 里报的总是 `Task`。

**自动记忆**（G5 重跑，`dontAsk`；工作目录是本次新建的）

| 格 | 配置 | `init.memory_paths` | 结果 |
| --- | --- | --- | --- |
| m0 | 不设开关、不改道 | `auto` = `~/.claude/projects/<编码后的工作目录>/memory/` | 没用任何工具就答出了放在那里的暗号 |
| m1 | 不设开关、设置里 `autoMemoryDirectory` 改道 | `auto` = 改道目录 | 答出了放在改道目录里的暗号。改道经 `--settings` 生效 |
| m2 | `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` 加改道（产品配置） | **没有这个键** | 没答出暗号 |
| w0 | 不设开关，给 Write、Edit | 同 m0 | 模型用 Write 把暗号写进了默认记忆目录 |
| w2 | 产品配置，给 Write、Edit | 没有这个键 | 没有任何工具调用；默认目录的文件没变 |

- 这一轮 9 个 G5 实例里，设了开关的 4 个 `init` 都没有 `memory_paths` 键，没设的 5 个都有。和 R2 的 105 比 2 同一个规律。
- 访问时间在这台机器上不能当证据：m0 的空白对照文件被别的程序读过。
- 环境行：带着指向测试插件的 `CLAUDE_CODE_PLUGIN_DIRS` 时 `init.plugins` 里有它，干净时没有。

**仓库里的子代理**（G5 守卫预检那一格，`dontAsk`，`--setting-sources ""`）：工作目录 `.claude/agents/` 里放的测试子代理不在 `init.agents` 里（只有 5 个内置的：`claude`、`Explore`、`general-purpose`、`Plan`、`statusline-setup`），调它时工具结果是「Agent type '…' not found」。**只有这一格，没有阳性对照**；对照等 `--part skill` 的 L2。

**没量到的**：产品设置下的进程关系与 SIGTERM 的效果；SIGTERM 时后台命令的去向；SIGTERM 之后这条会话能不能续接；关输入后命令跑完 CLI 退不退；子代理的 `memory` 字段在记忆开关下的效果；回合中途发第二条用户消息时 CLI 怎么处理（评估报告里设计了格子，没跑）。

**运行前后核对**：三个包 PASS。`…161328-5de10e` 报 FAIL：变化的 52 项全在另一个 `~/iwork-t0g-<随机>/` 根目录下，是同一时间有人跑 `--selftest` 留下的（文件名只出自脚本的 selftest 函数），原有的 10 个结果包与 `~/.npmrc` 没变，四格联网的结论不受影响。脚本已改：selftest 与实测互斥（main `af0b6a5`）。

#### `--part skill` 与 k3 重跑（2026-11-12 17:51–18:00，Claude Code 2.1.322，日常账号，普通终端起，`claude-haiku-5-5`；结果包 `…175237-8720d6`（skill）、`…180013-52d68a`（k3），8 个实例加 2 个探查）

只收有结构化证据的。分析全文在 architect 的报告 `r2y-analysis.md`。**各格 `init` 报的权限模式都是 `dontAsk`**（设置文件带了权限模式与沙箱）。

**k3 重跑**（前台命令 `/bin/sleep 61; touch <哨兵>` 跑着时给 CLI 发 SIGTERM；设置是 `dontAsk` 加产品那样的沙箱）

- 进程关系和没开沙箱时一样：命令的 shell 的父进程是 CLI，会话号 = 进程组号 = 它自己的进程号；`sleep` 的父进程是 shell，组号、会话号跟 shell；hook 进程的父进程是 CLI，进程组号是它自己。
- SIGTERM 后 5 秒内 `sleep` 没了，那次调用的工具结果是 `Exit code 137`；CLI 30 秒内退出，不出 `result`。
- **沙箱在这一格里没有直接证据**：没有哪个动作被沙箱挡下来，结果包里没有设置文件。间接的：设置文件生效了（权限模式是它给的），同一份设置里有 `sandbox.enabled: true` 与 `failIfUnavailable: true`，命令的输入里没有 `dangerouslyDisableSandbox`。
- k1、k2 没在产品设置下重跑。

**仓库里的 CLAUDE.md、技能、子代理、命令**（同一个工作目录里各放一个，CLAUDE.md 带暗号）

| 格 | 参数 | `init` | 问暗号 |
| --- | --- | --- | --- |
| L1 | 产品参数（`--setting-sources ""`） | `skills`、`slash_commands`、`agents` 里都没有仓库的那几个（`agents` 只有 5 个内置的） | 答 `NONE`，没调工具 |
| L2 | `--setting-sources project` | 三样都在 | 没调工具就答出了暗号 |

- **AGENTS.md 没测**：这两格放的是 CLAUDE.md。每个实例的 `init.plugins` 里都有内置插件 `cc-plugin-agents-md`，它做什么没量。

**技能正文里的内联命令**（技能做成测试插件、经 `--plugin-dir` 给；用斜杠命令调起，不经 Skill 工具；探针对一切调用答拒绝）

| 格 | 设置 | 结果 |
| --- | --- | --- |
| K3b | 技能带 `allowed-tools: Bash(touch:*)`，正文里有内联的 `touch <标记>` | **标记出现了，探针的 PreToolUse 一次都没被调用**；流里没有 `tool_use`；交给模型的技能内容里那一处是 `(Bash completed with no output)` |
| K3c | 同上，`--settings` 里加 `disableSkillShellExecution: true` | 标记没出现；那一处是 `[shell command execution disabled by policy]` |

- 没量：不带 `allowed-tools` 时的内联命令；只读的内联命令；内联命令在不在沙箱里跑。

**技能的 `allowed-tools` 与插件的 hook**

| 格 | 设置 | 结果 |
| --- | --- | --- |
| K2c 预检 | 同样的沙箱，好的探针 | 工作区内写得进；工作区外写不进（`Operation not permitted`）、读不到 |
| K2c | 插件技能带 `allowed-tools: Bash`；探针对目标命令不表态（退出码 0、不输出）；`--tools Bash` | **`touch <标记>` 执行了**，标记出现。S0 的矩阵里同样的不表态、没有这种技能时它不执行（对照不是同一次运行里的） |
| H1 | 插件自带的 PreToolUse hook 答允许，iWork 的探针答拒绝 | 没执行：工具结果是 `PreToolUse:Bash hook error`，`result.permission_denials` 有这一条；插件的 hook 被调用过并答了允许 |

- 没量：探针不表态而插件 hook 答允许时的结果；插件 hook 改写工具输入的情形；内置技能自带的放行名单（2.1.322 的程序里有这类字样，没对到具体技能）。

**运行前后核对**：两个包都 PASS。登记之外的调用 0 次。

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

为 T04 方案第 8 节 N1–N5 补的实测。脚本在 `tools/t0/iwork-t04-*`，原始输出在 `tools/t0/out/t04-*`（临时目录与家目录已脱敏）。N5 先做：第一轮 A1 不通过（7.1），产品负责人定了修法（`check_image_file` 不验包资源）后复测全部通过，再做 N1–N4。

### 7.1 N5 真实包上完成依据与签名能不能同时成立

环境：macOS 27.2（26B5086k）arm64，测时 loadavg 16–22（提交的输出是修法之后那一轮）；`pnpm -C client tauri build --bundles app` 的真实产物 + `cargo build --release -p iwork-daemon`。脚本 `tools/t0/iwork-t04-bundle-probe.sh`（macOS）与 `tools/t0/iwork-t04-linux-bundle.sh`（Linux，Docker `rust:1`，Linux 7.0.11 aarch64）。

后台现在还是空壳、客户端还不会连后台，所以「握手」与「运行映像身份」两件事用探针冒充两个角色，放在真实包的同一位置、按同样方式签名或登记清单：`iwork-t04-ipcprobe.rs`（依赖 `iwork-ipc`、**不开 dev feature**，走发布构建的 `RequireSigner`，`SelfIdentity::detect` 与对端判定都是 T03 已合入的代码）；`iwork-t04-imgprobe.c`（与 `peer/macos.rs` 同一串 Security 调用）。客户端主程序、资源、`Info.plist` 都是 tauri 的原样产物。两种包形态里「带清单那一行」「整包签名那一行」的判定按方案 2.9 的定义用 shell 写了一遍。

**第 0 项：tauri build 没配签名身份时不做整包签名。** 包里没有 `Contents/_CodeSignature`，主程序只有链接器的 ad-hoc 签名（`flags=0x20002(adhoc,linker-signed)`），与 `target/release/iwork-client` 逐字节相同。所以无证书形态不用额外去掉整包签名。

| 项 | 结果 | 说明 |
| --- | --- | --- |
| A1 启动 | 通过 | 真实客户端命令行起、`open` 起，5 秒后都在 |
| A1 两端互认 | 第一轮**不通过**，修法之后通过 | 第一轮：后台位置 `manifest_loaded{self_listed=true}`，**客户端位置（包的主程序）`SelfIdentity::detect` 失败**：`SecStaticCodeCheckValidity 失败（OSStatus -67056）`。修法之后：两个角色都 `self_listed=true`、握手成功；把客户端换成清单外的映像，后台以 `ImageNotTrusted` 拒 |
| A2 | 通过 | 真实客户端运行中的 cdhash（`codesign -dvvv <pid>`）= 清单 `cdhash:`；后台位置运行映像 cdhash = 清单 `cdhash:`；两个 `sha256:` = 文件全文哈希 |
| A3 | 通过 | 完整 → `Complete`；删清单 → `NotThisForm`；后台截断一半 → `Incomplete`；整包签名那一行 → `NotThisForm` |
| A4（记录） | — | `codesign --verify --deep --strict` 与 `spctl` 都报 `code has no resources but signature indicates they must be present`；带隔离标记时命令行直接起主程序照常运行；双击走 Gatekeeper 弹窗，无人值守没做 |
| B1 | 通过 | 自签证书整包签名、包里不放清单：`codesign --verify --deep --strict` 与 `SecStaticCodeCheckValidity`（严格 + 嵌套 + 全部架构）都过；整包签名那一行 `Complete`、带清单那一行 `NotThisForm`；对照：两个角色（证书签名）握手成功 |
| B2 | 通过 | 删资源、截断后台、截断客户端、改 `Info.plist` 一字节、多放一个文件：五种都验签不过、判 `Incomplete` |
| B3 | 通过 | 后台位置磁盘静态 cdhash = 运行中 cdhash；换构建标记重签后两者都变；真实客户端（证书签名）运行中 = 静态 |
| B4（记数） | — | 整包验签每次 20–84 ms（包 10 MB，两轮各 5 次，高负载下数值波动大） |
| B5 | 通过 | 签名后加清单 → 验签不过；签名前把清单放进 `Contents/MacOS` 再签 → codesign 直接失败（`In subcomponent: …/iwork-manifest.json`，它把 `MacOS` 下的文件当代码）。两条路都走不通，「有证书的包不放清单」不是可选项 |
| B6 | 通过 | 拿走一个资源 → `Incomplete`；放回 → `Complete`；三个时刻后台程序与封印文件的 size/mtime/inode 都相同 |
| C（Linux） | 通过 | 两个二进制 + 清单打 tar 再解开：带清单那一行 `Complete`、截断 → `Incomplete`、删清单 → `NotThisForm`；两端按清单握手成功；启动时的映像身份 = `/proc/self/exe` 的 SHA-256，路径上换文件后每次重取仍是原值；T03 `peer_manifest` 6 条通过 |
| D1–D3 | 通过 | A、B 两种包、mv 与 rm 两种换法：运行映像 cdhash 都取得到、等于自己路径的静态 cdhash；换文件后重取只会是原值（mv）或 -67034（rm），从不报成新文件；同一张证书签的两个构建取到的值不同；读静态 cdhash 不读全文（文件中部改坏一字节照样取到原值，10 MB 文件几毫秒到二十几毫秒，与 50 KB 的文件同一量级，不随文件大小增长）；通用二进制取的是本机架构那一片 |
| E1 | 通过 | A 目录整包换成 B → 清单那一行 `NotThisForm`、签名那一行 `Complete`；反方向对称 |
| E2（记录） | — | B 合并拷进 A（旧清单残留）→ 两行都 `Incomplete`，验签 -67054；删掉残留清单后签名那一行 `Complete` |

**A1 不通过的根因**（`A1g`）：同一个探针放在三处，对自己的运行映像做 `peer/macos.rs` `check_image_file` 那一步（`SecCodeCopyStaticCode` → `SecStaticCodeCheckValidity`）：

| 位置 | 默认参数 | `kSecCSDoNotValidateResources` | 动态校验 |
| --- | --- | --- | --- |
| 未封印 .app 的主程序（`CFBundleExecutable`） | **-67056**（`errSecCSResourcesNotFound`） | 0 | 0 |
| 未封印 .app 的 `Contents/MacOS` 里另一个程序 | 0 | 0 | 0 |
| 散放的二进制（T03 实测的形态） | 0 | 0 | 0 |

包的主程序的静态代码对象代表整个包：链接器签名没有资源封印，按默认参数就判「签名说有资源、却找不到」。T03 的实测（6.3）只测过散放的二进制，没碰到。影响两个方向：客户端算不出自己的身份、拒绝启动；后台判客户端这个对端时走同一个 `check_image_file`，同样会得到 `Unavailable` 并拒绝。后台（不是主程序）不受影响。

**修法**（产品负责人定，提交 `b34b518`）：`check_image_file` 的静态校验带上 `kSecCSDoNotValidateResources`。它只跳过包资源，代码页哈希照样验；T03 加这一步本来就是为了验「还没调入的页」，用 T03 的实测脚本重跑后，这两项仍是 -67061（`tools/t0/out/t04-t03-rerun/`）。代价是握手时不验对端包里的资源、嵌套程序与 Frameworks 里的库（这个参数把它们一起放过了），T04 的安装检查也不覆盖对端；登记为已知缺口，见技术设计第 1 节。约束：有证书的包里不放会被加载的 dylib、framework 或影响行为的资源；嵌套程序可以放（如 Contents/MacOS 里的 iwork-daemon），前提是别的进程只经握手校验过的连接和它往来；被信任进程直接起、且不经握手就信其结果的程序同样不放。修法之后：形态 A、B 两个角色都握手成功；把客户端换成清单外的映像（换签名标识重签，cdhash 必变），后台以 `ImageNotTrusted` 拒绝；平台测试见 `crates/iwork-ipc/tests/peer_bundle_macos.rs`。注意「在文件末尾追加一字节」造不出清单外的映像：签名之外的尾部不在代码目录覆盖范围内，cdhash 不变。

**D4（修法之后加的复核）**：用 `iwork-ipc` 的正式函数（T04 第 3 步的 `running_image_id`、`file_image_ids`、`verify_bundle`）在真实包里再走一遍：两种包里运行映像 = 自己路径的文件身份 = `codesign` 读的 cdhash；`verify_bundle` 对未封印的 A 包报错、对整包签名的 B 包通过；同一张证书签的两个构建，`SelfIdentity` 的签名者相同、运行映像不同。

### 7.2 N1 macOS 登录自启：SMAppService 与 LaunchAgent 退路

当时的脚本是 `iwork-t04-macos-smappservice.sh` + `iwork-t04-smprobe.m`（一个二进制两种用法：host 调 register/unregister/status，agent 被 launchd 起），本表是它的结果。产品负责人 2026-11-11 定 macOS 只走 LaunchAgent 之后，SMAppService 的实测部分已从脚本里删掉，脚本改名为 `iwork-t04-macos-launchagent.sh` + `iwork-t04-launchprobe.m`，只测 LaunchAgent，用固定的 label 与包路径（重跑复用同一条后台项目记录），输出在 `tools/t0/out/t04-launchagent/`，结果见本节末尾。探针在 sleep 模式下每 0.2 秒重读模式文件，模式一变就由正在跑的实例自己结束，不用 `kickstart -k` 换实例。所以「再拉」只数 launchd 的重启。当时的输出在 `tools/t0/out/t04-smappservice/`。

| 项 | 结果 |
| --- | --- |
| 1–4 能不能注册 | 未签名的 arm64 程序起不来。**链接器签名、不做整包签名（= N5 形态 A）：`register` 失败**，`SMAppServiceErrorDomain` 3「Codesigning failure loading plist … -67056」，和 A1 同一个根因：SMAppService 要求包有资源封印。ad-hoc 整包签名：主程序或包内另一个程序调用都能注册，立刻起一个实例，`unregister` 时实例收到 SIGTERM。自签证书整包签名：注册成功，状态 `Enabled`，但 3 秒内 launchd 停在 `spawn scheduled`、探针没起来（前两轮都这样，没再追） |
| 5 | `register` 立刻起一个实例；手动起的实例握着锁时，launchd 起的实例停在等锁；手动实例被 kill -9 后，它在毫秒内拿到锁 |
| 6 `KeepAlive={SuccessfulExit=false}` | exit 0 不再拉；exit 12、20、21、SIGKILL、SIGTERM、SIGSEGV、abort 都在约 10.0–10.1 秒后再拉（`ThrottleInterval=10`）。SMAppService 与退路结果相同 |
| 6 对照 `KeepAlive={Crashed=true}` | 只有 SIGSEGV、abort 再拉；exit 非零、SIGKILL、SIGTERM 都**不**再拉。这就是方案不用 `Crashed` 的原因 |
| 7 实例自己注销 | SMAppService `unregister` 与退路的 `launchctl bootout` 都**不返回**：调用后几十毫秒内实例收到 SIGTERM、被结束，任务随之消失，之后不再被拉。从别的进程注销：实例收到 SIGTERM、结束 |
| N3 第 3 项 | launchd 管着的进程 exec 自己之后 pid 不变、仍是 running；再 kill -9 它，10 秒后照常被拉起，说明它仍归这个任务管（两条路一样） |
| 9 挪动 .app | `launchctl print` 显示的是包内相对路径（`program identifier = Contents/MacOS/agent`）。挪了位置后，`status` 仍是 `Enabled`，kill 之后照样被拉起（smd 跟着包走）；从新位置 `unregister` 正常 |
| 10 BundleProgram 与 Program | plist 用 `BundleProgram`、`launchctl bootstrap gui/<uid>`：不管 plist 放在 `~/Library/LaunchAgents` 还是临时目录，链接器签名、ad-hoc 签名、自签证书签名，**都报 rc=5（Input/output error）**。只把这个键换成 `Program`（包内程序的绝对路径），每一格都 rc=0、能起来。结论：`BundleProgram` 只在 plist 放在包里、经 SMAppService 注册时才认（T09 通知探针的观察成立）。例外是链接器签名 + `~/Library/LaunchAgents` 那一格，两轮都报「写入后自己加载=yes」：刚被 SMAppService 注销的同名任务又被 smd 提交了一次，所以 bootstrap 报 5，这一格不算数 |
| 退路写入 plist | 写进 `~/Library/LaunchAgents` 后系统不会自己加载，要 `bootstrap`。用 `Program` 时三种签名都能起来（自签证书的包走这条路也正常，只有经 SMAppService 时停在 `spawn scheduled`） |

**删掉 SMAppService 之后的复测**（`iwork-t04-macos-launchagent.sh`，固定 label 与包路径，输出 `tools/t0/out/t04-launchagent/`，全程无弹框）：第 10 节三种签名 × 两处 plist 位置，`BundleProgram` 一律 rc=5、`Program` 一律起得来，这一轮没有「写入后自己加载」的格子（上表那一格确是 SMAppService 的残留）；第 5–7 步与 N3 第 3 项结果同上表（`SuccessfulExit=false` 下 exit 0 不拉，其余在 10.0–10.2 秒后再拉；`Crashed=true` 只拉 SIGSEGV、abort；实例自己 bootout 不返回、随即收到 SIGTERM；别的进程 bootout 后实例结束，这一轮日志里没记到 SIGTERM 那一行）。

结论：**无证书的包（M1 实际的发布形态）上 SMAppService 用不了**，按 Q3 走退路：`~/Library/LaunchAgents/<label>.plist` + `launchctl bootstrap` / `bootout`，plist 用 `Program = <包内 iwork-daemon 的绝对路径>`，不能用 `BundleProgram`。方案 2.12 关于注册、注销的假设（注册立刻起实例、注销结束正在跑的实例）两条路都成立，对齐表不用改。

清理：每轮结束都核对了：`launchctl print` 查不到这个任务，`~/Library/LaunchAgents` 里没有这个 plist，没有残留探针进程。系统后台项目数据库（BTM）里会留下 T04Probe 的记录，指向的包已经删了，不会再起任何东西。逐条删除要 root，`sfltool resetbtm` 又会清掉所有应用的记录，所以没动。

### 7.3 N2 单调钟在睡眠期间走不走

脚本 `tools/t0/iwork-t04-sleep-clock.sh` + `.rs`。没有让本机真的睡：`pmset schedule wake` 要 root，否则机器睡下去要等人来唤醒。做法是在同一时刻读 `Instant` 和几个系统钟，看 `Instant` 跟哪个钟对得上；再看「计睡眠的钟 − 不计睡眠的钟」。

- macOS：`Instant` 等于 `CLOCK_UPTIME_RAW`（不计睡眠）。开机以来 `CLOCK_MONOTONIC_RAW`（计睡眠，等于 `mach_continuous_time`）比它多 9.5 秒，电源日志记了 4 次短睡眠，对得上。所以 `Instant`（以及建在它上面的 tokio 计时器）在睡眠期间不走。**成立**。
- Linux（Docker 里的 OrbStack 虚拟机）：`Instant` 等于 `CLOCK_MONOTONIC`。这台虚拟机刚起来、没挂起过，`BOOTTIME − MONOTONIC = 0`，**挂起期间不走这一点没实测到**，只靠内核文档（`CLOCK_MONOTONIC` 不计挂起）。要实测得有能挂起的实体机或虚拟机，跑 `<程序> watch 180` 时挂起 60 秒以上。
- 想在 macOS 上真睡一次：`<程序> watch 180` 跑着时合盖或 `pmset sleepnow`，看 `wall_minus_instant`。

### 7.4 N3 exec 自行重启

脚本 `tools/t0/iwork-t04-exec-restart.sh` + `.rs`，macOS 与 Linux（Docker）各跑一遍，结果相同：

- 第 1 项：进程持有 `try_lock` 的锁和一个监听中的 socket，exec 自己之后：旧 socket 连接被拒（监听 fd 随 CLOEXEC 关掉了），能重新拿到锁，unlink 后能重新 bind。
- 第 2 项：exec 之前用 setsid 起的子进程（stdin 是管道）在 10–12 ms 内读到 EOF。
- 第 3 项：见 7.2（launchd）与 7.5（systemd）。exec 后仍归同一个任务管。
- 第 4 项：A 握锁，B 阻塞在 `File::lock()`；kill -9 A 之后 B 在 2–9 ms 内拿到锁，再 exec 自己，新映像能再拿到锁。

### 7.5 N4 systemd 用户单元

脚本 `tools/t0/iwork-t04-linux-systemd.sh`：Docker 里起一个以 systemd 为 1 号进程的 ubuntu:24.04 容器（systemd 255），用户 t 开 linger 得到用户实例，跑完删掉容器。单元配置照方案 2.12：`Restart=on-failure`、`RestartSec=10`、`StartLimitIntervalSec=300`、`StartLimitBurst=10`。

| 项 | 结果 |
| --- | --- |
| `enable --now` | 立刻起一个实例 |
| exit 0 | 不再拉 |
| exit 12、20、21 | 10.1–10.3 秒后再拉 |
| SIGKILL、SIGSEGV、SIGABRT | 10.1–10.2 秒后再拉 |
| **SIGTERM** | **不再拉**：systemd 把 SIGTERM（以及 SIGHUP、SIGINT、SIGPIPE）结束的进程当作干净退出（`Result=success`）。和 launchd 不同，见下 |
| exec 自己 | MainPID 不变、状态 active；之后 kill -9 照常被拉起 |
| `disable --now` | 正在跑的实例收到 SIGTERM、结束，单元 disabled |
| 次数上限 | 一直以 20 退出时，125 秒内起 9 次后单元进入 failed，不再拉 |

和方案的差异：方案写的是「非零或被信号结束就再拉」，systemd 默认对 SIGTERM 不再拉。后台自己装了 SIGTERM 处理，按 2.2 写 `Stopping{Signal}` 并报 0，本来就不该再拉，结果一样。只有在装上信号处理之前就被 SIGTERM 结束时，两者才不一样：状态文件停在 `Starting`、判据说 `Yes`，systemd 却不拉，要等客户端按需拉起。要和 launchd 完全一致，单元文件可以加 `RestartForceExitStatus=SIGTERM`。这一点交方案决定。

用真的后台在没有客户端的情况下走一遍（S11c），要等第 4 步有了后台再做，归第 9 步。

### 7.6 实测中踩到的弹框（2026-11-11）

两种操作会在用户的屏幕上弹框，无人值守的脚本里都不能出现：

- **临时钥匙串自动上锁**：`security create-keychain` 建的钥匙串默认 `lock-on-sleep timeout=300s`。脚本开头解锁后，几分钟后才签名的话，codesign 会弹框找人要钥匙串密码。现在的做法是：建好后 `security set-keychain-settings "$KC"`（不带 `-l`、`-t`，实测变成 `no-timeout`），每次签名前再 `security unlock-keychain`，签名都带 `--keychain`。被 timeout 掐断时照样走 cleanup 删掉钥匙串（bash 3.2 收到 TERM、INT 也会跑 EXIT trap，实测）。
- **`sfltool dumpbtm`**：读后台项目数据库要管理员授权，会弹 SecurityAgent 授权框（用 CGWindowList 抓到了屏幕上的对话框）。脚本里不再调用。

## 附：T06 S0 实测运行说明（给产品负责人；2026-11-11，developer 按 Claude Code 2.1.322 校对）

T06 方案 16.2、16.3 要在真机上重跑的东西，按下面的顺序跑。脚本都在 `tools/t0/`，在仓库根目录下跑。跑出来的结论写进本文 2.4 节（新加），一项一行，注明 CLI 版本。

### 先说清楚几件事

- **会花订阅额度**。每次 `claude -p` 起一个实例就是一次真实的模型请求。下面每一步都写了大约起几个实例。数法：脚本给每个实例留一份 `<用例名>-stream.jsonl`（G7h 被杀掉的那几个不留），结果包里数这类文件就是这一步实际起了几个。
- **`iwork-t0g.py` 只动自己建的目录**：`~/iwork-t0g-<随机>/`，跑完自己删；结果包是 `~/iwork-t0g-results-<时间>-<随机>.tar.gz`，不覆盖已有文件。两处例外：`~/.npmrc` 被写进沙箱的禁读名单（不读内容）；G4 ⑧ 的最小构建会写 `~/.npm`、`~/.cargo` 这些工具链缓存，不想动就加 `--skip-builds`。G4 的四条联网用例另由脚本自己在沙箱外各跑一次当对照，**默认会跑**：`curl https://example.com`、`nc github.com 22`、绑定并连接一个本机端口、`gh auth status`（会带着本机的 `gh` 登录去问 GitHub），只记退出码、输出丢弃，不写任何文件。不想让它们跑就加 `--no-host-control`，这时那四行对照记一行 INFO「跳过」，沙箱里那四条失败时就分不清是不是沙箱造成的。
- **结果包里有本机登录名**：hook 探针记下的输入路径（`/Users/<登录名>/…`）和 `system/init` 里横线编码的路径（`-Users-<登录名>-`）都带着它，脚本不脱敏（产品负责人 2026-11-12 定不改，脚本审查 1/F-7）。结果包只留在本机；要外传先自己处理。
- **结果包里没有账号信息**（2026-11-12 起）：`initialize` 控制请求的响应带登录账号，脚本收到就删掉，存下的流和 `summary.txt` 里都没有（G7c 一行只记键名和模型列表）。结果包新带一个 `obs/` 目录，是命令自己写下的输出与退出码、命令替身的调起记录；`gh auth status` 的输出可能有账号名，只留退出码和几项特征（`g4-7b.out.withheld.json`）。**2026-11-12 之前跑出的结果包**（S0 第一轮那份）`g7-live-stream.jsonl` 里有账号全文、`summary.txt` 的 G7c 一行有前 800 个字符，不要外传，要留档先删掉这两处。
- **另外三个老脚本会清空并重写固定目录**：`~/iwork-t0-results/`、`~/iwork-t0b-results/`、`~/iwork-t0f-results/` 和同名的 `.tar.gz`。跑之前看一眼，有要留的先挪走。
- **登录状态只留四个字段**：`iwork-t0.sh`、`iwork-t0c.py`、`iwork-t0f.py` 落盘前只留 `claude auth status` 的 `loggedIn`、`authMethod`、`subscriptionType`、`loginExpiresAt`，不是 JSON 时只记字节数；`codex login status` 只记退出码与字节数（AGENTS.md 安全不变量：邮箱等不落盘）。`iwork-t0.sh` 因此要 `jq`。
- **G2、G3、G5、G9d 的设置按产品设置写**（方案 3.2）：`disableAllHooks: false`、`permissions.disableBypassPermissionsMode: "disable"`、PreToolUse hook `timeout: 7200`（只有「超时后才拒绝 / 允许」两格是 5 秒，那是被测的东西；G3 测的就是各种 timeout 取值）、SessionStart 报到 hook（`timeout: 30`）。外层保护的预检与故障格子用同一份设置，只差被测的 hook。有意和产品不同的三处：`sandbox.allowUnsandboxedCommands` 写 `false`（外层保护：故障格子里坏掉的 hook 拦不住沙箱外重试；产品手动、自动档是 `true`）；不写 `sandbox.network.allowedDomains`（`["*"]` 本身要 G4 ② 实测，写进去万一校验不过，`-p` 模式会把整个设置文件静默忽略）；不写 `sandbox.credentials.envVars`（G9b 单独测）。文件系统的禁读名单用外层保护那一套，比产品的底线清单宽。
- **结论只认四种证据**：流里的结构化工具调用与结果、哨兵 / 观测文件、hook 探针的调用记录、常驻会话活着时的生命周期事件。不看模型怎么说。每行是 `PASS` / `FAIL` / `INCONCLUSIVE`（没测成）/ `INFO`（只记录）。`summary.txt` 最后一行是合计；有 `FAIL` 或 `INCONCLUSIVE` 时脚本退出码是 1，这是正常的，看具体哪几行。

### 跑之前要准备的

| 要什么 | 为什么 |
| --- | --- |
| 日常账号已经登录 Claude Code（`claude` 能正常回话） | 所有组都要 |
| 一个普通终端（不是 Claude Code 里的终端），环境里没有 `CLAUDE_CODE_PLUGIN_DIRS` 这类变量 | P14：脚本不筛选环境变量，干净的环境靠这一条（见下文「环境变量」） |
| `python3`（3.9 以上，只用标准库）；macOS 上要有 `lsof`（系统自带） | 脚本本身；进程清理靠 `lsof` 找工作目录在根目录里的进程，找不到就不开跑 |
| `cargo`、`npm`、`curl`、`nc`、`gh` 在 `PATH` 上 | G4 的 ②、⑦、⑧；没有的那几行会是 `FAIL`（命令自己退出码非零），记下就行，不影响别的行 |
| G4 ④ 的语料 `--corpus corpus.json` | 来自 `cargo run -p iwork-core --example dump-credential-corpus > corpus.json`（T07 交付，已合入）。不加 `--corpus` 时 G4 ④ 记 `INCONCLUSIVE` |
| 一次性环境（临时 macOS 用户或虚拟机） | G2 的故障格子、G5、G9d 的完整结论只在那里拿得到，见第 4 步 |
| 一台 Linux（能临时改名 `bwrap`） | 16.2 第 3 项 |
| 一块能合盖的 MacBook，人在旁边 | 16.2 第 1 项 |

### 顺序与估算

时间按每个实例 20～60 秒估（G3、G7 里有故意等的，单算）。实例数是从脚本里数出来的上限，实际会少一些（运行门没过的格子不起实例）。

| 步 | 在哪跑 | 命令 | 大约多久 | 大约起几个实例 | 看什么 |
| --- | --- | --- | --- | --- | --- |
| 1 | 日常账号 | `python3 tools/t0/iwork-t0g.py --selftest` | 15 秒 | 0 | 最后一行 `selftest ok`；跑完家目录下没有 `iwork-t0g-*` 残留 |
| 2 | 日常账号 | `python3 tools/t0/iwork-t0g.py --only G2 --quick` | 半小时左右 | 不超过 33：探查 1 + 选定配置（外层保护预检 5 + 普通 Bash 一行 11）+ 去掉 onFailure 的对照（预检 5 + read 行 11） | `外层保护 [...]` 那 2 行：哪几样保护在这台机器上成立。`G2 硬格子`、`G2 范围记录` 两行一定是 `INCONCLUSIVE`（`--quick` 不许进已验证表），正常 |
| 3 | 日常账号 | `python3 tools/t0/iwork-t0g.py`（全量，不带 `--disposable-env`） | 半天 | 约 200（不带 `--corpus`），见下面的细账 | G1、G3、G4、G6、G7、G8、G9a～c 的结论；G2 的格子哪些因为外层保护没成立而没跑；G5、G9d 一定是 `INCONCLUSIVE`（要一次性环境），正常；G2 只在一次性环境跑的那些格子（沙箱外重试、工作目录外的读的故障格、补充格、「不问工具」记录行、出沙箱格对）和两行总结论也一定是 `INCONCLUSIVE`，正常 |
| 4 | 一次性环境 | 见下文 R3 | — | — | **G2 的 PASS 只有这一步拿得到** |
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
| G2 | 日常账号约 105 | 候选配置 = `--modes` 里的每个模式（默认 `dontAsk`，方案 3.4a 写死）× 2：选定配置（带 `onFailure: "block"`）与去掉 onFailure 的对照。选定配置：预检 5、普通 Bash 一行 11，全 `PASS` 才往下跑；日常账号下能跑的是 bash 另 5 行、write、read、mcp 共 66 格，加「工作目录外的读」的允许格 1、只读命令的记录格 1、四个拒绝格、只读命令的失败格 1。对照：预检 5、read 行 11（结果只记 INFO，不计入总结论）。只在一次性环境跑的格子在日常账号下不起实例 |
| G3 | 11 | 预检 5 + 六个 timeout 取值，每个 hook 睡 90 秒 |
| G4 | 31 + 语料行数 | 基础 19（含 ①c′、①d′、①e′、②c′ 四格对照）、最小构建 3（`--skip-builds` 就没有）、⑥ 重试 5、⑨ 全开档 3、⑩ 写工作目录外 1；联网的四条另有沙箱外对照，不起实例 |
| G5 | 11 + 7 | 环境行与记忆各行 11 个日常账号也跑（守卫预检没过的行不起）；仓库设置的三种写法与对照 7 个只在 `--disposable-env` 下跑 |
| G6 | 10 | 思考强度 6、不存在的模型 1、换模型续接 2、换系统提示 1 |
| G7 | 10 + 2 × `--repeat` | 常驻会话 4（含后台命令一格）、只发 initialize 2（G7c′）、换配置续接 4、杀进程续接每次 2（默认 `--repeat 3`） |
| G8 | 8 | 工具清单 4、MCP 3、白名单里写了不存在的工具名 1（G8g） |
| G9 | 1～2 | G9d 只在 `--disposable-env` 下跑 |

### R2、R3（2026-11-12，方案 19 节 S0 行；脚本已照方案 16.1 第 7 版改过）

每条命令开头都另有 1 个探查实例（下表没算）。两份输入先在日常账号生成，R3 要把它们拷进一次性环境：
- 语料：`cargo run -p iwork-core --example dump-credential-corpus > corpus.json`（现在 44 行，清单并集 36 个条目）；
- 产品的永久禁用工具名单：`cargo run -p iwork-core --example dump-disabled-tools > disabled-tools.json`（现在 11 个：RemoteTrigger、CronCreate、CronDelete、CronList、ScheduleWakeup，加 P13 定的 DesignSync、PushNotification、SendMessage、Workflow、EnterWorktree、ExitWorktree）。名单从 T07 的工具登记表导出，和产品启动参数是同一个函数的结果，脚本里不另抄。

**环境变量**（P14，产品负责人 2026-11-12 定：不筛选；方案 16.1「交给脚本的」）：
- 脚本起每个实例时环境原样继承，不删任何变量，只设上产品的两个开关 `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`、`CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`（覆盖继承来的值；后一个是 2026-11-13 script6 加的，D5）；每个实例的设置里都带 `autoMemoryDirectory`，把自动记忆改到本次根目录里的一个空目录。有意不设的格子在脚本里写明理由：G5 里写明「不设开关」「只改道」的格子不设记忆开关；`g7-bg` 与 R2X `n1` 不设后台开关（只记厂商默认行为，M2 开放后台任务时用）。
- **同一时间只跑一个**：脚本启动时对 `/tmp/iwork-t0g.lock` 加排他锁，实测和 `--selftest` 都拿，拿不到就打印「不跑：另一次 iwork-t0g 正在跑（…pid=…）」并以非零退出码结束，等那一次结束再跑。`--selftest` 的根目录默认建在系统临时目录下，不和实测的根目录、结果包放在家目录同一处；原有文件核对的变化要是全落在别的运行的根目录里，说明里会点明（仍不算 PASS）。
- **R2、R3 都要从普通终端起，不要从 Claude Code 里起**：Claude Code 会把用户 `settings.json` 的 `env` 块放进自己的环境，从它里面起的实例会带上用户的插件（S0 第一轮就是这样，量的不是产品配置，以 R2、R3 为准）。脚本开头看一眼环境里有没有 `CLAUDE_CODE_PLUGIN_DIRS`（只看有没有，不读值），有就拒绝运行，`summary.txt` 里写一行「拒绝运行」和原因，换一个普通终端再跑。
- 每个实例的 `system/init.plugins` 都过方案 3.5 那条核对（`@builtin` 之外的都报）。报了东西，`summary.txt` 里多一行「环境不干净」，那一格记 `INCONCLUSIVE`，不下别的结论；出现这种行就交 supervisor。
- 结果包的 `env-names.json` 记下脚本启动时 `CLAUDE*`、`ANTHROPIC*` 开头的变量名（只有名字，没有值），供事后核对。

**G4 两轮不逐格直接比**：第一轮的 `--modes` 默认是 `default,dontAsk`，G4 取第一个，跑在 `default` 下；R2 起默认是 `dontAsk`，G4 跑在 `dontAsk` 下。两轮之间某一格变了，先分清是权限模式换了、脚本改了，还是 CLI 的行为变了；t0-findings 里写 G4 结论时注明模式，产品只用 `dontAsk`，以 R2 的为准。

**R2：日常账号**（都用好的探针，或者过得了日常账号的运行门）

| 序 | 命令 | 约几个实例 | 看什么 |
| --- | --- | --- | --- |
| 1 | `python3 tools/t0/iwork-t0g.py --selftest` | 0 | 最后一行 `selftest ok` |
| 2 | `python3 tools/t0/iwork-t0g.py --only G2 --g2-subset r2` | 5 | **先跑这一条**（S4 定型之前要结果）：`G2 [...] read-工作目录外 hook=allow` 一行 `PASS`（hook 允许之后读得到工作目录外；`FAIL` 就回 architect 重看方案 3.4a 第 2 条）；四个「补充 拒绝格」（Grep、Glob、只读命令、子代理工具）都是 `PASS`。两行总结论这时一定是 `INCONCLUSIVE`，正常 |
| 3 | `python3 tools/t0/iwork-t0g.py --only G4 --skip-builds --corpus corpus.json` | 28 + 44 = 72（R2 时是 70，之后加了 ①c′、①d′） | `⑩` 一行 `PASS`（选定配置下 Write 写得进工作目录外；`FAIL` 同样回 architect）；`①e′`、`②c′`、四条「沙箱外对照」、`⑥` 的计数、`④` 全部行重新实测；结果包 `obs/corpus-*.calls` 的祖先链：CLI 自己调的 `git` 是不是 CLI 的直接子进程、沙箱里的调起祖先链记不记得全（每一项是「pid:程序名」，记不全的在末尾是 `?`）。这两点交 architect：前者不成立，出沙箱格的归属判据不够用；后者记不全，R3 加 `--ancestry parent-only` |
| 4 | `python3 tools/t0/iwork-t0g.py --only G7` | 16 | `G7c'` 两行（只发 initialize 时 `current_permission_mode` 是 `dontAsk`；设置校验不过时的取值）；`G7e`、`G7f` 对前台命令的结论；后台命令一行 |
| 5 | `python3 tools/t0/iwork-t0g.py --only G1` | 10 | `G1d`；`g1-Agent-stream.jsonl` 里子代理是不是后台跑、有没有第二个 `result`；`G1c`、`G1f` |
| 6 | `python3 tools/t0/iwork-t0g.py --only G8` | 8 | `G8g`（白名单里写了不存在的工具名时 `init.tools` 只剩存在的）；`G8 MCP 阳性对照`、`G8f`、`G8e` |
| 7 | `python3 tools/t0/iwork-t0g.py --only G5` | 11 | **访问观测带空白对照**（方案 a69016f）：这台机器上家目录里的文件一改时间戳，几秒内就会被别的进程（多半是 Spotlight 或杀毒软件）读一次，所以观测多半总是无效（说明里会写「空白对照被读过」）。那样记忆读一行只靠「甲」：m2 报的就是改道目录，或者 m2 的 `init` 里没有 `memory_paths` 这个键而 m0 有并报了路径（2.1.322 上产品配置拿到的是后一种）；都没有就是 `INCONCLUSIVE`（P15）；子代理记忆一行观测无效就是 `INCONCLUSIVE`（P15）。脚本不为凑有效的观测重跑。看什么：方案 16.1 的 G5 各行（日常账号下仓库设置那几行一定是 `INCONCLUSIVE`，正常）：`G5 环境行` `PASS`（带着插件变量时核对报得出测试插件，干净时不误报）；`G5 记忆读` `PASS`（`INCONCLUSIVE` 且说明里写 P15 的，是改道没生效或产品配置那一格没有结构化证据，交 supervisor）；`G5 守卫预检` 一行写着两个预检过没过：没过的那一行（记忆写、子代理记忆）记「守卫没证实」，不在日常账号起，到 R3 再跑；`G5 记忆写` `PASS`；`G5 子代理记忆` `PASS`，或者 `INFO`「这条路不存在」。测试子代理经 `--agents` 给（带 `memory: user`；产品参数下工作目录 `.claude/agents/` 里的不加载）；`G5 守卫预检` 一行写着子代理预检那一格的 `init.agents` 里有没有它，没有 → `G5 子代理记忆` 记 `INCONCLUSIVE`「测试子代理没加载」，和「守卫没证实」分开报。任何一行 `FAIL` 回 architect（S6 那一组合入之前要这几行过）。**这几格会在 `~/.claude/` 下建自己的目录**：本次工作目录对应的 `projects/<…g5-work>/memory/`、`agent-memory/iwork-t0g-probe-<随机>/`，跑完连同这几个实例的会话记录一起删掉，只动这几个 |

R2 合计约 120 个实例，加 6 个探查（每条命令 1 个）。

**R2 补测**（2026-11-12，方案 19 节 S0 行「R2 补测」；日常账号、普通终端，都是好的探针。脚本已照 R2 分析第 10 节改过：根目录名只用小写字母和数字、④ 按「这次 Bash 调用发起的」认调起、「进程组」一行分出 hook 探针、G7e 改成记录格、⑦b 多记几类特征）

| 序 | 命令 | 约几个实例 | 看什么 |
| --- | --- | --- | --- |
| 1 | `python3 tools/t0/iwork-t0g.py --selftest` | 0 | 最后一行 `selftest ok` |
| 2 | `python3 tools/t0/iwork-t0g.py --only R2X --part d` | 5 | **先跑这一条，D2、D5 靠它定**，约 5 分钟（k1 要等 70 秒）。几行都只记录（`INFO`），看说明里的事实：<br>· `R2X k1`：SIGKILL CLI 的进程组之后，「停 CLI 5 秒后 sleep 还在」「70 秒后哨兵出现了（命令接着跑完）」<br>· `R2X k2`：只关输入之后，「CLI 30 秒内退出了」「CLI 退出之后 sleep 还在」；30 秒内没退的，再等到命令本该跑完之后，记「命令跑完之后（停 CLI 后约 76 秒内）CLI 退出了」<br>· `R2X k3`：发 SIGTERM 之后，同上两样<br>· k1–k3 每行的 `k4`：`sleep`、它的 shell、CLI 各自的父进程号、进程组号、会话号（shell 的父进程是不是 CLI，决定 D2 的 b′ 按后代找不找得到）<br>· `R2X b1`（设 `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1`，调子代理）：`is_backgrounded`、`init` 与 `result` 的次序和个数、目标调用在不在子代理里、进没进 hook<br>· `R2X b2`（同一变量，后台跑 Bash）：输入里还带不带 `run_in_background`、目标调用的结果、哨兵出没出现<br>任何一行 `INCONCLUSIVE` 是那一格没走到：模型没照做、没看到 `sleep`、发信号时 CLI 已经退出，或者会话的证据不齐（某个 `init` 的插件没过核对，即环境不干净）；b1 另要看见真实的子代理工具调用、目标 Bash 就在它里面。单独重跑这条命令 |
| 3 | `python3 tools/t0/iwork-t0g.py --only R2X --part rest` | 8 | · `G4 ①c′`、`G4 ①d′`：去掉 `credentials.files` 之后，假凭据目录里只读区之外的文件仍然读不到、写不了，两行都要 `PASS`（有一行 `FAIL` 回 architect，第 8 节翻译表那条改动要重想）<br>· `R2X n1`（不设变量，常驻流式输入下调子代理）：两个 `init`、两个 `result` 的先后<br>· `R2X 工具清单` 五行：`--tools Read,Task` 与 `--tools Read,Agent` 各自的 `init.tools` 里有没有子代理工具（P11 的白名单写的是 `Task`）；`--disallowedTools Task` 与 `--disallowedTools Agent` 之后子代理工具还在不在；只禁 Bash 时的 `init.tools`（Monitor 还在不在） |
| 4 | `python3 tools/t0/iwork-t0g.py --only G5` | 11 | G5 重跑（根目录名改了之后），看什么同上面 R2 第 7 条 |
| 5 | `python3 tools/t0/iwork-t0g.py --only R2X --part net` | 4 | 沙箱联网（architect 的沙箱联网报告第 4 节最要紧的几格；设置照产品：`allowedDomains: ["*"]`、`allowLocalBinding`）。都只记录：<br>· `R2X p1`：沙箱里命令拿到的代理类环境变量的**名字**（名字里带 `PROXY`、`SRT_`、`GIT_SSH`、`SANDBOX` 的；只打印名字，值里有代理口令，不落盘）<br>· `R2X p2`：沙箱里用 Claude Code 给的隧道（`SRT_SSH_PROXY_COMMAND`）连 `github.com 22`，「读到 SSH 问候」是不是真（只取头 8 个字节，隧道的错误输出丢掉）<br>· `R2X p4` 两行：脚本在沙箱外起一个只绑 `127.0.0.1` 的本机监听（随机端口，跑完就关），沙箱里的命令去连它，带 / 不带 `allowLocalBinding` 各一格：「读到问候」「监听这一格接到的连接」 |
| 6 | `python3 tools/t0/iwork-t0g.py --only R2X --part skill` | 6，日常账号下另加 1 个预检 | 仓库技能的三个口子（architect 的技能机制分析第 3 节）。测试插件、技能、`CLAUDE.md` 都只建在本次根目录里，插件只经 `--plugin-dir` 给（核对插件时这个目录下的算该有的），不经环境变量。技能都用斜杠命令 `/插件:技能` 调起。都只记录，缺证据记 `INCONCLUSIVE` 并点名缺什么：<br>· `R2X L1`（产品参数，`--setting-sources` 空）与 `R2X L2`（同一目录只换成 `project`，阳性对照）：说明里「init 里有」的技能、子代理、命令三项，「暗号的来源」（`context` 答出来了、`absent` 没答出来、`null` 模型有一次工具调用拿不出被拒的证据——探针在它上面没答拒绝、也不是 CLI 自己判的失败——暗号可能经它读来，归不了因；只看调用证据，不看工具结果里的文字）。L2 三项都在、来源是 `context` 才记 `INFO`，L1 的结果才算数；L2 没成立、或 L1 自己的来源归不了因，L1 记 `INCONCLUSIVE` 并点名缺什么。探针对一切答拒绝，暗号读不了文件，只能从上下文来<br>· `R2X K3b`（技能正文带 `` !`touch <标记>` ``、`allowed-tools: Bash(touch:*)`，探针对一切答拒绝）：「标记出现了」「探针收到了内联命令」（只认命令与内联那条逐字相同、`tool_use_id` 不是模型发的那一次的记录）「归得了因」「绕过」。绕过是真，就是内联命令不经 iWork 的 hook 执行了；模型发的调用有一次拿不出被拒的证据（只是报错不算：`touch …; exit 1` 建了标记才报错），绕过记 `null`、这一格记 `INCONCLUSIVE`<br>· `R2X K3c`（K3b 再加 `--settings` 里的 `disableSkillShellExecution: true`）：「标记出现了」该是假，「答复里有占位文字」看占位文字有没有换上<br>· `R2X K2c`（技能带 `allowed-tools: Bash`，探针只对带标记的那条 Bash 不表态，别的照登记）：故障探针，外层设置用 G2 那套，先跑一格 Bash 两项外层保护的预检（`R2X K2c 运行门` 一行），过不了门这一格记 `INCONCLUSIVE`、不起；`--disposable-env` 下不预检直接跑。看「标记出现了（allowed-tools 把范围放大了）」与每次结果是不是报错；带标记的 Bash 之外的调用都要有被拒的证据（「标记归得了因」）<br>· `R2X H1`（测试插件自带一个 PreToolUse hook，只对登记的那条 `touch` 答「允许」，别的工具、别的命令、沙箱外重试都答拒绝；它的日志只记 `tool_use_id` 与答了什么，不记命令原文。iWork 的探针答拒绝）：「标记出现了（插件的允许压过了 iWork 的拒绝）」；证据里要有 iWork 的探针在每一次目标调用上都有记录、插件的 hook 在每一次上都答了允许，目标调用之外的每一次调用都有被拒的证据（「标记归得了因」） |
| 7 | `python3 tools/t0/iwork-t0g.py --only R2X --cell k3` | 1 | 带产品设置重记 k3、k4（和第 6 条一起跑）。2026-11-12 起 R2X 的 k1–k3、b1、b2、n1、工具清单几格都带产品那样的设置（`dontAsk`、`allowedDomains: ["*"]`、`allowLocalBinding` 的沙箱，hook 的 `timeout` 与 `onFailure: block`），和 net、①c′ ①d′ 一样；之前那次 `--part d` 不带，`init` 报 `default`、命令不在沙箱里跑。看 `R2X k3` 一行：「停 CLI 5 秒后 sleep 还在」「CLI 30 秒内退出了」，以及 `k4` 里 `sleep`、它的父进程、CLI 的父进程号、进程组号、会话号（命令进了沙箱之后，`sleep` 的父进程可能是沙箱的包装进程而不是 shell，照实记）。k、b、n 几格的探针只放行登记的那条命令、沙箱外重试一律拒绝；流里出现过带 `dangerouslyDisableSandbox` 的 Bash（哪怕被拒了），那一格记 `INCONCLUSIVE`（缺「流里没有带 dangerouslyDisableSandbox 的 Bash 调用」），不当成带沙箱的证据。`--cell` 收逗号分隔的格名，只跑这几格；拼错、空项（如 `k3,`）、重复的格名在启动时（拿锁之前）就报错退出 |

R2 补测合计约 35 个实例，加 4 个探查与 K2c 的 1 个预检。第 2 条先跑，别的可以晚一点。

**script6 第一批**（2026-11-13，architect 47737e78 的格子表；日常账号、普通终端；「产品设置」= `dontAsk` + 产品那样的沙箱 + 两个环境开关，跑命令的格探针只放行登记的那条、沙箱外重试一律拒绝）

| 序 | 命令 | 约几个实例 | 看什么 |
| --- | --- | --- | --- |
| 1 | `python3 tools/t0/iwork-t0g.py --selftest` | 0 | 最后一行 `selftest ok` |
| 2 | `python3 tools/t0/iwork-t0g.py --only R2X --part recheck` | 27（G1 10、G8 8、G7 9） | G1、G8、G7 带产品设置重跑，各行看什么同上面 R2 的 G1、G7、G8。新加三行：`G1d′`（先要子代理真跑了：子代理里的目标命令结果不是报错、哨兵出现；再看一个 `init`、一个 `result`、`is_backgrounded` 不为真，要 `PASS`；模型没起子代理或子代理被拒记 `INCONCLUSIVE`）；`G1 BashBg`（只记录：`run_in_background` 被拒的报错原文、每次调用进没进 hook、输入里的字段名）；`G1a′`（只记录：产品设置下 `init.tools` 的个数，预期 24）。复核要盯：`G1d` 在 `dontAsk` 加沙箱下仍是 `PASS`；`G8e`、`G8g` 在 `dontAsk` 下成立（P11 d2）；`G7e` 里 shell 的父进程号、进程组号、会话号和 R2X `k3`、`k4` 对得上。G7 的 `g7-bg`、`g7c2-bad`、`g7-cfg-*` 这里不跑（有意不带产品设置，理由写在脚本里） |
| 3 | `python3 tools/t0/iwork-t0g.py --only R2X --part p4` | 2（加 1 个不经 claude 的预检） | macOS 上「沙箱起不来时 CLI 会不会停」（T06 方案第 8 节 F 格）。`R2X P4 预检`：外层全放行的 `sandbox-exec` 自己起得来、里面再套一层失败，预检没过两格都记 `INCONCLUSIVE`（造法不成立，回 architect 换替身的造法）。`R2X P4 F1`（带 `failIfUnavailable: true`）：要 `PASS`。`PASS` 要有沙箱不可用的原文（程序字样：启动时 `sandbox_required_unavailable` / 「Set sandbox.failIfUnavailable=false …」，会话中途「Sandbox is required (sandbox.failIfUnavailable) …」），而且要么 CLI 没开会话，要么目标命令每一次都是探针答了允许、CLI 以这句拒掉；认证失败、崩溃、外层包装起不来都只能是 `INCONCLUSIVE`；`FAIL` = 工作区外的文件写进去了，这个键拦不住。`R2X P4 F2`（去掉这个键，对照）：要 `PASS`（文件写进去了，证明沙箱确实没起）。两行的说明里另记 stderr 提没提 sandbox、`init` 的顶层键与 `capabilities` 的键（只有键名） |
| 4 | `python3 tools/t0/iwork-t0g.py --only R2X --cell k3` | 1 | 同上面 R2 补测第 7 行；k1–k3 的命令开头先用 `/usr/bin/touch` 写一笔工作区（对照）、再写一笔工作区外，说明里「沙箱在场」要是真：对照写进去了，工作区外那一笔退出码非 0、文件没出现、错误原文是 `Operation not permitted`（Linux 是 `Read-only file system`）。原文也记在说明里。不是真这一格记 `INCONCLUSIVE` |
| 5 | `python3 tools/t0/iwork-t0g.py --only R2X --part cfgdir` | 0（四次 `claude mcp list`，不起模型） | T10f A 审查 1/F-2：CLI 找 `.claude.json` 是按 `CLAUDE_CONFIG_DIR` 设没设分支，还是按值。HOME 换成本次根目录里的临时目录（不碰真实的 `~/.claude`、`~/.claude.json`），里面三份合成配置各带一个名字不同的 MCP 条目（命令 `/usr/bin/true`）。四行 `R2X cfgdir`：没设 → 读家目录那份；设成空串 → 读家目录那份（iWork 把空串当没设）；显式设成默认目录 → 读默认目录里那份；自定义 → 读自定义那份。每行要 `PASS`，说明里记命中哪几份、几份、退出码。`FAIL`（特别是空串那一行）回 supervisor；命令报错或输出认不出是 `INCONCLUSIVE` |

**R3：一次性环境**（16.2 第 5 项，搭法见下一小节）

| 命令 | 约几个实例 | 看什么 |
| --- | --- | --- |
| `python3 tools/t0/iwork-t0g.py --only G2,G5,G9 --disposable-env --modes dontAsk --corpus corpus.json --disabled-tools disabled-tools.json --ancestry parent-only` | G2 约 228：主矩阵 9 × 11 = 99、补充格 33、出沙箱格对 2 × 36 = 72、「不问工具」记录行 12 加它开头的探查 1（2.1.322 的工具清单去掉永久禁用的 11 个之后）、对照配置的 read 行 11；一次性环境不预检。G5 18（环境行与记忆各行 11，仓库设置三种写法与对照 7），G9 2。合计约 248，加 1 个探查 | `G2 硬格子 [dontAsk+block]` 一行 `PASS`（硬格子 114 个加出沙箱的 M 个全过）；`G2 范围记录 [dontAsk+block]` 一行；`G2 [...] 出沙箱这一组` 一行的 M、「这条写法没测到组合的条目」与「做过扣除的条目」（各自那一行写了 CLI 自己的调起对照里几笔、出沙箱格里几笔，请看一眼；M = 0 时整组 `INCONCLUSIVE`，要先换「条目 → 只读写法」表里的写法再跑）；`不问工具：带着永久禁用名单启动后的工具清单` 一行；read 行三个记录格写的「有执行的痕迹」；对照配置 read 行失败格的痕迹（有痕迹说明 `block` 在起作用）；`G5 环境行`、`G5 记忆读`、`G5 记忆写` 三行 `PASS`，`G5 子代理记忆` `PASS` 或 `INFO`「这条路不存在」，`G5 A --setting-sources 空值` `PASS`；`G9d` 一行 `PASS` |

**不带 `--disabled-tools`**：「不问工具」记录行整组记 `INCONCLUSIVE`（不知道会话里有哪些工具），不拿默认工具集顶替，`G2 范围记录` 一行到不了 `PASS`。

**「不问工具」记录行里会停在没测成的工具**：Skill、TaskStop 的探针换不出「能成功又不留副作用」的参数（会话里没有技能、没有在跑的任务，调用注定报错），只要 CLI 没留下权限层拒绝的证据，这两格就是 `INCONCLUSIVE`，结果里点名写了原因；它们会让 `G2 范围记录` 一行到不了 `PASS`，交 supervisor。脚本里还登记了一张不起实例的表（`UNPROMPTED_NOT_RUN`：执行了会在本次根目录之外留下副作用，或单独触发不了），表里的工具现在都在永久禁用名单里，带着 `--disabled-tools` 跑时不会出现；以后名单里去掉了哪个，它会回来，记 `INCONCLUSIVE` 并点名。

**第一轮 ④ 的结果不回填 T07**：第一轮 `g4-corpus-filled.json` 里记成「实测 outside」的 17 行不可信（审查复核：替身的调起没有和命令对上，有一行命令根本没跑也记成了实测）。T07 `credential_corpus` 第四列只用上面 R2 第 3 条（`--only G4 ... --corpus corpus.json`）重测的结果。

### 一次性环境怎么搭（16.2 第 5 项）

macOS 上：`sudo sysadminctl -addUser t0gtmp -password -`（按提示输密码），快速切换到这个用户，登录 Claude Code，`git clone` 一份仓库（或者拷过去），把日常账号里生成的 `corpus.json` 一起拷过去，跑 R3 的命令。跑完把 `~/iwork-t0g-results-*.tar.gz` 拷出来（比如放到 `/Users/Shared/`），切回自己的账号，`sudo sysadminctl -deleteUser t0gtmp`。这个用户里不要放任何要紧的东西：故障格子会故意弄坏 hook，靠的只是沙箱和这个用户本身的隔离。

### 结果不对时怎么办

**`FAIL`**：说明拿到了正面证据，证明行为不对。

- `G2 硬格子` 一行是 `FAIL`（选定配置有硬格子执行了不许执行的）：**整张卡停**，结果包交 supervisor，由产品负责人决定（方案 P5 选项 c）。只读命令的沙箱外重试格或底线格 `FAIL` 时，一并告诉产品负责人：这一项的上限不再是沙箱（方案 3.4）。
- `G2 范围记录` 一行是 `FAIL`（「不问工具」记录行报出接受清单之外的工具，或链接格读到了目录外）：不停卡，交产品负责人逐个定；定之前这个版本不进已验证表。
- 对照配置（不设 onFailure）的格子只记 `INFO`，不计入总结论。
- `进程清理` 或 `进程组` 出现 `FAIL`：脚本自己会停（清不干净不跑后面的）。看 `ps` 里还有没有工作目录在 `~/iwork-t0g-*` 下的进程，手动结束后再跑。
- `运行前后原有文件核对` 是 `FAIL`：家目录下原有的 `iwork-t0g*` 文件或 `~/.npmrc` 变了，结果包和 `summary.txt` 里有变了哪几项，交 supervisor。
- 别的组的 `FAIL`：就是实测结论，照实写进 2.4 节，交 supervisor 看对方案哪一节有影响（每组影响哪里见方案 16.1 的表）。

**`INCONCLUSIVE`**：没测成，不是测出了问题。按 `summary.txt` 那一行后面写的原因处理：

| 原因 | 怎么办 |
| --- | --- |
| 「外层保护没验证过（[...]），这个格子不在这台机器上跑」 | 运行门没过：日常账号下这几样保护不成立，故障格子不许跑。去一次性环境加 `--disposable-env` 跑（步 4）。同时把 `外层保护 [...]` 那几行原样记下来：产品里的本机验证就靠这几样 |
| 「只在一次性环境……里加 --disposable-env 跑」 | G5、G9d 在日常账号下本来就不跑，去步 4 |
| G2「外层保护没验证过（['unsandboxed']）」「（['read-outside']）」「（['supplement']）」「（['unprompted']）」 | 这几种没有预检，日常账号下本来就不跑，去 R3。G2 两行总结论因此在日常账号下恒为 `INCONCLUSIVE` |
| G2「不起实例：…」 | 「不问工具」记录行：没给 `--disabled-tools`（加上再跑），或者是登记成不跑的工具（`UNPROMPTED_NOT_RUN`，见上文），交 supervisor |
| G2「探针换不出能成功又不留副作用的参数」 | Skill、TaskStop 这类工具停在没测成，见上文，交 supervisor |
| G2「探针在目标调用上答了允许」 | 岔子没冲着目标调用，这一格说明不了被测的结局；看这一格的 `hook-calls.jsonl`，多半是模型调用的写法和登记的对不上。偶发就重跑，每次都这样交 developer |
| 「缺正面证据：……」「运行无效（没有 init、超时或退出码不对）」「没有归属对得上的目标调用」「目标调用不止一次（N 次）」「有一次目标调用没有按 id 对上的结果」「有一次目标调用探针没有被调起的记录」 | 这次运行本身不算数：模型没按要求发调用、超时，或 CLI 没起来。「没执行 / 被拦下」这类格子允许模型重发，只要每一次都有结果、都是报错、探针每一次都有记录（2026-11-12 起）；要读回值的格子（G1b、G6 读回、G9a 等）仍要恰好一次，重发了就是「不止一次」。先看这个用例的 `<名>-stream.jsonl` 和 `<名>-stderr.txt`；偶发的（模型没照做、网络抖）就用 `--only <组>` 单独重跑这一组；每次都这样的，交 supervisor |
| 「权限模式没生效：init 报 ...」 | 设置文件没被 CLI 收下（`-p` 模式下设置文件校验不过会被整个静默忽略）。看 `<名>-stderr.txt` 有没有设置校验的告警，交 supervisor，别重跑 |
| 对照没成立（`G5 阳性对照`、`G8 MCP 阳性对照`、G9b 对照变量看不到、外层保护里要成功的那一笔没成功） | 测的办法在这台机器上不灵，负向结论说明不了问题。看对照那次的 stream 和 stderr，交 supervisor |
| G4 ④「没给 --corpus」或「语料读不了」 | 用 `cargo run -p iwork-core --example dump-credential-corpus > corpus.json` 生成语料，再 `--only G4 --skip-builds --corpus corpus.json` 补跑 |
| 「登记之外的调用（探针一律拒绝）」一行次数很多 | 这行是 `INFO`，不算结论；但次数多说明模型老跑偏，相应用例容易变成没测成。看各用例目录 `hook-calls.jsonl` 里带 `refused` 的行。偶发就重跑那一组 |
| 「脚本自己出错：...」 | 脚本的错，交 developer，附上这一行和结果包 |

`--quick`、`--g2-subset r2` 下 G2 两行总结论恒为 `INCONCLUSIVE`，这是设计如此，不用处理。

### 跑之前已知的风险点（S0 校对时从 2.1.322 打包代码里看不出结论，要靠这次实测）

- G6 的思考强度读回：hook 输入里的 `effort` 字段只在当前模型支持思考强度时才带。脚本默认 `--model haiku`（2.1.322 里指 `claude-haiku-5-5`），它支不支持看不出来。要是 G6 五个取值都是 `FAIL` 且读回是 `None`，换 `--only G6 --model sonnet` 重跑一次再下结论；`xhigh`、`max` 也可能只有部分模型支持。
- G6 续接时换追加系统提示：2.1.322 新增 `--system-prompt-snapshot`，默认开，会话第一次的系统提示会被记下来，续接时原样再用，后来传的不同文本不生效。那一行只是 `INFO`，读结果时知道这一点。
- G5 的阳性对照：仓库设置里同时写了 `disableAllHooks: true`。命令行设置写的是 `false`，按设置层的优先级应当压过它，但从代码里没法确认合并后取哪个；要是对照没成立，G5 三行都是 `INCONCLUSIVE`，交 supervisor。
- G8a `--tools ""`：要是这个版本有不受 `--tools` 管的常驻工具，`init.tools` 不会是空的，G8a 记 `FAIL`。照实记。
- G4 ②：`allowedDomains: ["*"]` 能不能放行全部，文档只写了 `*.example.com` 这种通配，要实测。
- G4 ⑦a `security list-keychains` 只有 macOS 有；在 Linux 上跑 G4（步 7）这一行一定是 `FAIL`，不算问题。
- G2「沙箱外重试」：提示里要求模型给 Bash 带上 `dangerouslyDisableSandbox: true`，模型照不照做没实测过。不照做时目标调用（命令逐字相同且带这个参数）对不上，这一行记 `INCONCLUSIVE`，不会误判成 `PASS` 或 `FAIL`。在步 4 里大量出现时，看这些格子的 `<名>-stream.jsonl` 里 Bash 调用带没带这个参数，交 supervisor（可能要换提示或换模型）。
- `iwork-t0.sh` 按 `--help` 文字查参数在不在：2.1.322 的 `--help` 里没有 `--permission-prompt-tool`，日志会写 `flag --permission-prompt-tool: no`，但打包代码里这个参数还在（只是不在帮助里列出）；`iwork-t0f.py` 用的 `--max-turns` 也一样。不是退化。
