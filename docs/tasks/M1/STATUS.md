# M1 进度与决策（supervisor 交接记录）

> 最后更新：2026-11-11（T05、T07、T06 第一批已合入 main）。上下文压缩或换会话后，supervisor 先读这份文件再动手。
> 只记当前状态与还没写进其他文档的决定；已写进需求、技术设计、任务卡、方案的，以那些文档为准。

## 1 运行环境与规矩

- **CAO 会话**：`cao-a46fd0b6`，supervisor 终端 `f6c88a81`（12:30 机器重启后新开）。worker 回报只发给创建它的 supervisor 终端；换 supervisor 后看 `agent-scaffold ps`、worker 窗口、分支提交、审查账本来接续。
- **角色配置**（产品负责人重新生成；`.cao/agent-store/*.md`、`.cao/scaffold.json`、`.cao/workflows/*` 的改动未提交，不要动）：developer = claude_code `claude-opus-5-5`；code_reviewer = claude_code `claude-fable-5-2` effort medium（2026-11-10 11:00 产品负责人直接改 `.cao/agent-store/code_reviewer.md`，原 codex `gpt-6-astra` 额度用完；调优记录 T-2。`.cao/scaffold.json` 仍写 codex，跑 `agent-scaffold update` 会改回去，要先同步 scaffold.json）；functional_reviewer 仍是 codex，额度恢复前不派；ui_design_reviewer 从 antigravity gemini 换成 claude_code `claude-sonnet-5-5` medium（产品负责人 2026-11-11，直接改 `.cao/agent-store/ui_design_reviewer.md`；还没有审查记录，tuning log 等它审过一次再补）；architect = claude_code `claude-fable-5-2`。项目提示已把原规则填回新模板（`4f7db54`）。
- **推送**：main 推送已获同意（最近一次推到 `9cfb4ae`，之后的本地提交未推）；任务分支推送跑 CI 已获同意。gh 已登录，CI 日志 `gh run view <id> --repo mrkizy/iWork --log-failed`。
- **重测试排队**：`lockf /tmp/iwork-heavy-test.lock <命令>`（AGENTS.md）。本机先 `export PATH=$HOME/.cargo/bin:$PATH`（Homebrew 的旧 cargo 在前）。派活消息里写明。
- **性能验收**：mac120（`ssh mac120`，Apple M1 8 核，rustup 1.99.1，仓库 `~/work/iWork`）夜间空闲时段测；Kaspersky 不排除、作客观因素记录（T11 卡）。开发机基准只作粗筛。
- **UI/UX 问题**：按 AGENTS.md 问「设计窗口」；产品负责人负责转问——把问题列给他。设计窗口答复写在 `docs/design/ui-m1.md` 第 2a 节、画布、`design-system/`，改完由 supervisor 核对后提交（产品负责人 2026-11-11 改定；第一批 `8f5844a`）。
- **使用范围**：M1–M9 只有产品负责人本人使用（技术设计第 1 节）。
- 产品负责人工作区里未提交的改动：需求 v1.75（FR18 对话标题）、AGENTS.md（设计窗口一节、v1.75）、`docs/design/ui-m1.md`、`docs/design/ui/*`。技术设计 9.0a 已随 `182f71b` 提交（已告知，他没要求拆）。

## 2 已定、还没全部写进对应文档的决定（2026-11-10）

- **T04 方案**：Q1～Q3 按推荐；Q4 M1 不提供换回迁移前备份；Q5 登记已知限制 L6（看过完整代价后再确认）；**M1 要做有证书构建**（bundle_probe 整包验签，N5 两种包形态实测）。L6 定稿后写进需求第 9 章。
- **T05 旧缺口**（T04 审查 1/F-11、1/F-12）：单独修，任务 `2026-11-10-m1-t05-dispatch`，先于 T04 实施合入；修好后关 `2026-11-10-m1-t04-plan/1/F-11`、`1/F-12`（fixed，引用产品负责人「现在单独修」）。
- **T06 方案**：P1 基准随本机 CLI 版本（现 2.1.322）；P2 选 c：未验证版本首次使用前本机自动跑硬前提验证（约 20 次短调用、花订阅额度、几分钟内开不了会话），过了放行、不过阻断，状态只在运行时详情 / 诊断页，状态栏不显示；验证中是点名错误、没过落 Incompatible 带原因，显示归 T09 / T10；P6、P8 知情同意（P8 偏离技术设计 6.3，定稿后改）；P3/P4/P5/P7 等实测。 第 3 版补充（17:55）：本机验证实际约 80 次短调用、约十分钟（跑满 7×11 矩阵，已告知产品负责人）；P9 Windows 上 Claude 运行时 M1 不放行（与 T03 一致）；ApprovalDelivered 写进日志后旧程序起不来（S19），并入 T07 PD6 一起问；T06 S2 与 T07 折叠同一批合入，或 T07 折叠见到第一条 ApprovalDelivered 前不启用 awaiting_delivery。
- **T07**：方案 `4e27665` 定稿（+57b5c4d）；需求 v1.74 已登记静态分析限制；F-12 口径写进 AGENTS.md。**引擎按根因重做**（产品负责人批准）：PD1 加 grant_approval_id、PD2 卡片校验整张转换表、PD3 void_pending 返回 Result（删 aborted）、PD4 ApprovalDelivered / Decided 由 T07 先定义、两卡接线共同启用；PD5 已告知；**PD6 已定（产品负责人 2026-11-10）选 (a)**：新事件种类按「带迁移」对待，只能向前修，紧急修复版不夹带新事件种类（T04 Q4 的延伸；T07.md 1682–1689 改为已决定、T06 S19 引用）。**PD6 落地还要派的三件事**（不在 T07 卡）：`proto/compat/README.md` 加「新增事件种类 = 不可退版本」；发版检查表加一项；T04 补「旧程序遇到不认识的事件」的用例（告诉 T04 architect）。原选项：(a) 新事件种类按「带迁移」对待、只能向前修（Q4 的延伸）；(b) 两步发版保留能解码的回退版本；(c) 换回升级前备份；(d) 忽略未知事件不能当默认。T06 S19 同一问题并入。PD4 改为 T07 E5 连编排一侧「采纳时提交 ApprovalDelivered」一起做、E3–E5 一组合入——T06 方案 2.6 第 2–4 条与 S2 分工要同步（等 T06 第 3 轮复审回来告诉 architect `4f7c1e8b`）。引擎方案 F-2 让投影器校验读回合 / 对话 / 助手三行（归属存续），比原批准稍宽，已告知产品负责人。
- **T09 方案**：D1 画布已统一（ui-m1.md 第 2 节第 3 条、第 2a 节）；D2 按需求 v1.75；D3 空态；D4/D5/D7/D8 按推荐；D6 关窗后后台发通知进 M1（归 T10）；G1～G4、G7、G8 第 1 步只加 proto 字段，填值归 T10。 C1 帧从「Channel 推」改为「网页 invoke 长等待来取」（产品负责人 2026-11-10 同意，技术设计 3.6 与任务卡已改）。**设计窗口问题**：Q9 回到最新并提示、Q10 卡片消失不留痕、Q11 对话页不显示、Q13 随包带 Noto Sans SC（产品负责人 2026-11-11 定，写在 T09.md 第 10 节）；Q12 M1 不做项目默认值界面入口（需求 v1.76，产品负责人工作区未提交）、Q14 一行系统提示「这一轮没能开始」（ui-m1.md 2a 第 9、10 条）（重连重建时间线时的滚动位置、继续 / 放弃卡片处理后留不留痕、AGENTS.md 草稿卡片进对话后显不显示、项目级设置放哪）——已列给产品负责人转问。
- **T03**：M1 暂不支持 Windows；fork+exec 冒充登记 v1.73。

