// 时间线里的卡片（步 7b）：对着录像渲染，可见文字、按钮在不在与可不可点、发出的操作都来自 patch。
// 安全类卡片（审批、亲手执行）里没有任何轻松元素；轻松元素关掉时回合结束行没有印章节点（验收 7b）。
import { create } from "@bufbuild/protobuf";
import { fireEvent, render, screen, within } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import type { IntentKind } from "../bridge/intent";
import { IntentAckSchema, RowViewSchema, type RowView as Row } from "../gen/iwork/v1/view_pb";
import { HostProvider, type Host } from "../host";
import type { ViewState } from "../state/store";
import { loadScenario, noFixtures } from "../test-fixtures";
import { RowView } from "./RowView";

/** 录像里对话的行，按时间线顺序。 */
function rowsOf(scenario: string) {
  const store = loadScenario(scenario);
  const c = store.state.conversation;
  if (c === undefined) throw new Error(`${scenario} 没有打开的对话`);
  return { rows: c.order.map((k) => c.rows[k]), view: store.state };
}

/** 单独渲染一行；返回发出的操作。 */
function renderRow(row: Row, view: ViewState = {}) {
  const acts: NonNullable<IntentKind>[] = [];
  const host: Host = {
    view,
    async act(k) {
      if (k) acts.push(k);
      return create(IntentAckSchema);
    },
  };
  const utils = render(() => (
    <HostProvider value={host}>
      <RowView row={row} />
    </HostProvider>
  ));
  return { ...utils, acts };
}

const button = (root: HTMLElement, name: string | RegExp) => within(root).queryByRole("button", { name });

describe.skipIf(noFixtures)("审批卡片", () => {
  const approvals = () =>
    rowsOf("approval-states").rows.flatMap((r) => (r.row.case === "approval" ? [r.row.value] : []));

  it("对照：录像里各种状态都有（待定、已决定、已作废、折叠、必问）", () => {
    const a = approvals();
    expect(a.some((x) => !x.stateLabel && x.rememberOptions.length > 0), "待定且可记住").toBe(true);
    expect(a.some((x) => !x.stateLabel && x.rememberOptions.length === 0), "必问").toBe(true);
    expect(a.some((x) => x.muted), "已作废").toBe(true);
    expect(a.some((x) => x.folded), "切档放行").toBe(true);
    expect(a.some((x) => x.stateLabel && !x.muted && !x.folded), "已决定").toBe(true);
  });

  it("每一张：标题、标签、命令、状态文字逐字来自 patch；待定才有按钮，按钮可点与否跟 can_decide 走", () => {
    for (const a of approvals()) {
      const row = create(RowViewSchema, { key: a.approvalId, row: { case: "approval", value: a } });
      const { container, unmount } = renderRow(row);
      const text = container.textContent ?? "";
      expect(text, a.approvalId).toContain(a.code);
      if (a.folded) {
        expect(text).toContain(a.stateLabel);
        expect(button(container, "允许"), "折叠形态没有按钮").toBeNull();
      } else {
        expect(text).toContain(a.title);
        for (const t of a.tags) expect(text).toContain(t);
        for (const n of a.notes) expect(text).toContain(n);
        if (a.reviewLine) expect(text).toContain(a.reviewLine);
        if (a.stateLabel) {
          expect(text).toContain(a.stateLabel);
          expect(button(container, "允许"), `${a.approvalId} 已有结果，没有按钮`).toBeNull();
        } else {
          expect(button(container, "允许")?.hasAttribute("disabled")).toBe(!a.canDecide);
          expect(button(container, "拒绝")?.hasAttribute("disabled")).toBe(!a.canDecide);
          expect(button(container, /记住/) !== null, `${a.approvalId}：有记住选项才有下拉`).toBe(a.rememberOptions.length > 0);
        }
      }
      expect(container.querySelector(".iw-card--muted") !== null, "已作废置灰").toBe(a.muted);
      expect(container.querySelector(".iw-stamp, .iw-ch"), "安全类卡片里没有轻松元素").toBeNull();
      unmount();
    }
  });

  it("允许带上选中的记住范围；拒绝不带记住", () => {
    const a = approvals().find((x) => !x.stateLabel && x.rememberOptions.length > 1 && x.canDecide);
    expect(a, "对照：有一张可决定、可记住的").toBeDefined();
    const row = create(RowViewSchema, { key: a!.approvalId, row: { case: "approval", value: a! } });
    const { container, acts } = renderRow(row);
    // 默认是「仅这一次」
    expect(button(container, /记住/)?.textContent).toContain("仅这一次");
    fireEvent.click(button(container, /记住/)!);
    const last = a!.rememberOptions.at(-1)!;
    fireEvent.click(screen.getByRole("menuitem", { name: last.label }));
    expect(button(container, /记住/)?.textContent).toContain(last.label);
    fireEvent.click(button(container, "允许")!);
    fireEvent.click(button(container, "拒绝")!);
    expect(acts).toEqual([
      { case: "decideApproval", value: { approvalId: a!.approvalId, allow: true, remember: last.value } },
      { case: "decideApproval", value: { approvalId: a!.approvalId, allow: false, remember: 0 } },
    ]);
  });

  it("断线（can_decide 为假）：按钮还在、都不可点", () => {
    const a = approvals().find((x) => !x.stateLabel && x.rememberOptions.length > 0)!;
    const row = create(RowViewSchema, { key: "x", row: { case: "approval", value: { ...a, canDecide: false } } });
    const { container } = renderRow(row);
    for (const n of ["允许", "拒绝", /记住/]) expect(button(container, n)).toHaveAttribute("disabled");
  });
});

