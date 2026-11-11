// 弹出菜单：锚在按钮下方或鼠标位置（右键菜单）。点外面、按 Esc 关闭；菜单项用方向键移动焦点。
// 开合是网页本地的状态（方案 2.5）；菜单项可否点击由调用方按 Rust 给的字段传进来。
import { onCleanup, onMount, type JSX } from "solid-js";
import { Portal } from "solid-js/web";
import "./Menu.css";

export interface MenuAnchor {
  x: number;
  y: number;
  /** 从锚点往上展开（输入框工具栏的菜单）。 */
  above?: boolean;
}

export function anchorBelow(el: Element): MenuAnchor {
  const r = el.getBoundingClientRect();
  return { x: r.left, y: r.bottom + 4 };
}

export function anchorAbove(el: Element): MenuAnchor {
  const r = el.getBoundingClientRect();
  return { x: r.left, y: r.top - 4, above: true };
}

export function Menu(props: { anchor: MenuAnchor; onClose: () => void; label: string; children: JSX.Element; class?: string }) {
  let el!: HTMLDivElement;
  const onDown = (e: PointerEvent) => {
    if (!el.contains(e.target as Node)) props.onClose();
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      props.onClose();
      return;
    }
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    e.preventDefault();
    const items = [...el.querySelectorAll<HTMLButtonElement>(".iw-menu__item:not(:disabled)")];
    const at = items.indexOf(document.activeElement as HTMLButtonElement);
    const next = e.key === "ArrowDown" ? (at + 1) % items.length : (at - 1 + items.length) % items.length;
    items[next]?.focus();
  };
  onMount(() => {
    // 下一轮再挂，免得打开菜单的那次点击把它立刻关掉
    setTimeout(() => document.addEventListener("pointerdown", onDown), 0);
    document.addEventListener("keydown", onKey, true);
    // 超出窗口时往回收
    const r = el.getBoundingClientRect();
    if (r.right > window.innerWidth - 8) el.style.left = `${Math.max(8, window.innerWidth - r.width - 8)}px`;
    if (r.bottom > window.innerHeight - 8 && !props.anchor.above) el.style.top = `${Math.max(8, props.anchor.y - r.height - 8)}px`;
    el.querySelector<HTMLButtonElement>(".iw-menu__item:not(:disabled)")?.focus();
  });
  onCleanup(() => {
    document.removeEventListener("pointerdown", onDown);
    document.removeEventListener("keydown", onKey, true);
  });
  const style = (): JSX.CSSProperties =>
    props.anchor.above
      ? { left: `${props.anchor.x}px`, bottom: `${window.innerHeight - props.anchor.y}px` }
      : { left: `${props.anchor.x}px`, top: `${props.anchor.y}px` };
  return (
    <Portal>
      <div ref={el} class={`iw-menu${props.class ? ` ${props.class}` : ""}`} role="menu" aria-label={props.label} style={style()}>
        {props.children}
      </div>
    </Portal>
  );
}

export function MenuItem(props: {
  onSelect: () => void;
  disabled?: boolean;
  danger?: boolean;
  tip?: string;
  children: JSX.Element;
}) {
  return (
    <button
      type="button"
      role="menuitem"
      class={`iw-menu__item${props.danger ? " iw-menu__item--danger" : ""}`}
      disabled={props.disabled}
      data-tip={props.tip}
      onClick={() => props.onSelect()}
    >
      {props.children}
    </button>
  );
}

export function MenuTitle(props: { children: JSX.Element }) {
  return <div class="iw-menu__title">{props.children}</div>;
}

export function MenuSeparator() {
  return <div class="iw-menu__sep" role="separator" />;
}
