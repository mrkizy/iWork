// 锚点导轨，宽形态（ui-m1.md 2a 第 11 条，列表式）：数据来自 anchors-* 录像里 Rust 给的 AnchorsView，文字逐字来自 patch。
// jsdom 不排版：列表的可视高度与内容高度在用到的用例里手工给（clientHeight / scrollHeight）。
import { create } from "@bufbuild/protobuf";
import { fireEvent, render, screen } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AnchorsViewSchema, type AnchorsView } from "../gen/iwork/v1/view_pb";
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
      />
    </div>
  ));
  const rail = screen.getByRole("navigation", { name: "消息锚点" });
  const list = () => rail.querySelector<HTMLDivElement>(".iw-anchor__list");
  const ticks = () => [...rail.querySelectorAll<HTMLButtonElement>(".iw-tick")];
  const previewed = () => rail.querySelector<HTMLButtonElement>(".iw-tick[aria-current]");
  const card = () => rail.querySelector<HTMLButtonElement>(".iw-anchor__card");
  return { rail, list, ticks, previewed, card, jumps, browses, edges, setView };
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

  it("Home：最早一处不在已载入的窗口里就请 Rust 取最早一头，新列表到了预览移到第一条；End 到头时直接移到最后一条", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    expect(anchors.hasBefore && !anchors.hasAfter, "对照：前面还有、后面没有").toBe(true);
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    fireEvent.keyDown(document, { key: "Home" });
    expect(a.edges).toEqual([false]);
    expect(a.previewed()?.getAttribute("aria-label"), "新列表到之前预览不动").toBe(
      label(anchors.items.find((i) => i.key === current)!),
    );
    const earliest = anchors.items.slice(0, 5).map((i, n) => ({ ...i, key: `first-${n}` }));
    a.setView(create(AnchorsViewSchema, { ...anchors, items: [...earliest, ...anchors.items.slice(0, 20)], hasBefore: false, hasAfter: true }));
    expect(a.previewed()?.getAttribute("aria-label")).toBe(label(earliest[0]));
    // End：这份窗口后面还有，请最新一头；那一头到了（后面没有了）移到最后一条
    fireEvent.keyDown(document, { key: "End" });
    expect(a.edges).toEqual([false, true]);
    a.setView(create(AnchorsViewSchema, { ...anchors }));
    expect(a.previewed()?.getAttribute("aria-label")).toBe(label(anchors.items.at(-1)!));
    fireEvent.keyDown(document, { key: "Home" });
    fireEvent.keyDown(document, { key: "End" });
    expect(a.edges, "已经在最新一头：End 不再请").toEqual([false, true, false]);
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
    // 新列表：前面多了 20 条；原来顶上那一条还在顶上（往下挪了 20 条的距离）
    const extra = anchors.items.slice(0, 20).map((i, n) => ({ ...i, key: `earlier-${n}` }));
    a.setView(create(AnchorsViewSchema, { ...anchors, items: [...extra, ...anchors.items] }));
    expect(list.scrollTop).toBe(LIST_PAD + 23 * PITCH);
    expect(a.rail.querySelector(".iw-anchor__more"), "到了：加载点收掉").toBeNull();
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
