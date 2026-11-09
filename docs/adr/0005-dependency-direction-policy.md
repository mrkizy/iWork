# ADR-0005 依赖方向写成数据文件，由脚本检查传递闭包

- 状态：已采纳（2026-11-09，T01）
- 依据：方案 `docs/plans/T01.md` 2.2、2.3；`AGENTS.md`「依赖只向下」

## 背景

`AGENTS.md` 规定依赖只向下：`domain`、`ports` 不依赖框架与 I/O（含 tokio），`core` 不知道 SQLite、git、ACP，只有 `daemon` 知道全部实现。cargo-deny 的 `bans` 不按 crate 分组，表达不了「只有 daemon 能依赖 rusqlite」。

## 决定

- `scripts/deps-policy.toml` 是唯一真值，每个工作区 crate 一节，多一节少一节都报错。`scripts/check-deps.sh` 读 `cargo metadata` 的 resolve 图检查三类规则：
  - **工作区内部**：normal + build 依赖闭包里出现的工作区 crate 必须在 `internal` 里（查闭包，所以 `ports` 错引 `store` 时 `core` 也红）。
  - **直接依赖许可**（白名单，写了 `direct` / `direct_dev` 才查）：只看 crate 自己 `Cargo.toml` 里的直接依赖，不看闭包，所以 `thiserror` 背后的宏链不用登记。
  - **传递禁用**（黑名单 `io_and_framework`）：查闭包，范围由 `deny_transitive` 决定（`normal_build` / `normal_build_dev` / `none`）。`transitive_exempt` 里的框架本身和它带进来的依赖都不算违规（例如豁免 tokio 的 crate 不因 tokio 带进的 mio 报错）。
- `--selftest` 用 jq 往真实 `cargo metadata` 快照里注入违规，证明每类规则都会红、合法图会绿，不改 `Cargo.toml`、不联网。
- 判定逻辑在 `scripts/check-deps.jq`，策略解析在 `scripts/deps-policy.jq`；三平台只依赖 bash 与 jq。

## 备选

- 每个 crate 里写注释约定：不可执行。
- cargo-deny：粒度不够，它只管许可、来源与漏洞（`deny.toml`）。
- 白名单也按闭包查：合法的宏依赖链与 dev 依赖会误报。已否决。
- 方案原写法里 daemon 的 `transitive_exempt` 列出整张 `io_and_framework`；实施时改成 `deny_transitive = "none"`，含义相同，免得同一张清单抄两份。

## 影响

- 加 crate 或放开某个外部依赖都要改策略文件，diff 可审；审查把对策略文件的放宽按 `important` 看。
