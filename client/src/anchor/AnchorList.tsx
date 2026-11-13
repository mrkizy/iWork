// 消息锚点的翻看状态与刻度列表（ui-m1.md 2a 第 11 条「展开态」，列表式，画布 19、20）。宽形态的导轨（AnchorRail）
// 与窄形态的标题栏按钮面板（AnchorButton）共用这一份：列表、悬停预览、点击跳转、滚动与续载规则完全相同，只是怎么展开、
// 怎么收起不同。哪些算锚点、文字、当前位置、前后还有没有、正在跳哪一处都来自 patch；这里只有展示状态：正在预览哪一处
// （按 key 记，新列表到了按 key 对回去）、列表滚到哪、Home / End 在等哪一头。
import { createEffect, createMemo, createSignal, For, on, onCleanup, Show } from "solid-js";
import { AnchorKind, type AnchorItem, type AnchorsView } from "../gen/iwork/v1/view_pb";
import {
  browseTarget,
  centeredScrollTop,
  distanceClass,
  fadeSides,
  LIST_PAD,
  LOADING_DOT_DELAY_MS,
  PITCH,
  restoredScrollTop,
  visibleRange,
} from "./list";

/** 列表外框的边框宽，与 AnchorRail.css 的 .iw-anchor__frame 一致。 */
const FRAME_BORDER = 1;

export const KIND_CLASS: Record<AnchorKind, string> = {
  [AnchorKind.UNSPECIFIED]: "",
  [AnchorKind.USER]: "iw-tick--user",
  [AnchorKind.WAITING]: "iw-tick--waiting",
  [AnchorKind.FAILED]: "iw-tick--failed",
};

export const anchorLabel = (a: AnchorItem) => `${a.kindLabel} ${a.timeLabel} ${a.preview}`;

export interface AnchorSource {
  anchors: AnchorsView;
  currentKey?: string;
  /** 列表滚到离已载入的一端不到 8 条、那一侧还有：请 Rust 以这一条为中心重取。 */
  onBrowse: (key: string) => void;
  /** Home / End 要的那一头不在已载入的窗口里：请 Rust 以整个对话最早（假）/ 最新（真）的一头为中心重取。 */
  onBrowseEdge: (latest: boolean) => void;
}

/** 翻看状态：展开时 `reset` 回到当前位置；`preview` 换预览处；`edge` 是 Home / End。 */
export function createAnchorBrowse(props: AnchorSource) {
  const [previewKey, setPreviewKey] = createSignal<string>();
  // Home / End 请了重取、在等新列表的那一头
  const [pendingEdge, setPendingEdge] = createSignal<"start" | "end">();

  const items = () => props.anchors.items;
  const indexOf = (key: string | undefined) => (key === undefined ? -1 : items().findIndex((a) => a.key === key));
  // 预览处：展开时从当前位置开始；新列表到了按 key 对回去，对不上就回到当前位置
  const index = createMemo(() => {
    const i = indexOf(previewKey());
    if (i >= 0) return i;
    const c = indexOf(props.currentKey);
    return c >= 0 ? c : items().length - 1;
  });
  const previewed = () => items()[index()];

  const moveTo = (i: number) => {
    const a = items()[Math.max(0, Math.min(items().length - 1, i))];
    if (a) setPreviewKey(a.key);
  };
  const edge = (which: "start" | "end") => {
    const more = which === "start" ? props.anchors.hasBefore : props.anchors.hasAfter;
    if (!more) {
      setPendingEdge(undefined);
      moveTo(which === "start" ? 0 : items().length - 1);
      return;
    }
    setPendingEdge(which);
    props.onBrowseEdge(which === "end");
  };
  // 等的那一头到了（那一侧没有更多了）：预览移到那一头
  createEffect(
    on(
      () => props.anchors,
      (a) => {
        const want = pendingEdge();
        if (want === "start" && !a.hasBefore) moveTo(0);
        else if (want === "end" && !a.hasAfter) moveTo(a.items.length - 1);
        else return;
        setPendingEdge(undefined);
      },
    ),
  );

  return {
    items,
    index,
    previewed,
    preview: (key: string) => setPreviewKey(key),
    reset() {
      setPreviewKey(props.currentKey);
      setPendingEdge(undefined);
    },
    /** 展开时的按键：↑ / ↓ 换一处，Home / End 到整个对话的头尾，Enter 跳到预览那一处。处理了返回真。 */
    onKey(e: KeyboardEvent, jump: (key: string) => void): boolean {
      switch (e.key) {
        case "ArrowUp":
        case "ArrowDown":
          if (e.altKey) return false;
          e.preventDefault();
          moveTo(index() + (e.key === "ArrowUp" ? -1 : 1));
          return true;
        case "Home":
        case "End":
          e.preventDefault();
          edge(e.key === "Home" ? "start" : "end");
          return true;
        case "Enter": {
          const a = previewed();
          if (a === undefined) return false;
          e.preventDefault();
          jump(a.key);
          return true;
        }
        default:
          return false;
      }
    },
  };
}

export type AnchorBrowse = ReturnType<typeof createAnchorBrowse>;

