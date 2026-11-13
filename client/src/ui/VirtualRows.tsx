// 时间线的虚拟行（方案 2.4 第 9 条、第 6 节「十万条滚动」）。
// 只挂 Rust 发来的窗口行，窗口外用占位高度；滚动时报可见行的 ordinal（每帧最多一次，变了才报）；
// 行增删时按锚点行保持位置；停在底部且时间线接着最新时贴底；顶到最旧一行时请求往前翻页。
// 高度缓存以 ordinal 为键（稳定行号，重建前后不变）。
// 跳转（消息锚点、⌥↑ / ⌥↓、从「待你处理」进来）：Rust 给一次性的滚动指令，同一个 token 只滚一次，滚到那一行居中
// 并高亮一段时间（网页计时）；那一行还没随帧到达时等它到了再滚。另报告内容有没有超出一屏（锚点组件只在超出时显示，
// 布局事实网页自己量）。
//
// 谁定滚动位置（优先级从高到低；高的在同一拍里最后写）：
// 1. 用户自己滚动（之后 Rust 作废还在等的跳转）；
// 2. 显式跳转（scrollTo）：一次性意图由 Rust 唯一持有（锚点模块头的表），这里只执行，同一个 token 只用一次。
//    与 reset 在同一个 effect 里按固定次序处理（先回底、再跳），和写入次序无关；挂载之后才执行。
//    跳过去以后不再跟随最新：用户要看那一行，不因为它恰好离底部不到一屏就被新消息拉走；用户自己有滚动输入才恢复；
// 3. 行变化时保持位置（restore：停在底部且接着最新就贴底，否则锚点行放回原处）；
// 4. 首次挂载、时间线 reset 时贴底（默认位置）。
import { createEffect, createSignal, For, on, onCleanup, onMount, type JSX } from "solid-js";
import { averageHeight, isAtBottom, unsentAround, visibleOrdinals, type RowBox } from "./virtual";
import "./VirtualRows.css";

export interface VirtualRowsProps {
  /** 已收到的行（按位置排好）。 */
  keys: readonly string[];
  ordinalOf: (key: string) => bigint;
  /** 第一行在整条时间线里的下标与时间线总行数（WindowInfo）。 */
  firstIndex: number;
  total: number;
  /** 时间线接着最新（at_tail）：停在底部时新行进来贴底。 */
  atTail: boolean;
  hasOlder: boolean;
  renderRow: (key: string) => JSX.Element;
  /** 可见行变了。`byUser`：这次变化来自用户自己滚动（滚轮、触摸、按键、拖滚动条），不是跳转、贴底这类程序滚动；
   *  Rust 据此作废等待中的锚点跳转（方案 2.9「跳转」第 3 条）。 */
  onVisible: (first: bigint, last: bigint, byUser: boolean) => void;
  onReachOldest: () => void;
  /** 变了就回到底部（时间线 reset：打开对话、重建、回到最新）。 */
  resetToken?: number;
  /** 一次性的滚动指令（消息锚点跳转）：token 没见过才滚。 */
  scrollTo?: { rowKey: string; token: bigint; highlightMs: number };
  /** 内容是否超出一屏（变了才报）。 */
  onOverflow?: (overflow: boolean) => void;
  estimate?: number;
  class?: string;
}

const DEFAULT_ESTIMATE = 72;

