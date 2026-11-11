// 外壳、侧栏、首页与对话页对着录像渲染（方案第 7 节「组件测试」）：可见文字与按钮状态都来自 patch。
import { fireEvent, screen, waitFor, within } from "@solidjs/testing-library";
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
    const cell = quota.querySelector<HTMLElement>("[data-tip]")!;
    expect(cell.getAttribute("data-tip")).toBe("暂时取不到额度");
    // 键盘也能看到提示（F-18）；名字与「—」是两个元素，中间有间距（F-15）
    expect(cell.tabIndex).toBe(0);
    expect([...cell.children].map((c) => c.textContent)).toEqual(["Claude Code", "—"]);
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
    // 上限由 Rust 给（SidebarView.titleMaxChars）
    input = await rename();
    fireEvent.input(input, { target: { value: "字".repeat(60) } });
    expect(Array.from(input.value)).toHaveLength(50);
    fireEvent.keyDown(input, { key: "Escape" });
    input = await rename();
    fireEvent.input(input, { target: { value: " 新标题 " } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(userActs(acts)).toEqual([expect.objectContaining({ case: "renameConversation", value: expect.objectContaining({ title: "新标题" }) })]);
  });
});

describe.skipIf(noFixtures)("关闭弹层后焦点回到触发它的控件（审查 2a F-11）", () => {
  it("菜单：Esc 关闭后回到按钮", async () => {
    renderScenario("conversation-basic");
    const pill = screen.getByRole("button", { name: "审批方式：手动" });
    pill.focus();
    fireEvent.click(pill);
    const menu = await screen.findByRole("menu", { name: "审批方式" });
    expect(menu.contains(document.activeElement), "打开时焦点进菜单").toBe(true);
    fireEvent.keyDown(document, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
    expect(document.activeElement).toBe(pill);
  });

  it("右键菜单 → 确认框：取消后回到被右键的那一项", async () => {
    renderScenario("conversation-basic");
    const item = conversationItem(/帮我把登录页改成深色/);
    fireEvent.contextMenu(item);
    fireEvent.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "删除" }));
    const dialog = await screen.findByRole("alertdialog");
    fireEvent.click(within(dialog).getByRole("button", { name: "取消" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(document.activeElement).toBe(item);
  });

  it.each([["Enter"], ["Escape"]])("就地重命名：%s 收尾后回到那一项", async (key) => {
    renderScenario("conversation-basic");
    fireEvent.contextMenu(conversationItem(/帮我把登录页改成深色/));
    fireEvent.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "重命名" }));
    const input = screen.getByRole("textbox", { name: "重命名对话" });
    expect(document.activeElement).toBe(input);
    fireEvent.keyDown(input, { key });
    await waitFor(() => expect(document.activeElement).toBe(conversationItem(/帮我把登录页改成深色/)));
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
    // 选中项有底色；还没接入的运行时灰着、写「M2 接入」（F-19）
    expect(within(menu).getByRole("menuitem", { name: /Claude Code/ })).toHaveClass("iw-menu__item--selected");
    expect(within(menu).getByRole("menuitem", { name: /Fake Large/ })).toHaveClass("iw-menu__item--selected");
    expect(within(menu).getByRole("menuitem", { name: /Fake Small/ })).not.toHaveClass("iw-menu__item--selected");
    const codex = within(menu).getByRole("menuitem", { name: /Codex/ });
    expect(codex).toBeDisabled();
    expect(codex).toHaveTextContent("M2 接入");
    fireEvent.click(within(menu).getByRole("menuitem", { name: /Fake Small/ }));
    expect(userActs(acts)).toEqual([
      expect.objectContaining({ case: "setModel", value: expect.objectContaining({ modelId: "fake-small", effort: "medium" }) }),
    ]);
  });
});

describe.skipIf(noFixtures)("首页", () => {
  it("输入框和对话里一样带「运行时 · 模型」与档位；选的值交给 Rust（首页专用操作）", async () => {
    const { acts } = renderScenario("empty");
    expect(document.querySelector(".iw-hero__project"), "项目由 Rust 挑").toHaveTextContent("demo");
    fireEvent.click(screen.getByRole("button", { name: /Claude Code · fake-large/ }));
    fireEvent.click(within(await screen.findByRole("menu", { name: "运行时与模型" })).getByRole("menuitem", { name: /Fake Small/ }));
    fireEvent.click(screen.getByRole("button", { name: "审批方式：手动" }));
    fireEvent.click(within(await screen.findByRole("menu", { name: "审批方式" })).getByRole("menuitem", { name: /自动/ }));
    expect(userActs(acts).map((a) => a.case)).toEqual(["setHomeModel", "setHomeApprovalMode"]);
  });

  it("发出交给 Rust 一次 startConversation；受理后草稿还在，等跳转收尾才清", async () => {
    const { acts } = renderScenario("empty");
    const box = screen.getByRole("textbox", { name: "消息" }) as HTMLTextAreaElement;
    expect(box.placeholder, "占位文字由 Rust 按答话的助手给（F-14）").toBe("跟 iWork 说点什么");
    fireEvent.input(box, { target: { value: "首页的第一条" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    await Promise.resolve();
    expect(userActs(acts)).toEqual([
      expect.objectContaining({ case: "startConversation", value: expect.objectContaining({ projectId: "p-demo", text: "首页的第一条" }) }),
    ]);
    expect(box.value).toBe("首页的第一条");
  });
});

describe.skipIf(noFixtures)("不可信内容（R7 的 DOM 一侧；真实 WebView 的取证见 tests/e2e）", () => {
  it("正文、标题、文件名、工具输出里的脚本与链接都不变成可执行节点", () => {
    const { container } = renderScenario("xss-corpus");
    const root = container.ownerDocument;
    expect(root.querySelectorAll("script, iframe, object, embed").length).toBe(0);
    expect(root.querySelectorAll("[onerror], [onload], [onclick], [onmouseover]").length).toBe(0);
    expect(root.querySelectorAll("[href]").length, "正文里的链接都不带 href（任何元素）").toBe(0);
    // 页面上的图片只有随包带的 iWork 标志（侧栏品牌、iWork 的头像），正文里的远程图片不加载
    const srcs = [...root.querySelectorAll("img")].map((i) => i.getAttribute("src") ?? "");
    expect(srcs.length, "对照：确实有标志图").toBeGreaterThan(0);
    expect(new Set(srcs).size, "远程图片不加载：全是同一张标志").toBe(1);
    expect(srcs.filter((s) => s.includes("example.com")), "远程图片不加载").toEqual([]);
    // 对照：这些字符串确实被画出来了（当文字）
    expect(root.body.textContent).toContain("<script>alert(1)</script>");
  });
});
