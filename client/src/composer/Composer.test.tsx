// 输入框的草稿归属（审查 2a 1/F-6）：等受理期间换了对话，受理回来只清发出那个对话的草稿。
import { create } from "@bufbuild/protobuf";
import { fireEvent, render, screen } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { describe, expect, it } from "vitest";
import type { IntentKind } from "../bridge/intent";
import { ComposerViewSchema, IntentAckSchema, type IntentAck } from "../gen/iwork/v1/view_pb";
import { HostProvider, type Host } from "../host";
import { createViewStore } from "../state/store";
import { Composer } from "./Composer";
import { getDraft, setDraft } from "./drafts";

function setup() {
  const pending: ((a: IntentAck) => void)[] = [];
  const sent: NonNullable<IntentKind>[] = [];
  const host: Host = {
    view: createViewStore().state,
    act(kind) {
      if (kind) sent.push(kind);
      return new Promise((resolve) => pending.push(resolve));
    },
  };
  const [conv, setConv] = createSignal("c-a");
  render(() => (
    <HostProvider value={host}>
      <Composer conversationId={conv()} draftKey={conv()} composer={create(ComposerViewSchema, { canSend: true })} />
    </HostProvider>
  ));
  const box = () => screen.getByRole("textbox", { name: "消息" }) as HTMLTextAreaElement;
  return { pending, sent, setConv, box };
}

describe("发送后清草稿", () => {
  it("受理前换到别的对话：只清发出的那个对话，另一个对话的草稿留着", async () => {
    setDraft("c-b", "b 的草稿");
    const { pending, sent, setConv, box } = setup();
    fireEvent.input(box(), { target: { value: "发给 a" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    expect(sent.map((k) => k.case)).toEqual(["sendMessage"]);
    setConv("c-b");
    expect(box().value).toBe("b 的草稿");
    pending[0]!(create(IntentAckSchema, {}));
    await Promise.resolve();
    await Promise.resolve();
    expect(box().value, "b 的输入框没被清").toBe("b 的草稿");
    expect(getDraft("c-b")).toBe("b 的草稿");
    expect(getDraft("c-a"), "a 的草稿清掉了").toBe("");
  });

  it("对照：没换对话时受理后清空输入框", async () => {
    const { pending, box } = setup();
    fireEvent.input(box(), { target: { value: "你好" } });
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    pending[0]!(create(IntentAckSchema, {}));
    await Promise.resolve();
    await Promise.resolve();
    expect(box().value).toBe("");
    expect(getDraft("c-a")).toBe("");
  });
});
