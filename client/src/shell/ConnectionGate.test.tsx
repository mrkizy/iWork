// 连接类整页与「完全退出 iWork」确认框（步 7d）：文字、诊断行、出口都来自 ConnectionView；出口按 kind 接，
// 没有实现的 kind（查看诊断）不画；整页时盖住侧栏与主区域。
import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { fireEvent, render, screen, within } from "@solidjs/testing-library";
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

// ui-m1 2a 第 17 条的正文（网页只照 patch 画；这里用原文，防止渲染时被截断或改写）
const VERSION_MISMATCH =
  "这个窗口和后台服务不是同一版，暂时连不上。通常是刚装了新版：重新打开 iWork 就会用上新装的那一版；刚装回旧版的话，等后台服务重启好就会自动连上。后台服务仍在运行，重启时被打断的回合可以点「继续」接着做，待你批准的事项都还在。";

describe("连接类整页", () => {
  it("版本不一致：标题、说明来自 patch，不转圈；「重新打开 iWork」发 relaunch；没有实现的出口（查看诊断）不画", () => {
    const acts = mount({
      state: ConnectionState.WAITING,
      title: "后台服务与客户端版本不一致",
      message: VERSION_MISMATCH,
      exitActions: [
        { label: "重新打开 iWork", kind: ConnectionExitKind.RELAUNCH },
        { label: "查看诊断", kind: ConnectionExitKind.SHOW_DIAGNOSTICS },
      ],
    });
    expect(screen.getByRole("heading", { name: "后台服务与客户端版本不一致" })).toBeInTheDocument();
    expect(screen.getByText(VERSION_MISMATCH)).toBeInTheDocument();
    expect(screen.queryByLabelText("正在重新连接"), "要用户动手：不转圈").toBeNull();
    expect(screen.queryByRole("button", { name: "查看诊断" })).toBeNull();
    expect(screen.getAllByRole("button").map((b) => b.textContent), "出口只有一个").toEqual(["重新打开 iWork"]);
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
    expect(screen.getByText("/tmp/iwork.sock").closest("details"), "诊断行收在可展开的详细信息里").not.toBeNull();
    expect(screen.getByText("/tmp/iwork.sock").closest("details")!.open, "默认收起").toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "重试" }));
    expect(acts).toEqual([{ case: "retry", value: {} }]);
  });

  it("诊断页没有诊断行：不画空的详细信息框", () => {
    mount({ state: ConnectionState.DIAGNOSTICS, title: "连不上后台服务", diagnostics: { lines: [], canRetry: true } });
    expect(screen.queryByText("详细信息")).toBeNull();
    expect(document.querySelector(".iw-gate__lines")).toBeNull();
  });

  it("正在切换版本：没有出口，转圈（后台照常重连）", () => {
    mount({ state: ConnectionState.WAITING, title: "正在切换版本" });
    expect(screen.getByLabelText("正在重新连接")).toBeInTheDocument();
  });

  it("环境说明（M1 不支持 Windows）：标题、说明来自 patch；Rust 没给出口就没有按钮", () => {
    mount({ state: ConnectionState.ENVIRONMENT, title: "iWork 暂时不能在这台电脑上运行", message: "M1 暂不支持 Windows" });
    expect(screen.getByRole("heading", { name: "iWork 暂时不能在这台电脑上运行" })).toBeInTheDocument();
    expect(screen.getByText("M1 暂不支持 Windows")).toBeInTheDocument();
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("出口「重试」（Rust 给了的话）发 retry", () => {
    const acts = mount({
      state: ConnectionState.WAITING,
      title: "x",
      exitActions: [{ label: "重试", kind: ConnectionExitKind.RETRY }],
    });
    fireEvent.click(screen.getByRole("button", { name: "重试" }));
    expect(acts).toEqual([{ case: "retry", value: {} }]);
  });

  it("「完全退出 iWork」：Rust 设了 quit_requested 才弹；确认发 confirm=true，取消发 false", () => {
    const acts = mount({ state: ConnectionState.LIVE }, true);
    // ui-m1 2a 第 18 条 f 的原文与按钮
    const dlg = screen.getByRole("alertdialog");
    expect(dlg).toHaveTextContent("后台服务会一起停下，正在进行和排队中的回合都会取消。重新打开 iWork 后可以重新发送。");
    expect(within(dlg).getAllByRole("button").map((b) => b.textContent)).toEqual(["取消", "完全退出"]);
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
