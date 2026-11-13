// 开关（画布 13 的工具开关、设置页的轻松元素与登录自启）。开没开由调用方按 Rust 给的值传进来；点了只回调，
// 不在本地翻转（等下一帧，方案 2.5）。
import "./Switch.css";

export function Switch(props: { checked: boolean; label: string; disabled?: boolean; onChange: (on: boolean) => void }) {
  return (
    <button
      type="button"
      role="switch"
      class="iw-switch"
      aria-checked={props.checked}
      aria-label={props.label}
      disabled={props.disabled}
      onClick={() => props.onChange(!props.checked)}
    >
      <span class="iw-switch__knob" />
    </button>
  );
}
