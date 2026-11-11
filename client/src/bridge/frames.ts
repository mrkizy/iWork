// 取帧循环（T09 方案 2.4 第 3、4、8 条）。
//
// 模块里只有一个「当前代次」。attach 时先置成「换代中」（旧循环从此失效），拿到新代次后让接收方清空、
// 起一个新循环。循环自己带着代次与组装缓存（循环的局部变量，循环结束就没了）。
// 每次 await 回来先核两件事再碰数据：当前代次还是不是自己这一代；块上的代次是不是自己这一代。任一不是，
// 这个回包整个丢掉、循环结束。核过了才按 (frame, index) 去重、收块，收齐 last 再解码交出去。
//
// 只认自定义协议这一条路：收到 transport_fallback、或成功的回包不是 ArrayBuffer，立刻停，不应用，
// 交给接收方显示固定诊断页，不重试、不接受 JSON 回退。
// 还是当前代次时出了说不通的事（回包解不开、块上代次不对、被别的取帧顶掉）也一样停下报故障，不悄悄结束循环：
// 悄悄结束的话界面停在最后一帧，用户看不出已经不更新了。
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import {
  AttachReplySchema,
  FrameChunkSchema,
  FramePositionSchema,
  NextFrameSchema,
  ViewPatchSchema,
  type ViewPatch,
} from "../gen/iwork/v1/view_pb";
import { ErrorCode, type RawCall } from "./call";

/** 传输层出了不该继续的问题：网页显示固定诊断页。 */
export type TransportFailure =
  | { kind: "fallback" }
  | { kind: "not_array_buffer" }
  | { kind: "closed" }
  | { kind: "error"; message: string };

export interface FrameSink {
  /** 新的一代开始：清空网页状态（下一帧是全量）。 */
  reset(): void;
  apply(patch: ViewPatch): void;
  fail(failure: TransportFailure): void;
}

type Generation = { state: "unattached" } | { state: "switching"; attempt: number } | { state: "live"; epoch: number } | { state: "stopped" };

export class FrameBridge {
  #gen: Generation = { state: "unattached" };
  #attempts = 0;

  constructor(
    private readonly call: RawCall,
    private readonly sink: FrameSink,
  ) {}

  /** 当前代次；没有在跑的循环时为 undefined。 */
  get epoch(): number | undefined {
    return this.#gen.state === "live" ? this.#gen.epoch : undefined;
  }

  /** （重新）挂上：换代次，旧循环失效，新循环从全量开始。 */
  async attach(): Promise<void> {
    if (this.#gen.state === "stopped") return;
    const attempt = ++this.#attempts;
    this.#gen = { state: "switching", attempt };
    let reply: unknown;
    try {
      reply = await this.call("attach", new Uint8Array());
    } catch (e) {
      if (this.#isCurrentAttempt(attempt)) this.#fail(failureOf(e));
      return;
    }
    // 等回包期间又有一次 attach、或已经停了：这次作废。
    if (!this.#isCurrentAttempt(attempt)) return;
    if (!(reply instanceof ArrayBuffer)) {
      this.#fail({ kind: "not_array_buffer" });
      return;
    }
    let epoch: number;
    try {
      ({ epoch } = fromBinary(AttachReplySchema, new Uint8Array(reply)));
    } catch (e) {
      this.#fail({ kind: "error", message: `attach 的回包解不开：${String(e)}` });
      return;
    }
    this.#gen = { state: "live", epoch };
    this.sink.reset();
    void this.#loop(epoch);
  }

  #isCurrentAttempt(attempt: number): boolean {
    return this.#gen.state === "switching" && this.#gen.attempt === attempt;
  }

  #isCurrent(epoch: number): boolean {
    return this.#gen.state === "live" && this.#gen.epoch === epoch;
  }

  #fail(failure: TransportFailure): void {
    this.#gen = { state: "stopped" };
    this.sink.fail(failure);
  }

  async #loop(epoch: number): Promise<void> {
    let applied: { frame: bigint; index: number } | undefined;
    const parts: Uint8Array[] = [];
    for (;;) {
      const request = create(NextFrameSchema, {
        epoch,
        applied: applied && create(FramePositionSchema, applied),
      });
      let reply: unknown;
      try {
        reply = await this.call("next_frame", toBinary(NextFrameSchema, request));
      } catch (e) {
        if (!this.#isCurrent(epoch)) return;
        const code = String(e);
        if (code === ErrorCode.staleEpoch) {
          // Rust 那边换了代、我们却没收到（例如另一处调了 attach）：这一代作废，重新挂上。
          void this.attach();
          return;
        }
        if (code === ErrorCode.superseded) {
          // 同一代只有这一个循环在取帧，被顶掉说明有别处也在取：不该发生，停下报出来
          this.#fail({ kind: "error", message: "取帧被同一代的另一次取帧顶掉" });
          return;
        }
        this.#fail(failureOf(e));
        return;
      }
      if (!this.#isCurrent(epoch)) return;
      if (!(reply instanceof ArrayBuffer)) {
        this.#fail({ kind: "not_array_buffer" });
        return;
      }
      let chunk;
      try {
        chunk = fromBinary(FrameChunkSchema, new Uint8Array(reply));
      } catch (e) {
        this.#fail({ kind: "error", message: `帧块解不开：${String(e)}` });
        return;
      }
      if (chunk.epoch !== epoch) {
        this.#fail({ kind: "error", message: `帧块的代次 ${chunk.epoch} 不是当前代次 ${epoch}` });
        return;
      }
      if (applied !== undefined && applied.frame === chunk.frame && applied.index === chunk.index) continue;
      if (chunk.index === 0) parts.length = 0;
      parts.push(chunk.data);
      applied = { frame: chunk.frame, index: chunk.index };
      if (chunk.last) {
        const bytes = concat(parts);
        parts.length = 0;
        let patch;
        try {
          patch = fromBinary(ViewPatchSchema, bytes);
        } catch (e) {
          this.#fail({ kind: "error", message: `第 ${chunk.frame} 帧解不开：${String(e)}` });
          return;
        }
        this.sink.apply(patch);
      }
    }
  }
}

export function failureOf(e: unknown): TransportFailure {
  const code = String(e);
  if (code === ErrorCode.transportFallback) return { kind: "fallback" };
  if (code === ErrorCode.closed) return { kind: "closed" };
  return { kind: "error", message: code };
}

function concat(parts: Uint8Array[]): Uint8Array {
  if (parts.length === 1) return parts[0];
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}
