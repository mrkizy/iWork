// 助手主页（画布 09、13；需求 FR03）：左边是和它的私聊（同一个对话页），右边是资料栏——头像、名字、
// 「运行时 · 模型」，三个页签：动态、记忆（M1 只显示空态说明，D3）、配置。
// 配置改了就自动保存：网页只说「改了哪一项」（UpdateAssistantField），合并与版本由 Rust 的闸管
// （client-core store::assistant_edits）；显示的值来自 patch（摘要 + 还没落定的改动）。
// 文字框失焦或回车才提交、Esc 还原，编辑中不被下一帧覆盖；下拉与开关一改就提交。
// 资料栏（ui-m1 2a 第 18 条 d）：主区域放得下（≥ 924，回差见 profileLayout）且用户没手动收起时常驻；放不下时收起，
// 标题栏的「助手资料」按钮点开一块盖在对话上的浮层（Esc、点外面收起，变宽到放得下就回到常驻）。放得下时同一个按钮
// 手动收起 / 展开，这个选择是后台的全局偏好（SetProfilePanelCollapsed），不只记在网页里。
import { createEffect, createSignal, For, Match, onCleanup, onMount, Show, Switch as Choose } from "solid-js";
import type { AssistantProfileView } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import type { AssistantTab, Route } from "../shell/route";
import { Avatar } from "../ui/Avatar";
import { Icon } from "../ui/Icon";
import { Switch } from "../ui/Switch";
import { ConversationPage } from "./Conversation";
import { ModelSelects, pickedFrom } from "./AssistantModelSelects";
import { nextRoomy } from "./profileLayout";
import "./assistant.css";

/** 配置页改的一项（UpdateAssistantFieldIntent.field）。 */
type AssistantField =
  | { case: "name"; value: string }
  | { case: "prompt"; value: string }
  | { case: "model"; value: { modelId: string; effort?: string } }
  | { case: "tool"; value: { group: number; enabled: boolean } }
  | { case: "avatarColor"; value: string };

const TABS: { tab: AssistantTab; label: string }[] = [
  { tab: "activity", label: "动态" },
  { tab: "memory", label: "记忆" },
  { tab: "config", label: "配置" },
];

export function AssistantPage(props: {
  route: Extract<Route, { page: "assistant" }>;
  go: (r: Route) => void;
  openRuntimeDetails: () => void;
}) {
  const host = useHost();
  // 切换助手的那一刻 store 里可能还是上一个：只认 id 对得上的
  const view = () => (host.view.assistant?.assistantId === props.route.id ? host.view.assistant : undefined);

  // 主区域放不放得下资料栏：布局事实，网页自己量
  const [roomy, setRoomy] = createSignal(true);
  // 放不下时点开的浮层
  const [overlay, setOverlay] = createSignal(false);
  let page!: HTMLDivElement;
  let button!: HTMLButtonElement;
  let panel: HTMLElement | undefined;
  onMount(() => {
    const observer = new ResizeObserver((entries) => {
      const width = entries[0]?.contentRect.width;
      if (width !== undefined && width > 0) setRoomy((was) => nextRoomy(was, width));
    });
    observer.observe(page);
    onCleanup(() => observer.disconnect());
  });
  // 变宽到放得下：浮层收掉，回到常驻（或用户收起的状态）
  createEffect(() => {
    if (roomy()) setOverlay(false);
  });
  const userCollapsed = () => view()?.profilePanelCollapsed ?? false;
  const docked = () => roomy() && !userCollapsed();
  const shown = () => docked() || overlay();

  const closeOverlay = () => {
    setOverlay(false);
    button.focus();
  };
  const toggle = () => {
    if (roomy()) void host.act({ case: "setProfilePanelCollapsed", value: { on: !userCollapsed() } });
    else setOverlay((o) => !o);
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key !== "Escape") return;
    e.stopPropagation();
    closeOverlay();
  };
  const onPointer = (e: PointerEvent) => {
    const t = e.target as Node | null;
    if (t && (panel?.contains(t) || button.contains(t))) return;
    closeOverlay();
  };
  createEffect(() => {
    if (!overlay()) return;
    document.addEventListener("keydown", onKey, true);
    document.addEventListener("pointerdown", onPointer, true);
    onCleanup(() => {
      document.removeEventListener("keydown", onKey, true);
      document.removeEventListener("pointerdown", onPointer, true);
    });
  });

  const profileButton = (
    <button
      ref={button}
      type="button"
      class={`iw-profile-btn${shown() ? " iw-profile-btn--open" : ""}`}
      aria-label="助手资料"
      aria-expanded={shown()}
      aria-controls="iw-profile"
      onClick={toggle}
    >
      <Icon name="sidePanel" size={16} />
    </button>
  );

  return (
    <div ref={page} class="iw-assistant-page">
      <ConversationPage
        id={props.route.conversationId}
        rowKey={props.route.rowKey}
        openRuntimeDetails={props.openRuntimeDetails}
        headerEnd={profileButton}
      />
      <Show when={shown()}>
        <aside
          ref={panel}
          id="iw-profile"
          class={`iw-profile${docked() ? "" : " iw-profile--overlay"}`}
          aria-label="助手资料"
        >
          <Show when={view()}>
            {(v) => (
            <Profile view={v()} tab={props.route.tab} setTab={(tab) => props.go({ ...props.route, tab, rowKey: undefined })} />
          )}
          </Show>
        </aside>
      </Show>
    </div>
  );
}

