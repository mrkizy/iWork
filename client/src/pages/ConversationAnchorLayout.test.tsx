// 对话页里消息锚点的两种形态（ui-m1.md 2a 第 11 条「窄窗口与让位」）：按主区域宽度选宽形态（左边距里的导轨，
// 对话区左边留 --anchor-gutter）或窄形态（标题栏按钮），有回差；⌥↑ / ⌥↓ 两种形态都可用。
// jsdom 不排版：给对话页一个假的 ResizeObserver 报宽度，给时间线一个「内容超出一屏」的布局（锚点只在超出时显示）。
import { create } from "@bufbuild/protobuf";
import { fireEvent, screen } from "@solidjs/testing-library";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { AnchorsViewSchema, ViewPatchSchema } from "../gen/iwork/v1/view_pb";
import { loadScenario, noFixtures, renderScenario, userActs } from "../test-fixtures";

const pages = new Set<{ cb: ResizeObserverCallback; el: Element }>();
const realRO = globalThis.ResizeObserver;
const restores: (() => void)[] = [];

/** 把对话页的宽度报成 `width`（只报给观察对话页的那个 observer）。 */
function resizePage(width: number) {
  for (const p of pages) {
    p.cb([{ contentRect: { width } } as ResizeObserverEntry], {} as ResizeObserver);
  }
}

beforeEach(() => {
  globalThis.ResizeObserver = class {
    constructor(private cb: ResizeObserverCallback) {}
    observe(el: Element) {
      if (el.classList.contains("iw-conv-page")) pages.add({ cb: this.cb, el });
    }
    unobserve() {}
    disconnect() {
      for (const p of pages) if (p.cb === this.cb) pages.delete(p);
    }
  } as unknown as typeof ResizeObserver;
  // 时间线比视口高：内容超出一屏
  for (const [name, value] of [
    ["scrollHeight", 2000],
    ["clientHeight", 400],
  ] as const) {
    const old = Object.getOwnPropertyDescriptor(HTMLElement.prototype, name);
    Object.defineProperty(HTMLElement.prototype, name, {
      configurable: true,
      get() {
        return this.classList?.contains("iw-vrows") ? value : 0;
      },
    });
    restores.push(() => (old ? Object.defineProperty(HTMLElement.prototype, name, old) : undefined));
  }
});
afterEach(() => {
  globalThis.ResizeObserver = realRO;
  pages.clear();
  while (restores.length) restores.pop()!();
});

const rail = () => document.querySelector(".iw-anchor");
const headerButton = () => document.querySelector(".iw-conv-header .iw-anchor-btn");
const page = () => document.querySelector(".iw-conv-page")!;

/** 等下一帧（VirtualRows 在 requestAnimationFrame 里报「超出一屏」）。 */
const frame = () => new Promise((r) => requestAnimationFrame(() => r(undefined)));

describe.skipIf(noFixtures)("锚点的两种形态", () => {
  it("主区域 728：宽形态（导轨、左边距）；降到 599 切窄（标题栏按钮）；610 不切回、620 才切回", async () => {
    renderScenario("anchors-basic");
    resizePage(728);
    await frame();
    expect(rail(), "宽形态有导轨").not.toBeNull();
    expect(headerButton()).toBeNull();
    expect(page().classList.contains("iw-conv-page--wide")).toBe(true);

    resizePage(599);
    await frame();
    expect(rail(), "窄形态没有导轨").toBeNull();
    expect(headerButton(), "标题栏里有锚点按钮").not.toBeNull();
    expect(page().classList.contains("iw-conv-page--wide")).toBe(false);

    resizePage(610);
    await frame();
    expect(headerButton(), "回差内不切回").not.toBeNull();
    resizePage(620);
    await frame();
    expect(rail(), "620 切回宽形态").not.toBeNull();
    expect(headerButton()).toBeNull();
  });

  it("点了还没载入的消息：对话区顶端出 2px 进度线，读屏读 Rust 给的那句；跳完收掉", async () => {
    const { store } = renderScenario("anchors-basic");
    resizePage(728);
    await frame();
    expect(document.querySelector(".iw-seek-line"), "对照：没在跳").toBeNull();
    const conv = loadScenario("anchors-basic").state.conversation!;
    const patch = (anchors: Parameters<typeof create<typeof AnchorsViewSchema>>[1]) =>
      store.applyPatch(
        create(ViewPatchSchema, {
          changes: [
            {
              change: {
                case: "conversation",
                value: { conversationId: conv.id, anchors: create(AnchorsViewSchema, anchors) },
              },
            },
          ],
        }),
      );
    patch({ ...conv.anchors!, jumpLoading: "正在定位到所选消息", jumpLoadingKey: conv.anchors!.items[0].key });
    expect(document.querySelector(".iw-timeline .iw-seek-line")).not.toBeNull();
    expect(screen.getByRole("status")).toHaveTextContent("正在定位到所选消息");
    patch({ ...conv.anchors!, jumpLoading: undefined, jumpLoadingKey: "" });
    expect(document.querySelector(".iw-seek-line")).toBeNull();
  });

  it("窄形态下 ⌥↑ / ⌥↓ 照样直接发相对跳转", async () => {
    const { acts } = renderScenario("anchors-basic");
    resizePage(500);
    await frame();
    expect(headerButton()).not.toBeNull();
    fireEvent.keyDown(document.body, { key: "ArrowUp", altKey: true });
    fireEvent.keyDown(document.body, { key: "ArrowDown", altKey: true });
    expect(userActs(acts)).toEqual([
      expect.objectContaining({ case: "jumpRelative", value: expect.objectContaining({ delta: -1 }) }),
      expect.objectContaining({ case: "jumpRelative", value: expect.objectContaining({ delta: 1 }) }),
    ]);
    expect(screen.queryByRole("dialog", { name: "消息锚点" }), "不弹面板").toBeNull();
  });
});
