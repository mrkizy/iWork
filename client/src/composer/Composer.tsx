// 输入框一带（画布 01、04、10）：发不了时上方一行说明、附件托盘、正文、工具栏（附件、运行时 · 模型、审批档位、发送 / 停止）。
// 可否发送、说明文字、按钮上的字都来自 ComposerView；这里只有草稿、菜单开合与输入法状态这些本地状态。
// 首页和对话里是同一个输入框（ui-m1.md 第 2 节第 9、14 条），只是对着的目标不同（composer/target）。
import { createEffect, createSignal, For, on, Show } from "solid-js";
import { BlockAction, Tone, type ComposerView } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Spinner } from "../ui/Badge";
import { Icon } from "../ui/Icon";
import { anchorAbove, type MenuAnchor } from "../ui/Menu";
import { ApprovalModeMenu } from "./ApprovalModeMenu";
import { getDraft, setDraft } from "./drafts";
import { ModelMenu } from "./ModelMenu";
import { sendIntent, type ComposerTarget } from "./target";
import "./Composer.css";
import "../rows/rows.css";

export interface ComposerProps {
  target: ComposerTarget;
  composer?: ComposerView;
  /** 首页：正在新建对话并发出，发送按钮转圈。 */
  starting?: boolean;
  /** 首页：上一次没能发出去的原因（草稿还在）。 */
  startError?: string;
  draftKey: string;
  /** 这个输入框对着的项目：「重新定位」出口用它（私聊为空）。 */
  projectId?: string;
  /** 「查看详情」出口：去设置 › 运行时。没给就不画这个按钮。 */
  openRuntimeDetails?: () => void;
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
  // 输入框上方那一行的出口：哪一个由 Rust 给（ComposerView.blockAction），这里只接到对应的动作
  const exit = (): (() => void) | undefined => {
    switch (c()?.blockAction) {
      case BlockAction.OPEN_RUNTIME_DETAILS:
        return props.openRuntimeDetails;
      case BlockAction.RELOCATE_PROJECT: {
        const projectId = props.projectId;
        return projectId ? () => void host.act({ case: "pickProjectLocation", value: { projectId } }) : undefined;
      }
      default:
        return undefined;
    }
  };
  const canSend = () => text().trim() !== "" && (c()?.canSend ?? false) && !props.starting;
  const send = async () => {
    if (!canSend()) return;
    const body = text();
    const key = props.draftKey;
    const ack = await host.act(sendIntent(props.target, body, (c()?.tray ?? []).map((a) => a.attachmentId)));
    // 首页的草稿等 Rust 发出首条消息、跳过去时再清（App 按 NavigatePatch.startedProjectId），没发出去就留着。
    if (props.target.kind === "home") return;
    // 受理要等一会儿，期间可能换了对话或接着打字：只清发出的那个对话、且草稿还是发出的那段时才清
    if (ack.error || getDraft(key) !== body) return;
    setDraft(key, "");
    if (props.draftKey === key) setText("");
  };
  const stop = () => {
    const t = c()?.runningTurnId;
    if (t) void host.act({ case: "cancelTurn", value: { turnId: t } });
  };
  // 回形针：系统的文件选择框由 Rust 弹，路径不经网页（设计 3.7）。点不了时 Rust 给原因
  const pickFiles = () => {
    const t = props.target;
    if (t.kind === "conversation") void host.act({ case: "pickAttachments", value: { conversationId: t.id } });
  };
  const onPaste = async (e: ClipboardEvent) => {
    const files = [...(e.clipboardData?.files ?? [])];
    const t = props.target;
    if (files.length === 0 || t.kind !== "conversation") return;
    const id = t.id;
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
            <Show when={c()?.blockActionLabel ? exit() : undefined}>
              {(go) => (
                <button type="button" class="iw-btn iw-btn--link" onClick={() => go()()}>
                  {c()?.blockActionLabel}
                </button>
              )}
            </Show>
          </div>
        )}
      </Show>
      <Show when={props.startError}>
        {(err) => (
          <div class="iw-composer__block iw-composer__block--warning" role="alert">
            <Icon name="alert" size={14} />
            <span>{err()}</span>
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
          placeholder={c()?.placeholder}
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
          <button
            type="button"
            class="iw-pill"
            aria-label="添加附件"
            disabled={c()?.attachUnavailable !== undefined}
            data-tip={c()?.attachUnavailable}
            data-tip-side="top"
            onClick={pickFiles}
          >
            <Icon name="paperclip" />
          </button>
          <Show when={c()}>
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
      <Show when={menu()?.kind === "model" && c()?.modelMenu}>
        {(m) => <ModelMenu target={props.target} menu={m()} anchor={menu()?.at ?? { x: 0, y: 0 }} onClose={() => setMenu(undefined)} />}
      </Show>
      <Show when={menu()?.kind === "mode" && c()?.approvalMode}>
        {(m) => <ApprovalModeMenu target={props.target} mode={m()} anchor={menu()?.at ?? { x: 0, y: 0 }} onClose={() => setMenu(undefined)} />}
      </Show>
    </div>
  );
}
