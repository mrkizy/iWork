// 小件：未读徽标、状态圆点、转圈。数字、状态与状态名全由 Rust 侧给（ui-m1.md 第 2 节第 4 条、2a 第 1 条）。
import { Show } from "solid-js";
import { DotState } from "../gen/iwork/v1/view_pb";
import "./Badge.css";

/** 未读徽标：0 不显示。 */
export function Badge(props: { count: number; label?: string }) {
  return (
    <Show when={props.count > 0}>
      <span class="iw-badge" aria-label={props.label ?? `${props.count} 条未读`}>
        {props.count}
      </span>
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
