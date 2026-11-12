// 添加项目弹窗（步 7d，画布 07；T09 方案 15.2）：对着 add-project-* 录像渲染。路径、提示、按钮可否点击与文字都来自
// AddProjectView；网页发的意图都不带路径（RD1、SD5）。
import { fireEvent, screen, within } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { UiIntentSchema } from "../gen/iwork/v1/view_pb";
import { loadScenario, noFixtures, renderScenario, userActs } from "../test-fixtures";

const dialog = () => screen.getByRole("dialog", { name: "添加项目" });
const candidate = (name: string) => loadScenario(name).state.addProject!.candidate!;
const open = (name: string) => renderScenario(name, { page: "addProject" });

describe("添加项目一组意图不带路径（方案 15.2 SD5）", () => {
  it("UiIntent 里添加项目一组的消息都没有 path 字段；带路径的旧 add_project（70）已删", () => {
    const group = [
      "pickProjectFolder",
      "initCandidateRepository",
      "setProposeAgentsMdDraft",
      "confirmAddProject",
      "clearAddProject",
      "createProject",
      "startClone",
      "retryClone",
      "pickProjectsParent",
      "pickProjectsRoot",
    ];
    const fields = UiIntentSchema.fields.filter((f) => group.includes(f.localName));
    expect(fields.map((f) => f.localName).sort(), "对照：一组都在").toEqual([...group].sort());
    for (const f of fields) {
      const msg = f.message;
      expect(msg, f.localName).toBeDefined();
      expect(msg!.fields.map((x) => x.name), f.localName).not.toContain("path");
    }
    expect(UiIntentSchema.fields.find((f) => f.number === 70), "70 号保留不用").toBeUndefined();
  });
});

describe.skipIf(noFixtures)("添加项目弹窗", () => {
  it("能加：路径、AGENTS.md 一行、勾选框、主按钮来自 patch；确认、取消勾选各发对应意图", () => {
    const { acts } = open("add-project-ready");
    const c = candidate("add-project-ready");
    const d = dialog();
    expect(d.textContent).toContain(c.pathLabel);
    expect(d.textContent).toContain(`AGENTS.md ${c.agentsMdLabel}`);
    const box = within(d).getByRole("checkbox");
    expect(box).toBeChecked();
    const confirm = within(d).getByRole("button", { name: c.confirmLabel });
    expect(confirm).not.toBeDisabled();
    fireEvent.click(box);
    fireEvent.click(confirm);
    expect(userActs(acts)).toEqual([
      { case: "setProposeAgentsMdDraft", value: { on: false } },
      { case: "confirmAddProject", value: {} },
    ]);
  });

  it("没有版本控制：提示框与「初始化」按钮，主按钮不可点；点初始化发意图", () => {
    const { acts } = open("add-project-unversioned");
    const c = candidate("add-project-unversioned");
    const d = dialog();
    expect(d.textContent).toContain(c.noticeTitle);
    expect(within(d).getByRole("button", { name: "添加并开始对话" })).toBeDisabled();
    fireEvent.click(within(d).getByRole("button", { name: c.initLabel! }));
    expect(userActs(acts)).toEqual([{ case: "initCandidateRepository", value: {} }]);
  });

  it("已是项目：主按钮「打开已有项目」；选的是子目录：一行「将添加整个仓库」", () => {
    open("add-project-known");
    expect(within(dialog()).getByRole("button", { name: "打开已有项目" })).not.toBeDisabled();
  });

  it("选的是子目录：提示框写明添加整个仓库与根目录", () => {
    open("add-project-subdir");
    const c = candidate("add-project-subdir");
    expect(c.noticeTitle, "对照").toBe("将添加整个仓库");
    expect(dialog().textContent).toContain(c.noticeBody);
  });

  it("「更换」只说要选文件夹；关掉弹窗清草稿", () => {
    const { acts } = open("add-project-ready");
    fireEvent.click(within(dialog()).getByRole("button", { name: "更换" }));
    fireEvent.click(within(dialog()).getByRole("button", { name: "关闭" }));
    expect(userActs(acts)).toEqual([
      { case: "pickProjectFolder", value: {} },
      { case: "clearAddProject", value: {} },
    ]);
  });

  it("新建项目：名字原样交给 Rust（后台判是不是合法的目录名）；「更换」位置由 Rust 弹框", () => {
    const { acts } = open("add-project-ready");
    fireEvent.click(within(dialog()).getByRole("tab", { name: /新建项目/ }));
    fireEvent.input(within(dialog()).getByLabelText("项目名"), { target: { value: "settlement-svc" } });
    fireEvent.click(within(dialog()).getByRole("button", { name: "更换" }));
    fireEvent.click(within(dialog()).getByRole("button", { name: "创建并开始对话" }));
    expect(userActs(acts)).toEqual([
      { case: "pickProjectsParent", value: {} },
      { case: "createProject", value: { name: "settlement-svc" } },
    ]);
  });
});
