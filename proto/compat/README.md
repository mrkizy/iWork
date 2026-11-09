# 协议兼容契约与金样本

本目录保证一件事：**已经进过契约的字段，之后改号、改类型、改名、删掉，CI 都会红；新加的字段一进仓库就自动受同样的保护。** 规则出自 T01 方案 3.7，审查时按本文判。

```
proto/compat/
  README.md                       # 本文件
  v1/contract.binpb               # 主版本 1 的兼容契约：承诺不变的全部字段，只增不减
  v1/samples/<Message>.<n>.binpb  # 二进制金样本，用当时的类型编码；只增不改
  v1/samples/<Message>.<n>.txtpb  # 同一样本的 text format，人读，也作期望值
  v1/samples/<Message>.<n>.json   # 同一样本的 proto3 canonical JSON，TS 侧比对用
```

`<Message>` 是 `iwork.v1` 包内的短消息名（如 `Approval.1.binpb`），TS 侧按 `iwork.v1.<短名>` 查描述符。

## 身份只有一个：协议主版本

契约目录 `v<N>`、proto 包名 `iwork.v<N>`、握手里的 `protocol_major`、数据库 `meta.protocol_major`、`EventCodec::protocol_major()` 都是同一个数。真值是 proto 包名；Rust 侧 `iwork_proto::PROTOCOL_MAJOR` 由构建脚本从包名解析，不手抄。不另设「谱系号」之类的第二套身份。

## 两种操作，分开

- **契约随新增单调扩充**（日常开发唯一允许的操作）：

  ```sh
  cargo run -p iwork-proto --example update-contract -- --additive
  ```

  它把当前描述符里契约还没有的消息、字段、枚举值追加进 `v<N>/contract.binpb`。契约里已有的任何条目，只要当前描述符与它不一致（编号、类型、label、名字、oneof 归属、枚举值），或者当前已经没有这个条目，工具就拒绝写入，逐条列出差异，退出码 1。

- **破坏性变更 = 升主版本**：包名改成 `iwork.v2`，新建 `proto/compat/v2/`，旧的 `v1/` 目录原样保留（旧程序的契约仍可查）。没有「在 v1 里重置」这个操作，`update-contract` 不提供 `--reset`；`v<N>/` 目录已存在而契约文件不见了，工具也拒绝从空重建。允许升主版本的条件：产品负责人书面同意，且按下文「旧库与旧客户端」把门槛走完。M1 开发期也一样。

字段写错了怎么办（同一主版本内）：永远不改号。新加一个字段，旧字段标 `deprecated = true` 留着，`iwork-app` 的映射兼容读两份；旧编号只在升主版本时进 `reserved`。

## CI 检查（`crates/iwork-proto/tests/compat.rs`）

1. **契约比对，双向**：
   - 契约 → 当前：契约里每个字段「同名、同编号、同类型、同 label、同 oneof 归属」，每个枚举值「同名、同值」；契约里有、当前没有的一律报错。label 里区分 proto3 `optional`（有存在性）与普通单值字段。
   - 当前 → 契约：当前描述符里每个消息、字段、枚举值都必须已在契约里。新加字段忘了跑 `update-contract --additive` 就红，错误信息给出要跑的命令。
   - `view.proto` 排除在外：`ViewPatch` 只在客户端进程内由 Rust 侧传给网页层，不落盘、不跨进程，改结构没有数据风险（方案 S8）。排除清单在 `iwork_proto::compat::EXCLUDED_FILES`，带理由。依赖进来的 `google/protobuf/*` 也不进契约。
2. **契约目录号**：`proto/compat/` 下最高的 `v<N>` 必须等于 `PROTOCOL_MAJOR`，且 `v<N>/contract.binpb` 存在。改了包名忘建新目录就红。
3. **样本解码**：每个 `.binpb` 先用生成的 Rust 类型解码再编码（生成类型认不出的字段会在这一步丢掉），再按描述符与 `.txtpb`、`.json` 逐值比对。样本数为 0 时测试失败，空集不算通过。
4. **样本只增不改**：登记表里每个构造函数的输出必须与磁盘上的 `.binpb` 逐字节相同；登记了没生成文件也红。
5. **每个持久事件都有样本**：`DomainEvent` 的每个 oneof 变体至少有一个样本。

