// 对话页（画布 04、10、15）：标题栏（标题、私聊的项目选择器、本对话用量）、提示条、时间线（虚拟行）、回到最新、输入框。
// 还没有消息的对话显示和首页一样的「要做点什么？」。整页说明（如「数据不完整」）设了就不显示行。
// 拖文件进窗口时整块对话区盖一层虚线框（文字由 Rust 给，没在拖时不设）。
// 消息锚点（2a 第 11 条）：内容超出一屏才显示。宽形态（主区域 ≥ 600px）是左边距里的导轨，窄形态是标题栏最右端的按钮
// 加弹出面板（形态按主区域宽度判断、有回差，见 anchor/layout.ts）；⌥↑ / ⌥↓ 两种形态都不经滚轮直接逐处跳。
import { createSignal, onCleanup, onMount, Show } from "solid-js";
import { AnchorButton } from "../anchor/AnchorButton";
import { AnchorRail } from "../anchor/AnchorRail";
import { nextNarrow } from "../anchor/layout";
import { Composer } from "../composer/Composer";
import { useHost } from "../host";
import { RowView } from "../rows/RowView";
import { Button } from "../ui/Button";
import { Icon } from "../ui/Icon";
import { VirtualRows } from "../ui/VirtualRows";
import { ProjectSelector } from "./ProjectSelector";
import "./pages.css";

export function ConversationPage(props: { id: string; rowKey?: string; openRuntimeDetails: () => void }) {
  const host = useHost();
  // 切换对话的那一刻，store 里可能还是上一个对话：只认 id 对得上的
  const conv = () => (host.view.conversation?.id === props.id ? host.view.conversation : undefined);
  const header = () => conv()?.header;
  const win = () => conv()?.window;
  const empty = () => (conv()?.order.length ?? 0) === 0 && !win()?.hasOlder;
  const [overflow, setOverflow] = createSignal(false);
  const [narrow, setNarrow] = createSignal(false);
  let page!: HTMLDivElement;
  onMount(() => {
    const observer = new ResizeObserver((entries) => {
      const width = entries[0]?.contentRect.width;
      if (width !== undefined && width > 0) setNarrow((was) => nextNarrow(was, width));
    });
    observer.observe(page);
    onCleanup(() => observer.disconnect());
  });
  const anchors = () => conv()?.anchors;
  /** 有锚点、内容又超出一屏时才给锚点（两种形态同一个条件）。 */
  const shownAnchors = () => (overflow() && (anchors()?.items.length ?? 0) > 0 ? anchors() : undefined);
  const jumpTo = (key: string) => void host.act({ case: "jumpToAnchor", value: { conversationId: props.id, key } });
  const browseAt = (key: string) => void host.act({ case: "anchorBrowse", value: { conversationId: props.id, key } });
  const scrollTo = () => {
    const s = conv()?.scrollTo;
    return s && { rowKey: s.rowKey, token: s.token, highlightMs: s.highlightMs };
  };
  // ⌥↑ / ⌥↓：焦点在输入框里时是系统的光标移动，不抢
  const onKey = (e: KeyboardEvent) => {
    if (!e.altKey || (e.key !== "ArrowUp" && e.key !== "ArrowDown")) return;
    const t = e.target as HTMLElement | null;
    if (t && (t.isContentEditable || t.tagName === "TEXTAREA" || t.tagName === "INPUT")) return;
    e.preventDefault();
    void host.act({ case: "jumpRelative", value: { conversationId: props.id, delta: e.key === "ArrowUp" ? -1 : 1 } });
  };
  onMount(() => document.addEventListener("keydown", onKey));
  onCleanup(() => document.removeEventListener("keydown", onKey));
  return (
    <div ref={page} class={`iw-conv-page${narrow() ? "" : " iw-conv-page--wide"}`}>
      <div class="iw-conv-header">
        <span class="iw-conv-header__title">{header()?.title}</span>
        <Show when={header()}>{(h) => <ProjectSelector conversationId={props.id} header={h()} />}</Show>
        <Show when={header()?.usageText}>
          {(u) => (
            <span class="iw-conv-header__usage" data-tip={header()?.usageTooltip}>
              {u()}
            </span>
          )}
        </Show>
        <Show when={narrow() && shownAnchors()}>
          {(a) => <AnchorButton anchors={a()} currentKey={conv()?.anchorKey} onJump={jumpTo} onBrowse={browseAt} />}
        </Show>
      </div>
      <Show when={header()?.notice}>
        {(n) => (
          <div class="iw-notice" role="status">
            {n()}
          </div>
        )}
      </Show>
      <Show
        when={conv()?.blockingNotice === undefined}
        fallback={<div class="iw-blocking">{conv()?.blockingNotice}</div>}
      >
        <Show
          when={!empty()}
          fallback={
            <div class="iw-hero">
              <h1 class="iw-hero__title">要做点什么？</h1>
            </div>
          }
        >
          <div class="iw-timeline">
            <VirtualRows
              keys={conv()?.order ?? []}
              ordinalOf={(k) => conv()?.rows[k]?.ordinal ?? 0n}
              firstIndex={win()?.firstIndex ?? 0}
              total={win()?.total ?? 0}
              atTail={win()?.atTail ?? true}
              hasOlder={win()?.hasOlder ?? false}
              resetToken={conv()?.resets ?? 0}
              scrollTo={scrollTo()}
              onOverflow={setOverflow}
              renderRow={(k) => (
                <Show when={conv()?.rows[k]}>{(r) => <RowView row={r()} />}</Show>
              )}
              onVisible={(first, last, byUser) =>
                void host.act({
                  case: "setVisibleRows",
                  value: { conversationId: props.id, firstOrdinal: first, lastOrdinal: last, byUser },
                })
              }
              onReachOldest={() => void host.act({ case: "loadOlder", value: { conversationId: props.id } })}
            />
            <Show when={!narrow() && shownAnchors()}>
              {(a) => <AnchorRail anchors={a()} currentKey={conv()?.anchorKey} onJump={jumpTo} onBrowse={browseAt} />}
            </Show>
            <Show when={(win()?.newBelow ?? 0) > 0}>
              <Button
                class="iw-jump"
                onClick={() => void host.act({ case: "jumpToLatest", value: { conversationId: props.id } })}
              >
                <Icon name="arrowDown" size={14} />
                回到最新（{win()?.newBelow}）
              </Button>
            </Show>
          </div>
        </Show>
      </Show>
      <Show when={header()?.dropHint}>
        {(hint) => (
          <div class="iw-drop" role="status">
            <Icon name="paperclip" />
            {hint()}
          </div>
        )}
      </Show>
      <Composer
        target={{ kind: "conversation", id: props.id }}
        composer={conv()?.composer}
        draftKey={props.id}
        projectId={header()?.projectId || undefined}
        openRuntimeDetails={props.openRuntimeDetails}
      />
    </div>
  );
}