- **需求 v1.78（产品负责人 2026-11-11，工作区未提交）**：① 续接失败时编排核心用 iWork 的对话记录按规则拼摘要，自动新开会话线继续这一轮，不提示、只审计（和 T06 原写法「续接失败 → Failed」相反）；② 删对话 / 删助手时适配层一并删 Claude Code 的会话文件；③ iWork 的会话出现在用户 `claude --resume` 列表里，登记已知限制。已落进 T06 方案（`600807b`）：续接失败摘要新开归 T06 新步 S2b（S2 合入后），会话文件删除归 S6b（命令侧 T10a 删对话、T10b 删助手）；摘要放新会话第一条输入（产品负责人确认，技术设计 6.4 工作区已同步，随他的 v1.78 一起提交）；AGENTS.md 补删会话记录例外；S2 代码不动，合入后 S2b 改 resume_failure 用例预期；G10 实测会话记录路径形状待做。

## 3 各卡状态（2026-11-12 10:00）

| 卡 | 产物 | worker | 审查任务 / 轮次 | 开着的问题 | 下一步 |
| --- | --- | --- | --- | --- | --- |
| T01 / T02 / T03 / T05 / T08 | 已合入（ADR 0001–0012；T05 Windows 偶发修复 `a6a5007`） | — | 已关 | T01 验收 5 截图未补；T03 基准待 T11 空载复测；iwork-ipc `protocol_violation`（audit 事件数断言）在 ubuntu CI 偶发失败一次（2026-11-11，T06 分支 run 42948746991 重跑通过），待另开任务查 | — |
| T05 补丁 | **已合入 main `b18b947`**（2026-11-11） | — | `2026-11-10-m1-t05-dispatch` 两轮通过 | — | T04 1/F-11、1/F-12 已关；T04 可以开工 |
| T04 | 方案定稿（+实施期改 `004c7f9`、1/F-4 产品负责人定 `4827d77`、`84fb176`）；**组 1 `6d5e145`、组 2（步 4–6）`efe06f9` 已合入 main**（2026-11-12，审查三轮） | developer `e5b90212` 做组 3（步 7–11） | `2026-11-11-m1-t04-g2` 已关 | 留给 T10：无人认领那条道每次叫醒整批读 payload（可改只取 id、target）；`after_uncertain` 收尾失败时那条停在 Dispatched、道会先投后面的（下次启动对账收回）；`boot::read_defaults` 的 projects_root 是占位 | 组 3 = 9a 自启注销、autostart-align、install-watch、SetAutostart、lifecycle 基准（用真 PublishObserver）、Launcher、CI 加 `--features dev --test process`；L9 Linux 挂起待实测 |
| T06 | 方案定稿（D1=A 大改 `624aa28`…`4a96669`，审查任务 `2026-11-12-m1-t06-plan-d1` 三轮通过）；**第一批 `d075a94`、S2 `a0d6cba`、S2b `a0a4bfb`、S3a–S5 `75b59a0` 已合入 main**（2026-11-12；隔离方案 `2026-11-12-m1-t06-plan-iso` 三轮通过，P14 不筛选环境变量） | developer `4ea1efca` 做 P11 白名单（任务 `2026-11-12-m1-t06-p11`）；developer `b57765cc` 做脚本第二批（任务 `2026-11-12-m1-t06-s0-script2`，方案 3.4/16.1 的新格子）；architect `47737e78` 留着 | 脚本任务 `2026-11-12-m1-t06-s0-script` 第一批已合入 `655449e`，1/F-7（本机用户名进结果包，改动前就有）**等产品负责人定** | **产品负责人 2026-11-12 定**：D1=A（hook 不表态时 CLI 不问的动作照 CLI 规则走，已知限制；AGENTS.md 例外条款、技术设计第 1 节、t0-findings 2.4 已写）；只读命令一项接受；P12 记为已知限制（不在添加项目时拒绝）；需求第 9 章由产品负责人自己加（草稿已给）。**P11 已定 d + a（2026-11-12）**（S4 已在 P11 定之前写完，`builtin_tool_allowlist` 与 `--tools` 白名单另批补，S8 前要有）；P13 已定六个都禁（T07 登记表由 `b57765cc` 加）；**隔离线索待 architect 分析**：`--setting-sources ""` 下用户的插件 8、技能 19、命令 55、子代理 5 仍进会话（MCP 被挡），可能违反隔离不变量；P4（Linux 沙箱起不来时拒绝启动还是照跑，推荐 a）是 S6 开工前置：S4 的设置常量暂不写 `sandbox.failIfUnavailable`，定了再加；S5 样本由 developer 本机录（产品负责人同意，≤10 个 Haiku，入库前脱敏、grep 零命中，supervisor 合入前再核）；D2 后台命令脱组等 R2。S0 第一轮、R1、R1x（14 个实例，其中 7 个因我的分析脚本 bug 重跑）已跑；**R2 等脚本第二批审过再问产品负责人**（约百来个实例）；R3 要一次性环境（S6 起每组合入前要 R3 的 G2 硬格子全 PASS）；第一轮 ④ 的 17 行不回填 T07 |
- T06 R2（2026-11-12，产品负责人在普通终端跑，约 673 万 token）：分析 scratchpad `r2-analysis.md`，方案已记 `b543196`。G2 先行五格、G1、G8（P11 d2 两前提成立）、G7c′ 都成立；**G7e 前台命令也脱离 CLI 进程组（D2）**；子代理默认后台跑（D5）；G4 四条是 CLI 真实行为（D4 改翻译表、本机端口加 allowLocalBinding、非 HTTP 外连 P3 待产品负责人）；G5 记忆三行因脚本根目录名带下划线没测成。**R2 补测**（约 23 个实例，先跑 k1–k3、b1、b2 定 D2、D5）脚本由 `b57765cc` 在做（任务 `2026-11-12-m1-t06-s0-script3`），审过后请产品负责人在普通终端跑。D2、D5 定之前 S6 放开真实会话的那组不合入。S6 开工清单（developer 记忆 t06-s6-kickoff-checklist）：S5 样本是在带插件的环境录的，S6 录新样本时一起重录；子代理工具 Task / Agent 两个名字都要认（审批登记表不加 Task）。
| T07 | **已合入 main `d1086d4`**（2026-11-11），完成记录见 T07.md | — | 已关 | — | 遗留：性能 +13% 原因待查（开小任务）、启动折叠与 T04 冷启动一起定、真实观察者接线归 T10a；macOS CI 抖动 `turn_runner::answer_arriving_while_cancelling_is_rejected`（iwork-core，T04 组 2 CI 首跑红、重跑绿，2026-11-11）待开小任务查 |
| T07 引擎方案（已定稿、已实施） | `docs/plans/T07.md` 6.10 + `docs/plans/T02.md` 状态前提（未提交） | architect `890c712d`；reviewer `099b7f24` | `2026-11-10-m1-t07-engine-plan` **第 4 轮通过，定稿 `b6bf0a6`** | — | developer f8877913 在分支上按 6.10.14 做 E1–E7；真实观察者接线测试归 T10a 合入门槛 |
| T09 | 方案 `docs/plans/T09.md`（未提交，429 行） | architect `62db2f05`；reviewer `9cbb7f89` | `2026-11-10-m1-t09-plan` 第 2 轮不通过：2/F-1（断线丢在飞的摘要重查）、2/F-2（reload 不释放 Tauri 旧帧暂存）、2/F-3（非时间线大 patch 超 1 MiB 永远发不出，blocking）；1/F-3～F-7 已解决可关，1/F-1、F-2 部分 | 见左 | 第 3 轮不通过（3/F-1 interceptor 守门无效、3/F-2 跨代旧响应覆盖），**方案定稿 `1df7d74`**，实施见下一行；设计窗口 Q9～Q12 待答 |
| T09 实施 | **第一组 `414089b`、组 2a `d9fab70` 已合入 main**（2026-11-12，审查三轮）。剩：组 2b = 步 5b 锚点数据 + 锚点组件 + 步 5a（Dock、报在看哪、退出菜单）+ rfd 文件选择 + 拖入遮罩；组 3 = 7b、7c、7d；组 4 = 步 8、9；步 10 等 T04、步 11 等 T10 | developer `165268d4` 做 2b；code_reviewer `2a4efd5f`、ui_design_reviewer `0c8e300a` 留着复用 | `2026-11-11-m1-t09-ui`：2/F-1 令牌对比度（改动前就有）**等产品负责人定**；漏报 M-1 待产品负责人网站确认 | 全开档的赭红提示行要 NoteRow 加颜色，归 7b；2/F-2 找不到路径的项目行截断由 supervisor 暂定，待设计窗口补进 ui-m1 |
- T09 组 2b（2026-11-12）：步 5b、锚点组件、步 5a 已做（`eb0a1c0`）；developer `165268d4` 上下文满已交接、终端已关，`b20e49db` 接手做 rfd、拖入遮罩、P1 锚点断言。**产品负责人 2026-11-12 批准**：`objc2-app-kit` 加开 NSMenu、NSMenuItem，直接依赖 `objc2`、`objc2-foundation`（与 tauri 已带的版本相同）。待产品负责人真机试：按住 Option「退出 iWork」切成「完全退出 iWork」。首页附件：产品负责人 2026-11-12 定「先这样」（首页不能先加附件，回形针置灰）。归组 3 7d 的：「完全退出 iWork」网页确认框（Rust 侧 quit_requested → QuitIntent → ShutdownDaemon 已有），并补「只在有成员运行时才确认」（判断放 derive；现在不管有没有成员都置位）；「添加项目」页的打开本机文件夹改走 Pick 机制。
- 新表情（产品负责人 2026-11-12，设计 `cb6a9f2`，ui-m1 2a 第 14 条）：13 个形象 × 15 种表情，素材按清单数据驱动、不用改代码就能出现；但 `confused`（待回答卡片）、`tired`（运行时本机额度用完）、`sleep`（空闲 30 分钟）三种要接真实状态，并按新优先级（等你 > 疑惑 > 工作中 > 没电 > 出错 > 完成 > 瞌睡 > 空闲）改 `derive/avatar.rs` 的 MOODS：`confused` 与待回答卡片归 T09 组 3（7b），`tired` 的额度数据归 T10g，`sleep` 的计时可随组 3 做。彩蛋 5 种 M1 不触发。
- 费用显示（设计窗口 2026-11-12，ui-m1 2a 第 15 条、技术设计 9.4）：「本对话用量」标签只显示 token，金额只在悬停里写「按官方标价估算」；订阅或登录方式未知时加一行说明。要跟：本对话用量的视图数据带登录方式（订阅 / API Key / 未知，取 `authMethod`、`subscriptionType`）和「有回合缺少费用」标记（client-core 视图归 T10g，登录方式来自 T06 S8 探测）；`turns.cost_usd` 照原样存，缺失为 None（T06 6.3 已改）。
- 代码块与行内代码（设计窗口 2026-11-12，ui-m1 2a 第 16 条，截图 `ui/18-CodeBlock-*`，令牌 `--cd*`）：Markdown 代码块与编辑类工具卡片的代码 / diff 共用一个组件，超 20 行折叠成 12 行、流式中与刚生成的不折叠、未闭合围栏边输出边画、增量高亮；文件路径点击交 Rust 打开。归 T09 时间线渲染（组 3 7b/7c 的工具卡片与正文 Markdown）。深色 `--cdC` 已调到 `#909DA9`（新增行底上 4.81:1）。
- 侧栏可拖动调宽（设计窗口 2026-11-12，ui-m1 2a 第 12 条 `069b7bf`，令牌 `--sidebar-w-min/max/rail`）：64（仅头像）～390px，全局一个偏好值，走偏好命令（GlobalSettings 加字段、照 SetPlayfulUi 那样的命令，加法）；前端 `--sidebar-w` + rAF；验收 64、240、296、390 四个宽度。T09 2b 合入后由 T09 developer 做（协议、假后台、客户端），真后台受理归 T10a。
- 待排小卡（2026-11-12）：① CI Ubuntu 的 test job 用 33 分钟，离 40 分钟上限只剩 7 分钟（cargo test 本身约 25 分钟；前端已拆成单独 job，fed3802），可按 crate 拆 cargo test 或把 release 构建挪成单独 job；② e2e:webview 的 reload 用例偶发失败，像是启动 5 秒取基线时客户端还在预热（不是泄漏，峰值都在 120 MB 上下），改成等页面报加载完成或取稳定值。
| T10a～T10g、T11 | T10a 未开工（等 T04 组 3、T06 S6–S9）；**T10d/e/f 方案 2026-11-12 开写**（产品负责人「现在派」）：architect `93d94e7c`（T10d 附件）、`da37bce2`（T10e 亲手执行与安全清单）、`a2c030fb`（T10f 本机导入），各写 `docs/plans/T10x.md` | — | 写完各开审方案任务 `<任务>-plan` | — | 方案审过后等 T10a 合入再实施 |

