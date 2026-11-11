// commands_reject_foreign_origin：同来源但不是主窗口的副窗口调三个命令都被拒（capability 只给 main）。
// 非应用来源的页面进不了主窗口（navigation 用例），这里测的是「不是主窗口」这一道。
window.addEventListener("load", async () => {
  if (__e2e.label() === "other") {
    const results = {};
    for (const cmd of ["attach", "next_frame", "ui_intent"]) {
      try {
        await window.__TAURI_INTERNALS__.invoke(cmd, new Uint8Array());
        results[cmd] = "accepted";
      } catch (e) {
        results[cmd] = "rejected:" + String(e).slice(0, 120);
      }
    }
    __e2e.report({ case: "foreign", window: "other", results });
    return;
  }
  await __e2e.sleep(5000);
  __e2e.report({ case: "foreign", window: "main", done: true });
});