function Profile(props: { view: AssistantProfileView; tab: AssistantTab; setTab: (t: AssistantTab) => void }) {
  const v = () => props.view;
  const subtitle = () => {
    const m = v().modelMenu;
    if (m === undefined) return "";
    const runtime = m.runtimes.find((r) => r.selected)?.label;
    const model = m.models.find((x) => x.selected)?.label ?? (m.customModel || undefined);
    return [runtime, model].filter(Boolean).join(" · ");
  };
  return (
    <>
      <header class="iw-profile__head">
        <Avatar avatar={v().avatar} size={56} interactive label={v().name} />
        <h2 class="iw-profile__name">{v().name}</h2>
        <span class="iw-profile__sub">{subtitle()}</span>
      </header>
      <div class="iw-tabs" role="tablist" aria-label="助手资料">
        <For each={TABS}>
          {(t) => (
            <button
              type="button"
              role="tab"
              class="iw-tabs__tab"
              aria-selected={props.tab === t.tab}
              onClick={() => props.setTab(t.tab)}
            >
              {t.label}
            </button>
          )}
        </For>
      </div>
      <div class="iw-profile__body" role="tabpanel">
        <Choose>
          <Match when={props.tab === "activity"}>
            <p class="iw-profile__empty">{v().activityEmptyText}</p>
          </Match>
          <Match when={props.tab === "memory"}>
            <p class="iw-profile__empty">{v().memoryEmptyText}</p>
          </Match>
          <Match when={props.tab === "config"}>
            <Config view={v()} />
          </Match>
        </Choose>
      </div>
    </>
  );
}

function Config(props: { view: AssistantProfileView }) {
  const host = useHost();
  const v = () => props.view;
  const send = (field: AssistantField) =>
    void host.act({ case: "updateAssistantField", value: { assistantId: v().assistantId, field } });
  return (
    <div class="iw-config">
      <TextField label="名字" value={v().name} onCommit={(name) => send({ case: "name", value: name })} />
      <Show when={v().avatarColors.length > 0}>
        <div class="iw-field">
          <span class="iw-field__label" id="iw-avatar-colors">
            头像配色
          </span>
          <div class="iw-config__colors" role="radiogroup" aria-labelledby="iw-avatar-colors">
            <For each={v().avatarColors}>
              {(c) => (
                <button
                  type="button"
                  role="radio"
                  class="iw-config__color"
                  style={{ background: `var(--${c.token})` }}
                  aria-label={c.label}
                  aria-checked={c.selected}
                  onClick={() => !c.selected && send({ case: "avatarColor", value: c.token })}
                />
              )}
            </For>
          </div>
        </div>
      </Show>
      <TextField
        label="提示词"
        multiline
        value={v().prompt}
        onCommit={(prompt) => send({ case: "prompt", value: prompt })}
      />
      <Show when={v().modelMenu}>
        {(m) => (
          <ModelSelects
            menu={m()}
            value={pickedFrom(m())}
            onChange={(p) => send({ case: "model", value: { modelId: p.modelId, effort: p.effort } })}
          />
        )}
      </Show>
      <div class="iw-field">
        <span class="iw-field__label">工具</span>
        <For each={v().tools}>
          {(t) => (
            <div class="iw-config__tool">
              <span>{t.label}</span>
              <Switch
                checked={t.enabled}
                label={t.label}
                onChange={(on) => send({ case: "tool", value: { group: t.group, enabled: on } })}
              />
            </div>
          )}
        </For>
      </div>
      <Show when={v().saveError}>
        {(e) => (
          <p class="iw-config__error" role="alert">
            {e()}
          </p>
        )}
      </Show>
    </div>
  );
}

/** 失焦或回车（单行）提交、Esc 还原；没变不提交；编辑中不被新的值覆盖。 */
function TextField(props: { label: string; value: string; multiline?: boolean; onCommit: (v: string) => void }) {
  const [draft, setDraft] = createSignal(props.value);
  let editing = false;
  // 提交过、patch 还没追上的值：同一个值不重复提交（回车后的失焦、再点进来又点出去）
  let sent: string | undefined;
  createEffect(() => {
    const v = props.value;
    sent = undefined;
    if (!editing) setDraft(v);
  });
  const commit = () => {
    editing = false;
    if (draft() !== props.value && draft() !== sent) {
      sent = draft();
      props.onCommit(draft());
    }
  };
  const onKey = (e: KeyboardEvent & { currentTarget: HTMLInputElement | HTMLTextAreaElement }) => {
    if (e.key === "Escape") {
      setDraft(props.value);
      editing = false;
      e.currentTarget.blur();
    } else if (e.key === "Enter" && !props.multiline && !e.isComposing) {
      e.currentTarget.blur();
    }
  };
  const common = {
    class: `iw-input${props.multiline ? " iw-input--area" : ""}`,
    "aria-label": props.label,
    onFocus: () => (editing = true),
    onBlur: commit,
    onKeyDown: onKey,
  };
  return (
    <label class="iw-field">
      <span class="iw-field__label">{props.label}</span>
      <Show
        when={props.multiline}
        fallback={<input {...common} value={draft()} onInput={(e) => setDraft(e.currentTarget.value)} />}
      >
        <textarea {...common} value={draft()} onInput={(e) => setDraft(e.currentTarget.value)} />
      </Show>
    </label>
  );
}
