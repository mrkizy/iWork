// 消息锚点导轨，宽形态（ui-m1.md 2a 第 11 条「宽形态」与「展开态」，列表式，画布 18、19、20；方案 2.9「网页」）。
// 贴着主区域左边缘、在对话区左边距里上下居中（对话区左边留 --anchor-gutter，导轨不压头像与正文）。只有导轨自己这一块
// 响应鼠标：静止态是三条短线周围约 44×36，展开态是刻度列表加信息卡；左边距竖带的其他位置不响应悬停、不拦截滚轮。
// 静止态：三条短线 + 按需的芥末黄 / 赭红点。悬停或键盘焦点到达时展开成刻度列表（AnchorList，与窄形态的面板共用）。
// 翻看只换预览，不动对话；点刻度或信息卡才跳。Esc 立即收起，移开 300ms 后收起。
// 滚轮：列表超出最高高度时由列表自己滚（overscroll-behavior: contain，不带动对话）；不超出时交给对话滚。
import { createEffect, createSignal, onCleanup, Show } from "solid-js";
import type { AnchorsView } from "../gen/iwork/v1/view_pb";
import { AnchorList, AnchorRest, createAnchorBrowse } from "./AnchorList";
import "./AnchorRail.css";

/** 鼠标移开多久收起。 */
export const CLOSE_DELAY_MS = 300;

export interface AnchorRailProps {
  anchors: AnchorsView;
  currentKey?: string;
  onJump: (key: string) => void;
  /** 列表滚到离已载入的一端不远、那一侧还有：请 Rust 以这一条为中心重取。 */
  onBrowse: (key: string) => void;
  /** Home / End：请 Rust 以最早（假）/ 最新（真）的一头为中心重取。 */
  onBrowseEdge: (latest: boolean) => void;
  /** Rust 还在等 Home / End 时用户在列表里滚动或按键：请它作废。 */
  onListInput: () => void;
}

export function AnchorRail(props: AnchorRailProps) {
  const [open, setOpen] = createSignal(false);
  const browse = createAnchorBrowse(props);
  let rail!: HTMLDivElement;
  let closeTimer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(closeTimer));

  const show = () => {
    clearTimeout(closeTimer);
    if (!open()) browse.reset();
    setOpen(true);
  };
  const close = () => {
    clearTimeout(closeTimer);
    setOpen(false);
  };
  const jump = (key: string) => {
    close();
    props.onJump(key);
  };

  const onKey = (e: KeyboardEvent) => {
    if (!open()) return;
    if (e.key === "Escape") {
      e.stopPropagation();
      close();
    } else {
      browse.onKey(e, jump);
    }
  };
  createEffect(() => {
    if (!open()) return;
    document.addEventListener("keydown", onKey, true);
    onCleanup(() => document.removeEventListener("keydown", onKey, true));
  });

  // 列表滚得动就让它自己滚；滚不动（静止态、条数少）交给对话：导轨盖在对话区的左边距上，不在对话的滚动容器里
  const onWheel = (e: WheelEvent) => {
    const list = rail.querySelector<HTMLElement>(".iw-anchor__list");
    if (list && list.scrollHeight > list.clientHeight) return;
    const timeline = rail.closest(".iw-timeline")?.querySelector<HTMLElement>(".iw-vrows");
    if (!timeline) return;
    e.preventDefault();
    timeline.scrollTop += e.deltaY;
  };

  return (
    <div
      ref={rail}
      class={`iw-anchor${open() ? " iw-anchor--open" : ""}`}
      role="navigation"
      aria-label="消息锚点"
      tabIndex={0}
      onMouseEnter={show}
      onMouseLeave={() => {
        clearTimeout(closeTimer);
        closeTimer = setTimeout(close, CLOSE_DELAY_MS);
      }}
      onFocus={show}
      onWheel={onWheel}
    >
      <Show when={open()} fallback={<AnchorRest anchors={props.anchors} class="iw-anchor__rest" />}>
        <AnchorList
          browse={browse}
          anchors={props.anchors}
          currentKey={props.currentKey}
          onJump={jump}
          onBrowse={props.onBrowse}
        />
      </Show>
    </div>
  );
}
