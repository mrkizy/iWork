// 「全部对话」列表页（ui-m1.md 2a 第 5 条；画布未画，按设置页的列表样式）：标题「<项目名> · 全部对话」，顶部按标题
// 筛选，每行圆点、标题、最近一条消息首行、最近活动时间；按最近活动倒序（Rust 给的顺序）；右键同侧栏；滚到底自动
// 加载下一批（每批 50 条，后台分页）。没有对话时「还没有对话」带「+ 新对话」。条目、文字、空态都来自 patch；
// 筛选文字随路由声明给 Rust，由后台筛。
import { createSignal, For, Show } from "solid-js";
import type { ConversationItem } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { ConversationMenu, DeleteConversationDialog } from "../shell/ConversationActions";
import { InlineRename } from "../shell/InlineRename";
import type { Route } from "../shell/route";
import { Badge, Dot } from "../ui/Badge";
import { Button } from "../ui/Button";
import type { MenuAnchor } from "../ui/Menu";
import "./pages.css";

/** 离底部多近就要下一批（像素）。 */
const LOAD_MORE_WITHIN = 200;

export function AllConversationsPage(props: {
  route: Extract<Route, { page: "allConversations" }>;
  go: (r: Route) => void;
}) {
  const host = useHost();
  // 切换项目、改筛选的那一刻 store 里可能还是上一份：只认对得上的
  const view = () => {
    const v = host.view.conversationList;
    return v?.projectId === props.route.projectId ? v : undefined;
  };
  const projectName = () => host.view.sidebar?.projects.find((p) => p.projectId === props.route.projectId)?.name ?? "";
  const [menu, setMenu] = createSignal<{ item: ConversationItem; at: MenuAnchor }>();
  const [renaming, setRenaming] = createSignal<string>();
  const [deleting, setDeleting] = createSignal<ConversationItem>();
  const open = (id: string) => props.go({ page: "conversation", id });
  const onScroll = (e: Event) => {
    const el = e.currentTarget as HTMLElement;
    const v = view();
    if (v?.hasMore && !v.loading && el.scrollHeight - el.scrollTop - el.clientHeight < LOAD_MORE_WITHIN) {
      void host.act({ case: "loadMoreConversations", value: {} });
    }
  };
  return (
    <div class="iw-list-page" onScroll={onScroll}>
      <h1 class="iw-list-page__title">{projectName()} · 全部对话</h1>
      <input
        class="iw-input iw-list-page__filter"
        type="search"
        aria-label="按标题筛选"
        placeholder="按标题筛选"
        value={props.route.filter ?? ""}
        onInput={(e) => props.go({ ...props.route, filter: e.currentTarget.value })}
      />
      <Show when={view()}>
        {(v) => (
          <Show
            when={v().items.length > 0}
            fallback={
              <div class="iw-list-page__empty">
                <p>{v().emptyText}</p>
                <Show when={!props.route.filter}>
                  <Button onClick={() => props.go({ page: "home", projectId: props.route.projectId })}>+ 新对话</Button>
                </Show>
              </div>
            }
          >
            <ul class="iw-list" aria-label="全部对话">
              <For each={v().items}>
                {(c) => (
                  <li>
                    <Show
                      when={renaming() !== c.conversationId}
                      fallback={
                        <div class="iw-list__item">
                          <Dot state={c.dot} label={c.dotLabel} />
                          <InlineRename
                            original={c.title}
                            maxChars={host.view.sidebar?.titleMaxChars ?? Number.MAX_SAFE_INTEGER}
                            onSubmit={(title) =>
                              void host.act({ case: "renameConversation", value: { conversationId: c.conversationId, title } })
                            }
                            onDone={() => setRenaming(undefined)}
                          />
                        </div>
                      }
                    >
                      <button
                        type="button"
                        class="iw-list__item iw-convlist__item"
                        onClick={() => open(c.conversationId)}
                        onContextMenu={(e) => {
                          e.preventDefault();
                          setMenu({ item: c, at: { x: e.clientX, y: e.clientY } });
                        }}
                      >
                        <Dot state={c.dot} label={c.dotLabel} />
                        <span class="iw-convlist__text">
                          <span class="iw-list__title">{c.title}</span>
                          <Show when={c.lastMessageFirstLine}>
                            <span class="iw-convlist__line">{c.lastMessageFirstLine}</span>
                          </Show>
                        </span>
                        <Badge badge={c.unreadBadge} />
                        <span class="iw-list__meta">{c.lastActiveLabel}</span>
                      </button>
                    </Show>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        )}
      </Show>
      <Show when={menu()}>
        {(m) => {
          const item = m().item;
          return (
            <ConversationMenu
              item={item}
              at={m().at}
              onClose={() => setMenu(undefined)}
              onOpen={() => open(item.conversationId)}
              onRename={() => setRenaming(item.conversationId)}
              onDelete={() => setDeleting(item)}
            />
          );
        }}
      </Show>
      <Show when={deleting()}>{(c) => <DeleteConversationDialog item={c()} onClose={() => setDeleting(undefined)} />}</Show>
    </div>
  );
}
