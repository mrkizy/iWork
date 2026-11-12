// 工具栏药丸防换行（ui-m1 2a 第 12b 条，审查 7/F-1）：模型药丸的文字放在可省略的那一层，悬停给完整文字。
// 样式规则（nowrap、哪个可收窄、上限 240、省略号）在 client/tests/composer-pills.test.ts；宽度上的效果由 12b 的真机验收看。
import { create } from "@bufbuild/protobuf";
import { render, screen } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { ApprovalModeViewSchema, ComposerViewSchema, IntentAckSchema } from "../gen/iwork/v1/view_pb";
import { HostProvider, type Host } from "../host";
import { createViewStore } from "../state/store";
import { Composer } from "./Composer";

describe("模型药丸", () => {
  it("长模型 ID：文字放在可省略的那一层，悬停提示给完整文字；审批药丸不带收窄的类", () => {
    const label = "Claude Code · my-very-long-custom-model-id-2026-11-12-preview";
    const host: Host = {
      view: createViewStore().state,
      act: async () => create(IntentAckSchema),
    };
    render(() => (
      <HostProvider value={host}>
        <Composer
          target={{ kind: "conversation", id: "c" }}
          draftKey="c"
          composer={create(ComposerViewSchema, {
            canSend: true,
            runtimeModelLabel: label,
            approvalMode: create(ApprovalModeViewSchema, { label: "全开", fullAccess: true, overridden: true }),
          })}
        />
      </HostProvider>
    ));
    const model = screen.getByRole("button", { name: new RegExp(label) });
    expect(model).toHaveClass("iw-pill--model");
    expect(model).toHaveAttribute("data-tip", label);
    expect(model.querySelector(".iw-pill__text")?.textContent).toBe(label);
    const mode = screen.getByRole("button", { name: "审批方式：全开" });
    expect(mode).not.toHaveClass("iw-pill--model");
  });
});