/** 展开态：每个锚点一条刻度的可滚动列表；信息卡跟着预览那一条。点刻度或信息卡就跳。 */
export function AnchorList(props: {
  browse: AnchorBrowse;
  anchors: AnchorsView;
  currentKey?: string;
  onJump: (key: string) => void;
  onBrowse: (key: string) => void;
}) {
  let list!: HTMLDivElement;
  const [scroll, setScroll] = createSignal({ top: 0, client: 0, height: 0 });
  const [loadingSide, setLoadingSide] = createSignal<"start" | "end">();
  let loadingTimer: ReturnType<typeof setTimeout> | undefined;
  let lastBrowsed: string | undefined;
  // 顶上那一条与它往下偏了多少：新列表到了按它对回原位
  let topKey: string | undefined;
  let topOffset = 0;
  onCleanup(() => clearTimeout(loadingTimer));

  const items = () => props.browse.items();
  const measure = () => {
    setScroll({ top: list.scrollTop, client: list.clientHeight, height: list.scrollHeight });
    const r = visibleRange(list.scrollTop, list.clientHeight, items().length);
    topKey = items()[r.first]?.key;
    topOffset = list.scrollTop - (LIST_PAD + r.first * PITCH);
    const t = browseTarget(r, items().length, props.anchors.hasBefore, props.anchors.hasAfter);
    const key = t && items()[t.index]?.key;
    if (t && key !== undefined && key !== lastBrowsed) {
      lastBrowsed = key;
      props.onBrowse(key);
      clearTimeout(loadingTimer);
      loadingTimer = setTimeout(() => setLoadingSide(t.side), LOADING_DOT_DELAY_MS);
    }
  };

  // 展开时：当前阅读位置那一条放到中间
  const mount = (el: HTMLDivElement) => {
    list = el;
    queueMicrotask(() => {
      list.scrollTop = centeredScrollTop(props.browse.index(), list.clientHeight);
      measure();
    });
  };
  // 新列表到了：按 key 对回原位，滚动位置不跳；加载点收掉
  createEffect(
    on(
      () => props.anchors.items,
      (next) => {
        clearTimeout(loadingTimer);
        setLoadingSide(undefined);
        const top = restoredScrollTop(
          next.map((a) => a.key),
          topKey,
          topOffset,
        );
        if (top !== undefined) list.scrollTop = top;
        measure();
      },
      { defer: true },
    ),
  );
  // 键盘换的预览处滚进视野。按 key 盯（memo 只在值变了时通知）：新列表到了下标会变，但那不是换了预览处，
  // 不能把对回的位置又滚走
  const previewedKey = createMemo(() => props.browse.previewed()?.key);
  createEffect(
    on(
      previewedKey,
      () => {
        const i = props.browse.index();
        const s = scroll();
        const y = LIST_PAD + i * PITCH;
        if (y < s.top || y + PITCH > s.top + s.client) {
          list.scrollTop = centeredScrollTop(i, list.clientHeight);
          measure();
        }
      },
      { defer: true },
    ),
  );

  const fade = () => {
    const s = scroll();
    return fadeSides(s.top, s.client, s.height, props.anchors.hasBefore, props.anchors.hasAfter);
  };
  const seekingKey = () => props.anchors.jumpLoadingKey;
  // 信息卡的竖直中心对齐预览那一条刻度（外框的 1px 边框也算上）
  const cardY = () => FRAME_BORDER + LIST_PAD + props.browse.index() * PITCH + PITCH / 2 - scroll().top;

  return (
    <div class="iw-anchor__open">
      {/* 外框画边框与底色，里面的滚动区才渐隐：渐隐不能把外框也淡掉 */}
      <div class="iw-anchor__frame">
        <div
          ref={mount}
          class="iw-anchor__list"
          classList={{
            "iw-anchor__list--fade-top": fade().top,
            "iw-anchor__list--fade-bottom": fade().bottom,
            "iw-anchor__list--seeking": seekingKey() !== "",
          }}
          style={{ height: `${LIST_PAD * 2 + items().length * PITCH}px` }}
          onScroll={measure}
        >
          <Show when={loadingSide() === "start"}>
            <i class="iw-anchor__more iw-anchor__more--start" aria-hidden="true" />
          </Show>
          <For each={items()}>
            {(a, i) => (
              <button
                type="button"
                class={`iw-tick ${KIND_CLASS[a.kind]}${distanceClass(Math.abs(i() - props.browse.index()))}`}
                classList={{
                  "iw-tick--current": a.key === props.currentKey,
                  "iw-tick--seeking": a.key === seekingKey(),
                }}
                aria-label={anchorLabel(a)}
                aria-current={i() === props.browse.index() ? "true" : undefined}
                onMouseEnter={() => props.browse.preview(a.key)}
                onFocus={() => props.browse.preview(a.key)}
                onClick={() => props.onJump(a.key)}
              />
            )}
          </For>
          <Show when={loadingSide() === "end"}>
            <i class="iw-anchor__more iw-anchor__more--end" aria-hidden="true" />
          </Show>
        </div>
      </div>
      <Show when={props.browse.previewed()}>
        {(a) => (
          <button
            type="button"
            class="iw-anchor__card"
            style={{ top: `${cardY()}px` }}
            aria-label={anchorLabel(a())}
            onClick={() => props.onJump(a().key)}
          >
            <span class="iw-anchor__card-head">
              <span class={`iw-anchor__kind ${KIND_CLASS[a().kind]}`}>{a().kindLabel}</span>
              <span class="iw-anchor__time">{a().timeLabel}</span>
            </span>
            <span class="iw-anchor__preview">{a().preview}</span>
          </button>
        )}
      </Show>
    </div>
  );
}

/** 静止态的三条短线与状态点（等你处理：芥末黄；失败：赭红；都没有就不出）。导轨与按钮共用。 */
export function AnchorRest(props: { anchors: AnchorsView; class: string }) {
  return (
    <span class={props.class} aria-hidden="true">
      <span />
      <span />
      <span />
      <Show when={props.anchors.hasWaiting}>
        <i class="iw-anchor__dot iw-anchor__dot--waiting" />
      </Show>
      <Show when={props.anchors.hasFailed}>
        <i class="iw-anchor__dot iw-anchor__dot--failed" />
      </Show>
    </span>
  );
}
