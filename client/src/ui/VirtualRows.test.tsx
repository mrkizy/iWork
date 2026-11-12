// 虚拟行接锚点的滚动指令（方案 2.9）：同一个 token 只滚一次，滚到那一行居中并高亮，到时间去掉；
// 以及报可见行时分清是不是用户自己滚的（2.9「跳转」第 3 条）。
// jsdom 没有布局、也不真滚：这里给行和容器一个固定布局，并记下每一次对 scrollTop 的写入（审查 4/F-10：
// 只看高亮类名的话，删掉滚动那一行测试照样过）。
import { render } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { VirtualRows } from "./VirtualRows";

const ROW_GAP = 100; // 第 i 行的 offsetTop = i * 100
const ROW_HEIGHT = 40;
const VIEW_HEIGHT = 200;

const restores: (() => void)[] = [];
function stub(proto: object, name: string, get: (this: HTMLElement) => number) {
  const old = Object.getOwnPropertyDescriptor(proto, name);
  Object.defineProperty(proto, name, { configurable: true, get });
  restores.push(() => (old ? Object.defineProperty(proto, name, old) : delete (proto as Record<string, unknown>)[name]));
}

beforeEach(() => {
  const rowIndex = (el: HTMLElement) =>
    el.dataset.vrow === undefined ? -1 : [...el.parentElement!.querySelectorAll("[data-vrow]")].indexOf(el);
  stub(HTMLElement.prototype, "offsetTop", function () {
    const i = rowIndex(this);
    return i < 0 ? 0 : i * ROW_GAP;
  });
  stub(HTMLElement.prototype, "offsetHeight", function () {
    return this.dataset.vrow === undefined ? 0 : ROW_HEIGHT;
  });
  stub(HTMLElement.prototype, "clientHeight", function () {
    return this.classList.contains("iw-vrows") ? VIEW_HEIGHT : 0;
  });
  stub(HTMLElement.prototype, "scrollHeight", function () {
    return this.classList.contains("iw-vrows") ? 10 * ROW_GAP : 0;
  });
});

afterEach(() => {
  vi.useRealTimers();
  while (restores.length) restores.pop()!();
});

/** 记下容器 scrollTop 的每一次写入（程序滚动），读出最后一次的值。 */
function track(box: HTMLElement) {
  let top = 0;
  const writes: number[] = [];
  Object.defineProperty(box, "scrollTop", {
    configurable: true,
    get: () => top,
    set: (v: number) => {
      top = v;
      writes.push(v);
    },
  });
  return { writes, userScrollTo: (v: number) => (top = v) };
}

/** 第 i 行滚到居中时的 scrollTop（和 VirtualRows 里的算法独立写出）。 */
const centered = (i: number) => i * ROW_GAP - (VIEW_HEIGHT - ROW_HEIGHT) / 2;

function setup() {
  const [keys, setKeys] = createSignal(["a", "b"]);
  const [scroll, setScroll] = createSignal<{ rowKey: string; token: bigint; highlightMs: number }>();
  const visible: [bigint, bigint, boolean][] = [];
  const { container } = render(() => (
    <VirtualRows
      keys={keys()}
      ordinalOf={(k) => BigInt(k.charCodeAt(0))}
      firstIndex={0}
      total={keys().length}
      atTail={false}
      hasOlder={false}
      renderRow={(k) => <span>{k}</span>}
      onVisible={(first, last, byUser) => visible.push([first, last, byUser])}
      onReachOldest={() => {}}
      scrollTo={scroll()}
    />
  ));
  const box = container.querySelector(".iw-vrows") as HTMLElement;
  const row = (k: string) => container.querySelector(`[data-key="${k}"]`);
  return { keys, setKeys, setScroll, box, row, visible, scroll: track(box) };
}

describe("滚动指令", () => {
  it("行晚到：到了才滚到它居中；高亮 1 秒；同一个 token 不再滚第二次，新 token 照常", () => {
    vi.useFakeTimers();
    const t = setup();
    // 行还没到：不滚
    t.setScroll({ rowKey: "c", token: 1n, highlightMs: 1000 });
    expect(t.scroll.writes).toEqual([]);
    expect(t.row("c")).toBeNull();
    t.setKeys(["a", "b", "c"]);
    expect(t.box.scrollTop, "行到了，滚到第 3 行居中").toBe(centered(2));
    expect(t.row("c")?.classList.contains("iw-vrow--highlight")).toBe(true);
    vi.advanceTimersByTime(999);
    expect(t.row("c")?.classList.contains("iw-vrow--highlight")).toBe(true);
    vi.advanceTimersByTime(1);
    expect(t.row("c")?.classList.contains("iw-vrow--highlight"), "1 秒后去掉").toBe(false);

    // 用户自己滚回顶上（不经 scrollTop 的程序写入）
    t.scroll.userScrollTo(0);
    t.box.dispatchEvent(new Event("scroll"));
    const before = t.scroll.writes.length;
    // 同一个 token 再来一次（比如下一帧重发、行又变了）：不滚
    t.setScroll({ rowKey: "c", token: 1n, highlightMs: 1000 });
    t.setKeys(["a", "b", "c", "d"]);
    expect(t.box.scrollTop, "同一个 token 不把用户拉回去").toBe(0);
    expect(t.scroll.writes.slice(before)).not.toContain(centered(2));
    expect(t.row("c")?.classList.contains("iw-vrow--highlight")).toBe(false);

    // 新 token：照常滚
    t.setScroll({ rowKey: "d", token: 2n, highlightMs: 1000 });
    expect(t.box.scrollTop, "对照：新 token 滚到第 4 行居中").toBe(centered(3));
  });
});

describe("报可见行", () => {
  const flush = () => vi.advanceTimersByTime(20);

  it("用户滚轮之后的报告带 byUser；跳转带来的程序滚动不带", () => {
    vi.useFakeTimers();
    const t = setup();
    t.setKeys(["a", "b", "c", "d", "e"]);
    flush();
    t.visible.length = 0;
    // 程序滚动（跳转）
    t.setScroll({ rowKey: "e", token: 1n, highlightMs: 1000 });
    flush();
    expect(t.visible.length, "对照：跳转后报了新的可见行").toBeGreaterThan(0);
    expect(t.visible.every(([, , byUser]) => !byUser), "程序滚动不算用户").toBe(true);
    t.visible.length = 0;
    // 用户滚轮
    t.box.dispatchEvent(new WheelEvent("wheel", { deltaY: -300 }));
    t.scroll.userScrollTo(0);
    t.box.dispatchEvent(new Event("scroll"));
    flush();
    expect(t.visible.at(-1)?.[2], "用户滚动").toBe(true);
    // 用户输入只算一次：下一次程序滚动又回到不算
    t.visible.length = 0;
    t.setScroll({ rowKey: "c", token: 2n, highlightMs: 1000 });
    flush();
    expect(t.visible.length).toBeGreaterThan(0);
    expect(t.visible.every(([, , byUser]) => !byUser)).toBe(true);
  });
});
