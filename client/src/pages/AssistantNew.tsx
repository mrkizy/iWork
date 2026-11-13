// 新建助手弹窗（画布 13-AssistantConfig-new，侧栏「助手 +」打开）：名字、提示词、运行时 / 模型 / 思考强度。
// 表单是网页本地的输入状态（方案 2.5「输入框草稿」同类），点「创建」整份发 CreateAssistant；名字合不合法由后台
// 判断（AGENTS.md「能力与界面分离」）。等受理结果：受理了才关，被拒时弹窗留着、原因显示在弹窗里。
// 命令在途时「创建」不可点：Rust 给 creating_assistant（它同时拒第二次）；本地再挡一下回包到之前、下一帧到之前的连点。
// 头像配色等设计窗口定色值，这次不画；私聊目录的路径由后台规则算，客户端拿不到，这次不显示。
import { createSignal, Show } from "solid-js";
import { useHost } from "../host";
import { ConfirmDialog } from "../ui/Dialog";
import { ModelSelects, pickedFrom, type ModelPick } from "./AssistantModelSelects";
import "./assistant.css";

export function NewAssistantDialog(props: { onClose: () => void }) {
  const host = useHost();
  const menu = () => host.view.sidebar?.newAssistantMenu;
  const [name, setName] = createSignal("");
  const [prompt, setPrompt] = createSignal("");
  // 没改选过就跟着 Rust 给的默认（模型清单可能在弹窗打开后才到）
  const [picked, setPicked] = createSignal<ModelPick>();
  const model = () => picked() ?? (menu() ? pickedFrom(menu()!) : { runtimeId: "", modelId: "" });
  const [waiting, setWaiting] = createSignal(false);
  const [error, setError] = createSignal<string>();
  const busy = () => waiting() || (host.view.sidebar?.creatingAssistant ?? false);
  const create = async () => {
    if (busy()) return;
    setWaiting(true);
    const m = model();
    const ack = await host.act(
      {
        case: "createAssistant",
        value: { name: name(), prompt: prompt(), runtimeId: m.runtimeId, modelId: m.modelId, effort: m.effort },
      },
      { quiet: true },
    );
    setWaiting(false);
    if (ack.error === undefined) props.onClose();
    else setError(ack.error.message);
  };
  return (
    <ConfirmDialog
      title="新建助手"
      confirmLabel="创建"
      confirmDisabled={busy()}
      error={error()}
      form
      onCancel={props.onClose}
      onConfirm={() => void create()}
    >
      <label class="iw-field">
        <span class="iw-field__label">名字</span>
        <input class="iw-input" value={name()} onInput={(e) => setName(e.currentTarget.value)} />
      </label>
      <label class="iw-field">
        <span class="iw-field__label">提示词</span>
        <textarea
          class="iw-input iw-input--area"
          placeholder="它负责什么、怎么做事"
          value={prompt()}
          onInput={(e) => setPrompt(e.currentTarget.value)}
        />
      </label>
      <Show when={menu()}>{(m) => <ModelSelects menu={m()} value={model()} onChange={setPicked} />}</Show>
      <p class="iw-field__hint">工具默认全部启用，创建后可在“配置”里调整</p>
    </ConfirmDialog>
  );
}
