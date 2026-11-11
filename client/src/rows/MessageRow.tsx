// 消息行：用户（右侧气泡）、助手（头像 + 名字 + 正文）、系统。正文按 Markdown 渲染（不可信输入，见 ui/Markdown）。
// 「这一轮没能开始 / 中途失败」的说明行与「详情」（ui-m1.md 2a 第 10 条）文字都由 Rust 给。
import { createSignal, For, Match, Show, Switch } from "solid-js";
import { Speaker, type MessageRow as Row } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Avatar } from "../ui/Avatar";
import { Icon } from "../ui/Icon";
import { Markdown } from "../ui/Markdown";
import "./rows.css";

export function MessageRow(props: { row: Row }) {
  const host = useHost();
  const m = () => props.row;
  const open = (url: string) => void host.act({ case: "openExternal", value: { url } });
  const Body = () => (
    <Show when={!m().bubbleHidden}>
      <Show when={!m().bodyPending} fallback={<span class="iw-msg__pending">正在读取正文…</span>}>
        <Markdown class="iw-msg__body" text={m().text} onOpenLink={open} />
      </Show>
      <Show when={m().streaming}>
        <span class="iw-dots" aria-label="正在输出">
          <i />
          <i />
          <i />
        </span>
      </Show>
    </Show>
  );
  return (
    <Switch>
      <Match when={m().speaker === Speaker.USER}>
        <div class="iw-row iw-msg iw-msg--user">
          <div class="iw-msg__bubble">
            <Body />
          </div>
          <Attachments row={m()} />
        </div>
      </Match>
      <Match when={true}>
        <div class={`iw-row iw-msg${m().speaker === Speaker.SYSTEM ? " iw-msg--system" : ""}`}>
          <Show when={!m().bubbleHidden}>
            <div class="iw-msg__head">
              <Show when={m().avatar}>
                <Avatar avatar={m().avatar} expression={m().expression} size={22} interactive label={m().authorName} />
              </Show>
              <span>{m().authorName}</span>
              <Show when={m().statusLabel}>
                <span class="iw-msg__status">{m().statusLabel}</span>
              </Show>
            </div>
          </Show>
          <Body />
          <Attachments row={m()} />
          <Show when={m().failure}>{(f) => <FailureLine text={f().text} detail={f().detail} />}</Show>
        </div>
      </Match>
    </Switch>
  );
}

function Attachments(props: { row: Row }) {
  return (
    <Show when={props.row.attachments.length > 0}>
      <div class="iw-msg__attachments">
        <For each={props.row.attachments}>
          {(a) => (
            <span class="iw-chip">
              <Icon name="file" size={14} />
              {a.name}
              <span class="iw-chip__size">{a.sizeLabel}</span>
            </span>
          )}
        </For>
      </div>
    </Show>
  );
}

export function FailureLine(props: { text: string; detail: string }) {
  const [open, setOpen] = createSignal(false);
  return (
    <div class="iw-failure" role="alert">
      <Icon name="alert" size={16} />
      <span>{props.text}</span>
      <button type="button" class="iw-btn iw-btn--link" aria-expanded={open()} onClick={() => setOpen(!open())}>
        详情
      </button>
      <Show when={open()}>
        <div class="iw-failure__detail">{props.detail}</div>
      </Show>
    </div>
  );
}
