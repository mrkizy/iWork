// 输入框一带（画布 01、04、10）：发不了时上方一行说明、附件托盘、正文、工具栏（附件、运行时 · 模型、审批档位、发送 / 停止）。
// 可否发送、说明文字、按钮上的字都来自 ComposerView；这里只有草稿、菜单开合与输入法状态这些本地状态。
// 首页（还没有对话）没有 ComposerView：只有正文与发送，发出时先新建对话（shell/starter）。
import { createEffect, createSignal, For, on, Show } from "solid-js";
import { BlockAction, Tone, type ComposerView } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Spinner } from "../ui/Badge";
import { Icon } from "../ui/Icon";
import { anchorAbove, type MenuAnchor } from "../ui/Menu";
import { ApprovalModeMenu } from "./ApprovalModeMenu";
import { getDraft, setDraft } from "./drafts";
import { ModelMenu } from "./ModelMenu";
import "./Composer.css";
import "../rows/rows.css";

/** 回形针要系统的文件选择框（只传路径，设计 3.7），M1 还没接。 */
const PICKER_PENDING = "选择文件要接系统的文件选择框，还没接；可以拖入或粘贴";

export interface ComposerProps {
  /** 没有对话（首页）时为空。 */
  conversationId?: string;
  composer?: ComposerView;
  /** 首页：发出时由调用方新建对话。 */
  onStart?: (text: string) => void;
  /** 首页：正在新建对话，发送按钮转圈。 */
  starting?: boolean;
  draftKey: string;
  placeholder?: string;
  /** 输入框上方那一行的出口（「查看详情」「重新定位」）；返回假表示这个出口现在用不了，按钮置灰。 */
  blockAction?: (a: BlockAction) => (() => void) | undefined;
}

