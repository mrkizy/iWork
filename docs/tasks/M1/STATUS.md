# M1 进度与决策（supervisor 交接记录）

> 最后更新：2026-11-10 11:15。上下文压缩或换会话后，supervisor 先读这份文件再动手。
> 只记当前状态与还没写进其他文档的决定；已写进需求、技术设计、任务卡、方案的，以那些文档为准。

## 1 运行安排

- **11:15 产品负责人要重新配置角色、用 `--new-supervisor` 重开 supervisor 窗口，旧对话可能丢失。** 新 supervisor 接手时注意：
  - **在飞 worker 的回报会发给旧 supervisor 终端 `1bb602b5`**，新 supervisor 收不到。接手后先 `agent-scaffold ps` 看每个 worker 状态，用 `cao agent result <id>` 或直接看它的 tmux 窗口拿最后回复，再看各分支最新提交；必要时给 worker 发一条消息说明「以后回报发给新 supervisor」（若 CAO 不允许跨 supervisor 发消息，就让它停下，重新 assign 新 worker 接手同一 worktree）。
  - 在飞的 4 个：T03 developer `fe00e889`（codex，worktree `.cao/worktrees/2e7bcbe5`）、T07 developer `5638c08f`（codex，worktree `.cao/worktrees/009e8660`）、T07 方案复核 reviewer `ed326a3d`（任务 2026-11-09-m1-t07-plan 第 9 轮）、T08 复审 reviewer `e2991248`（任务 2026-11-09-m1-t08 第 6 轮）。复审结论以账本为准：`agent-scaffold findings list --task <任务 ID>`。
  - 旧 developer 终端 `2e7bcbe5`、`009e8660`、`49facf33` 是 use_worktree 建的，**删终端会清掉 worktree**，分支合入前不要删。
- 定时任务都只在旧会话里有效，重开后不存在；gh 已登录（账号 mrkizy），不用再提醒。
- **推送**：main 推送已获同意；T05、T08 分支已获同意推送跑 CI。T03 分支推送跑 Windows CI **还没获同意**。
- **重测试排队（产品负责人 2026-11-10 定）**：同一时间只跑一张卡的重测试，统一用 `lockf /tmp/iwork-heavy-test.lock <命令>`，规则写在 AGENTS.md 命令节。派活消息里要写明。
- **worker 配置被产品负责人改过（未提交，不要动）**：`.cao/agent-store/code_reviewer.md` 改成 `claude_code` + `claude-fable-5-2`，`architect.md` 改成 `claude-fable-5-2`。之后新开的 reviewer 跑在 Claude 上，审查指标按新配置分组；旧的 codex reviewer 终端仍是 codex。

- **11:05 产品负责人更新了角色配置，要求换新窗口**：developer → codex `gpt-6.1-sol`（high）；code_reviewer → claude_code `claude-fable-5-2`（high）；architect → `claude-fable-5-2`（`.cao/agent-store/*.md`，产品负责人的改动，未提交，不要动）。旧配置的 reviewer、architect 终端已删。**旧 developer `2e7bcbe5`（T03）、`009e8660`（T07）、`49facf33`（T08）不要删**：它们是 use_worktree 建的，删终端会把对应 worktree 目录清掉；新 developer 直接在这些 worktree 里接着做，等分支合入后再删旧终端。新 developer 是 codex：发给它的消息会立刻插进它正在做的这一轮，只发要它马上照做的指令。

## 2 等产品负责人定的事（重开后先问）

1. **T07 1/F-12：审批对 git 命令文字的分类能不能放在 core。** AGENTS.md「核心不写 git 特例」与 T07 方案 3.4（core 里 classify_git、git push 必问等）冲突。supervisor 建议在 AGENTS.md 架构约定加一句澄清：「审批按命令文字识别 git 等程序的子命令（如 git push 必问）属于审批规则，可以出现在 core 的规则登记表里；版本控制操作的差异仍收在 VcsProvider，核心不写 git 特例。」同意后改 AGENTS.md 与 `.cao/project-prompts/code_reviewer.md`，F-12 记 `intentional`。
2. ~~T03 fork+exec 冒充~~：产品负责人 11:20 定「登记」，已写进需求 v1.73 第 9 章已知限制。T03 合入时 t0-findings 6.5「未修」措辞改成「已登记已知限制」。
3. **T03 推送分支跑 Windows CI**：Windows 映像绑定没实测，代码统一返回 Unavailable，M1 在 Windows 上会拒绝启动。建议加 Windows job 跑 `tools/t0/iwork-t03-windows-authenticode.ps1` 和 Windows 测试，需同意推送 `cao/2e7bcbe5`。
4. **T07 方案第 8 轮（实施审查补）不通过**，见第 3 节 T07 行；其中「`cp -r` 递归复制只检查目的目录与目的/源目录名两层」是要产品负责人接受的已知限制。

