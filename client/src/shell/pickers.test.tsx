// 系统选择框的三个入口与拖入虚线框（T09 组 2b）：网页只发「要选」，不带路径；路径由 Rust 弹框拿。
// 点不点得了、虚线框的字都来自录像里的视图模型。
import { fireEvent, screen, within } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { noFixtures, renderScenario, userActs } from "../test-fixtures";

describe.skipIf(noFixtures)("选择框入口", () => {
  it("对话里点回形针：发 pickAttachments，只带对话 id", () => {
    const { acts } = renderScenario("conversation-basic");
    const clip = screen.getByRole("button", { name: "添加附件" });
    expect(clip).toBeEnabled();
    fireEvent.click(clip);
    expect(userActs(acts)).toEqual([{ case: "pickAttachments", value: { conversationId: "c-basic" } }]);
  });

  it("首页的回形针置灰，悬停说明来自 Rust", () => {
    const { acts } = renderScenario("projects-sidebar");
    const clip = screen.getByRole("button", { name: "添加附件" });
    expect(clip).toBeDisabled();
    expect(clip).toHaveAttribute("data-tip", "发出第一条消息后可以添加附件");
    fireEvent.click(clip);
    expect(userActs(acts)).toEqual([]);
  });

  it("输入框上方的「重新定位」：发 pickProjectLocation，带这个对话的项目", () => {
    const { acts } = renderScenario("composer-path-missing");
    const block = document.querySelector(".iw-composer__block") as HTMLElement;
    fireEvent.click(within(block).getByRole("button", { name: "重新定位" }));
    expect(userActs(acts)).toEqual([{ case: "pickProjectLocation", value: { projectId: "p-demo" } }]);
  });

  it("侧栏行内与右键菜单的「重新定位」：都发 pickProjectLocation", async () => {
    const { acts } = renderScenario("projects-sidebar");
    const nav = screen.getByRole("complementary", { name: "侧栏" });
    const row = nav.querySelector(".iw-project--missing .iw-project__row") as HTMLElement;
    fireEvent.click(within(row).getByRole("button", { name: "重新定位" }));
    fireEvent.contextMenu(within(nav).getByText("legacy-app"));
    const menu = await screen.findByRole("menu");
    fireEvent.click(within(menu).getByRole("menuitem", { name: "重新定位" }));
    const relocate = { case: "pickProjectLocation", value: { projectId: "p-legacy" } };
    expect(userActs(acts)).toEqual([relocate, relocate]);
  });
});

describe.skipIf(noFixtures)("拖入", () => {
  it("drop-overlay：拖着文件时对话区盖一层「松开以添加」", () => {
    renderScenario("drop-overlay");
    const overlay = document.querySelector(".iw-drop") as HTMLElement;
    expect(overlay).toHaveTextContent("松开以添加");
  });

  it("没在拖时没有虚线框（对照）", () => {
    renderScenario("conversation-basic");
    expect(document.querySelector(".iw-drop")).toBeNull();
  });
});
