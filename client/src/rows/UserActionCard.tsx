// 亲手执行卡片（ui-m1.md 第 2 节第 12 条，画布 15-UserAction-*）：系统底线拦下的动作，由你来执行。
// 标题「需要你亲手执行」、理由、等宽的命令（或补丁 diff）与目标；待定时「执行」「复制命令」「忽略」；执行中可停；
// 结果未知时「我已核对」。执行后显示状态与退出码、可展开的输出（只给你看，不回给助手）。
// 没有记住选项，全开档也出现；安全类卡片，不接任何轻松元素（3.4）。可否点击、状态文字都来自 patch。
import { createSignal, Show } from "solid-js";
import type { UserActionRow } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Spinner, Tag } from "../ui/Badge";
import { Button } from "../ui/Button";
import { Icon } from "../ui/Icon";
import "./cards.css";

export function UserActionCard(props: { row: UserActionRow }) {
  const host = useHost();
  const r = () => props.row;
  const [outputOpen, setOutputOpen] = createSignal(false);
  const card = (k: "executeUserAction" | "dismissUserAction" | "stopUserAction" | "confirmUserActionEnded") =>
    void host.act({ case: k, value: { cardId: r().cardId } });
  // 剪贴板是网页本地的事（方案 2.5）；复制的就是卡片上显示的命令。
  const copy = () => void navigator.clipboard?.writeText(r().command);
  // 待定（Rust 不给状态文字）才出三个按钮；断线时按钮还在、不可点。其余状态出结果一行
  const pending = () => !r().stateLabel;
  return (
    <div class="iw-row">
      <section class={`iw-card iw-card--action${r().muted ? " iw-card--muted" : ""}`} aria-label="需要你亲手执行">
        <header class="iw-card__head">
          <span class="iw-card__title">需要你亲手执行</span>
          <span class="iw-card__aside">不能记住 · 全开档也会出现</span>
        </header>
        <p class="iw-card__reason">{r().reason}</p>
        <div class="iw-card__codebox">
          <div class="iw-card__codehead">
            <Show when={r().command} fallback={<span>补丁 · {r().patchPath}</span>}>
              <span>命令 · 在 {r().cwd} 执行</span>
            </Show>
          </div>
          <pre class="iw-card__code iw-card__code--flat">{r().command || r().diff}</pre>
        </div>
        <Show when={r().notice}>{(n) => <p class="iw-card__notice">{n()}</p>}</Show>
        <Show when={pending()}>
          <div class="iw-card__actions">
            <Button variant="primary" disabled={!r().canExecute} onClick={() => card("executeUserAction")}>
              执行
            </Button>
            <Show when={r().canCopy}>
              <Button onClick={copy}>
                <Icon name="copy" size={14} />
                复制命令
              </Button>
            </Show>
            <Button disabled={!r().canDismiss} onClick={() => card("dismissUserAction")}>
              忽略
            </Button>
            <span class="iw-card__aside">由 iWork 以你的身份执行，不经助手、不进沙箱</span>
          </div>
        </Show>
        <Show when={!pending()}>
          <div class="iw-card__result">
            <Tag tone={r().succeeded ? "ok" : undefined}>{[r().stateLabel, r().exitLabel].filter(Boolean).join(" · ")}</Tag>
            <Show when={r().canStop}>
              <Spinner label={r().stateLabel} />
              <Button onClick={() => card("stopUserAction")}>停止</Button>
            </Show>
            <Show when={r().canConfirmEnded}>
              <Button onClick={() => card("confirmUserActionEnded")}>我已核对</Button>
            </Show>
          </div>
        </Show>
        <Show when={r().output}>
          <button
            type="button"
            class="iw-card__output-toggle"
            aria-expanded={outputOpen()}
            onClick={() => setOutputOpen(!outputOpen())}
          >
            <Icon name={outputOpen() ? "chevronDown" : "chevronRight"} size={12} />
            输出（只给你看，不回给助手）
          </button>
          <Show when={outputOpen()}>
            <pre class="iw-card__output">{r().output}</pre>
          </Show>
        </Show>
      </section>
    </div>
  );
}
