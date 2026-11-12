// 时间线 reset 后网页重新声明可见窗口（审查 7/F-3）：快照重建时 Rust 作废了自己记的窗口（6/F-3），网页这边
// 有更早的消息时 VirtualRows 不卸载，首尾行也可能和 reset 前完全一样。按「范围变了才报」去重就不会再报，Rust 的
// 窗口一直是空的，锚点当前位置改按最新一行算。reset 时范围去重缓存要失效，布局后重报一次。
// 用真实的 createViewStore + ConversationPage + VirtualRows；jsdom 不排版，行高、视口按假布局给。
import { create } from "@bufbuild/protobuf";
import { render } from "@solidjs/testing-library";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { IntentKind } from "../bridge/intent";
import { IntentAckSchema, RowViewSchema, ViewPatchSchema } from "../gen/iwork/v1/view_pb";
import { HostProvider, type Host } from "../host";
import { createViewStore } from "../state/store";
import { ConversationPage } from "./Conversation";

const ROW_H = 100;
const VIEW_H = 200;
const restores: (() => void)[] = [];

function stub(name: string, desc: PropertyDescriptor) {
  const old = Object.getOwnPropertyDescriptor(HTMLElement.prototype, name);
  Object.defineProperty(HTMLElement.prototype, name, { configurable: true, ...desc });
  restores.push(() =>
    old ? Object.defineProperty(HTMLElement.prototype, name, old) : Reflect.deleteProperty(HTMLElement.prototype, name),
  );
}

beforeEach(() => {
  vi.useFakeTimers();
  const rowIndex = (el: HTMLElement) => [...el.parentElement!.querySelectorAll("[data-vrow]")].indexOf(el);
  stub("offsetTop", {
    get(this: HTMLElement) {
      return this.dataset.vrow === undefined ? 0 : rowIndex(this) * ROW_H;
    },
  });
  stub("offsetHeight", {
    get(this: HTMLElement) {
      return this.dataset.vrow === undefined ? 0 : ROW_H;
    },
  });
  stub("clientHeight", {
    get(this: HTMLElement) {
      return this.classList.contains("iw-vrows") ? VIEW_H : 0;
    },
  });
  stub("scrollHeight", {
    get(this: HTMLElement) {
      return this.classList.contains("iw-vrows") ? this.querySelectorAll("[data-vrow]").length * ROW_H : 0;
    },
  });
  const top = new WeakMap<HTMLElement, number>();
  stub("scrollTop", {
    get(this: HTMLElement) {
      return top.get(this) ?? 0;
    },
    set(this: HTMLElement, v: number) {
      top.set(this, Math.max(0, Math.min(v, this.scrollHeight - this.clientHeight)));
    },
  });
});

afterEach(() => {
  vi.useRealTimers();
  while (restores.length) restores.pop()!();
});

/** 最新一页 10 行、前面还有更早的（hasOlder）：重连快照重建出完全相同的一页。 */
const snapshot = () =>
  create(ViewPatchSchema, {
    changes: [
      {
        change: {
          case: "conversation",
          value: {
            conversationId: "c",
            reset: true,
            window: { hasOlder: true, atTail: true, total: 10, firstIndex: 0 },
            rows: Array.from({ length: 10 }, (_, i) =>
              create(RowViewSchema, {
                key: `m${i}`,
                ordinal: BigInt(i * 2 + 1),
                row: { case: "note", value: { text: `row ${i}` } },
              }),
            ),
          },
        },
      },
    ],
  });

it("快照重建出同一页（首尾行不变）后，网页照样重新声明可见窗口", () => {
  const store = createViewStore();
  const acts: NonNullable<IntentKind>[] = [];
  const host: Host = {
    view: store.state,
    async act(k) {
      if (k) acts.push(k);
      return create(IntentAckSchema);
    },
  };
  store.applyPatch(snapshot());
  const { container } = render(() => (
    <HostProvider value={host}>
      <ConversationPage id="c" openRuntimeDetails={() => {}} />
    </HostProvider>
  ));
  vi.advanceTimersByTime(40);
  const reports = () => acts.filter((a) => a.case === "setVisibleRows");
  expect(reports().length, "对照：首次声明已发出").toBeGreaterThan(0);
  const box = container.querySelector(".iw-vrows");
  const last = reports().at(-1);

  acts.length = 0;
  store.applyPatch(snapshot());
  expect(container.querySelector(".iw-vrows"), "对照：有更早的消息，reset 时组件没卸载").toBe(box);
  vi.advanceTimersByTime(40);
  expect(reports(), "Rust 已作废窗口：首尾行没变也要重报一次").toEqual([last]);
});
