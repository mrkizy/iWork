# M1 进度与决策（supervisor 交接记录）

> 最后更新：2026-11-10 11:15。上下文压缩或换会话后，supervisor 先读这份文件再动手。
> 只记当前状态与还没写进其他文档的决定；已写进需求、技术设计、任务卡、方案的，以那些文档为准。

## 1 运行安排

- **12:30 机器重启、产品负责人重新生成了角色**：旧 CAO 会话 `cao-4b34c5da` 与其中所有 worker 都没了，新会话 `cao-a46fd0b6`，supervisor 终端 `f6c88a81`。worktree 都还在（`git worktree list`）。现在的角色：developer = claude_code `claude-opus-5-5`；code_reviewer = codex `gpt-6-astra`；architect = claude_code `claude-fable-5-2`。
- **重新生成把 `.cao/project-prompts/*.md` 换成了空模板（未提交）**：原来 code_supervisor 的 7 条项目规则和 code_reviewer 的项目定级规则都在 git HEAD 里（`git show HEAD:.cao/project-prompts/code_reviewer.md`）。等产品负责人决定是否把旧内容填回新模板；在那之前，派 reviewer 时把定级规则写进派活消息。
- 12:35 已派：T08 developer `4960bf0d`（worktree `.cao/worktrees/49facf33`，结束合并 + 6/F-1 变异 + 全量 + 推送 CI）；T03 developer `b271f92f`（worktree `.cao/worktrees/2e7bcbe5`，clippy 四组 + 基准前后对比 + 回报文档）。
- 定时任务都只在旧会话里有效，重开后不存在；gh 已登录（账号 mrkizy），不用再提醒。
- **推送**：main 推送已获同意；T05、T08 分支已获同意推送跑 CI。T03 分支推送跑 Windows CI **还没获同意**。
- **重测试排队（产品负责人 2026-11-10 定）**：同一时间只跑一张卡的重测试，统一用 `lockf /tmp/iwork-heavy-test.lock <命令>`，规则写在 AGENTS.md 命令节。派活消息里要写明。
- **worker 配置被产品负责人改过（未提交，不要动）**：`.cao/agent-store/code_reviewer.md` 改成 `claude_code` + `claude-fable-5-2`，`architect.md` 改成 `claude-fable-5-2`。之后新开的 reviewer 跑在 Claude 上，审查指标按新配置分组；旧的 codex reviewer 终端仍是 codex。

- **11:05 产品负责人更新了角色配置，要求换新窗口**：developer → codex `gpt-6.1-sol`（high）；code_reviewer → claude_code `claude-fable-5-2`（high）；architect → `claude-fable-5-2`（`.cao/agent-store/*.md`，产品负责人的改动，未提交，不要动）。旧配置的 reviewer、architect 终端已删。**旧 developer `2e7bcbe5`（T03）、`009e8660`（T07）、`49facf33`（T08）不要删**：它们是 use_worktree 建的，删终端会把对应 worktree 目录清掉；新 developer 直接在这些 worktree 里接着做，等分支合入后再删旧终端。新 developer 是 codex：发给它的消息会立刻插进它正在做的这一轮，只发要它马上照做的指令。

## 2 产品负责人 2026-11-10 12:40 前后的决定（重开后无须再问）

- 项目提示：把原项目规则填回新模板（已做，`4f7db54`）。
- fork+exec 冒充：登记已知限制（已写进需求 v1.73）。
- T03 分支推送跑 Windows CI：同意（已让 T03 developer `b271f92f` 加 Windows 实测 job 并推送）。
- T07 方案按 supervisor 建议收口（链接两种视图都过底线、find 未登记谓词整条 Unparsable 且 -delete 登记为删除、resource-limit 挪到 UserDeny 层、任何词里的命令替换都收；9/F-7 顺手修）；接受两条代价：替换指向受保护位置的链接会被拒（可走亲手执行）、`find … -exec …` 每次都问。已派 architect `e7a8c21c` 改方案。
- T07 `cp -r` 递归复制只检查两层：接受为已知限制。
- 以上 T07 的三条已知限制（cp -r 两层、替换受保护链接被拒、find -exec 每次问）在方案收口通过后一起写进需求第 9 章。

- **T07 方案定稿（产品负责人 2026-11-10 13:40）**：收口第 5 轮两处小改（flat_delete 连 `-delete` 也当候选；路径解析「查不清」打 PathUnresolved 三档拒绝）完成即定稿进实施，不再复核；需求第 9 章加总限制「命令文字静态分析覆盖常见写法与已知绕过，刻意构造的写法仍可能只被询问；批准解析不了的命令前要看清内容」，与 T07 其余已知限制一起写成 v1.74。之后发现的同类写法按普通缺陷在实施阶段修。
- T07 1/F-12：按建议，AGENTS.md 架构约定已加澄清、code_reviewer 项目提示同步，F-12 记 intentional。

