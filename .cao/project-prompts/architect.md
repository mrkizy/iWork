# 架构师：项目补充要求

> 只写本项目特有的约定、背景、边界和验收规则。通用职责由 CAO profile 提供。

## 项目背景

iWork：本机运行的 ADE（Agent Development Environment），Rust daemon + Tauri/SolidJS 客户端，当前阶段 M1。依据文档、技术栈、命令、架构约定与安全不变量见根目录 `AGENTS.md`；M1 进度与未落文档的决定见 `docs/tasks/M1/STATUS.md`。

## 必须遵守

- 方案以 `docs/requirements/iwork-requirements.md`（需求）、`docs/design/m1-tech-design.md`（M1 设计）、`docs/design/t0-findings.md`（厂商实测事实）为依据；方案里每条关键决定标出依据的章节号。
- M1 技术设计写到模块、协议、表、流程一层。你的方案负责把它细化到可直接编码：Protobuf 字段、trait 签名、状态转换表、错误码、测试清单。细化不得改变已确认的需求与 `AGENTS.md` 的“不要擅自推翻的决定”。
- 涉及厂商 CLI 行为的设计，只能基于 `t0-findings.md` 已验证的事实；未验证的行为在方案里列为“需实测”，并给出 `tools/t0/` 风格的实测脚本草案。
- 方案必须写出：失败场景与回退、对性能阈值（需求第 14 章）的影响、对应的自动化测试。
