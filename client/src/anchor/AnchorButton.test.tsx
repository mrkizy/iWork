// 窄形态的锚点按钮与弹出面板（ui-m1.md 2a 第 11 条「窄形态」）：数据来自 anchors-* 录像里 Rust 给的 AnchorsView。
// 面板里的列表、悬停预览、跳转、续载与宽形态共用 AnchorList，那些规则的细节在 AnchorRail.test.tsx；这里测窄形态自己的部分：
// 按钮与状态点、点开、不靠悬停、Esc / 点外面 / 跳转之后收起且焦点回到按钮。
import { fireEvent, render, screen } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import type { AnchorsView } from "../gen/iwork/v1/view_pb";
import { loadScenario, noFixtures } from "../test-fixtures";
import { AnchorButton } from "./AnchorButton";

function fromScenario(name: string) {
  const c = loadScenario(name).state.conversation;
  if (c?.anchors === undefined) throw new Error(`录像 ${name} 里没有锚点`);
  return { anchors: c.anchors, current: c.anchorKey };
}

function mount(anchors: AnchorsView, current?: string) {
  const jumps: string[] = [];
  const browses: string[] = [];
  render(() => (
    <div>
      <AnchorButton
        anchors={anchors}
        currentKey={current}
        onJump={(k) => jumps.push(k)}
        onBrowse={(k) => browses.push(k)}
        onBrowseEdge={() => {}}
      />
      <p data-testid="outside">别处</p>
    </div>
  ));
  const button = screen.getByRole("button", { name: "消息锚点" });
  const panel = () => document.querySelector<HTMLElement>(".iw-anchor-panel");
  const centerTick = () => panel()?.querySelector<HTMLButtonElement>(".iw-tick[aria-current]");
  return { button, panel, centerTick, jumps, browses };
}

describe.skipIf(noFixtures)("锚点按钮（窄形态）", () => {
  it("静止态：按钮右上角有状态点（等你处理、失败）；没有就不画", () => {
    const basic = fromScenario("anchors-basic");
    const a = mount(basic.anchors, basic.current);
    expect(a.button).toHaveAttribute("aria-expanded", "false");
    expect(a.button.querySelector(".iw-anchor__dot--waiting")).not.toBeNull();
    expect(a.button.querySelector(".iw-anchor__dot--failed")).not.toBeNull();
    a.button.parentElement!.parentElement!.remove();
    const plain = fromScenario("anchors-unloaded");
    const b = mount(plain.anchors, plain.current);
    expect(b.button.querySelector(".iw-anchor__dot")).toBeNull();
  });

  it("不靠悬停展开；点按钮弹出面板，停在当前位置", () => {
    const basic = fromScenario("anchors-basic");
    const a = mount(basic.anchors, basic.current);
    fireEvent.mouseEnter(a.button);
    expect(a.panel(), "悬停不展开").toBeNull();
    fireEvent.click(a.button);
    expect(a.button).toHaveAttribute("aria-expanded", "true");
    expect(a.panel()).not.toBeNull();
    const current = basic.anchors.items.find((i) => i.key === basic.current)!;
    expect(a.centerTick()?.getAttribute("aria-label")).toContain(current.preview);
    // 再点一次收起
    fireEvent.click(a.button);
    expect(a.panel()).toBeNull();
  });

  it("面板里是同一份列表：每个锚点一条刻度，悬停换预览、不发跳转", () => {
    const basic = fromScenario("anchors-basic");
    const a = mount(basic.anchors, basic.current);
    fireEvent.click(a.button);
    const ticks = [...a.panel()!.querySelectorAll<HTMLButtonElement>(".iw-tick")];
    expect(ticks).toHaveLength(basic.anchors.items.length);
    fireEvent.mouseEnter(ticks[0]);
    expect(a.centerTick()).toBe(ticks[0]);
    expect(a.jumps).toEqual([]);
  });

  it("Esc 收起，焦点回到按钮", () => {
    const basic = fromScenario("anchors-basic");
    const a = mount(basic.anchors, basic.current);
    fireEvent.click(a.button);
    (a.panel()!.querySelector("button") as HTMLButtonElement).focus();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(a.panel()).toBeNull();
    expect(document.activeElement).toBe(a.button);
  });

  it("点面板外面收起，焦点回到按钮；点面板里面不收", () => {
    const basic = fromScenario("anchors-basic");
    const a = mount(basic.anchors, basic.current);
    fireEvent.click(a.button);
    fireEvent.pointerDown(a.panel()!);
    expect(a.panel(), "点面板里面不收").not.toBeNull();
    fireEvent.pointerDown(screen.getByTestId("outside"));
    expect(a.panel()).toBeNull();
    expect(document.activeElement).toBe(a.button);
  });

  it("点刻度跳到那一处，跳转之后收起、焦点回到按钮", () => {
    const basic = fromScenario("anchors-basic");
    const a = mount(basic.anchors, basic.current);
    fireEvent.click(a.button);
    const ticks = [...a.panel()!.querySelectorAll<HTMLButtonElement>(".iw-tick")];
    const other = ticks.find((t) => !t.hasAttribute("aria-current"))!;
    const label = other.getAttribute("aria-label")!;
    fireEvent.click(other);
    expect(a.jumps).toHaveLength(1);
    const target = basic.anchors.items.find((i) => i.key === a.jumps[0])!;
    expect(label).toContain(target.preview);
    expect(a.panel()).toBeNull();
    expect(document.activeElement).toBe(a.button);
  });
});
