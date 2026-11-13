// 新建助手弹窗（画布 13-new；审查 g3 1/F-4、UI 审查 F-8、F-9）：等受理结果再关；被拒时弹窗留着、原因显示在弹窗里；
// 在途时「创建」不可点（Rust 给 creating_assistant，本地再挡回包前的连点）；右上角「×」等于取消。
import { create } from "@bufbuild/protobuf";
import { fireEvent, render, screen, within } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import type { IntentKind } from "../bridge/intent";
import { IntentAckSchema, type IntentAck } from "../gen/iwork/v1/view_pb";
import { HostProvider, type Host } from "../host";
import type { ViewState } from "../state/store";
import { loadScenario, noFixtures } from "../test-fixtures";
import { NewAssistantDialog } from "./AssistantNew";

function open(opts: { reply: () => Promise<IntentAck>; view?: (v: ViewState) => ViewState }) {
  const base = loadScenario("assistant-profile").state;
  const sent: { kind: NonNullable<IntentKind>; quiet?: boolean }[] = [];
  let closed = 0;
  const host: Host = {
    view: opts.view ? opts.view(base) : base,
    act(kind, o) {
      if (kind?.case === "createAssistant") sent.push({ kind, quiet: o?.quiet });
      return opts.reply();
    },
  };
  render(() => (
    <HostProvider value={host}>
      <NewAssistantDialog onClose={() => closed++} />
    </HostProvider>
  ));
  const dialog = screen.getByRole("dialog", { name: "新建助手" });
  fireEvent.input(within(dialog).getByLabelText("名字"), { target: { value: "小林" } });
  return { dialog, sent, closed: () => closed, button: () => within(dialog).getByRole("button", { name: "创建" }) };
}

const tick = () => new Promise((r) => setTimeout(r, 0));

describe.skipIf(noFixtures)("新建助手弹窗", () => {
  it("回包前连点两次只发一次；受理了才关", async () => {
    let resolve!: (a: IntentAck) => void;
    const d = open({ reply: () => new Promise((r) => (resolve = r)) });
    fireEvent.click(d.button());
    fireEvent.click(d.button());
    expect(d.sent.length).toBe(1);
    expect(d.button()).toBeDisabled();
    expect(d.closed(), "回包没到：不关").toBe(0);
    resolve(create(IntentAckSchema, {}));
    await tick();
    expect(d.closed()).toBe(1);
  });

  it("被拒：弹窗留着、原因显示在弹窗里（宿主不再弹提示），改了能再点", async () => {
    const d = open({
      reply: async () => create(IntentAckSchema, { error: { code: "invalid_argument", message: "名字不能为空" } }),
    });
    fireEvent.click(d.button());
    await tick();
    expect(d.closed()).toBe(0);
    expect(within(d.dialog).getByRole("alert").textContent).toBe("名字不能为空");
    expect(d.sent[0].quiet, "错误自己显示：叫宿主别弹提示").toBe(true);
    expect(d.button()).not.toBeDisabled();
    fireEvent.click(d.button());
    expect(d.sent.length, "对照：可以重试").toBe(2);
  });

  it("Rust 说正在创建：「创建」不可点、点了也不发", () => {
    const d = open({
      reply: async () => create(IntentAckSchema, {}),
      view: (v) => ({ ...v, sidebar: { ...v.sidebar!, creatingAssistant: true } }),
    });
    expect(d.button()).toBeDisabled();
    fireEvent.click(d.button());
    expect(d.sent.length).toBe(0);
  });

  it("右上角「×」等于取消", () => {
    const d = open({ reply: async () => create(IntentAckSchema, {}) });
    fireEvent.click(within(d.dialog).getByRole("button", { name: "关闭" }));
    expect(d.closed()).toBe(1);
    expect(d.sent.length).toBe(0);
  });
});
