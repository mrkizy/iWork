# 代码主管：项目补充要求

> 只写本项目特有的约定、背景、边界和验收规则。通用职责由 CAO profile 提供。

## 项目背景

iWork：本机运行的 ADE（Agent Development Environment），Rust daemon + Tauri/SolidJS 客户端，当前阶段 M1。依据文档、技术栈、命令、架构约定与安全不变量见根目录 `AGENTS.md`；M1 进度与未落文档的决定见 `docs/tasks/M1/STATUS.md`。

## 必须遵守

- 派活单位是 `docs/tasks/M1/` 下的任务卡。派活消息里写清任务卡路径、任务 ID、轮次；验收以卡上的“验收标准”为准，不自行增删。
- 按卡上“依赖”排序；同一列无依赖的卡可并行派给不同 developer（各用独立 worktree）。
- 卡上标“高风险”的（T01、T02、T03、T06、T07）先让 architect（或 developer）出方案写进 `docs/plans/`，经 code_reviewer 审方案后再实施。
- `docs/tasks/M1/README.md` 的“未决事项”（未划掉的）只能由产品负责人决定；相关卡在方案阶段整理选项与证据后交用户，不得替用户选定。
- 实现中发现文档冲突或设计行不通：暂停该任务，把问题、证据、可选方案整理给用户决定；用户拍板后先改 `docs/`，再继续。不要让 developer 在代码里悄悄偏离。
- `AGENTS.md` 的“安全不变量”出现任何违反，按 blocking 处理，不接受“以后再修”。
- 任务完成后在卡片末尾“完成记录”里追加：日期、合入提交、偏离设计之处（若有）及用户确认记录。
- 同一时间只跑一张卡的重测试（产品负责人 2026-11-10 定）：派活消息里写明重测试套 `lockf /tmp/iwork-heavy-test.lock <命令>`（规则在 `AGENTS.md` 命令节）。
- 用 use_worktree 建的 developer 终端，删终端会清掉它的 worktree；分支合入前不要删。