查开着的问题：`agent-scaffold findings list --task <任务 ID> --open`。记账工具轮次只能 1～9，超过换新任务 ID。

- **收窄（产品负责人 2026-11-11）**（已结束：T05、T07 合入，T04 开工）：机器负载 30、磁盘 12G，先让 T05 补丁与 T07 走完合入，T06、T09 暂停跑重测试（只推送让 CI 跑）；合入后删对应 worktree 腾磁盘，再放开 T06、T09。

## 3a 进行中快照（2026-11-12 16:20，清上下文前）

**worker 与在途任务**
| worker | 角色 | 在做 | 下一步 |
| --- | --- | --- | --- |
| `e5b90212` | developer | T04 组 3 第 1 轮修完（HEAD `bd5c883`，CI 绿；F-9 照 `9b6bdc1` 退出码 14） | reviewer `5166257f` 第 2 轮；**F-7 卡在打包**：tauri build 产物里没有后台程序和清单，要定打包方式（a tauri.conf 加 externalBin sidecar；b tauri build 后脚本放后台、写清单）与归属，待 supervisor/产品负责人定 |
| `b20e49db` | developer | T09 2b 第 5 轮修复中（`cao/165268d4`，HEAD `81d257c` 之后）：5/F-2 小幅滚动取消、5/F-1 强调按钮悬停对比度、13 个形象预览场景；5/F-3 选了行级游标 A（`before_seq/after_seq`），**等 architect 改 T09 方案 G11、2.2 第 4 条、R5、第 3 节的提交号再动 proto** | 下一轮是 2b 的第 3 轮（任务 round 6，最后一轮），code `e036441c` + UI `dbe31771`；预览 1444（PID 23651）。4/F-6 未关（随 5/F-2）；4/F-3 等设计窗口（1024 宽锚点导轨）|
| `4ea1efca` | developer | T06 S6-1 已送审（`cao/4ea1efca-s6-1` `9157424`，reviewer `6114386d` 第 1 轮，疑 hook 断开不发失败事件）；**在补 hook cold_start 基准**（9ef3808 对 9157424 交替 3 轮），之后做 S6-2 | S6 共 5 批（方案 `3096cb7`）：S6-4 合入等 D2、D5 |
| `b57765cc` | developer | 实测脚本 script4（`--part skill`，6 格）第 1 轮 5 条已修（`575e80c`） | reviewer `ad7b3f77` 第 2 轮；过了合入 main，请产品负责人跑 `--only R2X --part skill`。另：selftest 与真实运行同时在家目录跑会让「运行前后原有文件核对」误报，待单开 |
| `47737e78` | architect（T06） | ① 借改 T09 方案（行级游标，5/F-3）；② 写 R2 补测分析报告（结果解压在 scratchpad `r2x/`），给 D2、D5 推荐并改 T06 方案 | — |
| `93d94e7c` | architect（T10d） | T10d 方案三轮审过（`25c4b14`），1/F-2、1/F-4 等 PD-6、PD-9 | 等产品负责人定 PD-1～PD-4、PD-6～PD-9（PD-5 等实测 A2） |
| `da37bce2` | architect（T10e） | T10e 方案第 2 轮不过（2/F-1～F-4），在改，第 3 轮是最后一轮；reviewer `8036b426` | Q1～Q6、K1～K5 等产品负责人 |
| `a2c030fb` | architect（T10f） | T10f 方案第 2 版 `5460e55`，reviewer `9a5980a0` 第 2 轮在审 | PD-1～PD-9 等产品负责人；1/F-8（仓库技能 hooks/allowed-tools，改动前就有）等 `--part skill` 结果交产品负责人 |

