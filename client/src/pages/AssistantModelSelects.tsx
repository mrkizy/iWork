// 运行时 / 模型 / 思考强度三个下拉（ui-m1.md 第 2 节第 14 条：助手配置页与新建助手弹窗里是同一行三个下拉）。
// 选项、选中项、可不可选、不可选的说明都来自 Rust 的 ModelMenuView；思考强度为空（运行时不支持）时整个不显示。
// 选了只回调，不在本地改选中项：配置页等下一帧，新建弹窗由调用方记在自己的表单状态里。
import { For, Show } from "solid-js";
import type { ModelMenuView } from "../gen/iwork/v1/view_pb";
import "./assistant.css";

export interface ModelPick {
  runtimeId: string;
  modelId: string;
  effort?: string;
}

/** 菜单里选中的那一组（新建弹窗的初始值）。 */
export function pickedFrom(menu: ModelMenuView): ModelPick {
  return {
    runtimeId: menu.runtimes.find((r) => r.selected)?.value ?? "",
    modelId: menu.models.find((m) => m.selected)?.value ?? menu.customModel,
    effort: menu.efforts.find((e) => e.selected)?.value,
  };
}

export function ModelSelects(props: { menu: ModelMenuView; value: ModelPick; onChange: (v: ModelPick) => void }) {
  const set = (patch: Partial<ModelPick>) => props.onChange({ ...props.value, ...patch });
  return (
    <div class="iw-model-selects">
      <label class="iw-field">
        <span class="iw-field__label">运行时</span>
        <select
          class="iw-select"
          value={props.value.runtimeId}
          onChange={(e) => set({ runtimeId: e.currentTarget.value })}
        >
          <For each={props.menu.runtimes}>
            {(r) => (
              <option value={r.value} disabled={!r.enabled} title={r.tip || undefined}>
                {r.label}
              </option>
            )}
          </For>
        </select>
      </label>
      <label class="iw-field">
        <span class="iw-field__label">模型</span>
        <select
          class="iw-select"
          value={props.value.modelId}
          disabled={props.menu.modelsLoading}
          onChange={(e) => set({ modelId: e.currentTarget.value })}
        >
          <Show when={props.menu.customModel}>{(m) => <option value={m()}>{m()}</option>}</Show>
          <For each={props.menu.models}>
            {(m) => (
              <option value={m.value} disabled={!m.enabled}>
                {m.label}
              </option>
            )}
          </For>
        </select>
      </label>
      <Show when={props.menu.efforts.length > 0}>
        <label class="iw-field">
          <span class="iw-field__label">思考强度</span>
          <select
            class="iw-select"
            value={props.value.effort ?? ""}
            onChange={(e) => set({ effort: e.currentTarget.value })}
          >
            <For each={props.menu.efforts}>{(o) => <option value={o.value}>{o.label}</option>}</For>
          </select>
        </label>
      </Show>
    </div>
  );
}
