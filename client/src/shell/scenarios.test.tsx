// 预览场景确实录到了要审的状态（审查 2a F-13）：每个场景对着全量帧渲染，看画面上有没有那一处。
// 文字都来自 Rust 的视图模型；这里只证明录像里有，不重复验派生规则（那些在 client-core 的测试里）。
import { create } from "@bufbuild/protobuf";
import { fireEvent, screen, within } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { ConnectionState, ConnectionViewSchema, ViewPatchSchema } from "../gen/iwork/v1/view_pb";
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
    // 2a 第 7、8 条的原文；版本号来自假后台的运行时（2.1.322）
    ["composer-path-missing", "找不到项目文件夹，重新定位后才能继续", "重新定位"],
    ["composer-verifying", "正在检查 Claude Code 2.1.322 能否安全使用…", undefined],
    ["composer-incompatible", "Claude Code 2.1.322 没有通过安全检查，暂不能使用", "查看详情"],
    ["composer-not-installed", "没有找到", "查看详情"],
    ["composer-not-logged-in", "还没登录", "查看详情"],
  ])("%s：输入框上方的说明", (name, reason, action) => {
    renderScenario(name);
    const block = composerBlock() as HTMLElement;
    expect(block).toHaveTextContent(reason);
    if (action) expect(within(block).getByRole("button", { name: action })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "发送" })).toBeDisabled();
  });

  // 后台停了、连接被拒：照方案 2.3 的表进诊断页（整页，退避重拨，连上后自动消失）。输入框上方的「正在重新连接」
  // 在连接断开（顶部细条、不整页）时才看得到：在录像的 store 上把连接改成 RECONNECTING 再看。
  it("composer-offline：连不上时整页诊断；断开重连时输入框上方说明、发送不可点", () => {
    const { store } = renderScenario("composer-offline");
    const c = store.state.connection!;
    expect(c.state, "对照：录像停在诊断页").toBe(ConnectionState.DIAGNOSTICS);
    expect(screen.getByRole("heading", { name: c.title })).toBeInTheDocument();
    expect(composerBlock(), "诊断页盖住了输入框").toBeNull();
    store.applyPatch(
      create(ViewPatchSchema, {
        changes: [
          {
            change: {
              case: "connection",
              value: { view: create(ConnectionViewSchema, { state: ConnectionState.RECONNECTING, title: "正在重新连接" }) },
            },
          },
        ],
      }),
    );
    expect(composerBlock()).toHaveTextContent("正在重新连接，连上后就能发送");
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
    // 划线名、标签、「重新定位」在同一行（2a 第 7 条）；名字放不下时缩，悬停看全名
    const row = nav.querySelector(".iw-project--missing .iw-project__row") as HTMLElement;
    expect(row.querySelector(".iw-project__name")?.getAttribute("title")).toBe("legacy-app");
    expect(row.querySelector(".iw-project__missing-tag")).toHaveTextContent("找不到路径");
    expect(row.querySelector(".iw-project__relocate")).toHaveTextContent("重新定位");
    fireEvent.contextMenu(within(nav).getByText("legacy-app"));
    expect(await screen.findByRole("menu")).toBeInTheDocument();
  });

  it("tool-output：点开工具行看到输出", () => {
    renderScenario("tool-output");
    const line = screen.getByRole("button", { expanded: false, name: /Bash/ });
    fireEvent.click(line);
    expect(document.querySelector(".iw-tool__output")).toHaveTextContent("test result: ok. 12 passed");
  });

  it("switch-rows：项目、模型、档位切换，打开前（重建得出）与打开后（当次说明）各一轮", () => {
    renderScenario("switch-rows");
    const rows = [...document.querySelectorAll(".iw-divider, .iw-note")].map((el) => [
      el.classList.contains("iw-divider") ? "分隔线" : "说明行",
      el.textContent,
    ]);
    expect(rows).toEqual([
      // 打开前那一轮：当次说明随快照重建丢掉，留下重建得出的分隔线与模型切换行（档位没有重建得出的行）
      ["分隔线", "切换到 web"],
      ["说明行", "已切换为 fake-small · medium"],
      // 打开后那一轮：实时的当次说明，同类的重建行不再重复贴
      ["分隔线", "切换到 demo"],
      ["说明行", "已切换为 fake-large · medium"],
      ["说明行", "审批方式已切换为手动"],
    ]);
  });

  it("characters-sorry-preview-only：三个形象出错时都是 sorry，不浮动", () => {
    renderScenario("characters-sorry-preview-only");
    const nav = screen.getByRole("complementary", { name: "侧栏" });
    for (const [name, character] of [["阿森", "asen"], ["林岚", "linlan"], ["老周", "laozhou"]]) {
      const el = within(nav).getByRole("img", { name });
      expect(el.dataset.character, name).toBe(character);
      expect(el.dataset.mood, name).toBe("sorry");
      expect(el.querySelector(".iw-n svg")?.getAttribute("class"), name).toContain(`iw-char--${character} iw-mood--sorry`);
      expect(el.classList.contains("iw-float"), name).toBe(false);
    }
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