export function VirtualRows(props: VirtualRowsProps) {
  let box!: HTMLDivElement;
  const heights = new Map<bigint, number>();
  const [avg, setAvg] = createSignal(props.estimate ?? DEFAULT_ESTIMATE);
  let stick = true;
  let anchor: { key: string; offset: number } | undefined;
  let reported: [bigint, bigint] | undefined;
  let askedOlderAt: bigint | undefined;
  let frame = 0;
  let scrolledToken: bigint | undefined;
  // 挂载贴底之前不执行跳转：先执行的话会被挂载时的贴底盖掉，token 又已经用掉（g3b 1/F-2）
  let mounted = false;
  // 跳转之后暂停跟随最新，直到用户自己有滚动输入（UI g3b 2/F-1）
  let followHeld = false;
  // 上一次处理过的 reset（与跳转在同一个 effect 里按次序处理）
  let seenReset = props.resetToken;
  let overflow: boolean | undefined;
  // 上一次报告以来有没有用户自己的滚动输入。程序改 scrollTop 也会触发 scroll 事件，所以看输入事件，不看 scroll
  let userInput = false;
  const [highlight, setHighlight] = createSignal<string>();
  let highlightTimer: ReturnType<typeof setTimeout> | undefined;

  const rowEls = () => [...box.querySelectorAll<HTMLElement>("[data-vrow]")];

  const boxes = (): RowBox[] =>
    rowEls().map((el) => ({ ordinal: BigInt(el.dataset.ordinal ?? "0"), top: el.offsetTop, height: el.offsetHeight }));

  const remember = () => {
    stick = !followHeld && isAtBottom(box.scrollTop, box.clientHeight, box.scrollHeight);
    const first = rowEls().find((el) => el.offsetTop + el.offsetHeight > box.scrollTop);
    anchor = first ? { key: first.dataset.key ?? "", offset: first.offsetTop - box.scrollTop } : undefined;
  };

  const report = () => {
    frame = 0;
    const unsentRows = unsent();
    const now = box.scrollHeight > box.clientHeight + 1 || unsentRows.above + unsentRows.below > 0;
    if (now !== overflow) {
      overflow = now;
      props.onOverflow?.(now);
    }
    const range = visibleOrdinals(boxes(), box.scrollTop, box.clientHeight);
    // 两件事分开判断（审查 5/F-2）：范围变了才报是去重；用户自己滚了就算范围没变也要报，Rust 靠它作废等待中的跳转
    const changed = range !== undefined && (reported === undefined || range[0] !== reported[0] || range[1] !== reported[1]);
    if (range && (changed || userInput)) {
      reported = range;
      props.onVisible(range[0], range[1], userInput);
    }
    userInput = false;
    const oldest = props.keys[0];
    if (props.hasOlder && props.firstIndex === 0 && oldest !== undefined && box.scrollTop < box.clientHeight) {
      const o = props.ordinalOf(oldest);
      if (askedOlderAt !== o) {
        askedOlderAt = o;
        props.onReachOldest();
      }
    }
  };

  const onUserInput = () => {
    userInput = true;
    followHeld = false;
  };
  const SCROLL_KEYS = new Set(["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End", " "]);
  const onKey = (e: KeyboardEvent) => {
    if (SCROLL_KEYS.has(e.key)) onUserInput();
  };
  // 只算点在容器本身上（滚动条）；点行里的按钮不是滚动
  const onPointer = (e: PointerEvent) => {
    if (e.target === box) onUserInput();
  };

  const onScroll = () => {
    remember();
    if (frame === 0) frame = requestAnimationFrame(report);
  };

  const measure = new ResizeObserver((entries) => {
    for (const e of entries) {
      const el = e.target as HTMLElement;
      heights.set(BigInt(el.dataset.ordinal ?? "0"), el.offsetHeight);
    }
    setAvg(averageHeight(heights.values(), props.estimate ?? DEFAULT_ESTIMATE));
    restore();
  });

  // 行变了以后：贴底，或把锚点行放回原来的屏幕位置。
  const restore = () => {
    if (stick && props.atTail) {
      box.scrollTop = box.scrollHeight;
    } else if (anchor) {
      const el = box.querySelector<HTMLElement>(`[data-vrow][data-key="${CSS.escape(anchor.key)}"]`);
      if (el) box.scrollTop = el.offsetTop - anchor.offset;
    }
  };

  // 时间线 reset（Rust 重建：打开对话、重连走快照、回到最新）：回到底部，跟随最新
  const resetToBottom = () => {
    stick = true;
    followHeld = false;
    anchor = undefined;
    askedOlderAt = undefined;
    // Rust 重建时作废了它记的窗口（6/F-3）：去重缓存跟着失效，布局后重报一次，首尾行和 reset 前一样也报（审查 7/F-3）
    reported = undefined;
    box.scrollTop = box.scrollHeight;
    if (frame === 0) frame = requestAnimationFrame(report);
  };

  createEffect(
    on(
      () => [props.keys, props.firstIndex, props.total] as const,
      () => {
        restore();
        if (frame === 0) frame = requestAnimationFrame(report);
      },
      { defer: true },
    ),
  );

  // 滚动指令：行到了才滚（行随后面的帧到达时 keys 变，这里重跑）。挂载、reset 时在贴底之后也调一次
  const applyJump = () => {
    const s = props.scrollTo;
    if (!mounted || s === undefined || s.token === scrolledToken) return;
    const el = box.querySelector<HTMLElement>(`[data-vrow][data-key="${CSS.escape(s.rowKey)}"]`);
    if (el === null) return;
    scrolledToken = s.token;
    followHeld = true;
    box.scrollTop = el.offsetTop - Math.max(0, (box.clientHeight - el.offsetHeight) / 2);
    remember();
    setHighlight(s.rowKey);
    clearTimeout(highlightTimer);
    highlightTimer = setTimeout(() => setHighlight(undefined), s.highlightMs);
    if (frame === 0) frame = requestAnimationFrame(report);
  };
  // reset 与跳转同一个 effect、固定次序：先回底，再执行还没用过的跳转（行随后面的帧到达时 keys 变，这里重跑）
  createEffect(() => {
    const r = props.resetToken;
    void props.scrollTo;
    void props.keys;
    if (!mounted) return;
    if (r !== seenReset) {
      seenReset = r;
      resetToBottom();
    }
    applyJump();
  });

  onMount(() => {
    box.scrollTop = box.scrollHeight;
    mounted = true;
    seenReset = props.resetToken;
    applyJump();
    if (frame === 0) frame = requestAnimationFrame(report);
  });
  onCleanup(() => {
    measure.disconnect();
    clearTimeout(highlightTimer);
    if (frame !== 0) cancelAnimationFrame(frame);
  });

  const unsent = () => unsentAround(props.firstIndex, props.keys.length, props.total);

  return (
    <div
      ref={box}
      class={`iw-vrows${props.class ? ` ${props.class}` : ""}`}
      onScroll={onScroll}
      onWheel={onUserInput}
      onTouchMove={onUserInput}
      onKeyDown={onKey}
      onPointerDown={onPointer}
      role="log"
      aria-live="polite"
    >
      <div class="iw-vrows__spacer" style={{ height: `${unsent().above * avg()}px` }} />
      <For each={props.keys}>
        {(key) => (
          <div
            data-vrow=""
            data-key={key}
            classList={{ "iw-vrow--highlight": highlight() === key }}
            data-ordinal={String(props.ordinalOf(key))}
            ref={(el) => {
              measure.observe(el);
              onCleanup(() => measure.unobserve(el));
            }}
          >
            {props.renderRow(key)}
          </div>
        )}
      </For>
      <div class="iw-vrows__spacer" style={{ height: `${unsent().below * avg()}px` }} />
    </div>
  );
}
