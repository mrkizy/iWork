// 就地编辑标题：回车与失焦确认、Esc 取消；输入法组字期间不截断（ui-m1.md 2a 第 6 条）。
import { createSignal, onMount } from "solid-js";
import { clampTitle, renameOutcome } from "./rename";

export function InlineRename(props: { original: string; onSubmit: (title: string) => void; onDone: () => void }) {
  let input!: HTMLInputElement;
  const [value, setValue] = createSignal(props.original);
  let composing = false;
  let finished = false;
  const finish = (commit: boolean) => {
    if (finished) return;
    finished = true;
    if (commit) {
      const r = renameOutcome(props.original, value());
      if (r.kind === "submit") props.onSubmit(r.title);
    }
    props.onDone();
  };
  onMount(() => {
    input.focus();
    input.select();
  });
  return (
    <input
      ref={input}
      class="iw-rename"
      aria-label="重命名对话"
      value={value()}
      onCompositionStart={() => (composing = true)}
      onCompositionEnd={(e) => {
        composing = false;
        const v = clampTitle(e.currentTarget.value);
        e.currentTarget.value = v;
        setValue(v);
      }}
      onInput={(e) => {
        const v = composing ? e.currentTarget.value : clampTitle(e.currentTarget.value);
        if (v !== e.currentTarget.value) e.currentTarget.value = v;
        setValue(v);
      }}
      onKeyDown={(e) => {
        if (e.isComposing) return;
        if (e.key === "Enter") {
          e.preventDefault();
          finish(true);
        } else if (e.key === "Escape") {
          e.preventDefault();
          e.stopPropagation();
          finish(false);
        }
      }}
      onBlur={() => finish(true)}
      onClick={(e) => e.stopPropagation()}
    />
  );
}
