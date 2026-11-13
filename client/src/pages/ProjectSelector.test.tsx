// 私聊顶栏的项目选择器（UI 审查 g3 F-7；画布 09、13）：当前值与选项来自 ConversationHeader；选别的发
// switchDmProject，选当前那一项不发；项目对话没有选择器。
import { fireEvent, screen, within } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { loadScenario, noFixtures, renderScenario, userActs } from "../test-fixtures";

describe.skipIf(noFixtures)("私聊的项目选择器", () => {
  it("当前值、选项来自 patch；选别的发 switchDmProject，选当前的不发", () => {
    const { acts } = renderScenario("dm-project-switch");
    const conv = loadScenario("dm-project-switch").state.conversation!;
    const h = conv.header!;
    expect(h.projectOptions.length, "对照：有选项").toBeGreaterThan(1);
    const button = screen.getByRole("button", { name: `项目：${h.projectSelectorLabel}` });
    fireEvent.click(button);
    const menu = screen.getByRole("menu", { name: "项目" });
    const items = within(menu).getAllByRole("menuitem");
    expect(items.map((i) => i.textContent)).toEqual(h.projectOptions.map((o) => o.label));
    const current = h.projectOptions.find((o) => o.selected)!;
    fireEvent.click(within(menu).getByRole("menuitem", { name: current.label }));
    expect(userActs(acts).filter((a) => a.case === "switchDmProject"), "选当前的：不发").toEqual([]);
    fireEvent.click(button);
    const other = h.projectOptions.find((o) => !o.selected)!;
    fireEvent.click(within(screen.getByRole("menu", { name: "项目" })).getByRole("menuitem", { name: other.label }));
    expect(userActs(acts).filter((a) => a.case === "switchDmProject")).toEqual([
      { case: "switchDmProject", value: { conversationId: conv.id, projectId: other.projectId } },
    ]);
  });

  it("项目对话：没有项目选择器", () => {
    renderScenario("conversation-basic");
    expect(loadScenario("conversation-basic").state.conversation?.header?.isDm, "对照：确实是项目对话").toBe(false);
    expect(screen.queryByRole("button", { name: /^项目：/ })).toBeNull();
  });
});
