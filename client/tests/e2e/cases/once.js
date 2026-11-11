// 旧页面挂着的操作只执行一次（E1：刷新时挂着的自定义协议请求被中止，Tauri 把同一个 invoke 改走 postMessage 再发；
// 入口检查对重发只回 transport_fallback）。第一次加载发一个无副作用的操作（「全部对话」下一批，编码是 0x32 0x00）
// 并立刻刷新；第二次加载后等一会儿报。
// 存储在 load 之后才读（初始化脚本跑的时候文档还没建立）。
window.addEventListener("load", async () => {
  if (!sessionStorage.getItem("e2e-once")) {
    sessionStorage.setItem("e2e-once", "1");
    window.__TAURI_INTERNALS__.invoke("ui_intent", Uint8Array.of(0x32, 0x00)).catch(() => {});
    location.reload();
    return;
  }
  await __e2e.sleep(3000);
  __e2e.report({ case: "once", done: true });
});
