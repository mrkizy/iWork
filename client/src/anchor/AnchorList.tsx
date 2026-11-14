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
/** 连续型输入（滚轮、触摸、滚动）一个手势只报第一下：同一种输入这么久之内的再报就跳过（作废是幂等的）。
 *  按键每次都报。 */
const LIST_INPUT_GAP_MS = 100;

/** 列表里的用户输入：连续型按种类节流，按键不节流。 */
export type ListInput = "wheel" | "touch" | "scroll" | "key";

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
  /** Home / End 要的那一头不在已载入的窗口里：请 Rust 以整个对话最早（假）/ 最新（真）的一头为中心重取。
   *  这个意图由 Rust 持有：那一头到了，AnchorsView 带 edgeLanding（token）回来，网页只执行一次。 */
  onBrowseEdge: (latest: boolean) => void;
  /** 用户在列表里有输入（滚轮、触摸、方向键、本窗内直接落地的 Home / End、不是程序写的滚动）：一律告诉 Rust，
   *  由它判断有没有头尾意图可作废（没有就是空操作）。不看 edgePending：它是异步回来的，有往返空窗（g3b 3/F-1）。 */
  onListInput: () => void;
}

/** 翻看状态：展开时 `reset` 回到当前位置；`preview` 换预览处（悬停）；按键换预览处并滚进视野。
 *
 * 列表滚动位置由谁定（优先级从高到低，见 AnchorList）：用户滚动列表 > 用户按键（↑ / ↓、Home / End）把预览滚进视野、
 * Rust 给的头尾落点（edgeLanding，一次性，token 只用一次） > 窗口替换按 key 还原 > 展开时居中当前位置。「滚进视野」
 * 只由按键与落点触发，不挂在「预览处变了」上：窗口替换也会让预览处变（旧的被裁掉），那不是用户换了预览。
 * 等数据的头尾意图只在 Rust（锚点模块头的表）：迟到、作废都由它管，网页不记「在等哪一头」。 */
