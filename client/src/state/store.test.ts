// store 的写入规则（与 crates/iwork-client-core/tests/support/web.rs 的网页替身一致）。
import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { ConversationPatchSchema, RowViewSchema, ViewPatchSchema, type RowView } from "../gen/iwork/v1/view_pb";
import { createViewStore } from "./store";

const msg = (key: string, ordinal: bigint, text: string): RowView =>
  create(RowViewSchema, { key, ordinal, row: { case: "message", value: { messageId: key, text } } });

function conv(p: MessageInitShape<typeof ConversationPatchSchema>) {
  return create(ViewPatchSchema, { changes: [{ change: { case: "conversation", value: p } }] });
}

const text = (s: ReturnType<typeof createViewStore>, key: string) => {
  const r = s.state.conversation?.rows[key]?.row;
  return r?.case === "message" ? r.value.text : undefined;
};

describe("对话行", () => {
  it("按 ordinal 排序；removed_ids 删行；同 key 整行替换", () => {
    const s = createViewStore();
    s.applyPatch(conv({ conversationId: "c", rows: [msg("b", 4n, "二"), msg("a", 2n, "一")] }));
    expect(s.state.conversation?.order).toEqual(["a", "b"]);
    s.applyPatch(conv({ conversationId: "c", rows: [msg("z", -2n, "更早"), msg("a", 2n, "一改")], removedIds: ["b"] }));
    expect(s.state.conversation?.order).toEqual(["z", "a"]);
    expect(text(s, "a")).toBe("一改");
  });

  it("追加按 UTF-8 字节核对 offset：对得上就接，对不上就丢掉等整行重发", () => {
    const s = createViewStore();
    s.applyPatch(conv({ conversationId: "c", rows: [msg("m", 2n, "你好")] }));
    // 「你好」是 6 个字节
    s.applyPatch(conv({ conversationId: "c", appends: [{ messageId: "m", offset: 6n, text: "，世界" }] }));
    expect(text(s, "m")).toBe("你好，世界");
    s.applyPatch(conv({ conversationId: "c", appends: [{ messageId: "m", offset: 2n, text: "错位" }] }));
    expect(text(s, "m")).toBe("你好，世界");
    s.applyPatch(conv({ conversationId: "c", appends: [{ messageId: "m", offset: 15n, text: "！" }] }));
    expect(text(s, "m")).toBe("你好，世界！");
  });

  it("reset 或换了对话先清空", () => {
    const s = createViewStore();
    s.applyPatch(conv({ conversationId: "c", rows: [msg("a", 2n, "一")] }));
    s.applyPatch(conv({ conversationId: "c", reset: true, rows: [msg("b", 4n, "二")] }));
    expect(s.state.conversation?.order).toEqual(["b"]);
    s.applyPatch(conv({ conversationId: "d", rows: [msg("x", 2n, "新")] }));
    expect(s.state.conversation?.id).toBe("d");
    expect(s.state.conversation?.order).toEqual(["x"]);
  });
});

describe("整体视图", () => {
  it("脏了就整份替换；换代 reset 清空但保留传输故障", () => {
    const s = createViewStore();
    s.applyPatch(
      create(ViewPatchSchema, {
        changes: [{ change: { case: "pending", value: { view: { emptyText: "没有需要你处理的事", entries: [] } } } }],
      }),
    );
    expect(s.state.pending?.emptyText).toBe("没有需要你处理的事");
    s.failTransport({ kind: "fallback" });
    s.reset();
    expect(s.state.pending).toBeUndefined();
    expect(s.state.transportFailure).toEqual({ kind: "fallback" });
  });
});