**16:20 之后的变化**：
- T09：方案改完 `43034ce`（行级游标 R14、2.9 行级语义、步 5c，另补同 seq 一起丢、客户端取 limit 行、继续卡片跟范围走）。`b20e49db` 本地已提交 5/F-2 `0b32fb8`、5/F-1 `00e1322`（另把全开档药丸底色 12%→6%，请 UI reviewer 看）、形象一览 `c72b2f0`（`/preview.html#/characters`），在做步 5c，做完一起跑全量和重录。真后台的行级查询归 T10a（方案第 3 节五条要点，和卡片 `6376fce` 一致）。
- T10e：方案第 3 版 `6eb61c3`（换成「卡片宿主」对账机制），reviewer `8036b426` 第 3 轮（最后一轮）在审。**过了之后**把 1.4 的 A2（提交观察者唤醒卡片宿主）、A5（删除对话收尾挂在已提交事件上）写进 T10a 卡片。
- T10f：第 2 轮 5 条（2/F-1 blocking：门槛关着时 discover 仍读凭据内容；2/F-2～F-5 important），1/F-2/3/4/6/7/9 已关 fixed；交回 `a2c030fb` 改，第 3 轮最后一轮。问产品负责人 PD-9 时带上漏网例子（`secrets/production.json`、`credentials/team.yaml`、`credentials.dev.json`）。`--part skill` 还缺 K3e/K3f/K3g，T10f 启用前要 T06 那边补格。
- T06 S6-1：reviewer `6114386d` 中间结论 2 条 blocking（1/F-1 hook 连接断开不发失败事件、已入队 allow 会无声丢；1/F-2 后台答完就撤 backstop、写响应无期限），等最终汇报和 `4ea1efca` 的基准。
- T04 组 3：reviewer `5166257f` 第 2 轮初步看到 F-1 没修全（已有注册项更新失败：macOS bootout 失败、Linux daemon-reload 失败后下一轮早退），托管实例 set(true) 自 bootout 也成立；等它记账。
- AGENTS.md 加了「加删素材后清 .vitest-cache」（`9dcac56`）。

