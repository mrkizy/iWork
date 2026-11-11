// 外壳、侧栏、首页与对话页对着录像渲染（方案第 7 节「组件测试」）：可见文字与按钮状态都来自 patch。
import { fireEvent, screen, within } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { noFixtures, renderScenario, userActs } from "../test-fixtures";

/** 侧栏里的对话行（助手行的最近动态里也可能带着同一个标题）。 */
function conversationItem(title: RegExp): HTMLElement {
  const item = screen.getAllByRole("button", { name: title }).find((b) => b.classList.contains("iw-conv"));
  if (item === undefined) throw new Error(`侧栏里没有对话 ${title}`);
  return item;
}

describe.skipIf(noFixtures)("侧栏", () => {
  it("圆点四种状态带状态名，项目与对话两级未读来自 Rust", () => {
    renderScenario("sidebar-dots");
    const nav = screen.getByRole("complementary", { name: "侧栏" });
    for (const label of ["等你处理", "出错", "运行中", "空闲"]) {
      expect(within(nav).getAllByRole("img", { name: label }).length, label).toBeGreaterThan(0);
    }
    expect(within(nav).getByRole("button", { name: /demo/ })).toHaveTextContent("5");
  });

  it("额度取不到时显示「—」，悬停给说明（验收 5）", () => {
    renderScenario("quota-missing");
    const quota = screen.getByLabelText("本机额度");
    expect(quota).toHaveTextContent("—");
    expect(quota.querySelector("[data-tip]")?.getAttribute("data-tip")).toBe("暂时取不到额度");
  });

  it("对照：有额度时显示百分比与进度条", () => {
    renderScenario("conversation-basic");
    const quota = screen.getByLabelText("本机额度");
    expect(quota).toHaveTextContent("37%");
    expect(quota.querySelector(".iw-quota__fill")).not.toBeNull();
  });

  it("右键对话：打开、重命名、删除；删除先确认，确认后才发命令", async () => {
    const { acts } = renderScenario("conversation-basic");
    fireEvent.contextMenu(conversationItem(/帮我把登录页改成深色/));
    const menu = await screen.findByRole("menu", { name: "对话操作" });
    expect(within(menu).getAllByRole("menuitem").map((m) => m.textContent)).toEqual(["打开", "重命名", "删除"]);
    fireEvent.click(within(menu).getByRole("menuitem", { name: "删除" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(dialog).toHaveTextContent("对话记录将删除，不影响代码文件");
    expect(userActs(acts)).toEqual([]);
    fireEvent.click(within(dialog).getByRole("button", { name: "删除" }));
    expect(userActs(acts).map((a) => a.case)).toEqual(["deleteConversation"]);
  });

  it("就地重命名：回车提交、Esc 取消、空内容还原不提交", async () => {
    const { acts } = renderScenario("conversation-basic");
    const rename = async () => {
      fireEvent.contextMenu(conversationItem(/帮我把登录页改成深色/));
      fireEvent.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "重命名" }));
      return screen.getByRole("textbox", { name: "重命名对话" }) as HTMLInputElement;
    };
    let input = await rename();
    fireEvent.input(input, { target: { value: "   " } });
    fireEvent.keyDown(input, { key: "Enter" });
    input = await rename();
    fireEvent.input(input, { target: { value: "别的标题" } });
    fireEvent.keyDown(input, { key: "Escape" });
    expect(userActs(acts)).toEqual([]);
    input = await rename();
    fireEvent.input(input, { target: { value: " 新标题 " } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(userActs(acts)).toEqual([expect.objectContaining({ case: "renameConversation", value: expect.objectContaining({ title: "新标题" }) })]);
  });
});

describe.skipIf(noFixtures)("对话页", () => {
  it("回合一开始就失败：不画空气泡，一行「这一轮没能开始」带详情；中途失败照常显示正文（Q14）", async () => {
    renderScenario("turn-failed");
    const lines = screen.getAllByRole("alert");
    expect(lines.map((l) => l.textContent)).toEqual([
      expect.stringContaining("这一轮没能开始：Claude Code 启动后立刻退出了"),
      expect.stringContaining("这一轮中途失败：模型暂时不可用"),
    ]);
    expect(screen.getByText("我先看一下 CI 的日志，第一处报错在")).toBeInTheDocument();
    fireEvent.click(within(lines[0]).getByRole("button", { name: "详情" }));
    expect(lines[0]).toHaveTextContent("回合 t-start");
  });

  it("档位菜单：三档带说明，选全开先弹确认框，取消不发命令，确认才发", async () => {
    const { acts } = renderScenario("conversation-basic");
    fireEvent.click(screen.getByRole("button", { name: "审批方式：手动" }));
    const menu = await screen.findByRole("menu", { name: "审批方式" });
    expect(menu).toHaveTextContent("写文件、执行命令前始终询问");
    expect(menu).toHaveTextContent("仅本对话");
    fireEvent.click(within(menu).getByRole("menuitem", { name: /全开/ }));
    let dialog = await screen.findByRole("alertdialog", { name: "本对话切到全开？" });
    fireEvent.click(within(dialog).getByRole("button", { name: "取消" }));
    expect(userActs(acts)).toEqual([]);
    fireEvent.click(screen.getByRole("button", { name: "审批方式：手动" }));
    fireEvent.click(within(await screen.findByRole("menu", { name: "审批方式" })).getByRole("menuitem", { name: /全开/ }));
    dialog = await screen.findByRole("alertdialog");
    fireEvent.click(within(dialog).getByRole("button", { name: "切到全开" }));
    expect(userActs(acts).map((a) => a.case)).toEqual(["setApprovalMode"]);
  });

  it("模型菜单：三节；运行时已锁、带原因；选模型发 SetModel 并带上当前思考强度", async () => {
    const { acts } = renderScenario("conversation-basic");
    fireEvent.click(screen.getByRole("button", { name: /Claude Code · fake-large/ }));
    const menu = await screen.findByRole("menu", { name: "运行时与模型" });
    expect(menu).toHaveTextContent("运行时");
    expect(menu).toHaveTextContent("模型 · 仅本对话");
    expect(menu).toHaveTextContent("思考强度");
    expect(within(menu).getByRole("menuitem", { name: /Claude Code/ })).toBeDisabled();
    fireEvent.click(within(menu).getByRole("menuitem", { name: /Fake Small/ }));
    expect(userActs(acts)).toEqual([
      expect.objectContaining({ case: "setModel", value: expect.objectContaining({ modelId: "fake-small", effort: "medium" }) }),
    ]);
  });
});

describe.skipIf(noFixtures)("不可信内容（R7 的 DOM 一侧；真实 WebView 的取证见 tests/e2e）", () => {
  it("正文、标题、文件名、工具输出里的脚本与链接都不变成可执行节点", () => {
    const { container } = renderScenario("xss-corpus");
    const root = container.ownerDocument;
    expect(root.querySelectorAll("script, iframe, object, embed").length).toBe(0);
    expect(root.querySelectorAll("[onerror], [onload], [onclick], [onmouseover]").length).toBe(0);
    expect(root.querySelectorAll("[href]").length, "正文里的链接都不带 href（任何元素）").toBe(0);
    expect(root.querySelectorAll("img").length, "远程图片不加载").toBe(1); // 只有侧栏的 iWork 标志
    // 对照：这些字符串确实被画出来了（当文字）
    expect(root.body.textContent).toContain("<script>alert(1)</script>");
  });
});
