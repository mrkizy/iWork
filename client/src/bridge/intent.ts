// 用户操作：编码 UiIntent，经唯一出口交给 Rust，解出受理结果。
// 网页不在本地改状态：点了按钮就等下一帧（T09 方案 2.5）。
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { IntentAckSchema, UiIntentSchema, type IntentAck, type UiIntent } from "../gen/iwork/v1/view_pb";
import type { RawCall } from "./call";
import { failureOf, type TransportFailure } from "./frames";

export type IntentKind = UiIntent["kind"];

export type SendIntent = (kind: IntentKind) => Promise<IntentAck>;

/** 发操作的函数。传输层失败时报给 onFailure（显示固定诊断页），并回一个带错误的受理结果。 */
export function intentSender(call: RawCall, onFailure: (f: TransportFailure) => void): SendIntent {
  return async (kind) => {
    const body = toBinary(UiIntentSchema, create(UiIntentSchema, { kind }));
    let reply: unknown;
    try {
      reply = await call("ui_intent", body);
    } catch (e) {
      const failure = failureOf(e);
      if (failure.kind !== "error") onFailure(failure);
      return create(IntentAckSchema, { error: { code: String(e), message: String(e), retryable: false } });
    }
    if (!(reply instanceof ArrayBuffer)) {
      onFailure({ kind: "not_array_buffer" });
      return create(IntentAckSchema, { error: { code: "not_array_buffer", message: "界面传输不可用", retryable: false } });
    }
    return fromBinary(IntentAckSchema, new Uint8Array(reply));
  };
}
