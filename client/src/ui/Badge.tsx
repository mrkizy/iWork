// 小件：未读徽标、状态圆点、转圈。数字、状态与状态名全由 Rust 侧给（ui-m1.md 第 2 节第 4 条、2a 第 1 条）。
import { Show } from "solid-js";
import { DotState, type CountBadge } from "../gen/iwork/v1/view_pb";
import "./Badge.css";

/** 数字徽标：文字（含「99+」）与读屏标签都由 Rust 给；没有就不画。 */
export function Badge(props: { badge?: CountBadge }) {
  return (
    <Show when={props.badge}>
      {(b) => (
        <span class="iw-badge" aria-label={b().label}>
          {b().text}
        </span>
      )}
    </Show>
  );
}

// 圆点状态 → 样式类（只管颜色与动画；状态本身与状态名由 Rust 侧给）。
const DOT_CLASS: Record<DotState, string> = {
  [DotState.UNSPECIFIED]: "",
  [DotState.IDLE]: "",
  [DotState.RUNNING]: " iw-dot--running",
  [DotState.ERROR]: " iw-dot--error",
  [DotState.WAITING]: " iw-dot--waiting",
};

export function Dot(props: { state: DotState; label: string }) {
  return <span class={`iw-dot${DOT_CLASS[props.state]}`} role="img" aria-label={props.label} data-tip={props.label} />;
}

export function Spinner(props: { label?: string }) {
  return <span class="iw-spinner" role="status" aria-label={props.label ?? "进行中"} />;
}
