// 操作被拒绝时的短提示（IntentAck 带错误，方案 S8）。文字原样用 Rust 侧给的 message。
import { createSignal, For } from "solid-js";
import { Portal } from "solid-js/web";
import "./Toast.css";

const TOAST_MS = 4000;

export function createToaster() {
  const [items, setItems] = createSignal<{ id: number; text: string }[]>([]);
  let next = 0;
  const show = (text: string) => {
    const id = ++next;
    setItems((v) => [...v, { id, text }]);
    setTimeout(() => setItems((v) => v.filter((t) => t.id !== id)), TOAST_MS);
  };
  const View = () => (
    <Portal>
      <div class="iw-toasts" role="status" aria-live="polite">
        <For each={items()}>{(t) => <div class="iw-toast">{t.text}</div>}</For>
      </div>
    </Portal>
  );
  return { show, View };
}
