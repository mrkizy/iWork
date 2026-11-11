// 取帧循环（T09 方案第 7 节「网页 · 取帧循环」）：代次核对、去重、收齐再解码、回退即停。
import { create, toBinary } from "@bufbuild/protobuf";
import { beforeEach, describe, expect, it } from "vitest";
import { ViewPatchSchema, type ViewPatch } from "../gen/iwork/v1/view_pb";
import { attachReply, chunkReply, FakeCall, nextFrameOf, settle } from "./fake-call";
import { FrameBridge, type TransportFailure } from "./frames";

function patchBytes(frame: bigint, title: string): Uint8Array {
  return toBinary(
    ViewPatchSchema,
    create(ViewPatchSchema, {
      frame,
      changes: [{ change: { case: "connection", value: { view: { title } } } }],
    }),
  );
}

class Sink {
  applied: ViewPatch[] = [];
  resets = 0;
  failures: TransportFailure[] = [];
  reset() {
    this.resets++;
  }
  apply(p: ViewPatch) {
    this.applied.push(p);
  }
  fail(f: TransportFailure) {
    this.failures.push(f);
  }
  titles() {
    return this.applied.map((p) => (p.changes[0].change.case === "connection" ? p.changes[0].change.value.view?.title : "?"));
  }
}

let fake: FakeCall;
let sink: Sink;
let bridge: FrameBridge;

async function attachAs(epoch: number) {
  const p = bridge.attach();
  fake.take("attach").resolve(attachReply(epoch));
  await p;
  await settle();
}

beforeEach(() => {
  fake = new FakeCall();
  sink = new Sink();
  bridge = new FrameBridge(fake.call, sink);
});

describe("取帧循环", () => {
  it("收齐 last 才解码；按 (frame, index) 去重；applied 回报最后收下的块", async () => {
    await attachAs(1);
    expect(sink.resets).toBe(1);
    const bytes = patchBytes(1n, "第一帧");
    const half = Math.floor(bytes.length / 2);

    let call = fake.take("next_frame");
    expect(nextFrameOf(call).applied).toBeUndefined();
    call.resolve(chunkReply({ epoch: 1, frame: 1n, index: 0, last: false, data: bytes.slice(0, half) }));
    await settle();
    expect(sink.applied).toHaveLength(0);

    call = fake.take("next_frame");
    expect(nextFrameOf(call).applied).toMatchObject({ frame: 1n, index: 0 });
    // 回包丢了，Rust 原样再交一次同一块：网页丢掉重复
    call.resolve(chunkReply({ epoch: 1, frame: 1n, index: 0, last: false, data: bytes.slice(0, half) }));
    await settle();
    call = fake.take("next_frame");
    call.resolve(chunkReply({ epoch: 1, frame: 1n, index: 1, last: true, data: bytes.slice(half) }));
    await settle();
    expect(sink.titles()).toEqual(["第一帧"]);
    expect(nextFrameOf(fake.take("next_frame")).applied).toMatchObject({ frame: 1n, index: 1 });
  });

  it.each([
    ["transport_fallback", { kind: "fallback" }],
    ["closed", { kind: "closed" }],
  ] as const)("next_frame 回 %s：停掉循环、不应用、报故障", async (code, failure) => {
    await attachAs(1);
    fake.take("next_frame").reject(code);
    await settle();
    expect(sink.failures).toEqual([failure]);
    expect(fake.pending("next_frame")).toBe(0);
    expect(sink.applied).toHaveLength(0);
  });

  it("成功的回包不是 ArrayBuffer（JSON 回退）：停掉循环、不应用", async () => {
    await attachAs(1);
    const bytes = patchBytes(1n, "数组形式");
    fake.take("next_frame").resolve(Array.from(bytes));
    await settle();
    expect(sink.failures).toEqual([{ kind: "not_array_buffer" }]);
    expect(sink.applied).toHaveLength(0);
    expect(fake.pending("next_frame")).toBe(0);
  });

  it("attach 回 transport_fallback：不起循环，报故障", async () => {
    const p = bridge.attach();
    fake.take("attach").reject("transport_fallback");
    await p;
    expect(sink.failures).toEqual([{ kind: "fallback" }]);
    expect(fake.pending("next_frame")).toBe(0);
  });

  it("代次过期：这一轮结束，不应用", async () => {
    await attachAs(1);
    const p = bridge.attach();
    fake.take("next_frame").reject("stale_epoch");
    await settle();
    expect(sink.applied).toHaveLength(0);
    fake.take("attach").resolve(attachReply(2));
    await p;
    expect(bridge.epoch).toBe(2);
  });

  it("late_old_reply_cannot_overwrite_new_epoch：旧代整帧回包在新代全量之后才到，store 不变、旧循环结束", async () => {
    await attachAs(1);
    const oldCall = fake.take("next_frame");
    // 重新 attach 成第二代，第二代先应用全量
    await attachAs(2);
    fake.take("next_frame").resolve(chunkReply({ epoch: 2, frame: 1n, index: 0, last: true, data: patchBytes(1n, "第二代全量") }));
    await settle();
    expect(sink.titles()).toEqual(["第二代全量"]);
    // 第一代那次回包现在才到（一整帧、last 为真）
    oldCall.resolve(chunkReply({ epoch: 1, frame: 7n, index: 0, last: true, data: patchBytes(7n, "第一代旧帧") }));
    await settle();
    expect(sink.titles()).toEqual(["第二代全量"]);
    // 此后没有带第一代代次的 next_frame
    expect(fake.calls.filter((c) => c.command === "next_frame").map((c) => nextFrameOf(c).epoch)).toEqual([2]);
    expect(fake.history.filter((c) => c.command === "next_frame").map((c) => nextFrameOf(c).epoch)).toEqual([1, 2, 2]);
  });

  it("旧代回包在第二代收块收到一半时到：第二代的组装缓存不受影响", async () => {
    await attachAs(1);
    const oldCall = fake.take("next_frame");
    await attachAs(2);
    const bytes = patchBytes(1n, "第二代分两块");
    const half = Math.floor(bytes.length / 2);
    fake.take("next_frame").resolve(chunkReply({ epoch: 2, frame: 1n, index: 0, last: false, data: bytes.slice(0, half) }));
    await settle();
    oldCall.resolve(chunkReply({ epoch: 1, frame: 1n, index: 1, last: true, data: new Uint8Array([1, 2, 3]) }));
    await settle();
    fake.take("next_frame").resolve(chunkReply({ epoch: 2, frame: 1n, index: 1, last: true, data: bytes.slice(half) }));
    await settle();
    expect(sink.titles()).toEqual(["第二代分两块"]);
    expect(sink.failures).toEqual([]);
  });

  it("块上的代次与循环不符：丢掉并结束这个循环", async () => {
    await attachAs(3);
    fake.take("next_frame").resolve(chunkReply({ epoch: 9, frame: 1n, index: 0, last: true, data: patchBytes(1n, "别人的") }));
    await settle();
    expect(sink.applied).toHaveLength(0);
    expect(fake.pending("next_frame")).toBe(0);
  });

  it("全部调用只给两个参数（命令与载荷），不带选项", async () => {
    await attachAs(1);
    fake.take("next_frame").resolve(chunkReply({ epoch: 1, frame: 1n, index: 0, last: true, data: patchBytes(1n, "x") }));
    await settle();
    expect(fake.history.map((c) => c.command)).toEqual(["attach", "next_frame", "next_frame"]);
    expect(fake.history.every((c) => c.argc === 2)).toBe(true);
  });
});
