// 助手主页（步 7c）：对着 assistant-profile 录像渲染。资料栏的文字、配置页的值都来自 patch；配置改一项发一个
// UpdateAssistantField（只带那一项，不带版本、不拼整份配置）；文字框失焦或回车才提交、Esc 还原、没变不发。
// 侧栏：点助手进主页「动态」，右键「配置」停「配置」，删除要确认，内置 iWork 没有删除；「助手 +」开新建弹窗。
import { fireEvent, screen, within } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { loadScenario, noFixtures, renderScenario, userActs } from "../test-fixtures";

const ID = "as-asen";
const DM = "c-dm-asen";
const route = (tab: "activity" | "memory" | "config") => ({ page: "assistant" as const, id: ID, conversationId: DM, tab });
const profile = () => loadScenario("assistant-profile").state.assistant!;
const panel = () => screen.getByRole("complementary", { name: "助手资料" });
const fieldActs = (acts: ReturnType<typeof renderScenario>["acts"]) =>
  userActs(acts).filter((a) => a.case === "updateAssistantField");

describe.skipIf(noFixtures)("助手主页", () => {
  it("对照：录像里有资料与模型菜单", () => {
    const p = profile();
    expect(p.assistantId).toBe(ID);
    expect(p.modelMenu?.models.length).toBeGreaterThan(1);
  });

  it("私聊与资料栏同时在；声明的可见视图带着对话与助手", () => {
    const { acts } = renderScenario("assistant-profile", route("activity"));
    expect(within(panel()).getByText(profile().name)).toBeInTheDocument();
    expect(within(panel()).getByText(profile().activityEmptyText)).toBeInTheDocument();
    expect(document.querySelector(".iw-conv-page"), "左边是私聊").not.toBeNull();
    const v = acts.filter((a) => a.case === "setVisible").at(-1);
    expect(v?.value).toMatchObject({ conversationId: DM, assistantId: ID });
  });

  it("页签：点「记忆」显示记忆的空态说明", () => {
    renderScenario("assistant-profile", route("activity"));
    fireEvent.click(within(panel()).getByRole("tab", { name: "记忆" }));
    expect(within(panel()).getByText(profile().memoryEmptyText)).toBeInTheDocument();
    expect(within(panel()).getByRole("tab", { name: "记忆" })).toHaveAttribute("aria-selected", "true");
  });

  it("配置：名字、提示词、工具开关的值来自 patch", () => {
    renderScenario("assistant-profile", route("config"));
    const p = profile();
    expect(within(panel()).getByLabelText("名字")).toHaveValue(p.name);
    expect(within(panel()).getByLabelText("提示词")).toHaveValue(p.prompt);
    for (const t of p.tools) {
      expect(within(panel()).getByRole("switch", { name: t.label })).toHaveAttribute("aria-checked", String(t.enabled));
    }
  });

  it("头像配色：8 个色块来自 patch，选中的标 aria-checked；点没选中的发令牌名，点选中的不发", () => {
    const { acts } = renderScenario("assistant-profile", route("config"));
    const opts = profile().avatarColors;
    expect(opts.length, "对照：有 8 块").toBe(8);
    const group = within(panel()).getByRole("radiogroup", { name: "头像配色" });
    const radios = within(group).getAllByRole("radio");
    expect(radios.map((r) => r.getAttribute("aria-label"))).toEqual(opts.map((o) => o.label));
    expect(radios.map((r) => r.getAttribute("aria-checked"))).toEqual(opts.map((o) => String(o.selected)));
    const picked = opts.findIndex((o) => o.selected);
    fireEvent.click(radios[picked]);
    expect(fieldActs(acts), "点选中的不发").toEqual([]);
    const other = opts.findIndex((o) => !o.selected);
    fireEvent.click(radios[other]);
    expect(fieldActs(acts)).toEqual([
      { case: "updateAssistantField", value: { assistantId: ID, field: { case: "avatarColor", value: opts[other].token } } },
    ]);
  });

  it("名字：输入时不发，回车提交一次只带名字；Esc 还原不发；没变不发", () => {
    const { acts } = renderScenario("assistant-profile", route("config"));
    const name = within(panel()).getByLabelText("名字") as HTMLInputElement;
    name.focus();
    fireEvent.input(name, { target: { value: "阿森二号" } });
    expect(fieldActs(acts), "输入时不发").toEqual([]);
    fireEvent.keyDown(name, { key: "Enter" });
    fireEvent.blur(name);
    expect(fieldActs(acts)).toEqual([
      { case: "updateAssistantField", value: { assistantId: ID, field: { case: "name", value: "阿森二号" } } },
    ]);
    acts.length = 0;
    name.focus();
    fireEvent.input(name, { target: { value: "随便" } });
    fireEvent.keyDown(name, { key: "Escape" });
    fireEvent.blur(name);
    expect(name.value, "Esc 还原成 patch 的值").toBe(profile().name);
    expect(fieldActs(acts)).toEqual([]);
    name.focus();
    fireEvent.blur(name);
    expect(fieldActs(acts), "没变不发").toEqual([]);
  });

  it("工具开关与模型：一改就发，只带那一项", () => {
    const { acts } = renderScenario("assistant-profile", route("config"));
    const t = profile().tools[1];
    fireEvent.click(within(panel()).getByRole("switch", { name: t.label }));
    const other = profile().modelMenu!.models.find((m) => !m.selected)!;
    fireEvent.change(within(panel()).getByLabelText("模型"), { target: { value: other.value } });
    const sent = fieldActs(acts).map((a) => a.value.field);
    expect(sent[0]).toEqual({ case: "tool", value: { group: t.group, enabled: !t.enabled } });
    expect(sent[1]).toMatchObject({ case: "model", value: { modelId: other.value } });
    expect(sent.length).toBe(2);
  });
});

