// 「运行时 · 模型」菜单（ui-m1.md 第 2 节第 14 条，画布 01-Main-modelmenu）：运行时、模型 · 仅本对话、思考强度三节。
// 选中项、可否选、说明文字都来自 ModelMenuView；选了就发 SetModel（模型与思考强度一起带），等下一帧。
import { createSignal, For, Show } from "solid-js";
import type { MenuChoice, ModelMenuView } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Spinner } from "../ui/Badge";
import { Icon } from "../ui/Icon";
import { Menu, MenuItem, MenuSeparator, MenuTitle, type MenuAnchor } from "../ui/Menu";
import { modelIntent, type ComposerTarget } from "./target";
import "./Composer.css";

export function ModelMenu(props: { target: ComposerTarget; menu: ModelMenuView; anchor: MenuAnchor; onClose: () => void }) {
  const host = useHost();
  const [typing, setTyping] = createSignal(false);
  const [modelId, setModelId] = createSignal("");
  const currentModel = () => props.menu.models.find((m) => m.selected)?.value ?? (props.menu.customModel || undefined);
  const currentEffort = () => props.menu.efforts.find((e) => e.selected)?.value;
  const setModel = (model: string, effort: string | undefined) => {
    props.onClose();
    void host.act(modelIntent(props.target, model, effort));
  };
  return (
    <Menu anchor={props.anchor} onClose={props.onClose} label="运行时与模型">
      <MenuTitle>运行时</MenuTitle>
      <For each={props.menu.runtimes}>
        {(r) => (
          <MenuItem disabled={!r.enabled} selected={r.selected} tip={r.enabled ? undefined : r.tip} onSelect={props.onClose}>
            <Choice c={r} inlineTip />
          </MenuItem>
        )}
      </For>
      <MenuSeparator />
      <MenuTitle>模型 · 仅本对话</MenuTitle>
      <Show when={!props.menu.modelsLoading} fallback={<MenuTitle><Spinner label="正在取模型清单" /></MenuTitle>}>
        <For each={props.menu.models}>
          {(m) => (
            <MenuItem disabled={!m.enabled} selected={m.selected} onSelect={() => setModel(m.value, currentEffort())}>
              <Choice c={m} />
            </MenuItem>
          )}
        </For>
      </Show>
      <Show when={props.menu.customModel}>
        <MenuItem disabled selected onSelect={props.onClose}>
          <Choice c={{ value: props.menu.customModel, label: props.menu.customModel, selected: true, enabled: true, tip: "" } as MenuChoice} />
        </MenuItem>
      </Show>
      <Show
        when={typing()}
        fallback={
          <MenuItem onSelect={() => setTyping(true)}>
            <span class="iw-choice__check" />
            填写模型 ID…
          </MenuItem>
        }
      >
        <form
          class="iw-model-id"
          onSubmit={(e) => {
            e.preventDefault();
            const id = modelId().trim();
            if (id) setModel(id, currentEffort());
          }}
        >
          <input
            aria-label="模型 ID"
            ref={(el) => setTimeout(() => el.focus(), 0)}
            value={modelId()}
            onInput={(e) => setModelId(e.currentTarget.value)}
          />
        </form>
      </Show>
      <Show when={props.menu.efforts.length > 0}>
        <MenuSeparator />
        <MenuTitle>思考强度</MenuTitle>
        <div class="iw-segmented" role="group" aria-label="思考强度">
          <For each={props.menu.efforts}>
            {(e) => (
              <button
                type="button"
                aria-pressed={e.selected}
                disabled={!e.enabled}
                onClick={() => {
                  const model = currentModel();
                  if (model !== undefined) setModel(model, e.value);
                }}
              >
                {e.label}
              </button>
            )}
          </For>
        </div>
      </Show>
    </Menu>
  );
}

function Choice(props: { c: MenuChoice; inlineTip?: boolean }) {
  return (
    <span class="iw-choice">
      <span class="iw-choice__check">{props.c.selected ? <Icon name="check" size={14} /> : null}</span>
      <span class="iw-choice__label">{props.c.label}</span>
      <Show when={props.inlineTip && props.c.tip && !props.c.selected}>
        <span class="iw-choice__tip">{props.c.tip}</span>
      </Show>
    </span>
  );
}
