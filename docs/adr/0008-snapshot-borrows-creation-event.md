# ADR-0008 投影表的实体快照借用该实体「创建事件」的编码，装不下的状态另立列

- 状态：已采纳（2026-11-10，T02）
- 依据：方案 `docs/plans/T02.md` 2.6、第 12 节；ADR-0003

## 背景

投影表要存完整实体（含 `Action`、`RuleMatcher`、`Proposal` 等嵌套类型）。store 不依赖 proto，domain 没有 serde，不想再发明一套实体序列化。但创建事件的 proto 消息不一定装得下实体后来的全部状态：proto `Approval` 没有决定人与作废原因，`approval_to_proto` 对这两种终态报错（防丢信息）。

## 决定

- 每张实体表一列 `snapshot BLOB`：把实体包进它的创建事件（如 `ApprovalRequested{approval}`）后经 `EventCodec::encode` 得到的载荷；更新事件读快照 → 改字段 → 重编码；查询条件用的字段另抽成索引列。登记表在 `crates/iwork-store/src/snapshot.rs`。
- 创建事件装不下的后续状态另立列：审批主状态在 `approvals.state / decider / void_reason` 三列，快照里固定写 `Pending` 基底，读出时叠加；codec 的防丢信息校验原样保留。其余实体逐个核对过（方案 2.6 核对表），快照即真值。
- 投影表每张都带 `updated_seq`：最后一次改变这一行任何字段的事件 seq（供 T03 按实体版本应用事件，本卡不暴露到读模型）。

## 备选

- 每字段一列、手写映射：与 proto 两套格式，嵌套类型难拆。
- domain 加 serde：改依赖白名单，又多一套格式。
- 去掉 codec 校验，让终态也编进 `Approval`：静默丢掉决定人与作废原因。

## 影响

- 加实体字段只改 proto + domain + app 转换，store 自动跟上；新出现「创建事件装不下」的状态时要更新核对表并加列。
- 快照与事件共用同一个协议主版本门；读模型每行一次 proto 解码（10 万条消息的对话取一页在调试构建下也远低于 50ms）。
