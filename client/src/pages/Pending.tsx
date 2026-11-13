// 「待你处理」页（方案 2.7）：列出全部待处理项，点一项跳到对话里对应的那一行（助手私聊里的进助手主页，资料栏沿用
// 上次的页签，ui-m1 2a 第 18 条 b）；AGENTS.md 草稿没有对话归属，
// 跳到添加项目页（草稿卡片在那里，Q11：对话页里不显示）。条目、文字、顺序与空态文字都来自 Rust 的 PendingView。
import { For, Show } from "solid-js";
import type { PendingEntry } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Tag } from "../ui/Badge";
import type { AssistantTab, Route } from "../shell/route";
import "./pages.css";

/** 一项点开去哪儿：助手私聊里的去助手主页（`tab` 是资料栏上次的页签）、滚到那一行；别的对话去对话的那一行；
 *  没有对话（草稿）去添加项目页。 */
export function pendingTarget(e: PendingEntry, tab: AssistantTab): Route {
  if (e.conversationId === undefined) return { page: "addProject" };
  if (e.assistantId !== undefined) {
    return { page: "assistant", id: e.assistantId, conversationId: e.conversationId, tab, rowKey: e.rowKey };
  }
  return { page: "conversation", id: e.conversationId, rowKey: e.rowKey };
}

export function PendingPage(props: { open: (route: Route) => void; assistantTab: () => AssistantTab }) {
  const host = useHost();
  const view = () => host.view.pending;
  return (
    <div class="iw-list-page">
      <h1 class="iw-list-page__title">待你处理</h1>
      <Show when={view()}>
        {(v) => (
          <Show when={v().entries.length > 0} fallback={<p class="iw-list-page__empty">{v().emptyText}</p>}>
            <ul class="iw-list" aria-label="待你处理">
              <For each={v().entries}>
                {(e) => (
                  <li>
                    <button type="button" class="iw-list__item" onClick={() => props.open(pendingTarget(e, props.assistantTab()))}>
                      <Tag tone="warn">{e.kindLabel}</Tag>
                      <span class="iw-list__title">{e.title}</span>
                      <span class="iw-list__meta">{e.contextLabel}</span>
                    </button>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        )}
      </Show>
    </div>
  );
}