**11-13 01:00 进展（最新，冲突以这里为准；产品负责人睡前授权「除非重大、必须他做的决策，今晚由 supervisor 代裁定」）**：
- 合入：实例锁偶发（842c594）、S6-3（7c2eaf2，三轮）。
- 产品负责人睡前新定（附录 B）：T10a P1 a / P2 a / P3 接受；T04p PD-1 只出 .app 与 Linux 目录、PD-2 自签证书形态进 CI、PD-3 演练用专用 macOS 账号；仓库技能文件头 hook 照常跑；专用目录设置全部是用户决定并接受两处后果；管理员托管设置记已知限制。
- T06：S6-4 `4ea1efca` 在做（取消、D5、P4、故障注入；先写交错属性测试）；3.1d（V3）实测方案定稿（51 实例：日常 22、一次性 29，表在 scratchpad/script6-cells.md）。
- T09 2b：第 8 轮 UI 过（7/F-1 已关），代码 `e036441c` 在审 4518cbb；过了 + CI 绿就合入，之后侧栏拖动单开一组。
- T10a：义务表第 1 轮 6 条（5 blocking），`da37bce2` 在改；8036b426 审。0a.10 十处已定归 T10a。
- T04p：方案第 2 轮 4 条（2 blocking），`a2c030fb` 在改，9a5980a0 审；实施者定 e5b90212（含 T09 步 10），等方案过 + T09 2b 合入后开工。
- T10f A：`b57765cc` 本地 WIP 0f4d51e，在套锁跑全量，过了推送送审。
- **待产品负责人手验**（醒后）：T09 实时界面 iwork-client PID 80649（4518cbb，long-tool-turn，上限 60），日志 /private/tmp/claude-501/-Users-lilithgames-LiWork-iWork--cao-worktrees-b20e49db/a6310d3b-7a47-4ed4-a967-24c5a7f5c6c2/scratchpad/live-4518cbb.log：① 侧栏点开长工具回合对话，用滚轮滚，`grep -a SetVisibleRows` 见 by_user=true；② 锚点跳审批后两端各翻到头、接回最新；③ 拖侧栏/悬停展开导轨/全开真实切换；④ 真窗口缩不到 880 以下、真切自动/全开/自填模型 ID 时药丸正常。预览 http://127.0.0.1:1447/preview.html。
- **醒后要产品负责人定的（夜里攒的）**：① T10a KA1（完全退出时存储卡住 5 秒内记不下取消，排着的回合下次打开会被派发；在跑的不受影响）要不要登记为已知限制；② 改动前就有的窗口：第一笔提交后、prompt 前进程没了，回合是 Running 带会话编号，会话记录在但空时用户点「继续」这一轮请求丢失（T05/T06 派发顺序遗留）——这次修 / 另开任务 / 不改；③ 告知：只有排着的消息、没有在跑回合时点完全退出也会先问一次（supervisor 按 P2 a 认可）；重连后没回执的取消/继续命令客户端丢掉不重发（supervisor 定，交 T09）。
- **醒后追加问产品负责人**：④ T10a 0a.10 第 16 条第三种剩余行为（被取消的那一趟 5 秒内没收完尾、第一笔又没提交的，留在排队，下次打开被派发）并进 KA1 一起问；⑤ 第 17 条：回合中断着、用户批准了一条审批、还没续接就完全退出，下次打开会按 T07 现有规定自动续接（依据是没作废的批准），和「继续」不跨重启（R7）不一致——要不要改 T07 让它也不自动跑。supervisor 已认可第 16 条前两种剩余行为。
- **醒后追加 ⑥**（2026-11-13 夜，T10a 审方案 7/F-2，blocking、改动前就有，T08 代码）：子模块的 `sub/.git` 是指向仓库外普通文件的链接时，`workspace.rs:473–480` 接受了 Symlink 再交给 gix，gix 跟着链接把外部文件读进内存（status/diff 已能触发；可读到凭据或数据目录，没发现原文转发）。最小修法：交给 gix 前拒绝 `.git` 末级链接，补无害用例。supervisor 推荐「这次顺手修，放进 T10a 步 2」；方案已写成待确认，确认前这一项不开工。选项：这次修 / 另开任务 / 不改。注意：修好之前 T10a 的改动探针不启用（「搞定」章不盖，architect 提、supervisor 同意）；选「不修」则 FR22 的「搞定」章在 M1 兑现不了，要另定启用时机。
- **本机异常（01:15 起）**：`ps -ax`、`top` 这类枚举全部进程的命令卡住不返回（卡着的 ps 已累积约 80 个，父进程有 LiWork 的 Orca Terminal Host 和几个 claude 会话），`pgrep`、`ps -p <pid>` 正常；实际进程约 1178 个，没到上限（每用户 5333）；负载 270–300。疑为某进程卡在内核拖住枚举。已让 worker 避开 ps -ax。醒后请产品负责人决定要不要重启机器（重启前要先让各 worker 收尾）。
- T10a 正文已交（a388831），`da37bce2` 在补「搞定标记」一行义务表，补完开第 4 轮（8036b426，正文 + 新行）。T09 2b 第 8 轮 2 条（交错测试本身），`b20e49db` 修第 9 轮。T10f A 第 1 轮 2 条、script6 第一批（9d58eb1）第 1 轮 3 条 important（判据太松：P4 F1、G1d′、k1–k3，任务 2026-11-13-m1-t06-s0-script6），都在 `b57765cc` 手上。T04p 方案第 5 轮通过、定稿 8047e1f，实施交 `e5b90212`，等 T09 2b 合入。
- T10f A：92e3827 已推，ad7b3f77 审（任务 2026-11-13-m1-t10f-a）；`b57765cc` 开 script6。
- T04p 第 3 版 8fd61b2，9a5980a0 审第 3 轮（最后一轮）。T10a 表第 2 版 7a531fc，8036b426 审。
- 待转设计窗口：tokens.css 259 行 --sidebar-w-max 注释「主区域至少 480」与 12b 的 568 不一致；P4 沙箱依赖缺失时的提示文案。
- 小卡待排：iwork-ipc slow_consumer 延迟阈值在高负载下偶发；CI 主干 rust-cache 常 miss。

