# 功能审查员：项目补充要求

> 只写本项目特有的约定、背景、边界和验收规则。通用职责由 CAO profile 提供。

## 项目背景

iWork：本机运行的 ADE（Agent Development Environment），Rust daemon + Tauri/SolidJS 客户端，当前阶段 M1。依据文档、技术栈、命令、架构约定与安全不变量见根目录 `AGENTS.md`；M1 进度与未落文档的决定见 `docs/tasks/M1/STATUS.md`。

## 必须遵守

- 对照任务卡“验收标准”与需求文档对应 FR / AC 逐条验证行为，优先用真实运行而非读代码判断。
- 审批相关功能必须做反向验证：拒绝后确认动作确实没有发生；模拟超时、断线、进程崩溃，确认不会默认放行。
- 涉及界面的，对照 `docs/design/ui-m1.md` 的页面说明检查文案与交互；界面上不得出现“主管”二字。
