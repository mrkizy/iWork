# ADR-0001 ID 用 ULID 的强类型 newtype

- 状态：已采纳（2026-11-09，T01）
- 依据：方案 `docs/plans/T01.md` 4.1、第 10 节 Q1

## 背景

ID 要跨进程、跨语言保持稳定；消息与事件要按创建顺序做键集分页（单个对话十万条历史）；实体种类多，把一种 ID 传进要另一种 ID 的地方很容易发生。

## 决定

- 全部实体 ID 用 ULID，每种实体一个 newtype（`ProjectId`、`ConversationId`……），由 `define_id!` 宏生成；文本形式为 26 字符 Crockford base32。
- `iwork-domain` 不读时钟、不读随机源：ID 由 `iwork-ports::IdGenerator` 生成，domain 只提供 `from_raw`。生成器必须单调（同一毫秒内递增；时间回拨时沿用上一次的毫秒数），这是端口的文档契约。
- 项目 ID 也是 ULID（产品负责人 2026-11-09 决定，Q1）。

## 备选

- UUID v4：无序，分页要另加 `created_at` 索引。
- UUID v7：有序，但文本 36 字符，Rust 生态里 ULID 更常用。
- 自增整数：跨设备或远程接入时冲突（D05 预留的场景）。

## 影响

- `.iwork/project.toml` 的 `id` 字段是 ULID 文本；SQLite 主键是 26 字符文本。
- ID 混用在编译期报错。
