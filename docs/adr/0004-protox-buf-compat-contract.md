# ADR-0004 Rust 用 protox、TS 用 buf 编译 proto；兼容靠只增不减的描述符契约与跨语言金样本

- 状态：已采纳（2026-11-09，T01）
- 依据：方案 `docs/plans/T01.md` 3.7、第 8 节 S1/S3、第 10 节 Q5；规则全文见 `proto/compat/README.md`

## 背景

- 三平台 CI 都装系统 `protoc` 会多一处环境差异。
- Rust 与 TS 两条生成链路要证明产出同一线格式。
- 「字段只增不改号」要有机器检查，而且要保护以后新加的字段，不只是 T01 当时的字段。

## 决定

- Rust 侧 `build.rs` 用 `protox` 编译 `.proto`，再用 `prost-build` 生成；TS 侧用 npm 的 `@bufbuild/buf` + `protoc-gen-es`。两者读同一份 `.proto`，生成代码都不入库。
- `proto/compat/v<N>/contract.binpb` 是主版本 N 的兼容契约，只增不减：测试双向比对（契约里的字段编号、类型、label、名字、oneof 归属一个都不能变，也不能删；当前描述符里每个字段都必须已登记）。日常只允许 `update-contract --additive` 追加。
- 金样本 `proto/compat/v<N>/samples/` 只增不改，Rust 与 TS 解同一批样本。
- 破坏性变更只能升主版本。包名、契约目录、握手里的 `protocol_major`、数据库 `meta.protocol_major` 共用这一个数，真值是包名。数据库主版本不符时先备份再拒绝启动；带着数据升主版本是一道门槛（为这两个版本写显式转换程序并做无损演练），不是通用机制。

## 备选

- 只用系统 protoc：多一个环境依赖。
- `buf breaking`：要把 buf 二进制带进 Rust CI，而且不覆盖 Rust 的生成结果。
- 用 schema 版本做门：技术设计 5.5 的迁移只向前，版本 +1 会被迁移吃掉，然后用新类型错读旧事件。
- 另设一个契约谱系号：和主版本成了两套身份，握手比对不到它。已否决。
- 通用导出导入：改名、改类型时走不通，还会漏掉事件信封与幂等记录。已否决。

## 影响

- `.proto` 只用 protox 与 buf 都支持的 proto3 子集（不用 proto2、extensions、`Any`）。若 protox 跟不上某个语法，退回 `prost-build` + 系统 `protoc`，不改 `.proto`。
- 每次加字段多跑一条 `cargo run -p iwork-proto --example update-contract -- --additive`。
- 破坏性变更的代价是改包名、重新生成，这是有意为之。
