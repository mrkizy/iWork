// webview_csp_blocks_inline_and_remote：发布层 CSP 下内联脚本、远程脚本、eval 都不执行。
window.addEventListener("load", async () => {
  if (!(await __e2e.until("侧栏", () => document.querySelector(".iw-sidebar")))) return;
  const violations = [];
  document.addEventListener("securitypolicyviolation", (e) => violations.push(e.violatedDirective));
  const inline = document.createElement("script");
  inline.textContent = "window.__inline = 1";
  document.head.appendChild(inline);
  let remote = "pending";
  const s = document.createElement("script");
  s.src = "https://example.com/e2e.js";
  s.onload = () => (remote = "loaded");
  s.onerror = () => (remote = "blocked");
  document.head.appendChild(s);
  let evalResult;
  try {
    evalResult = "ran:" + eval("1 + 1");
  } catch (e) {
    evalResult = "blocked";
  }
  await __e2e.sleep(2000);
  __e2e.report({ case: "csp", inline: window.__inline ?? null, remote, eval: evalResult, violations, done: true });
});
