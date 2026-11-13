// 「待你处理」页（步 7b）：四种条目对着 pending-kinds 录像渲染，文字来自 patch；点有对话的一项跳到那个对话的
// 那一行，点草稿跳到添加项目页；空态文字来自 patch。
import { fireEvent, screen } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { create } from "@bufbuild/protobuf";
import { PendingEntrySchema, PendingKind } from "../gen/iwork/v1/view_pb";
import { loadScenario, noFixtures, renderScenario } from "../test-fixtures";
import { pendingTarget } from "./Pending";

describe.skipIf(noFixtures)("待你处理", () => {
  it("四种条目都在；种类、标题、所在对话或项目逐字来自 patch", () => {
    const entries = loadScenario("pending-kinds").state.pending!.entries;
    expect(new Set(entries.map((e) => e.kind))).toEqual(
      new Set([PendingKind.APPROVAL, PendingKind.USER_ACTION, PendingKind.RESUME, PendingKind.AGENTS_MD_DRAFT]),
    );
    renderScenario("pending-kinds", { page: "pending" });
    const list = screen.getByRole("list", { name: "待你处理" });
    const items = [...list.querySelectorAll("li")];
    expect(items.length).toBe(entries.length);
    entries.forEach((e, i) => {
      for (const t of [e.kindLabel, e.title, e.contextLabel]) expect(items[i].textContent).toContain(t);
    });
  });

  it("点一项：有对话的去那个对话的那一行；草稿去添加项目页", () => {
    const entries = loadScenario("pending-kinds").state.pending!.entries;
    for (const e of entries) {
      expect(e.assistantId, "对照：这个录像里没有私聊").toBeUndefined();
      const target = pendingTarget(e, "config");
      if (e.kind === PendingKind.AGENTS_MD_DRAFT) {
        expect(target).toEqual({ page: "addProject" });
      } else {
        expect(e.conversationId, "对照：除草稿外都有对话归属").toBeDefined();
        expect(target).toEqual({ page: "conversation", id: e.conversationId, rowKey: e.rowKey });
      }
    }
  });

  it("助手私聊里的一项：进助手主页（私聊 + 资料栏），沿用上次的页签，滚到那一行（ui-m1 2a 第 18 条 b）", () => {
    const e = create(PendingEntrySchema, {
      itemId: "ap1",
      kind: PendingKind.APPROVAL,
      conversationId: "c-dm-asen",
      rowKey: "ap1",
      assistantId: "as-asen",
    });
    expect(pendingTarget(e, "config")).toEqual({
      page: "assistant",
      id: "as-asen",
      conversationId: "c-dm-asen",
      tab: "config",
      rowKey: "ap1",
    });
  });

  it("点了以后真的换页：打开那个对话、声明对话可见", () => {
    const { acts } = renderScenario("pending-kinds", { page: "pending" });
    const approval = loadScenario("pending-kinds").state.pending!.entries.find((e) => e.kind === PendingKind.APPROVAL)!;
    fireEvent.click(screen.getByRole("button", { name: new RegExp(approval.title) }));
    const visible = acts.filter((a) => a.case === "setVisible").at(-1);
    expect(visible?.value).toMatchObject({ conversationId: approval.conversationId, pending: false });
  });

  it("点一项：先声明对话可见，紧跟着请 Rust 定位到那张卡片（不在时间线里由 Rust 取那一段，g3b 1/F-5）", () => {
    const { acts } = renderScenario("pending-kinds", { page: "pending" });
    const approval = loadScenario("pending-kinds").state.pending!.entries.find((e) => e.kind === PendingKind.APPROVAL)!;
    acts.length = 0;
    fireEvent.click(screen.getByRole("button", { name: new RegExp(approval.title) }));
    const at = acts.findIndex((a) => a.case === "setVisible");
    expect(acts[at]?.value).toMatchObject({ conversationId: approval.conversationId });
    expect(acts[at + 1]).toEqual({
      case: "jumpToRow",
      value: { conversationId: approval.conversationId, rowKey: approval.rowKey },
    });
  });

  it("对照：直接打开对话（路由不带行）不发定位", () => {
    const conv = loadScenario("pending-kinds").state.pending!.entries.find((e) => e.conversationId)!.conversationId!;
    const { acts } = renderScenario("pending-kinds", { page: "conversation", id: conv });
    expect(acts.some((a) => a.case === "setVisible")).toBe(true);
    expect(acts.some((a) => a.case === "jumpToRow")).toBe(false);
  });

  it("没有待处理项：显示 Rust 给的空态文字", () => {
    const empty = loadScenario("empty").state.pending!;
    expect(empty.entries.length, "对照：empty 场景没有待处理项").toBe(0);
    expect(empty.emptyText, "对照：空态文字非空").not.toBe("");
    renderScenario("empty", { page: "pending" });
    expect(screen.getByText(empty.emptyText)).toBeInTheDocument();
  });
});
