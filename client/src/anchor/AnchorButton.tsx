// 消息锚点，窄形态（ui-m1.md 2a 第 11 条「窄形态」）：主区域窄到放不下导轨时，锚点进对话标题栏最右端的 32×32 按钮。
// 图标是三条短线，静止态的状态点画在按钮右上角（规则同导轨静止态）。点按钮在它下方弹出面板（右对齐，约 312px：
// 左边刻度滚轮，右边信息卡），滚轮、点击跳转、翻看规则与宽形态的展开态完全相同（同一份 AnchorWheel）。
// Esc、点面板外面、跳转之后都收起，焦点回到按钮。窄形态下不靠悬停展开。
import { createEffect, createSignal, onCleanup, Show } from "solid-js";
import type { AnchorsView } from "../gen/iwork/v1/view_pb";
import { AnchorRest, AnchorWheel, createAnchorBrowse } from "./AnchorWheel";
import "./AnchorRail.css";

export interface AnchorButtonProps {
  anchors: AnchorsView;
  currentKey?: string;
  onJump: (key: string) => void;
  onBrowse: (key: string) => void;
}

export function AnchorButton(props: AnchorButtonProps) {
  const [open, setOpen] = createSignal(false);
  const browse = createAnchorBrowse(props);
  let button!: HTMLButtonElement;
  let panel: HTMLDivElement | undefined;

  const close = () => {
    setOpen(false);
    browse.clearBounce();
    button.focus();
  };
  const toggle = () => {
    if (open()) {
      close();
      return;
    }
    browse.reset();
    setOpen(true);
  };
  const jump = (key: string) => {
    close();
    props.onJump(key);
  };

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      close();
    } else {
      browse.onKey(e, jump);
    }
  };
  // 点面板外面收起（按钮自己的点击由 toggle 处理）
  const onPointer = (e: PointerEvent) => {
    const t = e.target as Node | null;
    if (t && (panel?.contains(t) || button.contains(t))) return;
    close();
  };
  createEffect(() => {
    if (!open()) return;
    document.addEventListener("keydown", onKey, true);
    document.addEventListener("pointerdown", onPointer, true);
    onCleanup(() => {
      document.removeEventListener("keydown", onKey, true);
      document.removeEventListener("pointerdown", onPointer, true);
    });
  });

  return (
    <div class="iw-anchor-btn-wrap">
      <button
        ref={button}
        type="button"
        class={`iw-anchor-btn${open() ? " iw-anchor-btn--open" : ""}`}
        aria-label="消息锚点"
        aria-expanded={open()}
        aria-haspopup="dialog"
        onClick={toggle}
      >
        <AnchorRest anchors={props.anchors} class="iw-anchor-btn__icon" />
      </button>
      <Show when={open()}>
        <div ref={panel} class="iw-anchor-panel" role="dialog" aria-label="消息锚点" onWheel={(e) => browse.onWheel(e)}>
          <AnchorWheel browse={browse} onJump={jump} />
          <Show when={props.anchors.jumpLoading}>
            {(t) => (
              <div class="iw-anchor__loading" role="status">
                {t()}
              </div>
            )}
          </Show>
        </div>
      </Show>
    </div>
  );
}
