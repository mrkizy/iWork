// 消息锚点导轨，宽形态（ui-m1.md 2a 第 11 条「宽形态」，画布 16-Anchor-*；方案 2.9「网页」）。
// 贴着侧栏右边缘、在对话区左边距里（对话区左边留 --anchor-gutter，导轨不压头像与正文）。
// 静止态：三条短线 + 按需的芥末黄 / 赭红点。悬停或键盘焦点到达时展开成刻度滚轮（翻看状态与画法见 AnchorWheel，
// 与窄形态的标题栏按钮共用）。翻看只换预览，不动对话；点中间刻度或信息卡、点别的刻度才跳。Esc 立即收起，
// 移开 300ms 后收起。
import { createEffect, createSignal, onCleanup, Show } from "solid-js";
import type { AnchorsView } from "../gen/iwork/v1/view_pb";
import { AnchorRest, AnchorWheel, createAnchorBrowse } from "./AnchorWheel";
import "./AnchorRail.css";

export { TICKS_EACH_SIDE } from "./AnchorWheel";

/** 鼠标移开多久收起。 */
export const CLOSE_DELAY_MS = 300;

export interface AnchorRailProps {
  anchors: AnchorsView;
  currentKey?: string;
  onJump: (key: string) => void;
  /** 预览到离窗口边缘不远、那一侧还有：请 Rust 以这一处为中心重取。 */
  onBrowse: (key: string) => void;
}

export function AnchorRail(props: AnchorRailProps) {
  const [open, setOpen] = createSignal(false);
  const browse = createAnchorBrowse(props);
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
    browse.clearBounce();
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

  return (
    <div
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
      onWheel={(e) => {
        if (open()) browse.onWheel(e);
      }}
    >
      <Show when={open()} fallback={<AnchorRest anchors={props.anchors} class="iw-anchor__rest" />}>
        <AnchorWheel browse={browse} onJump={jump} />
      </Show>
      <Show when={props.anchors.jumpLoading}>
        {(t) => (
          <div class="iw-anchor__loading" role="status">
            {t()}
          </div>
        )}
      </Show>
    </div>
  );
}
