// 消息锚点的翻看状态与刻度列表（ui-m1.md 2a 第 11 条「展开态」，列表式，画布 19、20）。宽形态的导轨（AnchorRail）
// 与窄形态的标题栏按钮面板（AnchorButton）共用这一份：列表、悬停预览、点击跳转、滚动与续载规则完全相同，只是怎么展开、
// 怎么收起不同。哪些算锚点、文字、当前位置、前后还有没有、正在跳哪一处都来自 patch；这里只有展示状态：正在预览哪一处
// （按 key 记，新列表到了按 key 对回去）、列表滚到哪、Home / End 在等哪一头。
import { createComputed, createEffect, createMemo, createSignal, For, on, onCleanup, Show } from "solid-js";
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
  /** Home / End 要的那一头不在已载入的窗口里：请 Rust 以整个对话最早（假）/ 最新（真）的一头为中心重取。
   *  这个意图由 Rust 持有：那一头到了，AnchorsView 带 edgeLanding（token）回来，网页只执行一次。 */
  onBrowseEdge: (latest: boolean) => void;
  /** 用户在列表里有输入：在事件发生那一刻一律告诉 Rust，由它判断有没有头尾意图可作废（没有就是空操作，不推
   *  帧）。源头：滚轮、触摸开始与移动、指针按下（点刻度也算）、会让列表滚动的键（↑ / ↓、PageUp / PageDown、
   *  Space / Shift+Space、本窗内直接落地的 Home / End），以及不是列表自己写出来的 scroll（读屏手势、中键自动滚动
   *  这类只产生 scroll 的路径）。不看 edgePending（异步回来，有往返空窗），也不比坐标（g3b 4/F-2、5/F-2）。 */
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
  // 上次发 Edge 请求之后用户有没有输入过：有就不执行那次的落点（作废还在去 Rust 的路上、落点已经先到时也不拉人）。
  // 每次新的 Edge 请求清零
  let inputSinceEdge = false;
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
  /** 用户在列表里有输入（事件发生那一刻调）：告诉 Rust，并记下「请求之后动过」。每次都报，不节流：Rust 那边作废
   *  是 O(1) 的，没有意图时是空操作、不推帧。 */
  const userMoved = () => {
    inputSinceEdge = true;
    props.onListInput();
  };
  const edge = (which: "start" | "end") => {
    const more = which === "start" ? props.anchors.hasBefore : props.anchors.hasAfter;
    if (!more) {
      // 那一头就在本窗：直接落地。这也是一次覆盖头尾意图的用户输入，旧的（反方向的）要作废
      userMoved();
      moveTo(which === "start" ? 0 : items().length - 1);
      return;
    }
    inputSinceEdge = false;
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
    /** Rust 给的头尾落点：没执行过、请求之后用户也没动过，就把预览移到那一头并滚进视野，返回真（优先于按 key
     *  还原）。用户动过：记成用过、不执行（让位给用户）。 */
    landEdge(): boolean {
      const l = props.anchors.edgeLanding;
      if (l === undefined || l.token === seenLanding) return false;
      seenLanding = l.token;
      if (inputSinceEdge) return false;
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
          userMoved();
          moveTo(index() + (e.key === "ArrowUp" ? -1 : 1));
          return true;
        case "Home":
        case "End":
          e.preventDefault();
          edge(e.key === "Home" ? "start" : "end");
          return true;
        // 焦点在列表里（刻度能拿到焦点）时这几个键会让列表原生滚动：报输入，不拦默认行为（g3b 5/F-2）
        case "PageUp":
        case "PageDown":
        case " ":
          userMoved();
          return false;
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
  // 用户正按着指针 / 正在触摸：位置归用户，程序不写（窗口替换的还原让位）
  let pointerHeld = false;
  let touching = false;
  const userControls = () => pointerHeld || touching;
  // 「这一下是程序引起的」：程序侧每次会让列表滚动的变化之前置上，它触发的那次 scroll 消费掉；没有触发 scroll
  // （位置没变）就在下一帧清掉。不带这个标记的 scroll 一律当用户输入上报（g3b 5/F-2）。程序侧的变化不只是写
  // scrollTop：替换列表、改变内容高度（窗口变短时浏览器自己夹紧 scrollTop，也会派 scroll，g3b 6/F-1）同样要置。
  // 以后加新的程序侧 DOM 或布局变化，记得在变化之前调 markSelf。程序写和用户的只产生 scroll 的滚动
  // 落在同一帧时，浏览器只派一次 scroll，分不开：这一次按程序的算，用户手势后面几帧的 scroll 照常上报。最坏是
  // 这一下没作废掉一个等着的头尾，被它迟到的落点拉回一次（取舍已认可，g3b 第 6 轮）
  let selfWrite = false;
  let selfWriteFrame = 0;
  const markSelf = () => {
    selfWrite = true;
    cancelAnimationFrame(selfWriteFrame);
    selfWriteFrame = requestAnimationFrame(() => (selfWrite = false));
  };
  const setTop = (v: number) => {
    markSelf();
    list.scrollTop = v;
  };
  const onScroll = () => {
    if (selfWrite) selfWrite = false;
    else props.browse.userMoved();
    measure();
  };
  onCleanup(() => cancelAnimationFrame(selfWriteFrame));
  // 上一份画着的列表
  let shownItems = props.anchors.items;
  // 窗口替换前取到的顶上那一条（g3b 5/F-1）：在 DOM 换成新列表之前取（createComputed 先于 DOM 更新跑），
  // 这时的 scrollTop 还没被新内容夹紧，也包含还没派发 scroll 的用户位移；DOM 换完以后按新列表对回
  let before: { keys: string[]; top: string | undefined; offset: number } | undefined;
  let mounted = false;
  createComputed(
    on(
      () => props.anchors.items,
      (next) => {
        if (!mounted || next === shownItems) return;
        // 列表要换了（内容高度可能变、浏览器可能夹紧 scrollTop）：这次更新引起的 scroll 是程序侧的。还原找不到
        // key、不再调 setTop 的那一支也靠它
        markSelf();
        const keys = shownItems.map((a) => a.key);
        const r = visibleRange(list.scrollTop, list.clientHeight, keys.length);
        before = { keys, top: keys[r.first], offset: list.scrollTop - (LIST_PAD + r.first * PITCH) };
      },
      { defer: true },
    ),
  );
  const releasePointer = () => {
    pointerHeld = false;
  };
  window.addEventListener("pointerup", releasePointer, true);
  window.addEventListener("pointercancel", releasePointer, true);
  onCleanup(() => {
    window.removeEventListener("pointerup", releasePointer, true);
    window.removeEventListener("pointercancel", releasePointer, true);
  });
  onCleanup(() => clearTimeout(loadingTimer));

  const items = () => props.browse.items();
  const measure = () => {
    setScroll({ top: list.scrollTop, client: list.clientHeight, height: list.scrollHeight });
    const r = visibleRange(list.scrollTop, list.clientHeight, items().length);
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
    mounted = true;
    queueMicrotask(() => {
      setTop(centeredScrollTop(props.browse.index(), list.clientHeight));
      measure();
    });
  };
  // 新列表到了（窗口替换）：Home / End 等的那一头到了（且请求之后用户没动过）就由它定位置；否则按顶上那条的 key
  // 对回原位，滚动位置不跳。顶上那一条用「替换前那份列表 + 此刻的位置」现算，不用之前存的（用户在这之前的位移
  // 不会丢）；新旧 key 序列相同不写；用户正按着指针或在触摸时不写（位置归用户）。旧的预览处被新窗口裁掉时，预览
  // 改到还原后顶上那一条。加载点收掉。
  createEffect(
    on(
      () => props.anchors,
      (view) => {
        const prevItems = shownItems;
        shownItems = view.items;
        const was = before;
        before = undefined;
        if (props.browse.landEdge()) {
          clearTimeout(loadingTimer);
          setLoadingSide(undefined);
          measure();
          return;
        }
        const next = view.items;
        if (next === prevItems) return;
        clearTimeout(loadingTimer);
        setLoadingSide(undefined);
        const keys = next.map((a) => a.key);
        const prevKeys = was?.keys ?? prevItems.map((a) => a.key);
        const keptTop = was?.top;
        const sameKeys = keys.length === prevKeys.length && keys.every((k, i) => k === prevKeys[i]);
        if (!sameKeys && !userControls() && was !== undefined) {
          const top = restoredScrollTop(keys, keptTop, was.offset);
          if (top !== undefined) setTop(top);
        }
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
          onWheel={() => props.browse.userMoved()}
          onTouchStart={() => {
            touching = true;
            props.browse.userMoved();
          }}
          onTouchMove={() => props.browse.userMoved()}
          onTouchEnd={() => (touching = false)}
          onTouchCancel={() => (touching = false)}
          onPointerDown={() => {
            pointerHeld = true;
            props.browse.userMoved();
          }}
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