**23:15 进展（最新，冲突以这里为准）**：
- 合入：S6-1（e949487）、T04 组 3（139b6f2，1/F-7 拆到新卡 T04p）、CI 余量（d7ac6bc）、偶发修复 replan（287529e）、S6-2（13b1d6c）。main CI 43228979354 绿。
- 产品负责人晚间新定（全部已进需求/AGENTS）：厂商能力提案 V1（默认开放，三种情况才关，已接受例外保留）、V2（保证面 = 底线 + 有副作用动作 + 越界读）、V3 选 b（专用配置目录），并定**用户亲手写进专用目录的设置是用户的决定**、接受两处后果（恶意仓库持久化写入；唯一出口与底线不可配置的例外）；管理员托管设置记已知限制不测；P16 改回仓库内容照常加载（iWork 读 CLAUDE.md/AGENTS.md、复制 .claude/skills|commands|agents 成插件）；PD-5 内联命令不关；P17 接受；技能文件头 hook 照常跑；PD-A1 名字照原样；D2=b″；D5=a（M1 关后台、M2 开放）；回合定义「到运行时静下来」；M1 回合中消息 0 档排队、不做停止并发送；Linux 出生时间刻度边界接受；Q7=a；最小窗口宽度 880（设计 12b）；我定 P4=a、超三轮授权（修法明确、范围不扩大可自行开轮）。
- T06：D2 方案定稿（6fcd26d）；3.1c 定稿（S6-6）；3.1d（V3）第 4 轮在改（f5380551 审；V6f–h 仓库技能 hook 改写已批准输入的补测——若「执行了改写后的」要回产品负责人，并回头套 3.1c）；S6-3 第 1 轮 7 条（4 blocking）`4ea1efca` 在修（含 AgentError::IsolationBroken{kind,plugin} 变体、current_thread、tracing 订阅器）；S6-4 停在 S6-3 之后。回合失败原因结构化记进 T10a 卡待评估。
- T09 2b：第 7 轮 6/F 已关、4/F-3 已关；第 8 轮 `b20e49db` 在修 7/F-1（最小窗口 880，侧栏拖动另开一组）、7/F-2～F-4，并补随机交错属性测试（先在 fefcce0 上证明有牙）。待产品负责人手验：滚轮 by_user=true、锚点跳审批两端翻页、拖侧栏/悬停展开/全开真实切换。
- 小卡：实例锁偶发（cao/e5b90212-instance-lock 0e72f65，CI 绿）派 5166257f 审；T09 CI 上的 boot::usage_exits_2 偶发由它补了诊断输出。
- 项目提示词新规则（273cd34）：architect 先交不变量与义务表、先测后定、核掉便宜前提；developer 交错属性测试要有牙、修复回报写新状态、变异判定与进程组收尾。
- 明早 9:47 提醒已设（会话内，清上下文会丢）。

**18:55 进展（最新，冲突以这里为准）**：
- **T10e 方案全部定稿**：安全清单 0e38d98（任务 lists 第 4 轮 []）、卡片部分 83fefad（第 5 轮 []），账上 0 开。待办：第 12 节交接转 T10a（删除四步、观察者唤醒）、T10b（删助手意图）、T02（投影器 check_cascade）、T04（ShutdownDaemon 受理改异步、card-host 致命任务、K6 存储卡 I/O、空闲 CPU 算 30 秒对账）、T07/T01（Edit 暂不出卡片、「用户执行失败」文案）；Q1–Q6 与 K6 交产品负责人。
- **T06 D2 方案定稿**（6fcd26d，第 4 轮只 1 nit 已改 5260d45）：S6-5 可按 5.4a 开工（要改 T05 process.rs 等退出那段；含产品负责人选「这次顺手修」的根组回收前清树）。
- **D5 = a**（M1 关后台任务 + Monitor 禁用；M2 和多成员一起开放），**P16 改回照常加载**（不经 --setting-sources project，iWork 读 CLAUDE.md/AGENTS.md 接系统提示、复制 .claude/skills|commands|agents 成插件经 --plugin-dir），**PD-5 内联命令不关**、**P17 接受**、**技能文件头 hook 照常跑**、**PD-A1 名字照原样**、**Linux 出生时间刻度边界接受**、回合定义「到运行时静下来」、M1 回合中消息排队（0 档，不做停止并发送）——全部进需求附录 B / 第 9 章 / AGENTS.md。
- T06 3.1c（P16 做法）方案第 1 轮不过（f5380551，任务 2026-11-12-m1-t06-plan-p16：1 blocking 插件放 run/h 被底线拦，4 important），`47737e78` 在改。P15 子代理记忆重开（放开真实会话前要 RC6 结论）。3.1c 不算放开真实会话的前提，实现排 S6-3 之后单开一批。
- S6-2 送审，`6114386d` 第 1 轮 3 条（1 blocking stdout 无界/协议错误被吞，2 important account 留存、取消 open 漏清目录），`4ea1efca` 在修。
- T04 组 3 第 3 轮（最后一轮）`5166257f` 在审 da2b0ac。
- T10f 第一部分 A 定稿（b88c802 PD-A1 版 + e877c60 插件只认 user 范围）；`b57765cc` 在做 A-1～A-3；script6（G1/G8/G7 改产品设置 28 实例 + RC1–RC7 12 实例 + AGENTS.md 两格）排在它之后，表在 scratchpad/script6-cells.md。
- 实测已跑：--part skill（175237）、--cell k3（180013），分析 scratchpad/r2y-analysis.md。
- **事故**：18:05 blob 办法写法错误把工作区需求文档截空，已从不可达 blob ae69b84 三方合并恢复（备份 2aad52f），记忆 blob-method-write-order。
- 磁盘 18:50 一度只剩 8 GB（StorageFull 打断 reviewer 全量），删了 c0814d74 worktree、main 的 target、各 scratchpad 变异副本的 target、两个空闲 worktree 的 incremental，回到 47 GB。

