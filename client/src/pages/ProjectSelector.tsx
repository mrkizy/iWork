// 私聊顶栏的项目选择器（画布 09、13；方案 2.7）：当前值与选项（全部项目 +「私聊」）都来自 ConversationHeader。
// 选了别的发 SwitchDmProject，不在本地改当前值，等下一帧；选当前那一项什么都不发。项目对话不给数据，不画。
import { createSignal, For, Show } from "solid-js";
import type { ConversationHeader } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Icon } from "../ui/Icon";
import { anchorBelow, Menu, MenuItem, type MenuAnchor } from "../ui/Menu";

export function ProjectSelector(props: { conversationId: string; header: ConversationHeader }) {
  const host = useHost();
  const [at, setAt] = createSignal<MenuAnchor>();
  const choose = (projectId: string, selected: boolean) => {
    setAt(undefined);
    if (!selected) void host.act({ case: "switchDmProject", value: { conversationId: props.conversationId, projectId } });
  };
  return (
    <Show when={props.header.projectSelectorLabel}>
      {(label) => (
        <>
          <button
            type="button"
            class="iw-project-selector"
            aria-haspopup="menu"
            aria-expanded={at() !== undefined}
            aria-label={`项目：${label()}`}
            onClick={(e) => setAt(anchorBelow(e.currentTarget))}
          >
            <span class="iw-project-selector__text">{label()}</span>
            <Icon name="chevronDown" size={12} />
          </button>
          <Show when={at()}>
            {(a) => (
              <Menu anchor={a()} onClose={() => setAt(undefined)} label="项目">
                <For each={props.header.projectOptions}>
                  {(o) => (
                    <MenuItem selected={o.selected} onSelect={() => choose(o.projectId, o.selected)}>
                      {o.label}
                    </MenuItem>
                  )}
                </For>
              </Menu>
            )}
          </Show>
        </>
      )}
    </Show>
  );
}
