// 回合结束行：有改动时是一张改动摘要卡（「改了 N 个文件 +x −y」，点开看文件），后台判定这一轮正常结束又有改动时
// 盖「搞定」印章（ui-m1.md 3.4，R9：只有这一行带 celebrate）；失败、取消写一行原因。都没有就不占地方。
// 轻松元素关掉时印章不渲染（不只是藏起来）；减少动态效果时 motion.css 停掉盖章动画。
import { createSignal, For, Show } from "solid-js";
import type { TurnEndRow as Row } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Icon } from "../ui/Icon";
import "./cards.css";

export function TurnEndRow(props: { row: Row }) {
  const host = useHost();
  const r = () => props.row;
  const [open, setOpen] = createSignal(false);
  const playful = () => host.view.settings?.playfulUi ?? true;
  return (
    <>
      <Show when={r().summaryLine}>
        <div class="iw-row">
          <section class="iw-card iw-card--changes" aria-label="这一轮的改动">
            <button
              type="button"
              class="iw-changes__line"
              aria-expanded={open()}
              disabled={r().paths.length === 0}
              onClick={() => setOpen(!open())}
            >
              <Icon name={open() ? "chevronDown" : "chevronRight"} size={12} />
              <span>{r().summaryLine}</span>
            </button>
            <Show when={r().celebrate && playful()}>
              <span class="iw-stamp iw-changes__stamp">搞定</span>
            </Show>
            <Show when={open()}>
              <ul class="iw-changes__paths">
                <For each={r().paths}>{(p) => <li>{p}</li>}</For>
              </ul>
            </Show>
          </section>
        </div>
      </Show>
      <Show when={r().outcomeLabel}>
        <div class={`iw-row iw-note${r().failed ? " iw-note--failed" : ""}`}>{r().outcomeLabel}</div>
      </Show>
    </>
  );
}
