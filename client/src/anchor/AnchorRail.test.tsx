// 锚点滚轮组件（方案第 7 节「锚点组件」）：数据来自 anchors-* 录像里 Rust 给的 AnchorsView，文字逐字来自 patch。
import { create } from "@bufbuild/protobuf";
import { fireEvent, render, screen } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AnchorsViewSchema, type AnchorsView } from "../gen/iwork/v1/view_pb";
import { loadScenario, noFixtures } from "../test-fixtures";
import { AnchorRail, CLOSE_DELAY_MS, TICKS_EACH_SIDE } from "./AnchorRail";

function fromScenario(name: string) {
  const c = loadScenario(name).state.conversation;
  if (c?.anchors === undefined) throw new Error(`录像 ${name} 里没有锚点`);
  return { anchors: c.anchors, current: c.anchorKey };
}

function mount(anchors: AnchorsView, current?: string) {
  const jumps: string[] = [];
  const browses: string[] = [];
  const [view, setView] = createSignal(anchors);
  render(() => (
    <AnchorRail anchors={view()} currentKey={current} onJump={(k) => jumps.push(k)} onBrowse={(k) => browses.push(k)} />
  ));
  const rail = screen.getByRole("navigation", { name: "消息锚点" });
  const ticks = () => [...rail.querySelectorAll<HTMLButtonElement>(".iw-tick")];
  const centerTick = () => rail.querySelector<HTMLButtonElement>(".iw-tick--center");
  const card = () => rail.querySelector<HTMLButtonElement>(".iw-anchor__card");
  return { rail, ticks, centerTick, card, jumps, browses, setView };
}

let now = 0;
beforeEach(() => {
  now = 1000;
  vi.spyOn(performance, "now").mockImplementation(() => now);
});
afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

const wheel = (el: HTMLElement, deltaY: number, deltaMode = 1) => fireEvent.wheel(el, { deltaY, deltaMode });

describe.skipIf(noFixtures)("锚点滚轮", () => {
  it("静止态：有等你处理出芥末黄点、有失败出赭红点；都没有就不出", () => {
    const basic = fromScenario("anchors-basic");
    const a = mount(basic.anchors, basic.current);
    expect(a.rail.querySelector(".iw-anchor__dot--waiting")).not.toBeNull();
    expect(a.rail.querySelector(".iw-anchor__dot--failed")).not.toBeNull();
    a.rail.remove();
    const plain = fromScenario("anchors-unloaded");
    const b = mount(plain.anchors, plain.current);
    expect(b.rail.querySelector(".iw-anchor__dot")).toBeNull();
    expect(b.rail.querySelectorAll(".iw-anchor__rest span")).toHaveLength(3);
  });

  it("展开：停在当前位置，上下各不超过 4 条；信息卡文字逐字来自 patch", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    const item = anchors.items.find((i) => i.key === current)!;
    expect(a.centerTick()?.getAttribute("aria-label")).toBe(`${item.kindLabel} ${item.timeLabel} ${item.preview}`);
    expect(a.ticks().length).toBeLessThanOrEqual(2 * TICKS_EACH_SIDE + 1);
    expect(a.card()).toHaveTextContent(item.kindLabel);
    expect(a.card()).toHaveTextContent(item.timeLabel);
    expect(a.card()).toHaveTextContent(item.preview);
  });

  it("鼠标滚轮一格换一处，不发跳转", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    const at = anchors.items.findIndex((i) => i.key === current);
    wheel(a.rail, -3);
    expect(a.centerTick()?.getAttribute("aria-label")).toContain(anchors.items[at - 1].preview);
    expect(a.jumps).toEqual([]);
  });

  it("触控板位移没到阈值不换", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    const before = a.centerTick()?.getAttribute("aria-label");
    wheel(a.rail, -10, 0);
    expect(a.centerTick()?.getAttribute("aria-label")).toBe(before);
    now += 200;
    wheel(a.rail, -60, 0);
    expect(a.centerTick()?.getAttribute("aria-label")).not.toBe(before);
  });

  it("连续甩动：一秒内换的处数有上限", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    const start = anchors.items.findIndex((i) => i.key === current);
    for (let i = 0; i < 100; i++) {
      now += 10;
      wheel(a.rail, -200, 0);
    }
    const label = a.centerTick()?.getAttribute("aria-label") ?? "";
    const moved = start - anchors.items.findIndex((i) => label.endsWith(i.preview) && label.startsWith(i.kindLabel));
    expect(moved).toBeGreaterThan(0);
    expect(moved).toBeLessThanOrEqual(10);
  });

  it("到头回弹、不循环", () => {
    const { anchors } = fromScenario("anchors-basic");
    const last = anchors.items[anchors.items.length - 1];
    const a = mount(create(AnchorsViewSchema, { ...anchors, hasAfter: false }), last.key);
    fireEvent.mouseEnter(a.rail);
    wheel(a.rail, 3);
    expect(a.centerTick()?.getAttribute("aria-label")).toContain(last.preview);
    expect(a.rail.querySelector(".iw-anchor__wheel--bounce-end")).not.toBeNull();
  });

  it("点中间刻度或信息卡：跳到那一处并收起；点别的刻度直接跳那一处", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    fireEvent.click(a.card()!);
    expect(a.jumps).toEqual([current]);
    expect(a.rail.classList.contains("iw-anchor--open"), "收起").toBe(false);
    fireEvent.mouseEnter(a.rail);
    const other = a.ticks().find((t) => !t.classList.contains("iw-tick--center"))!;
    fireEvent.click(other);
    expect(a.jumps).toHaveLength(2);
    expect(a.jumps[1]).not.toBe(current);
  });

  it("Esc 立即收起；移开 300ms 才收起（299ms 时还在）", () => {
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
    expect(a.jumps, "收起不动对话").toEqual([]);
  });

  it("翻看到边缘附近、那一侧还有：请 Rust 重取；新列表到了按 key 对回预览位置", () => {
    const { anchors, current } = fromScenario("anchors-unloaded");
    expect(anchors.hasBefore, "对照：前面还有").toBe(true);
    const a = mount(anchors, current);
    fireEvent.mouseEnter(a.rail);
    const at = anchors.items.findIndex((i) => i.key === current);
    for (let i = 0; i < at; i++) {
      now += 200;
      wheel(a.rail, -3);
    }
    expect(a.browses.length).toBeGreaterThan(0);
    const previewed = a.centerTick()?.getAttribute("aria-label");
    // 新列表：前面多了几处，正在预览的那一处换了位置
    const extra = anchors.items.slice(0, 3).map((i) => ({ ...i, key: `${i.key}-earlier` }));
    a.setView(create(AnchorsViewSchema, { ...anchors, items: [...extra, ...anchors.items] }));
    expect(a.centerTick()?.getAttribute("aria-label")).toBe(previewed);
  });
});
