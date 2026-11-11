// 侧栏（ui-m1.md 第 2 节第 3 条、2a 第 1–7 条；画布 01）。数字、圆点状态与状态名、标题、可否重命名、
// 最近 5 个对话、助手最近动态都由 Rust 给（SidebarView）；这里只有展开、菜单、就地编辑这些界面本地状态。
import { createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { AssistantRow, ConversationItem, ProjectRow, QuotaCell } from "../gen/iwork/v1/view_pb";
import { useHost } from "../host";
import { Avatar } from "../ui/Avatar";
import { Badge, Dot } from "../ui/Badge";
import { IconButton } from "../ui/Button";
import { ConfirmDialog } from "../ui/Dialog";
import { Icon } from "../ui/Icon";
import { Menu, MenuItem, MenuSeparator, type MenuAnchor } from "../ui/Menu";
import iconUrl from "@design/icon/iwork-icon-white-256.png";
import { loadExpanded, saveExpanded } from "./expanded";
import { followEyes } from "./eyes";
import { InlineRename } from "./InlineRename";
import type { Router } from "./route";
import "./Sidebar.css";

type MenuState =
  | { kind: "conversation"; item: ConversationItem; at: MenuAnchor }
  | { kind: "project"; project: ProjectRow; at: MenuAnchor }
  | { kind: "assistant"; assistant: AssistantRow; at: MenuAnchor };

/** 尚未实现的入口（下一组）上的说明。 */
const NEXT_GROUP = "下一组实现";
/** 要系统的文件夹选择框，M1 还没接（见报告）。 */
const NEEDS_PICKER = "需要系统的文件夹选择框，还没接";

export function Sidebar(props: { router: Router }) {
  const host = useHost();
  const view = () => host.view.sidebar;
  const [menu, setMenu] = createSignal<MenuState>();
  const [renaming, setRenaming] = createSignal<string>();
  const [deleting, setDeleting] = createSignal<ConversationItem>();
  const [expanded, setExpanded] = createSignal(loadExpanded());
  let root!: HTMLElement;

  onMount(() => onCleanup(followEyes(root)));

  const route = () => props.router.route();
  /** 「当前项目」：正在看的对话所在的项目，或首页选的项目；默认展开（2 节第 3 条）。 */
  const currentProject = () => {
    const r = route();
    if (r.page === "home") return host.view.home?.projectId;
    if (r.page === "allConversations") return r.projectId;
    return view()?.projects.find((p) => p.recent.some((c) => c.selected))?.projectId;
  };
  const isExpanded = (id: string) => expanded()[id] ?? id === currentProject();
  const toggle = (id: string) => {
    const next = { ...expanded(), [id]: !isExpanded(id) };
    setExpanded(next);
    saveExpanded(next);
  };
  const openMenu = (e: MouseEvent, m: (at: MenuAnchor) => MenuState) => {
    e.preventDefault();
    // 右键不移动焦点：先落到被右键的那一项上，菜单关了好回到它
    (e.currentTarget as HTMLElement).focus();
    setMenu(m({ x: e.clientX, y: e.clientY }));
  };
  /** 就地重命名用键盘收尾：那一项已经重新画出来了，把焦点放回去。 */
  const focusConversation = (id: string) =>
    queueMicrotask(() => root.querySelector<HTMLElement>(`button[data-conversation-id="${CSS.escape(id)}"]`)?.focus());

  return (
    <aside ref={root} class="iw-sidebar" aria-label="侧栏">
      <div class="iw-sidebar__brand">
        <img src={iconUrl} alt="" />
        iWork
      </div>
      <button
        type="button"
        class={`iw-side-item iw-side-item--new${route().page === "home" ? " iw-side-item--selected" : ""}`}
        onClick={() => props.router.go({ page: "home", projectId: currentProject() })}
      >
        <Icon name="plus" size={14} />
        <span class="iw-side-item__label">新对话</span>
      </button>
      <button
        type="button"
        class={`iw-side-item${route().page === "pending" ? " iw-side-item--selected" : ""}`}
        onClick={() => props.router.go({ page: "pending" })}
      >
        <span class="iw-side-item__label">待你处理</span>
        <Badge count={view()?.pendingCount ?? 0} label={`${view()?.pendingCount ?? 0} 项待你处理`} />
      </button>

      <div class="iw-sidebar__scroll">
        <div class="iw-side-section">
          <span>项目</span>
          <IconButton icon="plus" label="添加项目" iconSize={14} onClick={() => props.router.go({ page: "settings" })} />
        </div>
        <For each={view()?.projects}>
          {(p) => (
            <div class={p.pathMissing ? "iw-project iw-project--missing" : "iw-project"}>
              <button
                type="button"
                class="iw-side-item"
                aria-expanded={isExpanded(p.projectId)}
                onClick={() => toggle(p.projectId)}
                onContextMenu={(e) => openMenu(e, (at) => ({ kind: "project", project: p, at }))}
              >
                <Icon class="iw-project__chevron" name={isExpanded(p.projectId) ? "chevronDown" : "chevronRight"} size={14} />
                <span
                  class={`iw-side-item__label iw-project__name${p.projectId === currentProject() ? " iw-project__name--current" : ""}`}
                >
                  {p.name}
                </span>
                <Badge count={p.unread} />
              </button>
              <Show when={p.pathMissing}>
                <div class="iw-project__missing">
                  <span class="iw-project__missing-tag">{p.pathMissingLabel}</span>
                  <span data-tip={NEEDS_PICKER} class="iw-btn iw-btn--link" aria-disabled="true">
                    重新定位
                  </span>
                </div>
              </Show>
              <Show when={isExpanded(p.projectId)}>
                <For each={p.recent}>
                  {(c) => (
                    <Show
                      when={renaming() !== c.conversationId}
                      fallback={
                        <div class="iw-side-item iw-conv">
                          <Dot state={c.dot} label={c.dotLabel} />
                          <InlineRename
                            original={c.title}
                            onSubmit={(title) => void host.act({ case: "renameConversation", value: { conversationId: c.conversationId, title } })}
                            onDone={(byKeyboard) => {
                              setRenaming(undefined);
                              if (byKeyboard) focusConversation(c.conversationId);
                            }}
                          />
                        </div>
                      }
                    >
                      <button
                        type="button"
                        class={`iw-side-item iw-conv${c.selected ? " iw-side-item--selected" : ""}`}
                        data-conversation-id={c.conversationId}
                        aria-current={c.selected ? "page" : undefined}
                        onClick={() => props.router.go({ page: "conversation", id: c.conversationId })}
                        onContextMenu={(e) => openMenu(e, (at) => ({ kind: "conversation", item: c, at }))}
                      >
                        <Dot state={c.dot} label={c.dotLabel} />
                        <span class="iw-side-item__label">{c.title}</span>
                        <Badge count={c.unread} />
                      </button>
                    </Show>
                  )}
                </For>
                <button
                  type="button"
                  class="iw-side-item iw-conv--more"
                  onClick={() => props.router.go({ page: "allConversations", projectId: p.projectId })}
                >
                  全部对话
                </button>
              </Show>
            </div>
          )}
        </For>

        <div class="iw-side-section">
          <span>助手</span>
          <IconButton icon="plus" label="新建助手" iconSize={14} disabled data-tip={NEXT_GROUP} />
        </div>
        <For each={view()?.assistants}>{(a) => <AssistantItem a={a} />}</For>
      </div>

      <div class="iw-sidebar__footer">
        <button
          type="button"
          class={`iw-side-item${route().page === "settings" ? " iw-side-item--selected" : ""}`}
          onClick={() => props.router.go({ page: "settings" })}
        >
          <span class="iw-side-item__label">设置与运行时</span>
        </button>
        <div class="iw-quota" aria-label="本机额度">
          <For each={view()?.quota}>{(q) => <Quota q={q} />}</For>
        </div>
      </div>

      <Show when={menu()}>{(m) => <ContextMenu m={m()} />}</Show>
      <Show when={deleting()}>
        {(c) => (
          <ConfirmDialog
            title={`删除「${c().title}」？`}
            confirmLabel="删除"
            confirmVariant="accent"
            onCancel={() => setDeleting(undefined)}
            onConfirm={() => {
              void host.act({ case: "deleteConversation", value: { conversationId: c().conversationId } });
              setDeleting(undefined);
            }}
          >
            对话记录将删除，不影响代码文件。
          </ConfirmDialog>
        )}
      </Show>
    </aside>
  );

  function AssistantItem(p: { a: AssistantRow }) {
    const a = () => p.a;
    return (
      <button
        type="button"
        class={`iw-side-item iw-assistant${a().selected ? " iw-side-item--selected" : ""}`}
        onClick={() => props.router.go({ page: "conversation", id: a().dmConversationId })}
        onContextMenu={(e) => openMenu(e, (at) => ({ kind: "assistant", assistant: a(), at }))}
      >
        <Avatar avatar={a().avatar} size={32} interactive label={a().name} />
        <span class="iw-assistant__text">
          <span class="iw-assistant__name">
            {a().name}
            <Show when={a().builtin}>
              <span class="iw-assistant__tag">默认</span>
            </Show>
          </span>
          <Show when={a().activityLine}>{(line) => <span class="iw-assistant__activity">{line()}</span>}</Show>
        </span>
        <Badge count={a().unread} />
      </button>
    );
  }

  function ContextMenu(p: { m: MenuState }) {
    const close = () => setMenu(undefined);
    const m = p.m;
    switch (m.kind) {
      case "conversation":
        return (
          <Menu anchor={m.at} onClose={close} label="对话操作">
            <MenuItem onSelect={() => (close(), props.router.go({ page: "conversation", id: m.item.conversationId }))}>打开</MenuItem>
            <Show when={m.item.canRename}>
              <MenuItem onSelect={() => (close(), setRenaming(m.item.conversationId))}>重命名</MenuItem>
            </Show>
            <Show when={m.item.canDelete}>
              <MenuSeparator />
              <MenuItem danger onSelect={() => (close(), setDeleting(m.item))}>
                删除
              </MenuItem>
            </Show>
          </Menu>
        );
      case "project":
        return (
          <Menu anchor={m.at} onClose={close} label="项目操作">
            <MenuItem
              onSelect={() => {
                close();
                void host.act({ case: "pinProject", value: { projectId: m.project.projectId, pinned: !m.project.pinned } });
              }}
            >
              {m.project.pinned ? "取消置顶" : "置顶"}
            </MenuItem>
            <Show when={m.project.pathMissing}>
              <MenuItem disabled tip={NEEDS_PICKER} onSelect={close}>
                重新定位
              </MenuItem>
            </Show>
            <MenuSeparator />
            <MenuItem
              onSelect={() => {
                close();
                void host.act({ case: "removeProject", value: { projectId: m.project.projectId } });
              }}
            >
              从列表移除
            </MenuItem>
          </Menu>
        );
      case "assistant":
        return (
          <Menu anchor={m.at} onClose={close} label="助手操作">
            <MenuItem onSelect={() => (close(), props.router.go({ page: "conversation", id: m.assistant.dmConversationId }))}>
              打开对话
            </MenuItem>
            <MenuItem onSelect={() => (close(), props.router.go({ page: "assistant", id: m.assistant.assistantId }))}>配置</MenuItem>
            <Show when={m.assistant.canDelete}>
              <MenuSeparator />
              <MenuItem danger disabled tip={NEXT_GROUP} onSelect={close}>
                删除
              </MenuItem>
            </Show>
          </Menu>
        );
    }
  }
}

function Quota(p: { q: QuotaCell }) {
  return (
    <span class="iw-quota__cell" data-tip={p.q.tooltip} data-tip-side="top">
      {p.q.runtimeLabel}
      <Show when={p.q.usedPct !== undefined}>
        <span class="iw-quota__bar" aria-hidden="true">
          <span class="iw-quota__fill" style={{ width: `${p.q.usedPct ?? 0}%` }} />
        </span>
      </Show>
      {p.q.valueText}
    </span>
  );
}