export function Composer(props: ComposerProps) {
  const host = useHost();
  const [text, setText] = createSignal(getDraft(props.draftKey));
  const [menu, setMenu] = createSignal<{ kind: "model" | "mode"; at: MenuAnchor }>();
  let composing = false;

  createEffect(on(() => props.draftKey, (k) => setText(getDraft(k)), { defer: true }));
  const edit = (v: string) => {
    setText(v);
    setDraft(props.draftKey, v);
  };

  const c = () => props.composer;
  const exit = () => props.blockAction?.(c()?.blockAction ?? BlockAction.UNSPECIFIED);
  const canSend = () => text().trim() !== "" && (c()?.canSend ?? !props.starting);
  const send = async () => {
    if (!canSend()) return;
    const body = text();
    if (props.conversationId === undefined) {
      props.onStart?.(body);
      edit("");
      return;
    }
    const ack = await host.act({
      case: "sendMessage",
      value: { conversationId: props.conversationId, text: body, attachmentIds: (c()?.tray ?? []).map((a) => a.attachmentId) },
    });
    if (!ack.error) edit("");
  };
  const stop = () => {
    const t = c()?.runningTurnId;
    if (t) void host.act({ case: "cancelTurn", value: { turnId: t } });
  };
  const onPaste = async (e: ClipboardEvent) => {
    const files = [...(e.clipboardData?.files ?? [])];
    const id = props.conversationId;
    if (files.length === 0 || id === undefined) return;
    e.preventDefault();
    // 大小检查在 Rust 侧（attach.rs，20MB），超限时输入框上方显示原因
    for (const f of files) {
      const data = new Uint8Array(await f.arrayBuffer());
      void host.act({ case: "attachPasted", value: { conversationId: id, name: f.name || "粘贴的图片", mime: f.type, data } });
    }
  };

  return (
    <div class="iw-composer">
      <Show when={c()?.blockReason}>
        {(reason) => (
          <div class={`iw-composer__block${c()?.blockTone === Tone.WARNING ? " iw-composer__block--warning" : ""}`} role="status">
            <Show when={c()?.busy}>
              <Spinner />
            </Show>
            <span>{reason()}</span>
            <Show when={c()?.blockActionLabel}>
              {(label) => (
                <button
                  type="button"
                  class="iw-btn iw-btn--link"
                  disabled={exit() === undefined}
                  data-tip={exit() === undefined ? "需要系统的文件夹选择框，还没接" : undefined}
                  onClick={() => exit()?.()}
                >
                  {label()}
                </button>
              )}
            </Show>
          </div>
        )}
      </Show>
      <Show when={c()?.attachError}>
        {(err) => (
          <div class="iw-composer__block iw-composer__block--warning" role="alert">
            <Icon name="alert" size={14} />
            <span>{err()}</span>
          </div>
        )}
      </Show>
      <div class="iw-composer__box">
        <Show when={(c()?.tray.length ?? 0) > 0}>
          <div class="iw-composer__tray">
            <For each={c()?.tray}>
              {(a) => (
                <span class="iw-chip iw-composer__tray-chip">
                  <Icon name="file" size={14} />
                  {a.name}
                  <span class="iw-chip__size">{a.sizeLabel}</span>
                </span>
              )}
            </For>
          </div>
        </Show>
        <textarea
          class="iw-composer__input"
          aria-label="消息"
          placeholder={props.placeholder}
          value={text()}
          onInput={(e) => edit(e.currentTarget.value)}
          onCompositionStart={() => (composing = true)}
          onCompositionEnd={() => (composing = false)}
          onPaste={(e) => void onPaste(e)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey && !composing && !e.isComposing) {
              e.preventDefault();
              void send();
            }
          }}
        />
        <div class="iw-composer__bar">
          <button type="button" class="iw-pill" disabled aria-label="添加附件" data-tip={PICKER_PENDING} data-tip-side="top">
            <Icon name="paperclip" />
          </button>
          <Show when={props.conversationId !== undefined && c()}>
            {(view) => (
              <>
                <button
                  type="button"
                  class="iw-pill"
                  aria-haspopup="menu"
                  aria-expanded={menu()?.kind === "model"}
                  onClick={(e) => setMenu({ kind: "model", at: anchorAbove(e.currentTarget) })}
                >
                  {view().runtimeModelLabel}
                  <Icon name="chevronDown" size={12} />
                </button>
                <Show when={view().approvalMode}>
                  {(mode) => (
                    <button
                      type="button"
                      class={`iw-pill${mode().fullAccess ? " iw-pill--full" : ""}`}
                      aria-haspopup="menu"
                      aria-expanded={menu()?.kind === "mode"}
                      aria-label={`审批方式：${mode().label}`}
                      onClick={(e) => setMenu({ kind: "mode", at: anchorAbove(e.currentTarget) })}
                    >
                      <Icon name={mode().shieldChecked ? "shieldCheck" : "shield"} size={14} />
                      {mode().label}
                      <Show when={mode().overridden}>
                        <span class="iw-pill__override" data-tip="单独设置，不跟随 iWork">
                          <Icon name="unlink" size={12} />
                        </span>
                      </Show>
                      <Icon name="chevronDown" size={12} />
                    </button>
                  )}
                </Show>
              </>
            )}
          </Show>
          <span class="iw-composer__spacer" />
          <Show
            when={c()?.canStop}
            fallback={
              <button type="button" class="iw-send" aria-label="发送" data-tip="发送" data-tip-side="top" disabled={!canSend()} onClick={() => void send()}>
                {props.starting ? <Spinner label="正在新建对话" /> : <Icon name="arrowUp" />}
              </button>
            }
          >
            <button type="button" class="iw-send iw-send--stop" aria-label="停止" data-tip="停止" data-tip-side="top" onClick={stop}>
              <Icon name="stop" />
            </button>
          </Show>
        </div>
      </div>
      <Show when={menu()?.kind === "model" && props.conversationId !== undefined && c()?.modelMenu}>
        {(m) => <ModelMenu conversationId={props.conversationId ?? ""} menu={m()} anchor={menu()?.at ?? { x: 0, y: 0 }} onClose={() => setMenu(undefined)} />}
      </Show>
      <Show when={menu()?.kind === "mode" && props.conversationId !== undefined && c()?.approvalMode}>
        {(m) => <ApprovalModeMenu conversationId={props.conversationId ?? ""} mode={m()} anchor={menu()?.at ?? { x: 0, y: 0 }} onClose={() => setMenu(undefined)} />}
      </Show>
    </div>
  );
}