**17:45 进展（最新，冲突以这里为准）**：
- 合入：S6-1 → main e949487（CI 43187465223 绿）；`4ea1efca` 接着做 S6-2。
- 产品负责人新定：回合定义改成「到运行时静下来为止」（附录 B bf3bd9b）；Q7 选 a（4ef3479）。**待答**：PD-A1（本机导入列表显不显示合语法的名字，推荐 a，没答按 b 做）；M1 插话做到哪档（推荐 0 档 + 停止并发送，D5 选 a；1 档「发出就交 CLI」约 13–20 人天放 T10a 后或 M2；2 档放 M2；评估报告 scratchpad/non-strict-turns-assessment.md）。D5 随插话一起定。
- T10f 第一部分 A **定稿**（133a7a9 + 9b4da26，第 5 轮 []）。A-1～A-3 不依赖别的卡，可派 developer（打算 script5 合入后交 `b57765cc`）；A-4、A-5 等 T10a。设计窗口三问待转（列表怎么摆含固定句式、要不要写「M1 只能查看」、设置页摘要）。
- T10e：义务表审过（第 4 轮 --append []），`da37bce2` 在照表重写卡片正文；安全清单第 4 轮（0e38d98）`8036b426` 在审，过了定稿。K6（存储线程卡 I/O 查不出）要转 T04 并交产品负责人知情。
- T06 D2 方案审查（任务 `2026-11-12-m1-t06-plan-d2`，reviewer `f5380551`）第 1 轮 2 条 blocking（桥接退出后被 SIGSTOP 的 CLI 收到 SIGHUP/SIGCONT；只冻根组漏掉无组长组里新 fork 的后代），交 `47737e78`。S6-5 等它过审。
- script5 第 3 轮（42c1431）`ad7b3f77` 在审，最后一轮。

**17:15 进展（这一段比上面两段新，冲突以这里为准）**：
- 产品负责人今天下午定的（都已进需求附录 B）：T10e 开第 4 轮、补丁卡片拆到 T10h（e4e002c）、卡片任务与宿主 panic 一律 Fatal、安全清单先定稿；T10f 切开只做 A（能列出、不能导入）、撤回定为重启即撤回（3b5adc0）；联网口径按实测写进第 9 章（df56bd5）；**D2 = b″**（e4085e6）。**D5 未定**：产品负责人担心关后台任务后对话变成队列，已问 architect `47737e78` 回合中途发消息在 M1 怎么处理（设计 + 实测），答复后转产品负责人。
- T10e：义务表（0a，bd9ffb3）第 4 轮 4 条（4/F-1、4/F-3 blocking，4/F-2、4/F-4 important）交 `da37bce2` 改表，改完 reviewer `8036b426` 同轮 --append 审表，表过了再写正文。安全清单单独审（任务 `2026-11-12-m1-t10e-lists`）：第 2 版 3423f23，1/F-1、1/F-2 已关，新 2/F-1 important（会话启动后受保护路径改成链接，审批旧快照漏保护，K2/S11/U2 保证说过头），交 `da37bce2`；可能要产品负责人接受已知限制。P3「不认代理的程序由用户加进需凭据命令清单」就是 T10e 2.6 的增删命令，提示文案「全开档不受沙箱保护；带管道/重定向的写法会留在沙箱里失败」要转设计窗口。
- T10f：第 4 轮只审 A（013fc40），reviewer `9a5980a0`。3/F-1～F-3、2/F-3、1/F-5 已按范围关 not_worth；1/F-8（仓库技能 hooks/allowed-tools，改动前就有）仍在用户队列。技术设计第 1 节与 9.5、T10.md 已同步（f600c72）。设计窗口三问待转：「还没启用」列表怎么摆、要不要写「M1 只能查看」、设置页摘要写什么。
- T04 组 3：第 2 轮 4 条 blocking（2/F-1 更新失败误判已对齐、2/F-2 托管实例自 bootout、2/F-3 超时后同步诊断、2/F-4 测试没隔离真实 launchd——本机没有 com.iwork.daemon，未出事）；1/F-2/3/5/8/9 已关；2/F-2 的设计已写进 T04 方案 2.12（95efaed：托管实例只重写文件不重载）。`e5b90212` 修，第 3 轮是最后一轮；变异清单（22 项，21 杀 1 等价）第 3 轮派活时转 reviewer `5166257f`。1/F-6 结果已补（可接受），1/F-7 仍卡打包。
- T06 S6-1：第 1 轮 2 条 blocking 已修（ec59609、34615cb），reviewer `6114386d` 第 2 轮在审。cold_start 基准：负载 3–4 倍核数，A/B 差距小于噪声，正式数挪到 mac120（S9）。CI 43180045901 Ubuntu 偶发已重跑。
- 脚本：script4 已合 main（92453d5）；script5（87e702a）第 1 轮 3 条（1/F-1 硬链接、1/F-3 甲第二种形状 blocking，1/F-2 提示末行 important），另加 k/b/n/工具清单几格改带产品沙箱设置、加一格带产品设置重记 k3/k4。`b57765cc` 修，`ad7b3f77` 审。过了合 main，请产品负责人跑 `--only R2X --part skill`（周额度 15:25 时已 86%，提醒他）。
- T09：`b20e49db` 在做步 5c（43034ce 的行级游标）。
- 本机 launchd 现状：无 com.iwork.daemon；有 N1 留下的 com.iwork.t04probe 禁用覆盖记录，没动过。