- **T06 方案决定（产品负责人 2026-11-10）**：P1 基准随本机实际 CLI 版本走（现 2.1.322），T0 关键结论由 iwork-t0g.py 复核；P2 未验证版本照常运行 + 启动自检，在运行时详情 / 诊断页标「未验证版本」，状态栏不显示（FR02 / AC18 不改）；P6 令牌维持环境变量 + 沙箱内屏蔽（知情）；P8 等待上限主计时挪到后台（知情，偏离技术设计 6.3，定稿后同步改设计）。P3、P4、P5、P7 等实测结果。已改 T06 任务卡第 7 条与验收 8。

## 3 各卡状态

| 卡 | 产物 | worker（终端） | 审查任务 ID / 轮次 | 还开着的问题 | 下一步 |
| --- | --- | --- | --- | --- | --- |
| T01 | 已合入 main `bb6d69a` | — | `2026-11-09-m1-t01` 已关 | 验收 5 空窗口截图未补 | 有空时补截图 |
| T02 | **已合入 main `2018280`**（ADR 0007–0009） | developer `fecce968` 待删（删除超时，重开后再删）；retrospector 已复盘（给 developer 存 2 条 lesson），终端 `91a45c1b` 可删 | `2026-11-10-m1-t02` 已关（两轮 8 条全 fixed） | — | — |
| T03 | **已合入 main**（ADR 0011、0012） | developer `b271f92f` 可删；旧 worktree `.cao/worktrees/2e7bcbe5` 分支已合入 | `2026-11-10-m1-t03` 已关 | — | — |
| T05 | 已合入 main `a6987d4`；Windows 偶发测试修复已合入 `a6a5007` | — | 已关；`2026-11-10-m1-t05-flake` 1 轮通过 | — | — |
| （方案复盘） | T03、T07 方案 | retrospector `f77efc99` 已复盘（给 architect 存 3 条 lesson），终端可删 | — | — | — |
| T07 | 方案定稿 `4e27665`（+57b5c4d 实施审查第 2 轮补）；实施分支 `cao/009e8660` @ `2e0ff5c` | developer `f8877913`；reviewer A `4f01bbc2`、B `edeff392` | 实施 `2026-11-10-m1-t07` 第 2 轮不通过：8 条新（2/F-1～F-8）+ 残留 1/F-1、F-7、F-10、F-13 | 共 12 条开 | developer 第 3 轮返工中；回报后原 A/B 第 3 轮复审 |
| T08 | **已合入 main `c1aaf6b`**（ADR 0010） | developer `4960bf0d` 可删；旧 worktree `.cao/worktrees/49facf33` 分支已合入 | `2026-11-09-m1-t08` 已关（七轮） | — | 补任务卡完成记录 |
| T04 | 方案编写中 | architect `3b7032e0` | 方案 `2026-11-10-m1-t04-plan` | — | 方案回报后开 reviewer 审 |
| T06 | 方案编写中（高风险；O1、O4、O10 实测结论交产品负责人定） | architect `4f7c1e8b` | 方案 `2026-11-10-m1-t06-plan` | — | 方案回报后开 reviewer 审 |
| T09、T10、T11 | 未开工 | — | — | — | 依赖前面的卡 |

查开着的问题：`agent-scaffold findings list --task <任务 ID> --open`。

## 4 合入时要处理

- ADR 编号：main 已用 0001–0009（0006 T05，0007–0009 T02）。T03 写的是 `T03-a/b`，T07 写的是 `T07-a/b/c`，T08 写成 `0006`（撞号），合入时让 developer 在分支上定号。
- 合入 T03 时一并核对：技术设计 3.5、T03 任务卡、需求 v1.72 已按方案同步（`f6cf4a3`）。

## 5 运行中踩过的坑

- CAO 新开的 worker 窗口高度太小（7 行或 48×28）时，收件箱认不出输入框，消息一直挂起：运行 `tmux resize-window -t cao-4b34c5da:<窗口号> -x 140 -y 45`。
- 看门狗的「停下没回报」「卡住」提醒经常是误报，先看窗口画面再处理。
- CAO 只允许 worker 给创建它的终端和它自己创建的终端发消息；developer 要给 reviewer 的材料由 supervisor 转。
- `handoff` 给 retrospector 曾超时 900 秒；改用 `assign` 不阻塞。`delete_terminal` 偶尔超时，过一会儿重试。
- 各 worktree 的 `target/` 很大（单个 6–13GB），磁盘满过一次。
- 机器负载很高（loadavg 600+，大头是 node、浏览器内核、杀毒软件），基准绝对值不可信，只看交替测量的相对值。
- CI 日志：`gh run view <run-id> --repo mrkizy/iWork --log-failed`。
