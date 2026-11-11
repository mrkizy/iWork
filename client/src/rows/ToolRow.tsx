// 工具活动折叠行（画布 04）：「▸ 标题 摘要 …… 状态」，点开看输出（输出当文字显示，不解析）。
import { createSignal, Show } from "solid-js";
import type { ToolRow as Row } from "../gen/iwork/v1/view_pb";
import { Spinner } from "../ui/Badge";
import { Icon } from "../ui/Icon";
import "./rows.css";

export function ToolRow(props: { row: Row }) {
  const [open, setOpen] = createSignal(false);
  const t = () => props.row;
  return (
    <div class={`iw-row iw-tool${t().failed ? " iw-tool--failed" : ""}`}>
      <button type="button" class="iw-tool__line" aria-expanded={open()} onClick={() => setOpen(!open())}>
        <Icon name={open() ? "chevronDown" : "chevronRight"} size={12} />
        <span class="iw-tool__title">{t().title}</span>
        <span class="iw-tool__summary">{t().summary}</span>
        <Show when={t().running}>
          <Spinner label={t().statusLabel || "进行中"} />
        </Show>
        <span class="iw-tool__status">{t().statusLabel}</span>
      </button>
      <Show when={open() && t().output}>
        <pre class="iw-tool__output">{t().output}</pre>
      </Show>
    </div>
  );
}