**R2 补测已跑（产品负责人，2026-11-12 15:27–16:13）**：D2：SIGKILL CLI 进程组后前台命令照样跑完（k1），只关输入 CLI 不退（k2），**SIGTERM 时 CLI 自己收拾前台命令并退出（k3）**；D5：设 `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` 后子代理在前台跑、`run_in_background` 被拒后模型改前台重发（b1/b2），不设时子代理后台跑、2 个 init 2 个 result（n1）；D4 ①c′①d′ PASS；net：经隧道连 github:22 读到问候，带 allowLocalBinding 能连本机监听、不带连不上；`--tools`/`--disallowedTools` 认 Task 和 Agent 两个名字；G5 记忆写 PASS、记忆读 INCONCLUSIVE（脚本没跟上 b543196 放宽的甲）、子代理记忆守卫预检没过（改 R3）。**等 architect 报告后问产品负责人 D2、D5、P3 远端连接、P15**。

**等产品负责人的**：T10d PD 8 条（已发表格，推荐全按推荐）；T10e、T10f 的问题等审完整理；D2/D5/P3/P15 等分析；仓库 CLAUDE.md 在 `--setting-sources ""` 下不加载（等 `--part skill` 的 L1/L2）；要真机试：选择框「取消后再弹」与「重新定位」入口、Option 切「完全退出」、侧栏拖动（做好后）。

**待排**：侧栏可拖动调宽（T09，2b 合入后）；CI 两张小卡（Ubuntu test job 余量、e2e reload 预热）；Windows outbox 测试偶发（T04 组 3 CI）；iwork-vcs-git 测试 ETXTBSY 隐患；rootfs 临时文件名可预测（T04 g3 报）。

## 4 合入时要处理

- ADR：main 已用 0001–0012；T04/T06/T07/T09 方案里 `ADR-T0X-a…` 占位，合入时让 developer 在分支上定号。
- 合入前分支先 `git merge main`；STATUS.md 冲突用 main 版本。
- 每张卡合入后：任务卡「完成记录」、`report_outcome`、assign retrospector（handoff 会超时）、删 worker（use_worktree 的终端删了会清 worktree，分支合入后再删）。

- **完成标志 = 一次合入 main**（产品负责人 2026-11-10 同意；之后每个完成点会清上下文）。小卡一张卡算一个合入单元；大卡按方案的实施步骤分组，每组单独合入、单独验收（如 T07 E3–E5、T06 S2），方案定稿时在第 3 节列出分组。完成要同时满足：合入 main 并推送、CI 过；`findings list --task <任务> --open --queue developer` 为空，`--queue user` 已列给产品负责人并记账；任务卡完成记录、`report_outcome`、retrospector 做完；本文件对应行改成已合入并写下一步。
- 另一个能清上下文的点是方案定稿：审过、决定写进文档并提交、实施还没开始。审查轮次进行中、等产品负责人回答、worker 交活还没核对时不清；碰上了先把状态写进本文件再清。
- 每个合入单元用新的 developer 和 reviewer；审方案和审 diff 的 reviewer 分开。

## 5 踩过的坑

- **变异测试挂住、占着重测试锁（2026-11-11）**：T05 复审的 reviewer 在副本里做变异，被改坏的代码让 turn_runner 挂住 1.5 小时，一直占着 `/tmp/iwork-heavy-test.lock`，别的卡全排在后面。派 reviewer、developer 做变异时写明：测试命令外面套 `timeout`（如 120 秒），挂住按「测试抓住了」记。排查：`ps -o pid,etime,command` 看 lockf 进程，`lsof -p <pid> | grep cwd` 看是谁的副本。（调优窗口 T-2 期间不改 reviewer 项目提示词，先写在派活消息里。）

- **worker 窗口被缩成 93×7（2026-11-10 夜）**：所有 worker 收件箱卡了 9～12 小时。`agent-scaffold ps` 出现「收件箱有 N 条消息在等待投递」先看 `tmux list-windows -t cao-a46fd0b6 -F '#{window_index} #{window_width}x#{window_height}'`，太小就逐个 `resize-window -x 140 -y 45`。

- **supervisor 收件箱卡住（2026-11-10 11:22–13:30）**：CAO 把 supervisor 窗口认成「在等回答」，14 条消息一直 pending。看 `agent-scaffold ps` 里 supervisor 那行；可以用 `curl -s 'http://127.0.0.1:9895/terminals/f6c88a81/inbox/messages?status=pending&limit=100'` 直接读出来。
- 磁盘：`target/debug/incremental` 最占地方（T07 分支一个就 26G），空闲 worktree 可以直接删；派 reviewer 时让它复用 developer worktree 的 target，并设 `CARGO_INCREMENTAL=0`。

- **磁盘满（2026-11-10 10:30）**：各 worktree 的 target 加起来七八十 G，reviewer 和 developer 的临时副本又各带一份 release target。已合入分支 worktree 的 `target/` 直接删（`2d513126`、`2e7bcbe5`、`49facf33`、`fecce968`）。派活时写明：临时副本跑完就删。

- 新窗口太小消息投不进：`tmux resize-window -t cao-a46fd0b6:<窗口> -x 140 -y 45`。
- 看门狗「停下没回报 / 卡住 / 在等回答」常是误报（等后台任务或 CI），先看窗口。非安全类权限框可 `answer_user_prompt`，但有倒计时会自动拦下。
- CAO 只允许 worker 给创建它的终端发消息；给 reviewer 的材料由 supervisor 转。
- 机器负载常 600+，基准只看交替测量的相对值。
- `git add <文件>` 前确认文件里没有产品负责人未提交的改动（`182f71b` 误把技术设计 9.0a 一起提交过）。
- T07 实施 A 侧审到第 6 轮：每轮修法常带出新回归，派活时要求「精确语义不变、资源边界失败关闭」两条底线。
