// 连接类整页与「完全退出 iWork」确认框（步 7d）：文字、诊断行、出口都来自 ConnectionView；出口按 kind 接，
// 没有实现的 kind（查看诊断）不画；整页时盖住侧栏与主区域。
import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { fireEvent, render, screen } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import type { IntentKind } from "../bridge/intent";
import {
  ConnectionExitKind,
  ConnectionState,
  ConnectionViewSchema,
  IntentAckSchema,
  ViewPatchSchema,
} from "../gen/iwork/v1/view_pb";
import { HostProvider, type Host } from "../host";
import { noFixtures, renderScenario } from "../test-fixtures";
import { ConnectionPage, QuitConfirm } from "./ConnectionGate";

type ViewInit = MessageInitShape<typeof ConnectionViewSchema>;

function mount(view: ViewInit, quit = false) {
  const acts: NonNullable<IntentKind>[] = [];
  const connection = create(ConnectionViewSchema, { ...view, quitRequested: quit });
  const host: Host = {
    view: { connection },
    async act(k) {
      if (k) acts.push(k);
      return create(IntentAckSchema);
    },
  };
  render(() => (
    <HostProvider value={host}>
      <ConnectionPage view={connection} />
      <QuitConfirm />
    </HostProvider>
  ));
  return acts;
}

describe("连接类整页", () => {
  it("版本不一致：标题来自 patch；「重新打开 iWork」发 relaunch；没有实现的出口（查看诊断）不画", () => {
    const acts = mount({
      state: ConnectionState.WAITING,
      title: "后台服务与客户端版本不一致",
      exitActions: [
        { label: "重新打开 iWork", kind: ConnectionExitKind.RELAUNCH },
        { label: "查看诊断", kind: ConnectionExitKind.SHOW_DIAGNOSTICS },
      ],
    });
    expect(screen.getByRole("heading", { name: "后台服务与客户端版本不一致" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "查看诊断" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "重新打开 iWork" }));
    expect(acts).toEqual([{ case: "relaunch", value: {} }]);
  });

  it("诊断页：原因、几行诊断、提示都来自 patch；可重试时「重试」发 retry", () => {
    const acts = mount({
      state: ConnectionState.DIAGNOSTICS,
      title: "连不上后台服务",
      message: "后台服务没有响应",
      diagnostics: { lines: [{ label: "端点", value: "/tmp/iwork.sock" }], hint: "装回更新的那一版可继续使用", canRetry: true },
    });
    for (const t of ["后台服务没有响应", "端点", "/tmp/iwork.sock", "装回更新的那一版可继续使用"]) {
      expect(screen.getByText(t)).toBeInTheDocument();
    }
    fireEvent.click(screen.getByRole("button", { name: "重试" }));
    expect(acts).toEqual([{ case: "retry", value: {} }]);
  });

  it("环境说明（M1 不支持 Windows）：说明来自 patch，出口「重试」", () => {
    const acts = mount({
      state: ConnectionState.ENVIRONMENT,
      message: "M1 暂不支持 Windows",
      exitActions: [{ label: "重试", kind: ConnectionExitKind.RETRY }],
    });
    expect(screen.getByText("M1 暂不支持 Windows")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "重试" }));
    expect(acts).toEqual([{ case: "retry", value: {} }]);
  });

  it("「完全退出 iWork」：Rust 设了 quit_requested 才弹；确认发 confirm=true，取消发 false", () => {
    const acts = mount({ state: ConnectionState.LIVE }, true);
    fireEvent.click(screen.getByRole("button", { name: "取消" }));
    expect(acts).toEqual([{ case: "quit", value: { confirm: false } }]);
    acts.length = 0;
    fireEvent.click(screen.getByRole("button", { name: "完全退出" }));
    expect(acts).toEqual([{ case: "quit", value: { confirm: true } }]);
  });
});

describe.skipIf(noFixtures)("整页盖住界面", () => {
  it("连接进了诊断页：侧栏与主区域都不显示；回到 LIVE 后恢复", () => {
    const { store } = renderScenario("conversation-basic");
    expect(screen.getByRole("complementary", { name: "侧栏" })).toBeInTheDocument();
    const patch = (connection: ViewInit) =>
      store.applyPatch(
        create(ViewPatchSchema, {
          changes: [{ change: { case: "connection", value: { view: create(ConnectionViewSchema, connection) } } }],
        }),
      );
    patch({ state: ConnectionState.DIAGNOSTICS, title: "连不上后台服务", diagnostics: { lines: [], canRetry: true } });
    expect(screen.queryByRole("complementary", { name: "侧栏" })).toBeNull();
    expect(screen.getByRole("heading", { name: "连不上后台服务" })).toBeInTheDocument();
    patch({ state: ConnectionState.LIVE, commandsEnabled: true });
    expect(screen.getByRole("complementary", { name: "侧栏" })).toBeInTheDocument();
  });
});