describe.skipIf(noFixtures)("亲手执行卡片", () => {
  const cards = () =>
    rowsOf("user-action-states").rows.flatMap((r) => (r.row.case === "userAction" ? [r.row.value] : []));

  it("对照：录像里七种状态都在", () => {
    expect(cards().length).toBe(7);
  });

  it("每一张：理由、命令、状态与退出码来自 patch；各个按钮在不在、可不可点跟着 can_* 走", () => {
    for (const c of cards()) {
      const row = create(RowViewSchema, { key: c.cardId, row: { case: "userAction", value: c } });
      const { container, unmount } = renderRow(row);
      const text = container.textContent ?? "";
      expect(text, c.cardId).toContain(c.reason);
      expect(text).toContain(c.command);
      if (c.stateLabel) expect(text).toContain(c.stateLabel);
      if (c.exitLabel) expect(text).toContain(c.exitLabel);
      if (c.notice) expect(text).toContain(c.notice);
      if (!c.stateLabel) {
        expect(button(container, "执行")?.hasAttribute("disabled")).toBe(!c.canExecute);
        expect(button(container, "忽略")?.hasAttribute("disabled")).toBe(!c.canDismiss);
      } else {
        expect(button(container, "执行"), `${c.cardId}：有结果就没有「执行」`).toBeNull();
      }
      expect(button(container, "停止") !== null, c.cardId).toBe(c.canStop);
      expect(button(container, "我已核对") !== null, c.cardId).toBe(c.canConfirmEnded);
      expect(container.querySelector(".iw-tag--ok") !== null, `${c.cardId}：成功色`).toBe(c.succeeded);
      expect(container.querySelector(".iw-card--muted") !== null).toBe(c.muted);
      expect(container.querySelector(".iw-stamp, .iw-ch"), "安全类卡片里没有轻松元素").toBeNull();
      unmount();
    }
  });

  it("对照：成功只有一张（已执行、退出码 0）", () => {
    expect(cards().filter((c) => c.succeeded).map((c) => c.cardId)).toEqual(["ua-done"]);
  });

  it("执行、忽略、停止、我已核对各发一个带卡片 id 的操作", () => {
    const byId = (id: string) => cards().find((c) => c.cardId === id)!;
    const click = (id: string, name: string) => {
      const row = create(RowViewSchema, { key: id, row: { case: "userAction", value: byId(id) } });
      const { container, acts, unmount } = renderRow(row);
      fireEvent.click(button(container, name)!);
      unmount();
      return acts;
    };
    expect(click("ua-pending", "执行")).toEqual([{ case: "executeUserAction", value: { cardId: "ua-pending" } }]);
    expect(click("ua-pending", "忽略")).toEqual([{ case: "dismissUserAction", value: { cardId: "ua-pending" } }]);
    expect(click("ua-executing", "停止")).toEqual([{ case: "stopUserAction", value: { cardId: "ua-executing" } }]);
    expect(click("ua-unknown", "我已核对")).toEqual([
      { case: "confirmUserActionEnded", value: { cardId: "ua-unknown" } },
    ]);
  });
});

