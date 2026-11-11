# iwork-fake-agent：假运行时

按脚本扮演 ACP agent，经 stdin / stdout 说 ACP（JSON-RPC，一行一条），让回合、审批、取消、崩溃场景不依赖任何厂商就能自动测试（M1 技术设计第 10 节）。后台服务像启动真桥接一样启动它：

```sh
iwork-fake-agent --script <脚本.json> [--record <记录.jsonl>]
```

- `--script`：必填，脚本文件（JSON）。解析失败、有不认识的字段或步骤类型时以退出码 2 退出，错误写在 stderr。
- `--record`：可选，记录文件（追加写，每行一个 JSON），给测试核对对端发来了什么：
  - 启动时先写自己的进程号：`{"started_pid": <进程号>}`；
  - 收到的请求与通知：`{"method": "initialize" | "session/new" | "session/load" | "session/prompt" | "session/cancel", "params": {...}}`，`params` 是 ACP 原样参数；
  - 每次权限答复：`{"permission_response": {...}}`；
  - 客户端发来的每一行在读到时（派发之前）就记一条到达记录：`{"arrived": "<方法名>"}`，权限答复记成 `{"arrived": "permission_response <工具调用 id> <选项 id 或 cancelled>"}`。记录文件里到达记录的先后就是到达 stdin 的先后，测试用它比「答复在取消之前」；
  - 启动的子进程：`{"spawned_child": <进程号>}`。

## 脚本格式

```json
{
  "agent": {
    "name": "iwork-fake-agent",
    "version": "0.1.0",
    "cli": { "path": "/opt/fake/cli", "version": "1.2.3" },
    "load_session": true,
    "system_prompt": true,
    "image": false
  },
  "turns": [
    {
      "steps": [
        { "type": "text", "text": "我来看看目录。", "chunk_chars": 2, "rate_per_sec": 100 },
        { "type": "tool_call", "id": "t1", "name": "Bash", "input": { "command": "ls" } },
        {
          "type": "permission",
          "tool_call_id": "t1",
          "allowed": [
            { "type": "tool_result", "id": "t1", "ok": true, "output": "README.md" },
            { "type": "text", "text": "看完了。" }
          ],
          "denied": [
            { "type": "tool_result", "id": "t1", "ok": false, "output": "被拒绝" },
            { "type": "text", "text": "好的，不执行。" }
          ]
        }
      ],
      "stop_reason": "end_turn"
    },
    {
      "steps": [
        { "type": "repeat_text", "chunk": "x", "count": 1000, "rate_per_sec": 100 },
        { "type": "sleep", "ms": 500 },
        { "type": "crash", "exit_code": 3 }
      ]
    }
  ]
}
```

### `agent`（可省略，各字段都有默认值）

协议版本固定回 1：ACP SDK 的 v1 连接不允许回别的版本，所以「协议版本不一致」用 `iwork-agent-acp` 的单元测试覆盖，不经假运行时。

| 字段 | 默认 | 作用 |
| --- | --- | --- |
| `name` | `iwork-fake-agent` | `initialize` 响应的 `agentInfo.name`，运行时登记的适配器名 |
| `version` | 本 crate 版本 | `agentInfo.version`，适配器版本；配合名字可以模拟已知不兼容的组合 |
| `cli` | 无 | 底层 CLI 的路径与版本，放进响应的 `_meta.iwork.cli` |
| `load_session` | `true` | 是否声明支持 `session/load`（续接） |
| `system_prompt` | `true` | 是否声明支持会话系统提示 |
| `image` | `false` | 是否声明接收图片 |
| `permission_wait_limit_secs` | 无 | 原样放进 `initialize` 响应的 `_meta.iwork.permissionWaitLimitSecs`（运行时等一个权限请求最多几秒；可以填坏值测拒收） |
| `sandbox_platform` | 无 | 原样放进 `_meta.iwork.sandboxPlatform`（`"supported"` / `"unsupported"`） |
| `effort_levels` | 空 | 非空时放进 `_meta.iwork.effortLevels` |
| `models` | 空 | `[{id, label}]`，非空时放进 `_meta.iwork.models` |
| `session_sandbox` | 无 | 放进 `session/new`、`session/load` 响应的 `_meta.iwork.sandbox`（`"enforced"` / `"unavailable"`） |
| `resumed_session_id` | 无 | 放进 `session/load` 响应的 `_meta.iwork.cliSessionId`（续接后运行时换了会话 id） |

`_meta.iwork` 下的键名与取值是 T06 方案 2.5 的约定，假运行时照同一份字符串写、不依赖 `iwork-agent-acp`，两边对不对得上由 `tests/acp_port.rs` 核对。

