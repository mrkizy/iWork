// 时间线的虚拟行（方案 2.4 第 9 条、第 6 节「十万条滚动」）。
// 只挂 Rust 发来的窗口行，窗口外用占位高度；滚动时报可见行的 ordinal（每帧最多一次，变了才报）；
// 行增删时按锚点行保持位置；停在底部且时间线接着最新时贴底；顶到最旧一行时请求往前翻页。
// 高度缓存以 ordinal 为键（稳定行号，重建前后不变）。
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
  onVisible: (first: bigint, last: bigint) => void;
  onReachOldest: () => void;
  /** 变了就回到底部（时间线 reset：打开对话、重建、回到最新）。 */
  resetToken?: number;
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

  const rowEls = () => [...box.querySelectorAll<HTMLElement>("[data-vrow]")];

  const boxes = (): RowBox[] =>
    rowEls().map((el) => ({ ordinal: BigInt(el.dataset.ordinal ?? "0"), top: el.offsetTop, height: el.offsetHeight }));

  const remember = () => {
    stick = isAtBottom(box.scrollTop, box.clientHeight, box.scrollHeight);
    const first = rowEls().find((el) => el.offsetTop + el.offsetHeight > box.scrollTop);
    anchor = first ? { key: first.dataset.key ?? "", offset: first.offsetTop - box.scrollTop } : undefined;
  };

  const report = () => {
    frame = 0;
    const range = visibleOrdinals(boxes(), box.scrollTop, box.clientHeight);
    if (range && (reported === undefined || range[0] !== reported[0] || range[1] !== reported[1])) {
      reported = range;
      props.onVisible(range[0], range[1]);
    }
    const oldest = props.keys[0];
    if (props.hasOlder && props.firstIndex === 0 && oldest !== undefined && box.scrollTop < box.clientHeight) {
      const o = props.ordinalOf(oldest);
      if (askedOlderAt !== o) {
        askedOlderAt = o;
        props.onReachOldest();
      }
    }
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

  createEffect(
    on(
      () => props.resetToken,
      () => {
        stick = true;
        anchor = undefined;
        askedOlderAt = undefined;
        box.scrollTop = box.scrollHeight;
      },
      { defer: true },
    ),
  );

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

  onMount(() => {
    box.scrollTop = box.scrollHeight;
    frame = requestAnimationFrame(report);
  });
  onCleanup(() => {
    measure.disconnect();
    if (frame !== 0) cancelAnimationFrame(frame);
  });

  const unsent = () => unsentAround(props.firstIndex, props.keys.length, props.total);

  return (
    <div ref={box} class={`iw-vrows${props.class ? ` ${props.class}` : ""}`} onScroll={onScroll} role="log" aria-live="polite">
      <div class="iw-vrows__spacer" style={{ height: `${unsent().above * avg()}px` }} />
      <For each={props.keys}>
        {(key) => (
          <div
            data-vrow=""
            data-key={key}
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
