# M1 进度与决策（supervisor 交接记录）

> 最后更新：2026-11-10 17:50。上下文压缩或换会话后，supervisor 先读这份文件再动手。
> 只记当前状态与还没写进其他文档的决定；已写进需求、技术设计、任务卡、方案的，以那些文档为准。

## 1 运行环境与规矩

- **CAO 会话**：`cao-a46fd0b6`，supervisor 终端 `f6c88a81`（12:30 机器重启后新开）。worker 回报只发给创建它的 supervisor 终端；换 supervisor 后看 `agent-scaffold ps`、worker 窗口、分支提交、审查账本来接续。
- **角色配置**（产品负责人重新生成；`.cao/agent-store/*.md`、`.cao/scaffold.json`、`.cao/workflows/*` 的改动未提交，不要动）：developer = claude_code `claude-opus-5-5`；code_reviewer = codex `gpt-6-astra`；architect = claude_code `claude-fable-5-2`。项目提示已把原规则填回新模板（`4f7db54`）。
- **推送**：main 推送已获同意（最近一次推到 `9cfb4ae`，之后的本地提交未推）；任务分支推送跑 CI 已获同意。gh 已登录，CI 日志 `gh run view <id> --repo mrkizy/iWork --log-failed`。
- **重测试排队**：`lockf /tmp/iwork-heavy-test.lock <命令>`（AGENTS.md）。本机先 `export PATH=$HOME/.cargo/bin:$PATH`（Homebrew 的旧 cargo 在前）。派活消息里写明。
- **性能验收**：mac120（`ssh mac120`，Apple M1 8 核，rustup 1.99.1，仓库 `~/work/iWork`）夜间空闲时段测；Kaspersky 不排除、作客观因素记录（T11 卡）。开发机基准只作粗筛。
- **UI/UX 问题**：按 AGENTS.md 问「设计窗口」；产品负责人负责转问——把问题列给他。设计窗口答复写在 `docs/design/ui-m1.md` 第 2a 节等，只留工作区不提交。
- **使用范围**：M1–M9 只有产品负责人本人使用（技术设计第 1 节）。
- 产品负责人工作区里未提交的改动：需求 v1.75（FR18 对话标题）、AGENTS.md（设计窗口一节、v1.75）、`docs/design/ui-m1.md`、`docs/design/ui/*`。技术设计 9.0a 已随 `182f71b` 提交（已告知，他没要求拆）。

## 2 已定、还没全部写进对应文档的决定（2026-11-10）

- **T04 方案**：Q1～Q3 按推荐；Q4 M1 不提供换回迁移前备份；Q5 登记已知限制 L6（看过完整代价后再确认）；**M1 要做有证书构建**（bundle_probe 整包验签，N5 两种包形态实测）。L6 定稿后写进需求第 9 章。
- **T05 旧缺口**（T04 审查 1/F-11、1/F-12）：单独修，任务 `2026-11-10-m1-t05-dispatch`，先于 T04 实施合入；修好后关 `2026-11-10-m1-t04-plan/1/F-11`、`1/F-12`（fixed，引用产品负责人「现在单独修」）。
- **T06 方案**：P1 基准随本机 CLI 版本（现 2.1.322）；P2 选 c：未验证版本首次使用前本机自动跑硬前提验证（约 20 次短调用、花订阅额度、几分钟内开不了会话），过了放行、不过阻断，状态只在运行时详情 / 诊断页，状态栏不显示；验证中是点名错误、没过落 Incompatible 带原因，显示归 T09 / T10；P6、P8 知情同意（P8 偏离技术设计 6.3，定稿后改）；P3/P4/P5/P7 等实测。 第 3 版补充（17:55）：本机验证实际约 80 次短调用、约十分钟（跑满 7×11 矩阵，已告知产品负责人）；P9 Windows 上 Claude 运行时 M1 不放行（与 T03 一致）；ApprovalDelivered 写进日志后旧程序起不来（S19），并入 T07 PD6 一起问；T06 S2 与 T07 折叠同一批合入，或 T07 折叠见到第一条 ApprovalDelivered 前不启用 awaiting_delivery。
- **T07**：方案 `4e27665` 定稿（+57b5c4d）；需求 v1.74 已登记静态分析限制；F-12 口径写进 AGENTS.md。**引擎按根因重做**（产品负责人批准）：PD1 加 grant_approval_id、PD2 卡片校验整张转换表、PD3 void_pending 返回 Result（删 aborted）、PD4 ApprovalDelivered / Decided 由 T07 先定义、两卡接线共同启用；PD5 已告知；**PD6 选项已补（T07.md 1682–1689），待问产品负责人**：推荐 (a) 新事件种类按「带迁移」对待、只能向前修（Q4 的延伸）；(b) 两步发版保留能解码的回退版本；(c) 换回升级前备份；(d) 忽略未知事件不能当默认。T06 S19 同一问题并入。PD4 改为 T07 E5 连编排一侧「采纳时提交 ApprovalDelivered」一起做、E3–E5 一组合入——T06 方案 2.6 第 2–4 条与 S2 分工要同步（等 T06 第 3 轮复审回来告诉 architect `4f7c1e8b`）。引擎方案 F-2 让投影器校验读回合 / 对话 / 助手三行（归属存续），比原批准稍宽，已告知产品负责人。
- **T09 方案**：D1 画布已统一（ui-m1.md 第 2 节第 3 条、第 2a 节）；D2 按需求 v1.75；D3 空态；D4/D5/D7/D8 按推荐；D6 关窗后后台发通知进 M1（归 T10）；G1～G4、G7、G8 第 1 步只加 proto 字段，填值归 T10。**设计窗口 Q9～Q12 待答**（重连重建时间线时的滚动位置、继续 / 放弃卡片处理后留不留痕、AGENTS.md 草稿卡片进对话后显不显示、项目级设置放哪）——已列给产品负责人转问。
- **T03**：M1 暂不支持 Windows；fork+exec 冒充登记 v1.73。