### `turns`

第 n 次 `session/prompt` 执行第 n 个回合（从 0 数，跨会话累计）；回合用完后再 prompt 返回 JSON-RPC 错误（步骤出错时也一样，连接不断）。每个回合按顺序执行 `steps`，全部执行完以 `stop_reason` 结束（`end_turn` 默认、`max_tokens`、`refusal`）。

收到 `session/cancel` 后，回合在下一步开始前、或正在等待（`sleep`、限速输出的间隔）时结束，`stopReason` 为 `cancelled`。等权限答复时被取消，客户端会答「已取消」，同样按取消结束。

### 步骤（`type` 取值）

| `type` | 字段 | 行为 |
| --- | --- | --- |
| `text` | `text`；`chunk_chars`（每片字符数，不填整段一片）；`rate_per_sec`（每秒片数，不填不限速） | 以 `agent_message_chunk` 输出正文 |
| `repeat_text` | `chunk`、`count`、`rate_per_sec`、`stamp`（默认 false） | 同一片正文输出 `count` 次，用于输出速率与延迟基准（如 100 片段/秒）；`stamp` 为真时每片前加「发出时刻（Unix 纪元纳秒）`\|`」，接收方据此算延迟 |
| `stderr` | `text` | 往 stderr 写一行（后台服务只记行长度，不记内容） |
| `bad_line` | `kind`（`oversize` / `invalid_utf8`）、`bytes`（`oversize` 用） | 绕过 ACP 直接往 stdout 写一行坏数据（超长行或非 UTF-8），然后挂起，等后台服务结束进程 |
| `tool_call` | `id`、`name`、`title`（默认同 `name`）、`input`（任意 JSON，作为 `rawInput`） | 发 `tool_call`，状态 `in_progress` |
| `tool_result` | `id`、`ok`、`output` | 发 `tool_call_update`，状态 `completed` 或 `failed`，`output` 作为文字内容 |
| `permission` | `tool_call_id`、`cwd`（可选）、`allowed`（步骤数组）、`denied`（步骤数组） | 发 `session/request_permission`（带上同 id 的 `tool_call` 步骤给出的名字、标题与输入；给了 `cwd` 就放进请求的 `_meta.iwork.cwd`；选项：允许一次 `allow-once`、拒绝一次 `reject-once`），**一直等到客户端答复**；选了允许走 `allowed`，选了拒绝走 `denied`，答「已取消」按取消结束 |
| `parallel_permissions` | `tool_call_ids`（字符串数组） | 同时为这几次工具调用发 `session/request_permission`（选项同上），全部有了答复再往下走；每个答复记 `{"tool_call_id": …, "permission_response": {…}}` |
| `sleep` | `ms` | 等待；可被取消打断 |
| `crash` | `exit_code`（默认 101） | 等 200ms 让已发出的消息写完，然后退出进程，不回任何响应（模拟输出到一半时崩溃） |
| `hang` | 无 | 永不结束，也不理会取消，只能被结束进程（测试取消的 3 秒强制结束） |
| `spawn_child` | `program`、`args` | 启动子进程且不等它，进程号写进记录文件（测试结束进程树后无孤儿进程） |
| `report_model` | `model_id`、`effort`（可选）、`on_text`（可选） | 上报实际模型：一条空的 `session_info_update` 通知，`_meta.iwork.report` 是 base64 编码的 `RuntimeReport`（`agent.proto`）；给了 `on_text` 就改挂在一条带这段正文的 `agent_message_chunk` 上（不合约定的挂法，测两样都不丢） |
| `report_usage` | `input_tokens`、`output_tokens`、`cache_read_tokens`、`cache_write_tokens`（后两个默认 0）、`cost_usd`（可选） | 上报一个回合的用量，挂法同上 |
| `report_quota` | `windows`：`[{window, used_pct, resets_at_secs?}]` | 上报额度窗口，挂法同上；不带观测时间 |
| `report_raw` | `value`（任意 JSON） | 把 `value` 原样放进 `_meta.iwork.report`（造解不出来的上报） |
| `error` | `code`、`reason`（可选） | 以 JSON-RPC 错误结束这次 prompt，`error.data` 为 `{"iwork": {"code", "reason"}}`（运行时点名的错误码，T06 方案 2.6 第 7 条） |

`session/new` 总是返回会话 id `fake-session-1`；`session/load` 接受任何 id。两者的参数（含 `_meta.iwork.sessionSetup`）原样写进记录文件，由测试核对。
