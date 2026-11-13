// 助手资料栏的收起（ui-m1 2a 第 18 条 d）：主区域放得下（≥ 924，回差到 944）且用户没手动收起时常驻；放得下时
// 标题栏「助手资料」按钮发全局偏好（SetProfilePanelCollapsed），偏好从后台回来才变；放不下时资料栏收起，按钮点开
// 盖在对话上的浮层，Esc、点外面收起，变宽到放得下回到常驻。jsdom 不排版：给助手页一个假的 ResizeObserver 报宽度。
import { create } from "@bufbuild/protobuf";
import { fireEvent, screen } from "@solidjs/testing-library";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { AssistantProfileViewSchema, ViewPatchSchema } from "../gen/iwork/v1/view_pb";
import { loadScenario, noFixtures, renderScenario, userActs } from "../test-fixtures";

const ID = "as-asen";
const DM = "c-dm-asen";
const route = { page: "assistant" as const, id: ID, conversationId: DM, tab: "activity" as const };

const observers = new Set<ResizeObserverCallback>();
const realRO = globalThis.ResizeObserver;
/** 把助手页（主区域）的宽度报成 `width`。 */
function resizeMain(width: number) {
  for (const cb of observers) cb([{ contentRect: { width } } as ResizeObserverEntry], {} as ResizeObserver);
}
beforeEach(() => {
  globalThis.ResizeObserver = class {
    constructor(private cb: ResizeObserverCallback) {}
    observe(el: Element) {
      if (el.classList.contains("iw-assistant-page")) observers.add(this.cb);
    }
    unobserve() {}
    disconnect() {
      observers.delete(this.cb);
    }
  } as unknown as typeof ResizeObserver;
});
afterEach(() => {
  globalThis.ResizeObserver = realRO;
  observers.clear();
});

const panel = () => screen.queryByRole("complementary", { name: "助手资料" });
const button = () => screen.getByRole("button", { name: "助手资料" });
const overlay = () => panel()?.classList.contains("iw-profile--overlay");

describe.skipIf(noFixtures)("助手资料栏的收起", () => {
  it("放得下：常驻、按钮展开态；点按钮发「收起」偏好，偏好回来才收起；再点发「展开」", () => {
    const { acts, store } = renderScenario("assistant-profile", route);
    resizeMain(1144);
    expect(panel(), "常驻").not.toBeNull();
    expect(overlay()).toBe(false);
    expect(button()).toHaveAttribute("aria-expanded", "true");
    fireEvent.click(button());
    expect(userActs(acts)).toEqual([{ case: "setProfilePanelCollapsed", value: { on: true } }]);
    expect(panel(), "后台回来之前不变").not.toBeNull();
    const view = loadScenario("assistant-profile").state.assistant!;
    store.applyPatch(
      create(ViewPatchSchema, {
        changes: [
          {
            change: {
              case: "assistant",
              value: { view: create(AssistantProfileViewSchema, { ...view, profilePanelCollapsed: true }) },
            },
          },
        ],
      }),
    );
    expect(panel(), "偏好是收起").toBeNull();
    expect(button()).toHaveAttribute("aria-expanded", "false");
    acts.length = 0;
    fireEvent.click(button());
    expect(userActs(acts)).toEqual([{ case: "setProfilePanelCollapsed", value: { on: false } }]);
  });

  it("窄于 924 收起；按钮开浮层（不改偏好），Esc 收起、焦点回按钮；点外面也收起；变宽到 944 回常驻", () => {
    const { acts } = renderScenario("assistant-profile", route);
    resizeMain(1000);
    resizeMain(923);
    expect(panel(), "放不下：收起").toBeNull();
    expect(button()).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(button());
    expect(overlay(), "浮层").toBe(true);
    expect(userActs(acts), "浮层不改偏好").toEqual([]);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(panel()).toBeNull();
    expect(document.activeElement).toBe(button());
    fireEvent.click(button());
    fireEvent.pointerDown(document.querySelector(".iw-conv-page")!);
    expect(panel(), "点外面收起").toBeNull();
    fireEvent.click(button());
    resizeMain(930);
    expect(overlay(), "回差内不变").toBe(true);
    resizeMain(944);
    expect(panel(), "放得下：回到常驻").not.toBeNull();
    expect(overlay()).toBe(false);
  });

  it("窗口 880、侧栏 296：主区域 584，一开始就是收起的", () => {
    renderScenario("assistant-profile", route);
    resizeMain(584);
    expect(panel()).toBeNull();
  });
});