比对函数本身的「两步测试」在 `crates/iwork-proto/src/compat.rs` 的单元测试里，用合成的描述符，不依赖本目录：C0 = `M{a=1}`；D1 = `M{a=1,b=2}` 扩充得 C1 且比对通过；D2 = `M{a=1,b=3}` 比对失败并点名 `M.b 编号 2 → 3`，扩充也拒绝；D3 = `M{a=1,b=2,c=3}` 报 c 不在契约里，扩充后通过。

TS 侧 `client/src/gen.test.ts` 对同一批 `.binpb` 用 protobuf-es 的 `fromBinary` 解码、`toJson` 输出，与 `.json` 比对，证明 Rust（protox + prost）与 TS（buf + protobuf-es）两条生成链路产出同一线格式。`.json` 用 proto3 canonical JSON 的默认约定：跳过默认值、lowerCamelCase 字段名、枚举写名字、64 位整数写字符串、`bytes` 写标准 base64、`Timestamp` 写 RFC 3339。

## 样本怎么加

```sh
cargo run -p iwork-proto --example dump-compat -- proto/compat/v1/samples
```

构造函数手写，在 `crates/iwork-proto/tests/support/builders.rs`，登记表在同目录 `mod.rs`（`sample_types!`）。每个关键消息至少一条，字段尽量填满，取值不用默认值（proto3 不编码默认值，样本就保护不到它）。

构造函数的结构体字面量写全字段、不用 `..Default::default()`：给已有样本的消息加字段后，测试代码先编译不过，指出哪些构造函数要看。旧构造函数里补默认值即可（默认值不编码，旧样本字节不变）。

样本只增不改：目标文件已存在且内容不同，`dump-compat` 拒绝写入（退出码 1）。加了新字段想保护它的取值，就写一个新的构造函数、追加到该类型列表的末尾，生成带新编号的样本；旧构造函数与旧样本都不动。

`.txtpb` 里的中文由 prost-reflect 写成八进制转义。prost-reflect 内部用 HashMap 存 map 字段，所以 `.txtpb`、`.json` 里 map 条目的顺序每次生成都可能不同：测试与 `dump-compat` 都按含义比（解析后比消息或 JSON 值），不逐字节比；TS 侧比 `.json` 也要用与键顺序无关的深比较。`.binpb` 逐字节比（生成代码的 map 是 BTreeMap，编码确定）。

## 旧库与旧客户端怎么办（升主版本的门）

迁移只向前，schema 版本 +1 拦不住旧库，所以门走协议主版本，三处用同一个数：

1. **数据库门**（T02、T04）：`meta` 表记 `protocol_major`。后台服务启动时库里的主版本 ≠ 程序的主版本 → 先 `VACUUM INTO backups/<时间>-v<旧>.db`（备份失败照样拒绝并说明）→ 拒绝启动，日志与客户端诊断页写明两个主版本号与备份位置。不迁移、不删除、不做任何写入。
2. **握手门**（T01 交付纯函数，T03 接进连接）：`Hello.protocol_major ≠ 服务端 PROTOCOL_MAJOR` → 回 `Response{request_id = 0, error{PROTOCOL_VERSION_MISMATCH, details{client_major, server_major}}}` 后断开；客户端显示「正在切换版本」。判断是 `iwork_proto::handshake::check_hello`，旧客户端 × 新后台、新客户端 × 旧后台两个方向都单测。
3. **契约门**：`proto/compat/v<N>/` 与包名一致，见上文 CI 检查第 2 条。

**数据怎么前进**：只承诺「旧库原样保留 + 回退旧程序即可用」，不承诺把旧数据带进新主版本。真要带数据升级，升级前必须为这两个具体版本写显式转换程序，覆盖事件信封（`seq`、`at`、`actor`、`command_id`）、幂等记录、outbox、制品引用计数，并做一次无损演练（旧库 → 转换 → 新库 → 重建投影 → 与旧程序的投影逐表比对），演练通过才允许改包名合入。没做这件事就只能回退。现在不写任何导出导入代码。
