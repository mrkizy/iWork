# ADR-T07-a Bash 命令用 brush-parser 解析，隔离在 iwork-shell，只放行能证明安全的形态

- 状态：已采纳（2026-11-10，T07；编号合入时由 supervisor 统一）
- 依据：`docs/plans/T07.md` 3.3–3.6、第 16 节；技术设计 7.1；需求第 9 章已知限制

## 背景

审批要按文字理解 Bash 命令，解析器的质量直接决定底线能不能被绕过。完整建模 Bash 语义不可能不漏。

## 决定

1. 用 `brush-parser` 0.4（MIT、活跃、纯 Rust、完整 bash 语法、词保持未展开的原文），只在新 crate `iwork-shell` 里出现；对外是我们自己的小 AST，**小 AST 里只有字面词**（引号、转义去掉后的值）。表达不了的形态（`for` / `case` / 函数 / 复合命令上的重定向 / 任何展开、通配、`~user` / 前置赋值 / 未加引号的 here-doc / here-string / 进程替换 / `time` / `[[ ]]`）整条返回 `Unparsable` → 三档询问、不记住、不审查。
2. `parse` 两遍：先在完整 AST 上收集字面重定向与字面词（`PartialFacts`），再转换；放弃时事实随 `Unparsable` 返回，归一照常抽成路径动作过底线。第三方解析器整体失败（语法错误）时，事实取自词法分析的词；第三方解析器 panic 也按解析不了处理（失败即关闭）。
3. 子命令只在「白名单程序 + 选项全部登记（`ArgSpec`）+ 位置参数角色对得上」时归为读 / 写 / 删除，其余归执行；表外程序的位置参数、`name=value` 的值、`--opt=值` 记读 + 写；登记过的路径选项（`PathOptions`）按角色；没登记的短选项粘值整条解析不了。解析不了的调用用通用兜底规则 `fallback_candidates` 宁可多抽；`eval "…"` / `bash -c '…'` 的参数再解析一层抽事实。
4. 实施中补的两处（报告「偏离方案处」列出）：
   - `git` 的未登记短选项粘值不让整条解析不了，改为按兜底候选抽读 + 写：方案 11.3 验收 4b 要求 `git clean -fd` 是必问而不是解析不了，与 3.4「没登记的粘值整条 Unparsable」冲突；`git` 的种类由子命令决定，候选里任何一个解析到受保护路径仍是底线拒绝。其他程序仍按方案。
   - `git -C` / `--git-dir` / `--work-tree` / `--namespace`（改变仓库位置）与子命令前未登记的全局选项整条解析不了，否则 `git --namespace x push` 能把推送藏成普通执行。

## 备选及放弃原因

`tree-sitter-bash`（容错解析与需求相反，带 C 构建进三平台 CI）；`yash-syntax`（GPL-3，不在许可白名单）；`conch-parser`（2019 年起无人维护）；自写（bash 引号、here-doc、展开的边角正是攻击面）；完整语义建模（漏一处即放行有副作用的命令）。

## 影响

- 传递依赖多约 77 个 crate（`brush-parser` 把 `insta` 等写成了普通依赖），许可都在白名单内、闭包里没有禁用 crate（`cargo deny`、`scripts/check-deps.sh` 通过）。换解析器只改 `iwork-shell`。
- 已知限制：自动档下带 `$VAR`、通配、组合短选项（`tar -xzf`、`make -j8`，`git` 除外）的日常命令会多问。
