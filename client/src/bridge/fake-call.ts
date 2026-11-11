// 测试用的可控「invoke」：每次调用挂起，由测试决定回包何时到、是什么。只给测试 import。
import { create, toBinary } from "@bufbuild/protobuf";
import { fromBinary } from "@bufbuild/protobuf";
import { AttachReplySchema, FrameChunkSchema, NextFrameSchema, type NextFrame } from "../gen/iwork/v1/view_pb";
import type { Command, RawCall } from "./call";

export interface PendingCall {
  command: Command;
  body: Uint8Array;
  /** 调用方传进来的参数个数：只能是 2（command、body），不能带选项。 */
  argc: number;
  resolve(reply: unknown): void;
  reject(error: unknown): void;
}

export class FakeCall {
  /** 还没被测试取走的调用。 */
  readonly calls: PendingCall[] = [];
  /** 发生过的全部调用（含已取走的）。 */
  readonly history: PendingCall[] = [];

  readonly call: RawCall = (...args: Parameters<RawCall>) =>
    new Promise((resolve, reject) => {
      const [command, body] = args;
      const call = { command, body, argc: args.length, resolve, reject };
      this.calls.push(call);
      this.history.push(call);
    });

  /** 最近一次还没回包的某个命令。 */
  take(command: Command): PendingCall {
    const i = this.calls.findIndex((c) => c.command === command);
    if (i < 0) throw new Error(`没有挂着的 ${command}；现有 ${this.calls.map((c) => c.command).join(",")}`);
    return this.calls.splice(i, 1)[0];
  }

  pending(command: Command): number {
    return this.calls.filter((c) => c.command === command).length;
  }
}

export const attachReply = (epoch: number) => toBinary(AttachReplySchema, create(AttachReplySchema, { epoch })).slice().buffer;

export function chunkReply(c: { epoch: number; frame: bigint; index: number; last: boolean; data: Uint8Array }): ArrayBuffer {
  return toBinary(FrameChunkSchema, create(FrameChunkSchema, c)).slice().buffer;
}

export const nextFrameOf = (call: PendingCall): NextFrame => fromBinary(NextFrameSchema, call.body);

/** 等挂着的 promise 回调都跑完。 */
export const settle = () => new Promise((r) => setTimeout(r, 0));
