// 锚点导轨，宽形态（ui-m1.md 2a 第 11 条，列表式）：数据来自 anchors-* 录像里 Rust 给的 AnchorsView，文字逐字来自 patch。
// jsdom 不排版：列表的可视高度与内容高度在用到的用例里手工给（clientHeight / scrollHeight）。
import { create } from "@bufbuild/protobuf";
import { fireEvent, render, screen } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AnchorKind, AnchorsViewSchema, EdgeLandingSchema, type AnchorsView } from "../gen/iwork/v1/view_pb";
import { loadScenario, noFixtures } from "../test-fixtures";
import { AnchorRail, CLOSE_DELAY_MS } from "./AnchorRail";
import { BROWSE_EDGE, LIST_PAD, LOADING_DOT_DELAY_MS, PITCH } from "./list";

function fromScenario(name: string) {
  const c = loadScenario(name).state.conversation;
  if (c?.anchors === undefined) throw new Error(`录像 ${name} 里没有锚点`);
  return { anchors: c.anchors, current: c.anchorKey };
}

function mount(anchors: AnchorsView, current?: string) {
  const jumps: string[] = [];
  const browses: string[] = [];
  const edges: boolean[] = [];
  let inputs = 0;
  const [view, setView] = createSignal(anchors);
  render(() => (
    <div class="iw-timeline">
      <div class="iw-vrows" data-testid="timeline" />
      <AnchorRail
        anchors={view()}
        currentKey={current}
        onJump={(k) => jumps.push(k)}
        onBrowse={(k) => browses.push(k)}
        onBrowseEdge={(latest) => edges.push(latest)}
        onListInput={() => inputs++}
      />
    </div>
  ));
  const rail = screen.getByRole("navigation", { name: "消息锚点" });
  const list = () => rail.querySelector<HTMLDivElement>(".iw-anchor__list");
  const ticks = () => [...rail.querySelectorAll<HTMLButtonElement>(".iw-tick")];
  const previewed = () => rail.querySelector<HTMLButtonElement>(".iw-tick[aria-current]");
  const card = () => rail.querySelector<HTMLButtonElement>(".iw-anchor__card");
  return {
    rail,
    list,
    ticks,
    previewed,
    card,
    jumps,
    browses,
    edges,
    setView,
    get inputs() {
      return inputs;
    },
  };
}

/** 给列表一个可视高度与内容高度（jsdom 不排版）。 */
function sizeList(el: HTMLElement, client: number, items: number) {
  Object.defineProperty(el, "clientHeight", { configurable: true, value: client });
  Object.defineProperty(el, "scrollHeight", { configurable: true, value: LIST_PAD * 2 + items * PITCH });
}

const label = (a: { kindLabel: string; timeLabel: string; preview: string }) => `${a.kindLabel} ${a.timeLabel} ${a.preview}`;
const settle = () => Promise.resolve();

afterEach(() => vi.useRealTimers());

