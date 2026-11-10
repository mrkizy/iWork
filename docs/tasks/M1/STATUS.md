# M1 进度与决策（supervisor 交接记录）

> 最后更新：2026-11-10 10:55。上下文压缩或换会话后，supervisor 先读这份文件再动手。
> 只记当前状态与还没写进其他文档的决定；已写进需求、技术设计、任务卡、方案的，以那些文档为准。

## 1 运行安排

- **11:02 产品负责人说「继续」，已恢复派活**（10:53 曾要求关窗口前停下）。 已让在干活的 developer（T03 `2e7bcbe5`、T07 `009e8660`、T08 `49facf33`）立刻做 WIP 提交并停下；它们的进度以各自分支上的最新提交为准（见第 3 节）。
- 定时任务都只在旧会话里有效，重开后不存在；gh 已登录（账号 mrkizy），不用再提醒。
- **推送**：main 推送已获同意；T05、T08 分支已获同意推送跑 CI。T03 分支推送跑 Windows CI **还没获同意**。
- **重测试排队（产品负责人 2026-11-10 定）**：同一时间只跑一张卡的重测试，统一用 `lockf /tmp/iwork-heavy-test.lock <命令>`，规则写在 AGENTS.md 命令节。派活消息里要写明。
- **worker 配置被产品负责人改过（未提交，不要动）**：`.cao/agent-store/code_reviewer.md` 改成 `claude_code` + `claude-fable-5-2`，`architect.md` 改成 `claude-fable-5-2`。之后新开的 reviewer 跑在 Claude 上，审查指标按新配置分组；旧的 codex reviewer 终端仍是 codex。

## 2 等产品负责人定的事（重开后先问）

1. **T07 1/F-12：审批对 git 命令文字的分类能不能放在 core。** AGENTS.md「核心不写 git 特例」与 T07 方案 3.4（core 里 classify_git、git push 必问等）冲突。supervisor 建议在 AGENTS.md 架构约定加一句澄清：「审批按命令文字识别 git 等程序的子命令（如 git push 必问）属于审批规则，可以出现在 core 的规则登记表里；版本控制操作的差异仍收在 VcsProvider，核心不写 git 特例。」同意后改 AGENTS.md 与 `.cao/project-prompts/code_reviewer.md`，F-12 记 `intentional`。
2. **T03 fork+exec 冒充绕过对端校验**（`docs/design/t0-findings.md` 6.5，在 T03 分支上）：攻击进程 connect → fork 留连接 → exec 正版程序，校验看到正版、收发的是攻击者子进程；Linux、macOS 都复现，两个方向都有。supervisor 建议登记需求第 9 章已知限制：沙箱默认不放行本机 socket，能利用它的只有已在沙箱外跑任意代码的同用户程序，它本来就能直接改数据库；macOS 能修一半、Linux 没有对应手段，不加只管一半的机制。
3. **T03 推送分支跑 Windows CI**：Windows 映像绑定没实测，代码统一返回 Unavailable，M1 在 Windows 上会拒绝启动。建议加 Windows job 跑 `tools/t0/iwork-t03-windows-authenticode.ps1` 和 Windows 测试，需同意推送 `cao/2e7bcbe5`。
4. **T07 方案第 8 轮（实施审查补）不通过**，见第 3 节 T07 行；其中「`cp -r` 递归复制只检查目的目录与目的/源目录名两层」是要产品负责人接受的已知限制。

## 3 各卡状态

| 卡 | 产物 | worker（终端） | 审查任务 ID / 轮次 | 还开着的问题 | 下一步 |
| --- | --- | --- | --- | --- | --- |
| T01 | 已合入 main `bb6d69a` | — | `2026-11-09-m1-t01` 已关 | 验收 5 空窗口截图未补 | 有空时补截图 |
| T02 | **已合入 main `2018280`**（ADR 0007–0009） | developer `fecce968` 待删（删除超时，重开后再删）；retrospector 已复盘（给 developer 存 2 条 lesson），终端 `91a45c1b` 可删 | `2026-11-10-m1-t02` 已关（两轮 8 条全 fixed） | — | — |
| T03 | 方案定稿 `ae913d3`；实施分支 `cao/2e7bcbe5` @ `e3d6d5b`（WIP） | developer `2e7bcbe5`；代码 reviewer A `b520b815`、B `9c2a6c04` | 实施 `2026-11-10-m1-t03` 第 1 轮不通过（10 条） | F-1～F-8 已修（各自提交、带测试与变异），**F-9 进行中**（服务端快照交接已改回方案 2.8，还没补测试；proptest 夹具与客户端模型重写没开始），**F-10 没做**；全量检查与 ipc 基准前后对比没跑 | 重开后让 developer 继续 F-9、F-10，跑全量与基准，再交原 A/B 第 2 轮；另见第 2 节 2、3 |
| T05 | 已合入 main `a6987d4` | — | 已关 | — | — |
| （方案复盘） | T03、T07 方案 | retrospector `f77efc99` 已复盘（给 architect 存 3 条 lesson），终端可删 | — | — | — |
| T07 | 方案定稿 `a597d87`（实施时补了两行：PermissionDecider 接缝 `54ee753`、ArgumentPath `2d92a17`）；实施分支 `cao/009e8660` @ `88e7a7f`（WIP，已 merge main） | developer `009e8660`；architect `62a1aeb0`；代码 reviewer A `2eb2187c`、B `c2ab0fc2` | 实施 `2026-11-10-m1-t07` 第 1 轮不通过（25 条：22 blocking）；方案补丁 `2026-11-09-m1-t07-plan` 第 8 轮不通过（4 条） | 实施：A 块 F-3/4/5/8/9/10/11 已修（WIP `88e7a7f`，反例测试 review_round1_a.rs 只剩 redirect_dash 里 tee 一条未过），B 块 F-13～F-25 **还没开始**；F-1、F-2、F-6、F-7 等方案补丁；F-12 等产品负责人。方案第 8 轮：8/F-1 命令替换递归超 8 层按空处理仍可批准底线写；8/F-2 文件目标也追加「目标/源名」子路径，ENOTDIR 让普通复制降成 Unparsable、堵住亲手执行卡片；8/F-3 mv、ln 替换链接被当成写穿链接；8/F-4 find 的保底只有 Read，`find <根> -delete` 丢删除（旧缺口）。方案层面 F-2 已解决；F-1 原例已解决（find 同族另记） | 重开后：architect 改方案第 8 轮四条（`docs/plans/T07.md` 当前未提交改动已由 supervisor 先提交保存），reviewer `2eb2187c` 复核；再把方案补丁转给 developer |
| T08 | 分支 `cao/49facf33` @ `1f85580`（已推送） | developer `49facf33`；reviewer `8c64ada5` | `2026-11-09-m1-t08` 第 6 轮复审中（只审 bbefd46..1f85580） | — | **CI run 42745754671 三平台全绿**；第 6 轮复审通过后让 developer 把 ADR 0006 改成 0010 再合入 |
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
