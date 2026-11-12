// 「待你处理」页（方案 2.7）：列出全部待处理项，点一项跳到对话里对应的那一行；AGENTS.md 草稿没有对话归属，
// 跳到添加项目页（草稿卡片在那里，Q11：对话页里不显示）。条目、文字、顺序与空态文字都来自 Rust 的 PendingView。
import { For, Show } from "solid-js";
import type { PendingEntry } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Tag } from "../ui/Badge";
import type { Route } from "../shell/route";
import "./pages.css";

/** 一项点开去哪儿：有对话就去对话的那一行，没有（草稿）就去添加项目页。 */
export function pendingTarget(e: PendingEntry): Route {
  return e.conversationId !== undefined
    ? { page: "conversation", id: e.conversationId, rowKey: e.rowKey }
    : { page: "addProject" };
}

export function PendingPage(props: { open: (route: Route) => void }) {
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
                    <button type="button" class="iw-list__item" onClick={() => props.open(pendingTarget(e))}>
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
