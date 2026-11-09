# ADR-T07-b 需凭据命令的 hook 侧匹配与 excludedCommands 同源、同一会话快照，匹配器三态且只许更保守

- 状态：已采纳（2026-11-10，T07；编号合入时由 supervisor 统一）
- 依据：`docs/plans/T07.md` 3.7、4.3、6.9、第 16 节；技术设计 7.1、7.2c；需求 FR11

## 背景

Claude Code 按自己的规则决定哪些命令排除出沙箱。审批以为在沙箱内、实际在沙箱外，是唯一会让「沙箱外执行」绕过审批的路径；运行中的 CLI 不会跟着规则表变。

## 决定

1. 会话启动时算一份 `SessionSnapshot`（内置规则集 + 三张清单 + 临时敏感条目 + 约束 + 边界 + 清单游标），T06 注入 CLI 的与引擎 hook 侧用的是同一个对象（`SessionKeys.snapshot`）。清单变化只把对话标为 `rebuild_session_needed`，要立即生效须重建会话。
2. 匹配器 `credential::classify` 三态：每个简单命令都精确命中才 `Outside`；程序名带路径前缀、命中程序的子命令前有选项、都命中但有重定向时 `Ambiguous`，按沙箱外求值。
3. 对拍语料 `credential_corpus::CORPUS`（44 行）进 CI；`cargo run -p iwork-core --example dump-credential-corpus` 导出 JSON（清单按「程序 + 子命令」结构导出，翻成 `excludedCommands` 文字只在 T06 一处做），T06 的 O1 ④ 回填 Claude 实测列；断言 Claude 为 Outside 的行我们不得为 Inside。
4. 内置需凭据清单写到子命令一级（整程序只有 `gh`、`ssh`）；`scp` / `sftp` / `rsync` 不内置且在拒收名单（Q3）。

## 备选及放弃原因

两边各写一份规则（迟早分叉）；引擎实时读规则表（与 CLI 不一致）；一律按沙箱外求值（每条 `git status` 都要问）。

## 影响

带路径前缀的命令会多问；固定版本升级多一步对拍；清单改动要重开会话才对运行中的会话生效。
