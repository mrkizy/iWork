// webview_navigation_blocked：改 location、开新窗口都不改变主窗口的来源。
// 主窗口真被导航走的话，这段脚本所在的页面就没了，报不出 done，编排脚本按超时判失败。
window.addEventListener("load", async () => {
  if (!(await __e2e.until("侧栏", () => document.querySelector(".iw-sidebar")))) return;
  const origin = location.origin;
  let opened = "not-called";
  try {
    opened = window.open("https://example.com/") === null ? "null" : "window";
  } catch (e) {
    opened = "threw:" + e;
  }
  try {
    location.href = "https://example.com/";
  } catch (_) {}
  await __e2e.sleep(2000);
  __e2e.report({ case: "navigation", origin, sameOrigin: location.origin === origin, opened, done: true });
});
