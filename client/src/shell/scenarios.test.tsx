// 预览场景确实录到了要审的状态（审查 2a F-13）：每个场景对着全量帧渲染，看画面上有没有那一处。
// 文字都来自 Rust 的视图模型；这里只证明录像里有，不重复验派生规则（那些在 client-core 的测试里）。
import { fireEvent, screen, within } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { noFixtures, renderScenario } from "../test-fixtures";

const composerBlock = () => document.querySelector(".iw-composer__block");

describe.skipIf(noFixtures)("预览场景", () => {
  it("back-to-latest-notice：Q9 提示", () => {
    renderScenario("back-to-latest-notice");
    expect(screen.getByText("已重新连接，回到最新")).toBeInTheDocument();
  });

  it("new-below-count：回到最新（N）", () => {
    renderScenario("new-below-count");
    expect(screen.getByRole("button", { name: /回到最新（\d+）/ })).toBeInTheDocument();
  });

  it.each([
    ["composer-path-missing", "找不到项目文件夹，重新定位后才能发送", "重新定位"],
    ["composer-verifying", "正在验证这个版本的", undefined],
    ["composer-incompatible", "没通过验证", "查看详情"],
    ["composer-not-installed", "没有找到", "查看详情"],
    ["composer-not-logged-in", "还没登录", "查看详情"],
    ["composer-offline", "正在重新连接，连上后就能发送", undefined],
  ])("%s：输入框上方的说明", (name, reason, action) => {
    renderScenario(name);
    const block = composerBlock() as HTMLElement;
    expect(block).toHaveTextContent(reason);
    if (action) expect(within(block).getByRole("button", { name: action })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "发送" })).toBeDisabled();
  });

  it("attachments-tray：托盘两个文件，超过 20MB 的说明原因", () => {
    renderScenario("attachments-tray");
    const tray = document.querySelector(".iw-composer__tray") as HTMLElement;
    expect(tray).toHaveTextContent("设计稿.png");
    expect(tray).toHaveTextContent("接口说明.pdf");
    expect(screen.getByRole("alert")).toHaveTextContent("超过 20MB");
  });

  it("projects-sidebar：置顶、找不到路径、99+；右键项目有菜单", async () => {
    renderScenario("projects-sidebar");
    const nav = screen.getByRole("complementary", { name: "侧栏" });
    // 项目与它下面的对话各有一个徽标，都写「99+」
    expect(within(nav).getAllByText("99+").length).toBeGreaterThan(0);
    expect(nav).toHaveTextContent("找不到路径");
    fireEvent.contextMenu(within(nav).getByText("legacy-app"));
    expect(await screen.findByRole("menu")).toBeInTheDocument();
  });

  it("tool-output：点开工具行看到输出", () => {
    renderScenario("tool-output");
    const line = screen.getByRole("button", { expanded: false, name: /Bash/ });
    fireEvent.click(line);
    expect(document.querySelector(".iw-tool__output")).toHaveTextContent("test result: ok. 12 passed");
  });

  it("characters-preview-only：三个形象与圆团子；老周出错是 sorry", () => {
    renderScenario("characters-preview-only");
    const nav = screen.getByRole("complementary", { name: "侧栏" });
    const mood = (name: string) => within(nav).getByRole("img", { name }).dataset.mood;
    expect(mood("阿森")).toBe("work");
    expect(mood("林岚")).toBe("wait");
    expect(mood("老周")).toBe("sorry");
    expect(within(nav).getByRole("img", { name: "小团" }).dataset.character).toBe("blob");
    expect(within(nav).getByRole("img", { name: "iWork" }).querySelector("img"), "iWork 画标志").not.toBeNull();
  });
});
