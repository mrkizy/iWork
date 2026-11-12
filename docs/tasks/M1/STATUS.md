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
| T06 | 方案定稿（D1=A 大改 `624aa28`…`4a96669`，审查任务 `2026-11-12-m1-t06-plan-d1` 三轮通过）；**第一批 `d075a94`、S2 `a0d6cba`、S2b `a0a4bfb` 已合入 main**（2026-11-12） | developer `4ea1efca` 做 S3a、S4、S5（任务 `2026-11-12-m1-t06-s3a-s5`）；developer `b57765cc` 做脚本第二批（任务 `2026-11-12-m1-t06-s0-script2`，方案 3.4/16.1 的新格子）；architect `47737e78` 留着 | 脚本任务 `2026-11-12-m1-t06-s0-script` 第一批已合入 `655449e`，1/F-7（本机用户名进结果包，改动前就有）**等产品负责人定** | **产品负责人 2026-11-12 定**：D1=A（hook 不表态时 CLI 不问的动作照 CLI 规则走，已知限制；AGENTS.md 例外条款、技术设计第 1 节、t0-findings 2.4 已写）；只读命令一项接受；P12 记为已知限制（不在添加项目时拒绝）；需求第 9 章由产品负责人自己加（草稿已给）。**P11 已定 d + a（2026-11-12）**；P4（Linux 沙箱起不来时拒绝启动还是照跑，推荐 a）是 S6 开工前置：S4 的设置常量暂不写 `sandbox.failIfUnavailable`，定了再加；S5 样本由 developer 本机录（产品负责人同意，≤10 个 Haiku，入库前脱敏、grep 零命中，supervisor 合入前再核）；D2 后台命令脱组等 R2。S0 第一轮、R1、R1x（14 个实例，其中 7 个因我的分析脚本 bug 重跑）已跑；**R2 等脚本第二批审过再问产品负责人**（约百来个实例）；R3 要一次性环境（S6 起每组合入前要 R3 的 G2 硬格子全 PASS）；第一轮 ④ 的 17 行不回填 T07 |
| T07 | **已合入 main `d1086d4`**（2026-11-11），完成记录见 T07.md | — | 已关 | — | 遗留：性能 +13% 原因待查（开小任务）、启动折叠与 T04 冷启动一起定、真实观察者接线归 T10a；macOS CI 抖动 `turn_runner::answer_arriving_while_cancelling_is_rejected`（iwork-core，T04 组 2 CI 首跑红、重跑绿，2026-11-11）待开小任务查 |
| T07 引擎方案（已定稿、已实施） | `docs/plans/T07.md` 6.10 + `docs/plans/T02.md` 状态前提（未提交） | architect `890c712d`；reviewer `099b7f24` | `2026-11-10-m1-t07-engine-plan` **第 4 轮通过，定稿 `b6bf0a6`** | — | developer f8877913 在分支上按 6.10.14 做 E1–E7；真实观察者接线测试归 T10a 合入门槛 |
| T09 | 方案 `docs/plans/T09.md`（未提交，429 行） | architect `62db2f05`；reviewer `9cbb7f89` | `2026-11-10-m1-t09-plan` 第 2 轮不通过：2/F-1（断线丢在飞的摘要重查）、2/F-2（reload 不释放 Tauri 旧帧暂存）、2/F-3（非时间线大 patch 超 1 MiB 永远发不出，blocking）；1/F-3～F-7 已解决可关，1/F-1、F-2 部分 | 见左 | 第 3 轮不通过（3/F-1 interceptor 守门无效、3/F-2 跨代旧响应覆盖），**方案定稿 `1df7d74`**，实施见下一行；设计窗口 Q9～Q12 待答 |
| T09 实施 | **第一组 `414089b`、组 2a `d9fab70` 已合入 main**（2026-11-12，审查三轮）。剩：组 2b = 步 5b 锚点数据 + 锚点组件 + 步 5a（Dock、报在看哪、退出菜单）+ rfd 文件选择 + 拖入遮罩；组 3 = 7b、7c、7d；组 4 = 步 8、9；步 10 等 T04、步 11 等 T10 | developer `165268d4` 做 2b；code_reviewer `2a4efd5f`、ui_design_reviewer `0c8e300a` 留着复用 | `2026-11-11-m1-t09-ui`：2/F-1 令牌对比度（改动前就有）**等产品负责人定**；漏报 M-1 待产品负责人网站确认 | 全开档的赭红提示行要 NoteRow 加颜色，归 7b；2/F-2 找不到路径的项目行截断由 supervisor 暂定，待设计窗口补进 ui-m1 |
- T09 组 2b（2026-11-12）：步 5b、锚点组件、步 5a 已做（`eb0a1c0`）；developer `165268d4` 上下文满已交接、终端已关，`b20e49db` 接手做 rfd、拖入遮罩、P1 锚点断言。**产品负责人 2026-11-12 批准**：`objc2-app-kit` 加开 NSMenu、NSMenuItem，直接依赖 `objc2`、`objc2-foundation`（与 tauri 已带的版本相同）。待产品负责人真机试：按住 Option「退出 iWork」切成「完全退出 iWork」。
- 新表情（产品负责人 2026-11-12，设计 `cb6a9f2`，ui-m1 2a 第 14 条）：13 个形象 × 15 种表情，素材按清单数据驱动、不用改代码就能出现；但 `confused`（待回答卡片）、`tired`（运行时本机额度用完）、`sleep`（空闲 30 分钟）三种要接真实状态，并按新优先级（等你 > 疑惑 > 工作中 > 没电 > 出错 > 完成 > 瞌睡 > 空闲）改 `derive/avatar.rs` 的 MOODS：`confused` 与待回答卡片归 T09 组 3（7b），`tired` 的额度数据归 T10g，`sleep` 的计时可随组 3 做。彩蛋 5 种 M1 不触发。
| T10a～T10g、T11 | 未开工；T10 已拆成七张小卡（产品负责人 2026-11-10 同意，见 T10.md） | — | — | — | T10a 等 T04、T06、T07、T09 第一组合入；T10d/e/f 先出方案，可以和 T10a 的实施同时写 |

查开着的问题：`agent-scaffold findings list --task <任务 ID> --open`。记账工具轮次只能 1～9，超过换新任务 ID。

- **收窄（产品负责人 2026-11-11）**（已结束：T05、T07 合入，T04 开工）：机器负载 30、磁盘 12G，先让 T05 补丁与 T07 走完合入，T06、T09 暂停跑重测试（只推送让 CI 跑）；合入后删对应 worktree 腾磁盘，再放开 T06、T09。

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
