// 「全部对话」列表页（步 7e，ui-m1.md 2a 第 5 条）：对着 all-conversations 录像渲染。每行的文字来自 patch；
// 筛选文字随可见视图声明给 Rust；滚到底要下一批；右键同侧栏（打开 / 重命名 / 删除）；空态带「+ 新对话」。
import { fireEvent, screen, within } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { loadScenario, noFixtures, renderScenario, userActs } from "../test-fixtures";

const PROJECT = "p-demo";
const route = (filter?: string) => ({ page: "allConversations" as const, projectId: PROJECT, filter });
const list = () => loadScenario("all-conversations").state.conversationList!;

describe.skipIf(noFixtures)("全部对话", () => {
  it("对照：录像里有 50 条、还有下一批", () => {
    expect(list().items.length).toBe(50);
    expect(list().hasMore).toBe(true);
  });

  it("标题、每行的标题 / 首行 / 时间逐字来自 patch，顺序照 Rust 给的", () => {
    renderScenario("all-conversations", route());
    const name = loadScenario("all-conversations").state.sidebar!.projects.find((p) => p.projectId === PROJECT)!.name;
    expect(screen.getByRole("heading", { name: `${name} · 全部对话` })).toBeInTheDocument();
    const rows = within(screen.getByRole("list", { name: "全部对话" })).getAllByRole("listitem");
    expect(rows.length).toBe(list().items.length);
    list().items.forEach((c, i) => {
      for (const t of [c.title, c.lastMessageFirstLine, c.lastActiveLabel]) expect(rows[i].textContent).toContain(t);
    });
  });

  it("筛选：输入就重新声明可见视图，带上筛选文字", () => {
    const { acts } = renderScenario("all-conversations", route());
    fireEvent.input(screen.getByRole("searchbox", { name: "按标题筛选" }), { target: { value: "第 3" } });
    const v = acts.filter((a) => a.case === "setVisible").at(-1);
    expect(v?.value).toMatchObject({ conversationList: { projectId: PROJECT, filter: "第 3" } });
  });

  it("滚到底：要下一批（还有下一批、没在加载时）", () => {
    const { acts } = renderScenario("all-conversations", route());
    const page = document.querySelector(".iw-list-page") as HTMLElement;
    Object.defineProperties(page, {
      scrollHeight: { value: 5000, configurable: true },
      clientHeight: { value: 800, configurable: true },
      scrollTop: { value: 4100, configurable: true, writable: true },
    });
    fireEvent.scroll(page);
    expect(userActs(acts).filter((a) => a.case === "loadMoreConversations").length).toBe(1);
  });

  it("点一行打开那个对话；右键菜单同侧栏，删除要确认", () => {
    const { acts } = renderScenario("all-conversations", route());
    const first = list().items[0];
    const row = within(screen.getByRole("list", { name: "全部对话" })).getAllByRole("button")[0];
    expect(row.textContent, "对照：第一行就是 Rust 给的第一条").toContain(first.title);
    fireEvent.contextMenu(row);
    const menu = screen.getByRole("menu", { name: "对话操作" });
    expect(within(menu).getByRole("menuitem", { name: "打开" })).toBeInTheDocument();
    expect(within(menu).queryByRole("menuitem", { name: "重命名" }) !== null).toBe(first.canRename);
    fireEvent.click(within(menu).getByRole("menuitem", { name: "删除" }));
    fireEvent.click(within(screen.getByRole("alertdialog")).getByRole("button", { name: "删除" }));
    expect(userActs(acts).filter((a) => a.case === "deleteConversation")).toEqual([
      { case: "deleteConversation", value: { conversationId: first.conversationId } },
    ]);
    fireEvent.click(row);
    expect(acts.filter((a) => a.case === "setVisible").at(-1)?.value).toMatchObject({ conversationId: first.conversationId });
  });
});
