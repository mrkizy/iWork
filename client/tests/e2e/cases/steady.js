// frames_use_custom_protocol 与 forced_fallback_stops_and_shows_diagnostic 共用：跑满 E2E_SECONDS 秒后报，
// 带上「有没有显示固定诊断页」「侧栏在不在」。两条用例的区别只在编排脚本给的 CSP。
window.addEventListener("load", async () => {
  await __e2e.sleep(Number(window.__E2E_SECONDS || 60) * 1000);
  __e2e.report({
    case: "steady",
    diagnostic: Boolean(document.querySelector(".iw-transport-failure")),
    sidebar: Boolean(document.querySelector(".iw-sidebar")),
    done: true,
  });
});
