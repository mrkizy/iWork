// 网页 → Rust 的唯一出口（T09 方案 2.4 第 7 条）：attach、next_frame、ui_intent 三个命令全经这里发。
//
// 不接受调用方传选项，尤其是自定义请求头：Rust 侧靠请求头里有没有 Tauri 自己加的 `Tauri-Callback` 认出
// 走的是自定义协议还是回退到了 postMessage，网页代码不许伪造或干扰这个判断。
// `client/src` 里除了这里，别处不许直接调 Tauri 的 invoke（tests/bridge-call.test.ts 检查）。
import { invoke } from "@tauri-apps/api/core";

export type Command = "attach" | "next_frame" | "ui_intent";

/** 发一个命令，载荷与回包都是原始字节。回包在自定义协议路径上是 ArrayBuffer；失败时 reject 一段错误码文字。 */
export type RawCall = (command: Command, body: Uint8Array) => Promise<unknown>;

export const tauriCall: RawCall = (command, body) => invoke(command, body);

/** Rust 侧命令返回的错误码（client/src-tauri/src/bridge.rs）。 */
export const ErrorCode = {
  /** invoke 回退到了 postMessage：不交数据。网页停掉取帧，显示固定诊断页。 */
  transportFallback: "transport_fallback",
  /** 代次已过期：有新的 attach。 */
  staleEpoch: "stale_epoch",
  /** 同一代里有更新的 next_frame 顶掉了这一次。 */
  superseded: "superseded",
  /** 客户端会话已结束。 */
  closed: "closed",
} as const;