describe.skipIf(noFixtures)("继续 / 放弃卡片", () => {
  it("原因来自 patch；继续、放弃各发带回合 id 的操作", () => {
    const r = rowsOf("resume-card").rows.find((x) => x.row.case === "resume");
    expect(r, "对照：录像里有继续卡片").toBeDefined();
    const v = r!.row.value as { reason: string; turnId: string };
    const { container, acts } = renderRow(r!);
    expect(container.textContent).toContain(v.reason);
    fireEvent.click(button(container, "继续")!);
    fireEvent.click(button(container, "放弃")!);
    expect(acts).toEqual([
      { case: "resumeTurn", value: { turnId: v.turnId } },
      { case: "abandonTurn", value: { turnId: v.turnId } },
    ]);
  });
});

describe.skipIf(noFixtures)("回合结束行", () => {
  const ends = () => {
    const { rows } = rowsOf("turn-end");
    return rows.filter((r) => r.row.case === "turnEnd");
  };

  it("对照：录像里有盖章的、失败的、取消的", () => {
    const v = ends().map((r) => r.row.value as { celebrate: boolean; failed: boolean; outcomeLabel: string });
    expect(v.some((x) => x.celebrate)).toBe(true);
    expect(v.some((x) => x.failed)).toBe(true);
    expect(v.some((x) => !x.failed && x.outcomeLabel)).toBe(true);
  });

  it("摘要与原因来自 patch；印章只在 celebrate 时出现", () => {
    for (const r of ends()) {
      const v = r.row.value as { celebrate: boolean; summaryLine: string; outcomeLabel: string };
      const { container, unmount } = renderRow(r, { settings: { playfulUi: true } as ViewState["settings"] });
      if (v.summaryLine) expect(container.textContent).toContain(v.summaryLine);
      if (v.outcomeLabel) expect(container.textContent).toContain(v.outcomeLabel);
      expect(container.querySelector(".iw-stamp") !== null, r.key).toBe(v.celebrate);
      unmount();
    }
  });

  it("轻松元素关掉：没有印章节点（不只是藏起来）", () => {
    const stamped = ends().find((r) => (r.row.value as { celebrate: boolean }).celebrate)!;
    const { container } = renderRow(stamped, { settings: { playfulUi: false } as ViewState["settings"] });
    expect(container.textContent, "对照：摘要还在").toContain("改了");
    expect(container.querySelector(".iw-stamp")).toBeNull();
  });
});

describe("说明行", () => {
  it("切到全开的那一行按字段标赭红，不看文字", () => {
    const note = (fullAccess: boolean) =>
      create(RowViewSchema, { key: "n", row: { case: "note", value: { text: "同一句话", fullAccess } } });
    const full = renderRow(note(true));
    expect(full.container.querySelector(".iw-note--full")).not.toBeNull();
    full.unmount();
    const plain = renderRow(note(false));
    expect(plain.container.textContent).toContain("同一句话");
    expect(plain.container.querySelector(".iw-note--full")).toBeNull();
  });
});
