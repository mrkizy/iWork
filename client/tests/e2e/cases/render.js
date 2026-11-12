// webview_render_executes_nothing：xss-corpus 的对话里，正文、标题、文件名、工具输出带脚本、事件属性、
// javascript: 链接与内嵌框架；渲染后没有任何探针执行，DOM 里没有可执行节点，Rust 只收到视图声明。
history.replaceState(null, "", "#/c/c-xss");
window.__alerts = 0;
window.alert = () => window.__alerts++;
window.addEventListener("load", async () => {
  if (!(await __e2e.until("对话的消息行", () => document.querySelectorAll(".iw-msg").length >= 2))) return;
  await __e2e.sleep(2000);
  const root = document.getElementById("root");
  __e2e.report({
    case: "render",
    probe: window.__probe ?? null,
    alerts: window.__alerts,
    executable: root.querySelectorAll("script, iframe, object, embed").length,
    handlers: root.querySelectorAll("[onerror], [onload], [onclick], [onmouseover]").length,
    hrefs: root.querySelectorAll("[href]").length,
    shownAsText: document.body.textContent.includes("<script>alert(1)</script>"),
    done: true,
  });
});
