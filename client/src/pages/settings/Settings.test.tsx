// 设置页（步 7d，画布 12）：整页，左栏几节。值、文字、可否点击来自 SettingsView；点了只发操作。
// 对着 settings-rules 录像（运行时就绪）与 empty 录像（没有规则）。记住的规则只在连上时查一次（rule 流不在订阅范围
// 里，T10 补上），录像录不到场景里后加的规则，审批一节在录像的 store 上再叠一帧带规则的设置视图；规则从后台来、
// 撤销真的删掉，见 Rust 侧 tests/settings_rules.rs。
import { create } from "@bufbuild/protobuf";
import { fireEvent, screen, within } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { RuleGroupSchema, ViewPatchSchema } from "../../gen/iwork/v1/view_pb";
import { loadScenario, noFixtures, renderScenario, userActs } from "../../test-fixtures";

const view = (name: string) => loadScenario(name).state.settings!;
const body = () => screen.getByRole("main");

describe.skipIf(noFixtures)("设置页", () => {
  it("整页：没有侧栏，左栏有几节；「‹ 返回」回到进来之前的那一页", () => {
    const { acts } = renderScenario("settings-rules", { page: "home" });
    fireEvent.click(screen.getByRole("button", { name: "设置与运行时" }));
    expect(screen.queryByRole("complementary", { name: "侧栏" }), "设置页盖住侧栏").toBeNull();
    const nav = screen.getByRole("navigation", { name: "设置" });
    for (const s of ["通用", "项目位置", "运行时", "审批", "关于"]) expect(within(nav).getByRole("button", { name: s })).toBeInTheDocument();
    fireEvent.click(within(nav).getByRole("button", { name: "‹ 返回" }));
    expect(acts.filter((a) => a.case === "setVisible").at(-1)?.value).toMatchObject({ home: { projectId: "" } });
  });

  it("通用：两个开关的状态来自 patch，点了发对应操作", () => {
    const { acts } = renderScenario("settings-rules", { page: "settings", section: "general" });
    const v = view("settings-rules");
    const playful = within(body()).getByRole("switch", { name: "轻松元素" });
    expect(playful).toHaveAttribute("aria-checked", String(v.playfulUi));
    fireEvent.click(playful);
    fireEvent.click(within(body()).getByRole("switch", { name: "登录时启动" }));
    expect(userActs(acts)).toEqual([
      { case: "setPlayfulUi", value: { on: !v.playfulUi } },
      { case: "setAutostart", value: { on: !v.autostart } },
    ]);
  });

  it("项目位置：显示 Rust 给的路径；「更换」只说要换，不带路径（Rust 弹框）", () => {
    const { acts } = renderScenario("settings-rules", { page: "settings", section: "projectsRoot" });
    expect(within(body()).getByText(view("settings-rules").projectsRoot)).toBeInTheDocument();
    fireEvent.click(within(body()).getByRole("button", { name: "更换" }));
    expect(userActs(acts)).toEqual([{ case: "pickProjectsRoot", value: {} }]);
  });

  it("运行时：每一行的文字来自 patch；M2 接入的置灰、没有「重新检测」", () => {
    renderScenario("settings-rules", { page: "settings", section: "runtimes" });
    const v = view("settings-rules");
    expect(v.runtimes.some((r) => r.upcoming), "对照：有还没接入的").toBe(true);
    for (const r of v.runtimes) {
      const card = within(body()).getByRole("region", { name: r.displayName });
      expect(card.textContent).toContain(r.stateLabel);
      expect(card.classList.contains("iw-settings__card--upcoming")).toBe(r.upcoming);
      expect(within(card).queryByRole("button", { name: "重新检测" }) !== null).toBe(r.canRedetect);
    }
  });

  it("审批：记住的规则按 Rust 给的分组列出，撤销发带 id 的操作", () => {
    const { acts, store } = renderScenario("settings-rules", { page: "settings", section: "approval" });
    const groups = [
      create(RuleGroupSchema, {
        title: "永久",
        rules: [
          {
            ruleId: "r1",
            label: "执行 `npm test …` 不再问",
            canRevoke: true,
            labelSpans: [
              { text: "执行 " },
              { text: "npm test …", code: true },
              { text: " 不再问" },
            ],
            ownerLabel: "所有项目与助手",
            createdLabel: "14:05",
          },
        ],
      }),
      create(RuleGroupSchema, {
        title: "本项目 · demo",
        rules: [
          {
            ruleId: "r2",
            label: "写 `src/`",
            canRevoke: false,
            labelSpans: [{ text: "写 " }, { text: "src/", code: true }],
            ownerLabel: "项目 demo",
          },
        ],
      }),
    ];
    store.applyPatch(
      create(ViewPatchSchema, {
        changes: [{ change: { case: "settings", value: { view: { ...view("settings-rules"), ruleGroups: groups, rulesEmptyText: "" } } } }],
      }),
    );
    for (const g of groups) {
      const sec = within(body()).getByRole("region", { name: g.title });
      for (const r of g.rules) {
        // 反引号不原样显示：代码那段画成 code，「记在哪 · 何时」一行小字
        expect(sec.textContent).not.toContain("`");
        for (const s of r.labelSpans.filter((x) => x.code)) expect(within(sec).getByText(s.text).tagName).toBe("CODE");
        expect(sec.textContent).toContain([r.ownerLabel, r.createdLabel].filter(Boolean).join(" · "));
      }
      expect(within(sec).queryByRole("button", { name: "撤销" }) !== null, "撤销按钮跟着 can_revoke").toBe(g.rules[0].canRevoke);
    }
    fireEvent.click(within(within(body()).getByRole("region", { name: "永久" })).getByRole("button", { name: "撤销" }));
    expect(userActs(acts)).toEqual([{ case: "revokeRule", value: { ruleId: "r1" } }]);
  });

  it("审批：没有记住的规则时显示 Rust 给的空态", () => {
    renderScenario("empty", { page: "settings", section: "approval" });
    const v = view("empty");
    expect(v.ruleGroups.length, "对照：没有规则").toBe(0);
    expect(within(body()).getByText(v.rulesEmptyText)).toBeInTheDocument();
  });
});
