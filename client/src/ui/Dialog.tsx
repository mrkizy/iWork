// 确认框：标题、说明、取消与确认。Esc 与点遮罩等于取消；打开时焦点落在取消上（确认是有后果的那一个）。
// 确认框属于安全与严肃类界面，不接任何轻松元素（ui-m1.md 3.4）。关闭后焦点回到打开它的控件（ui/focus）。
// 表单弹窗（form）右上角另有「×」（画布 13-new），等于取消；受理被拒的原因（error）显示在按钮上方，弹窗留着。
import { onCleanup, onMount, Show, type JSX } from "solid-js";
import { Portal } from "solid-js/web";
import { Button, IconButton, type ButtonVariant } from "./Button";
import { rememberFocus } from "./focus";
import "./Dialog.css";

export function ConfirmDialog(props: {
  title: string;
  children: JSX.Element;
  confirmLabel: string;
  cancelLabel?: string;
  confirmVariant?: ButtonVariant;
  confirmDisabled?: boolean;
  /** 表单弹窗（新建助手等）：不是警示确认，读屏按普通对话框读，打开时焦点落在第一个输入框。 */
  form?: boolean;
  /** 上一次提交被拒的原因（后台给的）。 */
  error?: string;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  let cancel!: HTMLButtonElement;
  let box!: HTMLDivElement;
  const restoreFocus = rememberFocus();
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      props.onCancel();
    }
  };
  onMount(() => {
    document.addEventListener("keydown", onKey, true);
    const first = props.form ? box.querySelector<HTMLElement>("input, textarea, select") : null;
    (first ?? cancel).focus();
  });
  onCleanup(() => {
    document.removeEventListener("keydown", onKey, true);
    restoreFocus(box);
  });
  return (
    <Portal>
      <div class="iw-dialog-scrim" onClick={(e) => e.target === e.currentTarget && props.onCancel()}>
        <div
          ref={box}
          class={`iw-dialog${props.form ? " iw-dialog--form" : ""}`}
          role={props.form ? "dialog" : "alertdialog"}
          aria-modal="true"
          aria-label={props.title}
        >
          <Show when={props.form} fallback={<h2 class="iw-dialog__title">{props.title}</h2>}>
            <header class="iw-dialog__head">
              <h2 class="iw-dialog__title">{props.title}</h2>
              <IconButton icon="close" label="关闭" onClick={() => props.onCancel()} />
            </header>
          </Show>
          <div class="iw-dialog__body">{props.children}</div>
          <Show when={props.error}>
            {(e) => (
              <p class="iw-dialog__error" role="alert">
                {e()}
              </p>
            )}
          </Show>
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
