// 消息锚点的翻看状态与刻度滚轮（ui-m1.md 2a 第 11 条）。宽形态的导轨（AnchorRail）与窄形态的标题栏按钮面板
// （AnchorButton）共用这一份：两种形态的滚轮、点击跳转、翻看规则完全相同，只是怎么展开、怎么收起不同。
// 哪些算锚点、文字、当前位置、前后还有没有都来自 patch；这里只有展示状态：正在预览哪一处（按 key 记，新列表到了按
// key 对回去）、滚轮累计量、回弹。
import { createMemo, createSignal, For, Show } from "solid-js";
import { AnchorKind, type AnchorItem, type AnchorsView } from "../gen/iwork/v1/view_pb";
import { INITIAL_WHEEL, moveWithin, nearEdge, wheelStep } from "./wheel";

/** 中间之外上下各显示几条刻度。 */
export const TICKS_EACH_SIDE = 4;

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
  /** 预览到离窗口边缘不远、那一侧还有：请 Rust 以这一处为中心重取。 */
  onBrowse: (key: string) => void;
}

/** 翻看状态：展开时 `reset` 回到当前位置；`move` 换一处；`onWheel` 把滚轮换算成步数。 */
export function createAnchorBrowse(props: AnchorSource) {
  const [previewKey, setPreviewKey] = createSignal<string>();
  const [bounce, setBounce] = createSignal<"start" | "end">();
  let wheel = INITIAL_WHEEL;
  let lastBrowsed: string | undefined;

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

  return {
    visible,
    center,
    bounce,
    move,
    reset() {
      setPreviewKey(props.currentKey);
      setBounce(undefined);
      wheel = INITIAL_WHEEL;
    },
    clearBounce: () => setBounce(undefined),
    onWheel(e: WheelEvent) {
      e.preventDefault();
      const r = wheelStep(wheel, { deltaY: e.deltaY, deltaMode: e.deltaMode, now: performance.now() });
      wheel = r.state;
      move(r.step);
    },
    /** 展开时的按键：↑ / ↓ 换一处，Enter 跳到中间那一处。处理了返回真。 */
    onKey(e: KeyboardEvent, jump: (key: string) => void): boolean {
      if (e.key === "ArrowUp" || e.key === "ArrowDown") {
        e.preventDefault();
        move(e.key === "ArrowUp" ? -1 : 1);
        return true;
      }
      if (e.key === "Enter" && center()) {
        e.preventDefault();
        jump(center()!.key);
        return true;
      }
      return false;
    },
  };
}

export type AnchorBrowse = ReturnType<typeof createAnchorBrowse>;

/** 展开态：以预览处为中心上下各最多 4 条刻度，中间那条旁是信息卡。点刻度或信息卡就跳。 */
export function AnchorWheel(props: { browse: AnchorBrowse; onJump: (key: string) => void }) {
  return (
    <div class={`iw-anchor__wheel${props.browse.bounce() ? ` iw-anchor__wheel--bounce-${props.browse.bounce()}` : ""}`}>
      <For each={props.browse.visible()}>
        {({ a, d }) => (
          <button
            type="button"
            class={`iw-tick ${KIND_CLASS[a.kind]}${d === 0 ? " iw-tick--center" : ""}`}
            data-distance={Math.abs(d)}
            style={{ "--d": Math.abs(d), "--off": d }}
            aria-label={anchorLabel(a)}
            aria-current={d === 0 ? "true" : undefined}
            onClick={() => props.onJump(a.key)}
          />
        )}
      </For>
      <Show when={props.browse.center()}>
        {(a) => (
          <button type="button" class="iw-anchor__card" aria-label={anchorLabel(a())} onClick={() => props.onJump(a().key)}>
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
