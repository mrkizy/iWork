# ADR-0003 领域事件在 proto 里定义一次，存储与推送共用

- 状态：已采纳（2026-11-09，T01）
- 依据：方案 `docs/plans/T01.md` 3.6、5.2；技术设计 3.4、5.2

## 背景

事件日志的载荷和推给客户端的事件如果分开定义，回放重建出来的视图会和实时视图分叉。`iwork-domain` 不能依赖 proto，`iwork-store` 按技术设计第 2 节也只依赖 ports。

## 决定

- `proto/iwork/v1/events.proto` 的 `DomainEvent` 既是 `events.payload` 的编码，也是 `Event` 帧的载荷。
- `iwork-domain` 有对应的 Rust 枚举 `DomainEvent`，两者靠穷举对拍测试绑定：`iwork_app::convert` 的 `event_to_proto` / `event_from_proto` 往返相等，`DomainEvent::KINDS` 与 proto oneof 字段名逐一对应。转换写成普通函数（两边都是 `iwork-app` 的外部类型，孤儿规则不允许 `From`）。
- 存储层通过 `EventCodec` 端口编解码（实现是 `iwork-app` 的 `ProtoEventCodec`，由 daemon 注入）；投影由 `Projector` 端口完成，在线提交与历史重建走同一个实现。

## 备选

- 存储用自定义序列化：多一份格式，要再维护一份兼容规则。
- domain 直接依赖 proto：违反依赖方向。
- store 直接依赖 proto：违反技术设计第 2 节的依赖表，且 proto 一改 store 就重编。

## 影响

- 加一个事件要同时改 proto、domain、app 转换函数三处；漏了任何一处，对拍测试会红。
- store 对 codec 泛型。