export function createAnchorBrowse(props: AnchorSource) {
  const [previewKey, setPreviewKey] = createSignal<string>();
  // 已经执行过的头尾落点：建这份状态时已有的落点算用过（收起再展开不重放）
  let seenLanding = props.anchors.edgeLanding?.token;
  // 列表挂上来时给：把第 i 条滚进视野
  let reveal: ((i: number) => void) | undefined;

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
    const at = Math.max(0, Math.min(items().length - 1, i));
    const a = items()[at];
    if (a === undefined) return;
    setPreviewKey(a.key);
    reveal?.(at);
  };
  // 连续型输入一个手势里只报第一下：作废是幂等的，滚轮一秒几十次不必次次都发
  const lastReport = new Map<ListInput, number>();
  /** 用户在列表里有输入：一律告诉 Rust（见 AnchorSource.onListInput）。 */
  const userMoved = (kind: ListInput) => {
    if (kind !== "key") {
      const now = performance.now();
      if (now - (lastReport.get(kind) ?? -Infinity) < LIST_INPUT_GAP_MS) return;
      lastReport.set(kind, now);
    }
    props.onListInput();
  };
  const edge = (which: "start" | "end") => {
    const more = which === "start" ? props.anchors.hasBefore : props.anchors.hasAfter;
    if (!more) {
      // 那一头就在本窗：直接落地。这也是一次覆盖头尾意图的用户输入，旧的（反方向的）要作废
      userMoved("key");
      moveTo(which === "start" ? 0 : items().length - 1);
      return;
    }
    props.onBrowseEdge(which === "end");
  };

  return {
    items,
    index,
    previewed,
    previewKey,
    /** 悬停换预览处（鼠标就在那一条上，不用滚）。 */
    preview: (key: string) => setPreviewKey(key),
    setReveal: (f: ((i: number) => void) | undefined) => (reveal = f),
    userMoved,
    /** Rust 给的头尾落点：没执行过就把预览移到那一头并滚进视野，返回真（优先于按 key 还原）。 */
    landEdge(): boolean {
      const l = props.anchors.edgeLanding;
      if (l === undefined || l.token === seenLanding) return false;
      seenLanding = l.token;
      moveTo(l.latest ? items().length - 1 : 0);
      return true;
    },
    reset() {
      setPreviewKey(props.currentKey);
    },
    /** 展开时的按键：↑ / ↓ 换一处，Home / End 到整个对话的头尾，Enter 跳到预览那一处。处理了返回真。 */
    onKey(e: KeyboardEvent, jump: (key: string) => void): boolean {
      switch (e.key) {
        case "ArrowUp":
        case "ArrowDown":
          if (e.altKey) return false;
          e.preventDefault();
          userMoved("key");
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
  // 最近一次知道的滚动位置：程序写的、或上一次 scroll 事件看到的。scroll 事件里对不上就是用户造成的
  // （拖滚动条、键盘翻页），要上报（g3b 3/F-1）；程序写的那一次对得上，不算
  let knownTop = 0;
  const setTop = (v: number) => {
    list.scrollTop = v;
    knownTop = list.scrollTop;
  };
  const onScroll = () => {
    if (Math.abs(list.scrollTop - knownTop) > 0.5) props.browse.userMoved("scroll");
    knownTop = list.scrollTop;
    measure();
  };
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
      setTop(centeredScrollTop(props.browse.index(), list.clientHeight));
      measure();
    });
  };
  // 新列表到了（窗口替换）：Home / End 等的那一头到了就由它定位置；否则按顶上那条的 key 对回原位，滚动位置不跳。
  // 旧的预览处被新窗口裁掉时，预览改到还原后顶上那一条（不回落到末项，也不因此居中）。加载点收掉。
  createEffect(
    on(
      () => props.anchors,
      (view, prev) => {
        if (props.browse.landEdge()) {
          clearTimeout(loadingTimer);
          setLoadingSide(undefined);
          measure();
          return;
        }
        const next = view.items;
        if (prev !== undefined && next === prev.items) return;
        clearTimeout(loadingTimer);
        setLoadingSide(undefined);
        const keys = next.map((a) => a.key);
        const keptTop = topKey;
        const top = restoredScrollTop(keys, keptTop, topOffset);
        if (top !== undefined) setTop(top);
        const shown = props.browse.previewKey();
        if (keptTop !== undefined && keys.includes(keptTop) && (shown === undefined || !keys.includes(shown))) {
          props.browse.preview(keptTop);
        }
        measure();
      },
      { defer: true },
    ),
  );
  // 按键换的预览处滚进视野（只由按键触发，见 createAnchorBrowse 的优先级说明）
  props.browse.setReveal((i) => {
    const s = scroll();
    const y = LIST_PAD + i * PITCH;
    if (y < s.top || y + PITCH > s.top + s.client) {
      setTop(centeredScrollTop(i, list.clientHeight));
      measure();
    }
  });
  onCleanup(() => props.browse.setReveal(undefined));

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
        {/* 续载加载点画在外框的上下边缘，不占列表的流内高度：出现、消失时刻度与信息卡都不跳 */}
        <Show when={loadingSide() === "start"}>
          <i class="iw-anchor__more iw-anchor__more--start" aria-hidden="true" />
        </Show>
        <Show when={loadingSide() === "end"}>
          <i class="iw-anchor__more iw-anchor__more--end" aria-hidden="true" />
        </Show>
        <div
          ref={mount}
          class="iw-anchor__list"
          classList={{
            "iw-anchor__list--fade-top": fade().top,
            "iw-anchor__list--fade-bottom": fade().bottom,
            "iw-anchor__list--seeking": seekingKey() !== "",
          }}
          style={{ height: `${LIST_PAD * 2 + items().length * PITCH}px` }}
          onScroll={onScroll}
          onWheel={() => props.browse.userMoved("wheel")}
          onTouchMove={() => props.browse.userMoved("touch")}
        >
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
                // 整个导轨一个 Tab 停点（导轨本身）；列表里用方向键、Home / End、Enter
                tabIndex={-1}
                onMouseEnter={() => props.browse.preview(a.key)}
                onFocus={() => props.browse.preview(a.key)}
                onClick={() => props.onJump(a.key)}
              />
            )}
          </For>
        </div>
      </div>
      <Show when={props.browse.previewed()}>
        {(a) => (
          <button
            type="button"
            class="iw-anchor__card"
            style={{ top: `${cardY()}px` }}
            tabIndex={-1}
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
