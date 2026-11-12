// 弹层关闭后焦点回到打开它的控件（WAI-ARIA 菜单与对话框的做法；审查 2a F-11）。
// 打开时记下当时的焦点；关闭时它还在页面上、而焦点已经没处可去（落在 body 或刚关掉的弹层里），就还回去。
// 关闭时焦点已经去了别处（比如选的菜单项又打开了确认框），不抢。

export function rememberFocus(): (closing?: Element) => void {
  const trigger = document.activeElement instanceof HTMLElement ? document.activeElement : undefined;
  return (closing) => {
    if (trigger === undefined || !trigger.isConnected) return;
    const now = document.activeElement;
    const lost = now === null || now === document.body || (closing !== undefined && closing.contains(now));
    if (lost) trigger.focus();
  };
}
