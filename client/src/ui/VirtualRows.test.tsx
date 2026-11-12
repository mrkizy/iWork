// 虚拟行接锚点的滚动指令（方案 2.9）：同一个 token 只滚一次，滚到那一行并高亮，到时间去掉。
import { render } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { afterEach, describe, expect, it, vi } from "vitest";
import { VirtualRows } from "./VirtualRows";

afterEach(() => vi.useRealTimers());

describe("滚动指令", () => {
  it("同一个 token 只滚一次、高亮 1 秒后去掉；行晚到时到了再滚", () => {
    vi.useFakeTimers();
    const [keys, setKeys] = createSignal(["a", "b"]);
    const [scroll, setScroll] = createSignal<{ rowKey: string; token: bigint; highlightMs: number }>();
    const { container } = render(() => (
      <VirtualRows
        keys={keys()}
        ordinalOf={(k) => BigInt(k.charCodeAt(0))}
        firstIndex={0}
        total={keys().length}
        atTail
        hasOlder={false}
        renderRow={(k) => <span>{k}</span>}
        onVisible={() => {}}
        onReachOldest={() => {}}
        scrollTo={scroll()}
      />
    ));
    const row = (k: string) => container.querySelector(`[data-key="${k}"]`);
    // 行还没到：先不滚
    setScroll({ rowKey: "c", token: 1n, highlightMs: 1000 });
    expect(container.querySelector(".iw-vrow--highlight")).toBeNull();
    setKeys(["a", "b", "c"]);
    expect(row("c")?.classList.contains("iw-vrow--highlight"), "行到了就滚、高亮").toBe(true);
    vi.advanceTimersByTime(999);
    expect(row("c")?.classList.contains("iw-vrow--highlight")).toBe(true);
    vi.advanceTimersByTime(1);
    expect(row("c")?.classList.contains("iw-vrow--highlight"), "1 秒后去掉").toBe(false);
    // 同一个 token 再来一次（比如下一帧重发）：不再滚、不再高亮
    setScroll({ rowKey: "c", token: 1n, highlightMs: 1000 });
    setKeys(["a", "b", "c", "d"]);
    expect(container.querySelector(".iw-vrow--highlight")).toBeNull();
    setScroll({ rowKey: "a", token: 2n, highlightMs: 1000 });
    expect(row("a")?.classList.contains("iw-vrow--highlight"), "对照：新 token 照常").toBe(true);
  });
});
