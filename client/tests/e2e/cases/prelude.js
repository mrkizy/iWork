// 每条用例脚本前面拼上这一段（webview.ts 拼）。作为初始化脚本，在页面脚本之前跑，每次加载、刷新都跑。
// 报告写进 document.title（前缀 E2E:），壳经 on_document_title_changed 收下；排队、间隔开写，免得连着改漏掉。
const __e2e = (() => {
  const queue = [];
  let busy = false;
  const pump = () => {
    if (busy || queue.length === 0) return;
    busy = true;
    document.title = "E2E:" + JSON.stringify(queue.shift());
    setTimeout(() => {
      busy = false;
      pump();
    }, 80);
  };
  return {
    report(obj) {
      queue.push(obj);
      if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", pump, { once: true });
      else pump();
    },
    /** 等条件成立（每 100ms 看一次），超时就报告失败原因。 */
    until(what, cond, timeoutMs = 15000) {
      const start = Date.now();
      return new Promise((resolve) => {
        const tick = () => {
          let ok = false;
          try {
            ok = cond();
          } catch (_) {}
          if (ok) return resolve(true);
          if (Date.now() - start > timeoutMs) {
            __e2e.report({ error: "等不到：" + what, done: true });
            return resolve(false);
          }
          setTimeout(tick, 100);
        };
        tick();
      });
    },
    sleep: (ms) => new Promise((r) => setTimeout(r, ms)),
    label: () => window.__TAURI_INTERNALS__?.metadata?.currentWindow?.label ?? "?",
  };
})();