describe.skipIf(noFixtures)("锚点导轨（宽形态，列表式）", () => {
  it("静止态：三条短线；有等你处理出芥末黄点、有失败出赭红点；都没有就不出", () => {
    const basic = fromScenario("anchors-basic");
    const a = mount(basic.anchors, basic.current);
    expect(a.rail.querySelector(".iw-anchor__dot--waiting")).not.toBeNull();
    expect(a.rail.querySelector(".iw-anchor__dot--failed")).not.toBeNull();
    a.rail.parentElement!.remove();
    const plain = fromScenario("anchors-unloaded");
    const b = mount(plain.anchors, plain.current);
    expect(b.rail.querySelector(".iw-anchor__dot")).toBeNull();
    expect(b.rail.querySelectorAll(".iw-anchor__rest span")).toHaveLength(3);
  });

  it("展开：每个锚点一条刻度、按时间排；列表高度 = 条数 × 11 + 上下各 8；预览停在当前位置，那条也标成当前阅读位置", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    expect(a.ticks().map((t) => t.getAttribute("aria-label"))).toEqual(anchors.items.map(label));
    expect(a.list()!.style.height).toBe(`${anchors.items.length * PITCH + LIST_PAD * 2}px`);
    const item = anchors.items.find((i) => i.key === current)!;
    expect(a.previewed()?.getAttribute("aria-label")).toBe(label(item));
    expect(a.previewed()).toHaveClass("iw-tick--current");
    expect(a.ticks().filter((t) => t.classList.contains("iw-tick--current")), "当前阅读位置只有一条").toHaveLength(1);
    for (const t of [item.kindLabel, item.timeLabel, item.preview]) expect(a.card()).toHaveTextContent(t);
  });

  it("悬停哪条，信息卡跟到哪条，它与上下相邻按距离分级；移到信息卡上预览不变，对话不动", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    const i = 5;
    fireEvent.mouseEnter(a.ticks()[i]);
    expect(a.card()).toHaveTextContent(anchors.items[i].preview);
    expect(a.ticks()[i]).toHaveClass("iw-tick--hover");
    expect(a.ticks()[i - 1]).toHaveClass("iw-tick--near1");
    expect(a.ticks()[i + 2]).toHaveClass("iw-tick--near2");
    expect(a.ticks()[i + 3].className).not.toMatch(/hover|near/);
    fireEvent.mouseEnter(a.card()!);
    expect(a.card()).toHaveTextContent(anchors.items[i].preview);
    expect(a.jumps).toEqual([]);
  });

  it("点刻度或信息卡：跳到那一处并收起", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    fireEvent.click(a.card()!);
    expect(a.jumps).toEqual([current]);
    expect(a.rail.classList.contains("iw-anchor--open"), "收起").toBe(false);
    fireEvent.mouseEnter(a.rail);
    fireEvent.click(a.ticks()[2]);
    expect(a.jumps[1]).toBe(anchors.items[2].key);
  });

  it("Esc 立即收起；移开 300ms 才收起（299ms 时还在）；都不动对话", () => {
    vi.useFakeTimers();
    const { anchors, current } = fromScenario("anchors-basic");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(a.rail.classList.contains("iw-anchor--open")).toBe(false);
    fireEvent.mouseEnter(a.rail);
    fireEvent.mouseLeave(a.rail);
    vi.advanceTimersByTime(CLOSE_DELAY_MS - 1);
    expect(a.rail.classList.contains("iw-anchor--open")).toBe(true);
    vi.advanceTimersByTime(1);
    expect(a.rail.classList.contains("iw-anchor--open")).toBe(false);
    expect(a.jumps).toEqual([]);
  });

  it("整个导轨一个 Tab 停点：导轨本身可聚焦，刻度与信息卡不进 Tab 顺序（列表里用方向键）", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    expect(a.rail.tabIndex).toBe(0);
    fireEvent.mouseEnter(a.rail);
    expect(a.ticks().every((t) => t.tabIndex === -1)).toBe(true);
    expect(a.card()!.tabIndex).toBe(-1);
  });

  it("键盘：↑ / ↓ 换一处预览，Enter 跳过去；⌥↑ / ⌥↓ 不在这里处理", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    const at = anchors.items.findIndex((i) => i.key === current);
    fireEvent.keyDown(document, { key: "ArrowUp" });
    expect(a.previewed()?.getAttribute("aria-label")).toBe(label(anchors.items[at - 1]));
    fireEvent.keyDown(document, { key: "ArrowUp", altKey: true });
    expect(a.previewed()?.getAttribute("aria-label"), "⌥↑ 不换预览").toBe(label(anchors.items[at - 1]));
    fireEvent.keyDown(document, { key: "Enter" });
    expect(a.jumps).toEqual([anchors.items[at - 1].key]);
  });

  it("Home / End：那一头不在已载入的窗口里就请 Rust 取；Rust 给落点（token）才移过去，同一个 token 只用一次；到头时直接移", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    expect(anchors.hasBefore && !anchors.hasAfter, "对照：前面还有、后面没有").toBe(true);
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    fireEvent.keyDown(document, { key: "Home" });
    expect(a.edges).toEqual([false]);
    const earliest = anchors.items.slice(0, 5).map((i, n) => ({ ...i, key: `first-${n}` }));
    const first = create(AnchorsViewSchema, { ...anchors, items: [...earliest, ...anchors.items.slice(0, 20)], hasBefore: false, hasAfter: true });
    a.setView(first);
    // 造出来的「更早的几处」是复制原条目改 key，文字会重复：按元素比，不按文字
    expect(a.previewed(), "窗口到了但没有落点：不落到最早一处").not.toBe(a.ticks()[0]);
    a.setView(create(AnchorsViewSchema, { ...first, edgeLanding: create(EdgeLandingSchema, { token: 41n, latest: false }) }));
    expect(a.previewed()).toBe(a.ticks()[0]);
    fireEvent.mouseEnter(a.ticks()[3]);
    a.setView(create(AnchorsViewSchema, { ...first, edgeLanding: create(EdgeLandingSchema, { token: 41n, latest: false }) }));
    expect(a.previewed()?.getAttribute("aria-label"), "同一个 token 不再用").toBe(label(first.items[3]));
    // 已经到了最新一头：End 不请 Rust，直接移到最后一条
    a.setView(create(AnchorsViewSchema, { ...anchors }));
    fireEvent.keyDown(document, { key: "End" });
    expect(a.edges).toEqual([false]);
    expect(a.previewed()?.getAttribute("aria-label")).toBe(label(anchors.items.at(-1)!));
  });

  it("列表里的用户输入一律上报，由 Rust 判断有没有意图可作废，不看 edgePending（g3b 3/F-1）", async () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    expect(anchors.hasBefore && !anchors.hasAfter, "对照：最新一头已在本窗").toBe(true);
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    await settle();
    const list = a.list()!;
    sizeList(list, 10 * PITCH, anchors.items.length);
    // Home 在等（edgePending 还没回来），按 End：最新一头在本窗，直接落地，也要报
    fireEvent.keyDown(document, { key: "Home" });
    fireEvent.keyDown(document, { key: "End" });
    expect(a.inputs, "End 在本窗直接落地").toBe(1);
    // edgePending 帧还没到就滚轮
    fireEvent.wheel(list, { deltaY: 40 });
    expect(a.inputs, "滚轮").toBe(2);
    // 指针按下（点刻度、中键等）：按下那一刻就报（g3b 4/F-2：不再从滚动坐标推断）
    fireEvent.pointerDown(list);
    expect(a.inputs, "指针按下").toBe(3);
    fireEvent.pointerUp(window);
  });

  // ---------- 第 5 轮：用户输入在事件发生那一刻上报；程序写位置先让位（g3b 4/F-1、4/F-2） ----------

  /** anchors-unloaded：前面还有（Home 要请 Rust）。打开列表、给定几何。 */
  async function openUnloaded() {
    const { anchors, current } = fromScenario("anchors-unloaded");
    expect(anchors.hasBefore, "对照：Home 要请 Rust").toBe(true);
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    await settle();
    const list = a.list()!;
    sizeList(list, 10 * PITCH, anchors.items.length);
    return { a, list, anchors };
  }
  const landing = (anchors: AnchorsView, token: bigint) =>
    create(AnchorsViewSchema, { ...anchors, items: [...anchors.items], edgeLanding: create(EdgeLandingSchema, { token, latest: false }) });

  it("A1：滚轮、Home、再滚轮：每一下都报（不节流）；Home 之后有输入，这次 Home 的落点不执行", async () => {
    const { a, anchors } = await openUnloaded();
    const list = a.list()!;
    fireEvent.wheel(list, { deltaY: 40 });
    fireEvent.keyDown(document, { key: "Home" });
    fireEvent.wheel(list, { deltaY: 40 });
    expect(a.inputs, "两下滚轮都报").toBe(2);
    const before = a.previewed()?.getAttribute("aria-label");
    a.setView(landing(anchors, 51n));
    expect(a.previewed()?.getAttribute("aria-label"), "Home 之后用户动过：不落到最早一处").toBe(before);
  });

  it("A2：同 A1，换成触摸", async () => {
    const { a, anchors } = await openUnloaded();
    const list = a.list()!;
    fireEvent.touchStart(list);
    fireEvent.touchMove(list);
    fireEvent.touchEnd(list);
    fireEvent.keyDown(document, { key: "Home" });
    fireEvent.touchStart(list);
    fireEvent.touchMove(list);
    fireEvent.touchEnd(list);
    expect(a.inputs, "Home 之后的触摸也报到").toBeGreaterThanOrEqual(3);
    const before = a.previewed()?.getAttribute("aria-label");
    a.setView(landing(anchors, 52n));
    expect(a.previewed()?.getAttribute("aria-label")).toBe(before);
  });

  it("A3（对照）：滚轮 → Home → 之后没有输入 → 落点照常执行（新的 Edge 请求把「有过输入」清零）", async () => {
    const { a, anchors } = await openUnloaded();
    fireEvent.wheel(a.list()!, { deltaY: 40 });
    fireEvent.keyDown(document, { key: "Home" });
    a.setView(landing(anchors, 53n));
    expect(a.previewed()?.getAttribute("aria-label")).toBe(label(anchors.items[0]));
  });

  it("B1：Home → 指针按下 → 位置被拖到 261 → 内容不变的新窗口 → scroll：停在 261，按下那一刻就报了", async () => {
    const { a, list, anchors } = await openUnloaded();
    fireEvent.keyDown(document, { key: "Home" });
    fireEvent.pointerDown(list);
    list.scrollTop = 261;
    a.setView(create(AnchorsViewSchema, { ...anchors, items: [...anchors.items], edgePending: true }));
    fireEvent.scroll(list);
    expect(list.scrollTop).toBe(261);
    expect(a.inputs).toBeGreaterThanOrEqual(1);
    fireEvent.pointerUp(window);
  });

  it("B2：Home → 指针按下 → 位置被拖到 261 → 落点先于 scroll 到：不落地，停在 261，报过输入", async () => {
    const { a, list, anchors } = await openUnloaded();
    fireEvent.keyDown(document, { key: "Home" });
    fireEvent.pointerDown(list);
    list.scrollTop = 261;
    a.setView(landing(anchors, 54n));
    fireEvent.scroll(list);
    expect(list.scrollTop).toBe(261);
    expect(a.inputs).toBeGreaterThanOrEqual(1);
    fireEvent.pointerUp(window);
  });

  it("拖动中来了内容变化的窗口替换：不写位置（用户在控制）；松手以后不补写", async () => {
    const { a, list, anchors } = await openUnloaded();
    fireEvent.pointerDown(list);
    list.scrollTop = 300;
    const extra = anchors.items.slice(0, 10).map((i, n) => ({ ...i, key: `drag-${n}` }));
    a.setView(create(AnchorsViewSchema, { ...anchors, items: [...extra, ...anchors.items] }));
    expect(list.scrollTop, "拖动中不写").toBe(300);
    fireEvent.pointerUp(window);
    expect(list.scrollTop).toBe(300);
  });

  it("点一条刻度去跳：先报输入（只作废头尾），再发跳转；跳转不被这次作废吃掉", async () => {
    const { a } = await openUnloaded();
    const order: string[] = [];
    const tick = a.ticks()[3];
    const was = a.inputs;
    fireEvent.pointerDown(tick);
    order.push(a.inputs > was ? "input" : "-");
    fireEvent.click(tick);
    order.push(a.jumps.length === 1 ? "jump" : "-");
    expect(order).toEqual(["input", "jump"]);
  });

  // ---------- 第 6 轮（g3b 5/F-1、5/F-2） ----------

  /** 模拟浏览器的上下界：内容高随刻度条数变，scrollTop 读写都夹在 [0, 内容高 - 视口高]（内容变短后浏览器自己夹紧）。 */
  function browserBounds(list: HTMLElement, client: number) {
    let stored = 0;
    const max = () => Math.max(0, LIST_PAD * 2 + list.querySelectorAll(".iw-tick").length * PITCH - client);
    Object.defineProperty(list, "clientHeight", { configurable: true, value: client });
    Object.defineProperty(list, "scrollHeight", {
      configurable: true,
      get: () => LIST_PAD * 2 + list.querySelectorAll(".iw-tick").length * PITCH,
    });
    Object.defineProperty(list, "scrollTop", {
      configurable: true,
      get: () => Math.min(stored, max()),
      set: (v: number) => (stored = Math.min(Math.max(0, v), max())),
    });
  }
  const userKeys = (from: number, to: number) =>
    Array.from({ length: to - from }, (_, i) => ({ key: `k${from + i}`, kind: AnchorKind.USER, kindLabel: "你", timeLabel: "10:00", preview: `第 ${from + i} 处` }));

  it("5/F-1：120 条换成 60 条，浏览器先夹紧了 scrollTop：顶上那条按替换前取，替换后对回（340，不是夹紧后的 566）", async () => {
    const before = create(AnchorsViewSchema, { items: userKeys(0, 120), hasBefore: true, hasAfter: true });
    const a = mount(before, "k90");
    fireEvent.mouseEnter(a.rail);
    await settle();
    const list = a.list()!;
    browserBounds(list, 110);
    list.scrollTop = 1000;
    fireEvent.scroll(list);
    // 第 90 处在顶上、往下偏 2px；新窗口是 60..119
    a.setView(create(AnchorsViewSchema, { items: userKeys(60, 120), hasBefore: true, hasAfter: false }));
    expect(list.scrollTop).toBe(LIST_PAD + (90 - 60) * PITCH + 2);
  });

  it("6/F-1：窗口变短、浏览器自己夹紧 scrollTop 引起的 scroll 也是程序侧的，不报；等着的 Home 照常落地", async () => {
    // 120 条、在 1000：先按 End 再按 Home；旧 End 的最新窗口（60 条、不含原来顶上那条）先到，没有落点
    const before = create(AnchorsViewSchema, { items: userKeys(0, 120), hasBefore: true, hasAfter: true });
    const a = mount(before, "k90");
    fireEvent.mouseEnter(a.rail);
    await settle();
    const list = a.list()!;
    browserBounds(list, 110);
    list.scrollTop = 1000;
    await new Promise((r) => requestAnimationFrame(() => r(undefined)));
    fireEvent.keyDown(document, { key: "End" });
    fireEvent.keyDown(document, { key: "Home" });
    expect(a.edges, "对照：两头都请了").toEqual([true, false]);
    const was = a.inputs;
    a.setView(create(AnchorsViewSchema, { items: userKeys(200, 260), hasBefore: true, hasAfter: false }));
    expect(list.scrollTop, "对照：浏览器夹紧了").toBe(LIST_PAD * 2 + 60 * PITCH - 110);
    fireEvent.scroll(list);
    expect(a.inputs, "夹紧引起的 scroll 不报").toBe(was);
    // Home 的窗口回来、带落点：落到第一条
    a.setView(
      create(AnchorsViewSchema, {
        items: userKeys(0, 60),
        hasBefore: false,
        hasAfter: true,
        edgeLanding: create(EdgeLandingSchema, { token: 71n, latest: false }),
      }),
    );
    expect(a.previewed(), "Home 落到第一条").toBe(a.ticks()[0]);
  });

  it("5/F-2：焦点在列表里时会让列表滚动的键都报：PageUp、PageDown、Space、Shift+Space、↑↓、本窗内的 Home / End", async () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(create(AnchorsViewSchema, { ...anchors, hasBefore: false }), current);
    fireEvent.mouseEnter(a.rail);
    await settle();
    for (const [key, shiftKey] of [["PageDown", false], ["PageUp", false], [" ", false], [" ", true]] as const) {
      const was = a.inputs;
      fireEvent.keyDown(document, { key, shiftKey });
      expect(a.inputs, `${shiftKey ? "Shift+" : ""}${key === " " ? "Space" : key}`).toBe(was + 1);
    }
  });

  it("5/F-2：只产生 scroll 的用户滚动（读屏手势、中键自动滚动）也报；程序自己写的那一次不报", async () => {
    const { a, list } = await openUnloaded();
    await new Promise((r) => requestAnimationFrame(() => r(undefined)));
    const was = a.inputs;
    // 没有任何输入事件，只有 scroll
    list.scrollTop = list.scrollTop + 3 * PITCH;
    fireEvent.scroll(list);
    expect(a.inputs, "只有 scroll 的用户滚动").toBe(was + 1);
    // 程序写：↓ 把预览滚进视野（按键本身报一次），它触发的 scroll 不再算一次
    const afterKey = a.inputs;
    fireEvent.keyDown(document, { key: "End" });
    fireEvent.scroll(list);
    expect(a.inputs, "程序写的那一次 scroll 不报").toBe(afterKey + 1);
  });

  it("5/F-2 与 5/F-1 配合：窗口替换后自己还原的位置触发的 scroll 不算用户输入（不作废等着的 Home）", async () => {
    const { a, list, anchors } = await openUnloaded();
    fireEvent.keyDown(document, { key: "Home" });
    const was = a.inputs;
    const extra = anchors.items.slice(0, 10).map((i, n) => ({ ...i, key: `restore-${n}` }));
    a.setView(create(AnchorsViewSchema, { ...anchors, items: [...extra, ...anchors.items] }));
    fireEvent.scroll(list);
    expect(a.inputs, "还原触发的 scroll 不报").toBe(was);
    a.setView(create(AnchorsViewSchema, { ...anchors, items: [...extra, ...anchors.items], edgeLanding: create(EdgeLandingSchema, { token: 61n, latest: false }) }));
    expect(a.previewed(), "Home 的落点照常执行").toBe(a.ticks()[0]);
  });

  it("在等 Home / End 的那一头时用户滚列表或按方向键：请 Rust 作废；迟到的窗口按 key 还原，不把用户拉回顶（g3b 2/F-1）", async () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(create(AnchorsViewSchema, { ...anchors, edgePending: true }), current);
    fireEvent.mouseEnter(a.rail);
    await settle();
    const list = a.list()!;
    sizeList(list, 10 * PITCH, anchors.items.length);
    expect(a.inputs, "对照：还没动").toBe(0);
    fireEvent.wheel(list, { deltaY: 40 });
    list.scrollTop = LIST_PAD + 30 * PITCH;
    fireEvent.scroll(list);
    fireEvent.keyDown(document, { key: "ArrowDown" });
    expect(a.inputs, "滚轮、方向键各报一次（scroll 事件不认输入）").toBe(2);
    // Rust 作废了：迟到的窗口没有落点，前面多了 10 条；用户看着的那一段原样留着（往下挪 10 条的距离），不回顶
    const before = list.scrollTop;
    expect(before, "对照：不在顶上").toBeGreaterThan(0);
    const extra = anchors.items.slice(0, 10).map((i, n) => ({ ...i, key: `late-${n}` }));
    a.setView(create(AnchorsViewSchema, { ...anchors, items: [...extra, ...anchors.items], edgePending: false }));
    expect(list.scrollTop).toBe(before + 10 * PITCH);
  });

  it("滚到离已载入的一端不到 8 条、那一侧还有：请 Rust 以那一条为中心重取；约 300ms 没到出加载点；新列表到了按 key 对回原位", async () => {
    vi.useFakeTimers();
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    await settle();
    const list = a.list()!;
    sizeList(list, 10 * PITCH, anchors.items.length);
    // 滚到第 3 条在顶上
    list.scrollTop = LIST_PAD + 3 * PITCH;
    fireEvent.scroll(list);
    expect(3).toBeLessThan(BROWSE_EDGE);
    expect(a.browses).toEqual([anchors.items[3].key]);
    expect(a.rail.querySelector(".iw-anchor__more")).toBeNull();
    vi.advanceTimersByTime(LOADING_DOT_DELAY_MS);
    expect(a.rail.querySelector(".iw-anchor__more--start"), "300ms 还没到：顶端加载点").not.toBeNull();
    expect(list.querySelector(".iw-anchor__more"), "加载点不在列表的流里（出现时刻度与信息卡不跳，UI g3b 1/F-1）").toBeNull();
    expect(list.firstElementChild).toHaveClass("iw-tick");
    // 新列表：前面多了 20 条；原来顶上那一条还在顶上（往下挪了 20 条的距离）
    const extra = anchors.items.slice(0, 20).map((i, n) => ({ ...i, key: `earlier-${n}` }));
    a.setView(create(AnchorsViewSchema, { ...anchors, items: [...extra, ...anchors.items] }));
    expect(list.scrollTop).toBe(LIST_PAD + 23 * PITCH);
    expect(a.rail.querySelector(".iw-anchor__more"), "到了：加载点收掉").toBeNull();
  });

  it("整窗替换、旧预览与当前位置都不在新窗口里：按顶上那条的 key 对回原位，不被居中拉走；预览改到那一条（g3b 1/F-1）", async () => {
    const item = (k: number) => ({ key: String(k), kind: AnchorKind.USER, kindLabel: "你", timeLabel: "10:00", preview: `第 ${k} 处` });
    const range = (from: number, to: number) => Array.from({ length: to - from + 1 }, (_, i) => item(from + i));
    const before = create(AnchorsViewSchema, { items: range(100, 219), hasBefore: true, hasAfter: true });
    const a = mount(before, "200");
    fireEvent.mouseEnter(a.rail);
    await settle();
    const list = a.list()!;
    sizeList(list, 110, before.items.length);
    // 用户沿列表往上滚，第 103 处在顶上
    list.scrollTop = LIST_PAD + 3 * PITCH;
    fireEvent.scroll(list);
    expect(a.browses, "对照：近前端，请了重取").toEqual(["103"]);
    // 续载回来的是整窗替换：44..163，200 与当前位置都被裁掉了
    sizeList(list, 110, 120);
    a.setView(create(AnchorsViewSchema, { items: range(44, 163), hasBefore: true, hasAfter: true }));
    expect(list.scrollTop, "第 103 处还在顶上").toBe(LIST_PAD + (103 - 44) * PITCH);
    expect(a.previewed()?.getAttribute("aria-label"), "预览落在还原后顶上那一条").toBe(label(item(103)));
  });

  it("渐隐：有更多内容的那一侧才渐隐，到头那一侧不渐隐", async () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(create(AnchorsViewSchema, { ...anchors, hasBefore: false, hasAfter: false }), current);
    fireEvent.mouseEnter(a.rail);
    await settle();
    const list = a.list()!;
    sizeList(list, 10 * PITCH, anchors.items.length);
    list.scrollTop = 0;
    fireEvent.scroll(list);
    expect(list).not.toHaveClass("iw-anchor__list--fade-top");
    expect(list).toHaveClass("iw-anchor__list--fade-bottom");
    list.scrollTop = list.scrollHeight - list.clientHeight;
    fireEvent.scroll(list);
    expect(list).toHaveClass("iw-anchor__list--fade-top");
    expect(list).not.toHaveClass("iw-anchor__list--fade-bottom");
  });

  it("滚轮：列表滚不动（静止态或条数少）时交给对话；列表滚得动时不拦，由列表自己滚", () => {
    const { anchors, current } = fromScenario("anchors-basic");
    const a = mount(anchors, current);
    const timeline = screen.getByTestId("timeline");
    fireEvent.wheel(a.rail, { deltaY: 40 });
    expect(timeline.scrollTop, "静止态：交给对话").toBe(40);
    fireEvent.mouseEnter(a.rail);
    sizeList(a.list()!, 1000, anchors.items.length);
    fireEvent.wheel(a.rail, { deltaY: 40 });
    expect(timeline.scrollTop, "条数少、不超出：仍交给对话").toBe(80);
    sizeList(a.list()!, 2 * PITCH, anchors.items.length);
    fireEvent.wheel(a.rail, { deltaY: 40 });
    expect(timeline.scrollTop, "超出：列表自己滚，不带动对话").toBe(80);
  });

  it("点了还没载入的那一处：那条刻度流光、其余变淡（Rust 给 jump_loading_key）；没在跳时都不画", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    expect(a.list()).not.toHaveClass("iw-anchor__list--seeking");
    expect(a.rail.querySelector(".iw-tick--seeking")).toBeNull();
    const target = anchors.items[1].key;
    a.setView(create(AnchorsViewSchema, { ...anchors, jumpLoading: "正在定位到所选消息", jumpLoadingKey: target }));
    expect(a.list()).toHaveClass("iw-anchor__list--seeking");
    expect([...a.rail.querySelectorAll(".iw-tick--seeking")]).toEqual([a.ticks()[1]]);
    expect(a.rail.textContent, "不显示「正在取那一段」这类文字").not.toContain("正在");
  });
});
