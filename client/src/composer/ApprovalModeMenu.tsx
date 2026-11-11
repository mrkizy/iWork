// 审批档位菜单（ui-m1.md 第 2 节第 9 条，画布 10、11）：标题写生效范围；三档各带一句说明；
// 选到要确认的那一档（全开）先弹确认框。文字与「哪一档要确认」都来自 ApprovalModeView。
// 单独设置的成员与「恢复跟随」属于 M2 的多成员对话，M1 不出现（Rust 不给数据）。
import { createSignal, For, Show } from "solid-js";
import type { ApprovalModeView, ConfirmView } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { ConfirmDialog } from "../ui/Dialog";
import { Icon } from "../ui/Icon";
import { Menu, MenuItem, MenuTitle, type MenuAnchor } from "../ui/Menu";
import "./Composer.css";

export function ApprovalModeMenu(props: { conversationId: string; mode: ApprovalModeView; anchor: MenuAnchor; onClose: () => void }) {
  const host = useHost();
  const [confirming, setConfirming] = createSignal<{ value: number; confirm: ConfirmView }>();
  const choose = (value: number) => {
    if (value === props.mode.current) return props.onClose();
    const c = props.mode.fullAccessConfirm;
    if (c !== undefined && c.forValue === value) return setConfirming({ value, confirm: c });
    props.onClose();
    void host.act({ case: "setApprovalMode", value: { conversationId: props.conversationId, mode: value } });
  };
  return (
    <Show
      when={confirming()}
      fallback={
        <Menu anchor={props.anchor} onClose={props.onClose} label="审批方式" class="iw-mode-menu">
          <MenuTitle>审批方式 · {props.mode.scopeTitle}</MenuTitle>
          <For each={props.mode.options}>
            {(o) => (
              <MenuItem onSelect={() => choose(o.value)}>
                <span class="iw-choice">
                  <span class="iw-choice__check">{o.value === props.mode.current ? <Icon name="check" size={14} /> : null}</span>
                  <span class="iw-choice__main">
                    <span class="iw-choice__label">
                      {o.label}
                      <Show when={o.tag}>
                        <span class="iw-choice__tag">{o.tag}</span>
                      </Show>
                    </span>
                    <span class="iw-choice__desc">{o.description}</span>
                  </span>
                </span>
              </MenuItem>
            )}
          </For>
        </Menu>
      }
    >
      {(c) => (
        <ConfirmDialog
          title={c().confirm.title}
          confirmLabel={c().confirm.confirmLabel}
          confirmVariant="accent"
          onCancel={() => {
            setConfirming(undefined);
            props.onClose();
          }}
          onConfirm={() => {
            const value = c().value;
            setConfirming(undefined);
            props.onClose();
            void host.act({ case: "setApprovalMode", value: { conversationId: props.conversationId, mode: value } });
          }}
        >
          <p class="iw-mode-confirm__body">{c().confirm.body}</p>
          <p class="iw-mode-confirm__foot">{c().confirm.footnote}</p>
        </ConfirmDialog>
      )}
    </Show>
  );
}
