// 消息锚点滚轮（ui-m1.md 2a 第 11 条，画布 16-Anchor-*；方案 2.9「网页」）。
// 哪些算锚点、文字、当前位置、前后还有没有都来自 patch（AnchorsView、当前位置的 key）；这里只有展示状态：
// 展开与否、正在预览哪一处（按 key 记，新列表到了按 key 对回去）、滚轮累计量、回弹。
// 静止态：三条短线 + 按需的芥末黄 / 赭红点。展开态：以预览处为中心上下各最多 4 条刻度，中间那条旁是信息卡。
// 翻看只换预览，不动对话；点中间刻度或信息卡、点别的刻度才跳。Esc 立即收起，移开 300ms 后收起。
import { createEffect, createMemo, createSignal, For, onCleanup, Show } from "solid-js";
import { AnchorKind, type AnchorItem, type AnchorsView } from "../gen/iwork/v1/view_pb";
import { INITIAL_WHEEL, moveWithin, nearEdge, wheelStep } from "./wheel";
import "./AnchorRail.css";

/** 中间之外上下各显示几条刻度。 */
export const TICKS_EACH_SIDE = 4;
/** 鼠标移开多久收起。 */
export const CLOSE_DELAY_MS = 300;

const KIND_CLASS: Record<AnchorKind, string> = {
  [AnchorKind.UNSPECIFIED]: "",
  [AnchorKind.USER]: "iw-tick--user",
  [AnchorKind.WAITING]: "iw-tick--waiting",
  [AnchorKind.FAILED]: "iw-tick--failed",
};

export interface AnchorRailProps {
  anchors: AnchorsView;
  currentKey?: string;
  onJump: (key: string) => void;
  /** 预览到离窗口边缘不远、那一侧还有：请 Rust 以这一处为中心重取。 */
  onBrowse: (key: string) => void;
}

const label = (a: AnchorItem) => `${a.kindLabel} ${a.timeLabel} ${a.preview}`;

export function AnchorRail(props: AnchorRailProps) {
  const [open, setOpen] = createSignal(false);
  const [previewKey, setPreviewKey] = createSignal<string>();
  const [bounce, setBounce] = createSignal<"start" | "end">();
  let wheel = INITIAL_WHEEL;
  let closeTimer: ReturnType<typeof setTimeout> | undefined;
  let lastBrowsed: string | undefined;
  onCleanup(() => clearTimeout(closeTimer));

  const items = () => props.anchors.items;
  const indexOf = (key: string | undefined) => (key === undefined ? -1 : items().findIndex((a) => a.key === key));
  // 预览处：展开时从当前位置开始；新列表到了按 key 对回去，对不上就回到当前位置
  const index = createMemo(() => {
    const i = indexOf(previewKey());
    if (i >= 0) return i;
    const c = indexOf(props.currentKey);
    return c >= 0 ? c : items().length - 1;
  });
  const visible = createMemo(() => {
    const at = index();
    return items()
      .map((a, i) => ({ a, d: i - at }))
      .filter(({ d }) => Math.abs(d) <= TICKS_EACH_SIDE);
  });
  const center = () => items()[index()];

  const show = () => {
    clearTimeout(closeTimer);
    if (!open()) {
      setPreviewKey(props.currentKey);
      wheel = INITIAL_WHEEL;
    }
    setOpen(true);
  };
  const close = () => {
    clearTimeout(closeTimer);
    setOpen(false);
    setBounce(undefined);
  };
  const jump = (key: string) => {
    close();
    props.onJump(key);
  };

  const move = (step: number) => {
    if (step === 0 || items().length === 0) return;
    const m = moveWithin(index(), step, items().length);
    setBounce(m.bounce);
    const a = items()[m.index];
    setPreviewKey(a.key);
    if (nearEdge(m.index, items().length, props.anchors.hasBefore, props.anchors.hasAfter) && lastBrowsed !== a.key) {
      lastBrowsed = a.key;
      props.onBrowse(a.key);
    }
  };

  const onWheel = (e: WheelEvent) => {
    if (!open()) return;
    e.preventDefault();
    const r = wheelStep(wheel, { deltaY: e.deltaY, deltaMode: e.deltaMode, now: performance.now() });
    wheel = r.state;
    move(r.step);
  };

  const onKey = (e: KeyboardEvent) => {
    if (!open()) return;
    if (e.key === "Escape") {
      e.stopPropagation();
      close();
    } else if (e.key === "ArrowUp" || e.key === "ArrowDown") {
      e.preventDefault();
      move(e.key === "ArrowUp" ? -1 : 1);
    } else if (e.key === "Enter" && center()) {
      e.preventDefault();
      jump(center()!.key);
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
      onWheel={onWheel}
    >
      <Show
        when={open()}
        fallback={
          <div class="iw-anchor__rest" aria-hidden="true">
            <span />
            <span />
            <span />
            <Show when={props.anchors.hasWaiting}>
              <i class="iw-anchor__dot iw-anchor__dot--waiting" />
            </Show>
            <Show when={props.anchors.hasFailed}>
              <i class="iw-anchor__dot iw-anchor__dot--failed" />
            </Show>
          </div>
        }
      >
        <div class={`iw-anchor__wheel${bounce() ? ` iw-anchor__wheel--bounce-${bounce()}` : ""}`}>
          <For each={visible()}>
            {({ a, d }) => (
              <button
                type="button"
                class={`iw-tick ${KIND_CLASS[a.kind]}${d === 0 ? " iw-tick--center" : ""}`}
                data-distance={Math.abs(d)}
                style={{ "--d": Math.abs(d), "--off": d }}
                aria-label={label(a)}
                aria-current={d === 0 ? "true" : undefined}
                onClick={() => jump(a.key)}
              />
            )}
          </For>
          <Show when={center()}>
            {(a) => (
              <button type="button" class="iw-anchor__card" aria-label={label(a())} onClick={() => jump(a().key)}>
                <span class="iw-anchor__card-head">
                  <span class={`iw-anchor__kind ${KIND_CLASS[a().kind]}`}>{a().kindLabel}</span>
                  <span class="iw-anchor__time">{a().timeLabel}</span>
                </span>
                <span class="iw-anchor__preview">{a().preview}</span>
              </button>
            )}
          </Show>
        </div>
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