describe.skipIf(noFixtures)("侧栏的助手入口", () => {
  const row = (name: string) => screen.getAllByRole("button").find((b) => b.classList.contains("iw-assistant") && b.textContent?.includes(name))!;

  it("点助手：进它的主页、停在「动态」", () => {
    const { acts } = renderScenario("assistant-profile", { page: "home" });
    fireEvent.click(row("阿森"));
    expect(within(panel()).getByRole("tab", { name: "动态" })).toHaveAttribute("aria-selected", "true");
    expect(acts.filter((a) => a.case === "setVisible").at(-1)?.value).toMatchObject({ conversationId: DM, assistantId: ID });
  });

  it("右键「配置」停在配置页签；「删除」要确认才发", () => {
    const { acts } = renderScenario("assistant-profile", { page: "home" });
    fireEvent.contextMenu(row("阿森"));
    fireEvent.click(screen.getByRole("menuitem", { name: "配置" }));
    expect(within(panel()).getByRole("tab", { name: "配置" })).toHaveAttribute("aria-selected", "true");
    fireEvent.contextMenu(row("阿森"));
    fireEvent.click(screen.getByRole("menuitem", { name: "删除" }));
    expect(userActs(acts).some((a) => a.case === "deleteAssistant"), "确认前不发").toBe(false);
    // ui-m1 2a 第 18 条 f 的原文与按钮
    const dlg = screen.getByRole("alertdialog");
    expect(dlg).toHaveTextContent("和它的私聊记录会一起删除，不影响代码文件。删除后无法恢复。");
    expect(within(dlg).getAllByRole("button").map((b) => b.textContent)).toEqual(["取消", "删除"]);
    fireEvent.click(within(dlg).getByRole("button", { name: "删除" }));
    expect(userActs(acts).filter((a) => a.case === "deleteAssistant")).toEqual([
      { case: "deleteAssistant", value: { assistantId: ID } },
    ]);
  });

  it("内置 iWork 的右键菜单没有删除", () => {
    renderScenario("assistant-profile", { page: "home" });
    fireEvent.contextMenu(row("iWork"));
    expect(screen.queryByRole("menuitem", { name: "删除" })).toBeNull();
    expect(screen.getByRole("menuitem", { name: "配置" })).toBeInTheDocument();
  });

  it("「助手 +」：新建弹窗，默认选中 Rust 给的运行时与模型；创建整份发出", async () => {
    const { acts, store } = renderScenario("assistant-profile", { page: "home" });
    const menu = store.state.sidebar!.newAssistantMenu!;
    fireEvent.click(screen.getByRole("button", { name: "新建助手" }));
    const dialog = screen.getByRole("dialog", { name: "新建助手" });
    fireEvent.input(within(dialog).getByLabelText("名字"), { target: { value: "小林" } });
    fireEvent.input(within(dialog).getByLabelText("提示词"), { target: { value: "它负责写文档" } });
    fireEvent.click(within(dialog).getByRole("button", { name: "创建" }));
    await Promise.resolve();
    expect(userActs(acts).filter((a) => a.case === "createAssistant")).toEqual([
      {
        case: "createAssistant",
        value: {
          name: "小林",
          prompt: "它负责写文档",
          runtimeId: menu.runtimes.find((r) => r.selected)!.value,
          modelId: menu.models.find((m) => m.selected)!.value,
          effort: menu.efforts.find((e) => e.selected)?.value,
        },
      },
    ]);
  });
});