## 3 各卡状态（17:50）

| 卡 | 产物 | worker | 审查任务 / 轮次 | 开着的问题 | 下一步 |
| --- | --- | --- | --- | --- | --- |
| T01 / T02 / T03 / T05 / T08 | 已合入（ADR 0001–0012；T05 Windows 偶发修复 `a6a5007`） | — | 已关 | T01 验收 5 截图未补；T03 基准待 T11 空载复测 | — |
| T05 补丁 | 任务 `2026-11-10-m1-t05-dispatch`（prompt 前提交开始执行、会话 ID 绑定事件），use_worktree | developer `e6df3dbe`（看门狗说已停、没回报——先看它窗口 / 分支） | 未送审 | — | 拿到回报后开 reviewer；合入后关 T04 1/F-11、F-12 |
| T04 | 方案 `docs/plans/T04.md`（未提交，1343 行） | architect `3b7032e0`；reviewer `e4187f1a` | `2026-11-10-m1-t04-plan` **第 3 轮不通过**（已满三轮）：新 3/F-1（重拉函数不全、Usage 旁路）、3/F-2（有证书包监测漏完整性依赖，资源最后到位就永不验签）、3/F-3（旧进程 signer 固定探针，跨包形态升级永不重启）、3/F-4（有证书 SelfIdentity 没有运行中 cdhash） | 2/F-1、2/F-2 原场景已解决可关；1/F-8 按 Q5 记 intentional | **按规则交产品负责人决定是否开第 4 轮**（建议开：问题都在有证书构建这条新路径上）；改时顺手把三处「待判断」措辞改为已确认 |
| T06 | 方案 + `tools/t0/iwork-t0g.py`（未提交） | architect `4f7c1e8b`；reviewer `4674b8d1` | `2026-11-10-m1-t06-plan` 第 3 轮复审中 | 2/F-1～F-6 + 1/F-3/F-6/F-8/F-10 残留 | 改完原 reviewer 第 3 轮；ApprovalDelivered / Decided 引用 T07 |
| T07 实施 | 分支 `cao/009e8660` @ `f716b15`（worktree `.cao/worktrees/009e8660`） | developer `f8877913`；A 侧 reviewer `4f01bbc2`；B 侧 reviewer `edeff392`（空闲） | `2026-11-10-m1-t07` A 侧第 7 轮复审中（含三版全面回归对照） | A：3/F-2、5/F-1、6/F-1～F-4；B：1/F-13、2/F-1、2/F-2、3/F-3～F-8（等引擎重做） | 派 4f01bbc2 第 7 轮核 6/F-*，并跑一次全面回归对照；B 侧等引擎方案 |
| T07 引擎方案 | `docs/plans/T07.md` 6.10 + `docs/plans/T02.md` 状态前提（未提交） | architect `890c712d`；reviewer `099b7f24` | `2026-11-10-m1-t07-engine-plan` 第 2 轮复审中 | 1/F-1～F-5 | 改完原 reviewer 第 2 轮；PD6 问产品负责人 |
| T09 | 方案 `docs/plans/T09.md`（未提交，429 行） | architect `62db2f05`；reviewer `9cbb7f89` | `2026-11-10-m1-t09-plan` 第 2 轮不通过：2/F-1（断线丢在飞的摘要重查）、2/F-2（reload 不释放 Tauri 旧帧暂存）、2/F-3（非时间线大 patch 超 1 MiB 永远发不出，blocking）；1/F-3～F-7 已解决可关，1/F-1、F-2 部分 | 见左 | architect 第 3 轮修改中；设计窗口 Q9～Q12 |
| T10、T11 | 未开工 | — | — | — | T10 依赖 T06、T07、T08、T09 |

查开着的问题：`agent-scaffold findings list --task <任务 ID> --open`。记账工具轮次只能 1～9，超过换新任务 ID。

## 4 合入时要处理

- ADR：main 已用 0001–0012；T04/T06/T07/T09 方案里 `ADR-T0X-a…` 占位，合入时让 developer 在分支上定号。
- 合入前分支先 `git merge main`；STATUS.md 冲突用 main 版本。
- 每张卡合入后：任务卡「完成记录」、`report_outcome`、assign retrospector（handoff 会超时）、删 worker（use_worktree 的终端删了会清 worktree，分支合入后再删）。

## 5 踩过的坑

- 新窗口太小消息投不进：`tmux resize-window -t cao-a46fd0b6:<窗口> -x 140 -y 45`。
- 看门狗「停下没回报 / 卡住 / 在等回答」常是误报（等后台任务或 CI），先看窗口。非安全类权限框可 `answer_user_prompt`，但有倒计时会自动拦下。
- CAO 只允许 worker 给创建它的终端发消息；给 reviewer 的材料由 supervisor 转。
- 机器负载常 600+，基准只看交替测量的相对值。
- `git add <文件>` 前确认文件里没有产品负责人未提交的改动（`182f71b` 误把技术设计 9.0a 一起提交过）。
- T07 实施 A 侧审到第 6 轮：每轮修法常带出新回归，派活时要求「精确语义不变、资源边界失败关闭」两条底线。
