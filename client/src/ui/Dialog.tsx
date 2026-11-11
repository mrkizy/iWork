// 确认框：标题、说明、取消与确认。Esc 与点遮罩等于取消；打开时焦点落在取消上（确认是有后果的那一个）。
// 确认框属于安全与严肃类界面，不接任何轻松元素（ui-m1.md 3.4）。
import { onCleanup, onMount, type JSX } from "solid-js";
import { Portal } from "solid-js/web";
import { Button, type ButtonVariant } from "./Button";
import "./Dialog.css";

export function ConfirmDialog(props: {
  title: string;
  children: JSX.Element;
  confirmLabel: string;
  cancelLabel?: string;
  confirmVariant?: ButtonVariant;
  confirmDisabled?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  let cancel!: HTMLButtonElement;
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      props.onCancel();
    }
  };
  onMount(() => {
    document.addEventListener("keydown", onKey, true);
    cancel.focus();
  });
  onCleanup(() => document.removeEventListener("keydown", onKey, true));
  return (
    <Portal>
      <div class="iw-dialog-scrim" onClick={(e) => e.target === e.currentTarget && props.onCancel()}>
        <div class="iw-dialog" role="alertdialog" aria-modal="true" aria-label={props.title}>
          <h2 class="iw-dialog__title">{props.title}</h2>
          <div class="iw-dialog__body">{props.children}</div>
          <div class="iw-dialog__actions">
            <Button ref={cancel} onClick={() => props.onCancel()}>
              {props.cancelLabel ?? "取消"}
            </Button>
            <Button variant={props.confirmVariant ?? "primary"} disabled={props.confirmDisabled} onClick={() => props.onConfirm()}>
              {props.confirmLabel}
            </Button>
          </div>
        </div>
      </div>
    </Portal>
  );
}