## 3 各卡状态

| 卡 | 产物 | worker（终端） | 审查任务 ID / 轮次 | 还开着的问题 | 下一步 |
| --- | --- | --- | --- | --- | --- |
| T01 | 已合入 main `bb6d69a` | — | `2026-11-09-m1-t01` 已关 | 验收 5 空窗口截图未补 | 有空时补截图 |
| T02 | **已合入 main `2018280`**（ADR 0007–0009） | developer `fecce968` 待删（删除超时，重开后再删）；retrospector 已复盘（给 developer 存 2 条 lesson），终端 `91a45c1b` 可删 | `2026-11-10-m1-t02` 已关（两轮 8 条全 fixed） | — | — |
| T03 | 方案定稿 `ae913d3`；实施分支 `cao/2e7bcbe5` @ `d45eaea`（WIP） | **新 developer `fe00e889`**（codex，在 worktree `.cao/worktrees/2e7bcbe5` 里做）；旧 developer `2e7bcbe5` 保留勿删 | 实施 `2026-11-10-m1-t03` 第 1 轮不通过（10 条） | F-1～F-8 已修；F-9 剩客户端模型重写与两种变异；F-10 没做；全量检查与 ipc 基准前后对比没跑（基线要空闲时套锁重跑 c3e8cda 副本） | 新 developer 做完回报后，开新 reviewer（Claude）做第 2 轮；另见第 2 节 2、3 |
| T05 | 已合入 main `a6987d4` | — | 已关 | — | — |
| （方案复盘） | T03、T07 方案 | retrospector `f77efc99` 已复盘（给 architect 存 3 条 lesson），终端可删 | — | — | — |
| T07 | 方案 main `d2effd5`；实施分支 `cao/009e8660` @ `14e4513`（B 块回报草稿 docs/tasks/M1/T07-review-round1-b.md） | 新 developer `5638c08f`（codex）；旧 developer `009e8660` 保留勿删；方案复核 reviewer `ed326a3d`（回报没送到，结论从它窗口取） | 实施 `2026-11-10-m1-t07` 第 1 轮：A 块 7 条与 B 块 13 条已修（待第 2 轮审）；方案补丁 `2026-11-09-m1-t07-plan` 第 9 轮不通过（7 开：9/F-1～F-7） | 方案第 9 轮：9/F-1 末级不跟随没管尾斜杠（rm -rf g/ 删 .git）；9/F-2 find 保底 Delete 让只读 find 在工作区根被底线拒；9/F-3 install 实为替换目录项；9/F-4 WriteMode 不全；9/F-5 受保护路径自身是链接（可能）；9/F-6 resource-limit 应放 UserDeny 层；9/F-7 参数展开 / 算术 / here-string 里的命令替换没收（改动前就有） | **待产品负责人定 T07 方案收口方式**（supervisor 建议：写 / 删目标「链接本身」与「解析后目标」两种视图都过底线、任一命中就拒，去掉按程序分写穿 / 替换目录项；find 不加 Delete 保底、未登记谓词整条 Unparsable、-delete 登记为 Delete；resource-limit 挪 UserDeny；命令替换在任何词里都收） |
| T08 | 分支 `cao/49facf33` @ `1f85580`（已推送；CI run 42745754671 三平台全绿） | **新 developer `117437fb`**（codex，在 worktree `.cao/worktrees/49facf33` 里做）；旧 developer `49facf33` 保留勿删；reviewer `e2991248` | `2026-11-09-m1-t08` 第 6 轮不通过：6/F-1（important，x.r 主断言部分运行靠 racy 分支通过，测试加两行） | 6/F-1 | 新 developer 修测试 + merge main + ADR 0006→0010 + 全量 + 推送看 CI；回报后 reviewer `e2991248` 只核 6/F-1，通过即合入 |
| T04、T06、T09、T10、T11 | 未开工 | — | — | — | 依赖前面的卡 |

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
