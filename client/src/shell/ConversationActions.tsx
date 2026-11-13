// 对话的右键菜单与删除确认（ui-m1.md 第 2 节第 3 条、2a 第 5 条）：侧栏和「全部对话」页共用同一套——打开 / 重命名 /
// 删除；能不能改名、能不能删由 Rust 给（ConversationItem.can_rename / can_delete）。
import { Show } from "solid-js";
import type { ConversationItem } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { ConfirmDialog } from "../ui/Dialog";
import { Menu, MenuItem, MenuSeparator, type MenuAnchor } from "../ui/Menu";

export function ConversationMenu(props: {
  item: ConversationItem;
  at: MenuAnchor;
  onClose: () => void;
  onOpen: () => void;
  onRename: () => void;
  onDelete: () => void;
}) {
  // 先关菜单再做事：菜单关掉时焦点回到被右键的那一项，接着弹出的确认框记下的才是它（取消后回到这里）。
  // 回调里要用的那一项，调用方在建菜单时就取好，别在关掉以后再去读菜单状态
  const pick = (f: () => void) => () => {
    props.onClose();
    f();
  };
  return (
    <Menu anchor={props.at} onClose={props.onClose} label="对话操作">
      <MenuItem onSelect={pick(props.onOpen)}>打开</MenuItem>
      <Show when={props.item.canRename}>
        <MenuItem onSelect={pick(props.onRename)}>重命名</MenuItem>
      </Show>
      <Show when={props.item.canDelete}>
        <MenuSeparator />
        <MenuItem danger onSelect={pick(props.onDelete)}>
          删除
        </MenuItem>
      </Show>
    </Menu>
  );
}

export function DeleteConversationDialog(props: { item: ConversationItem; onClose: () => void }) {
  const host = useHost();
  return (
    <ConfirmDialog
      title={`删除「${props.item.title}」？`}
      confirmLabel="删除"
      confirmVariant="accent"
      onCancel={props.onClose}
      onConfirm={() => {
        void host.act({ case: "deleteConversation", value: { conversationId: props.item.conversationId } });
        props.onClose();
      }}
    >
      对话记录将删除，不影响代码文件。
    </ConfirmDialog>
  );
}
