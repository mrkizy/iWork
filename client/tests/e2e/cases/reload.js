// reload_storm_leaves_nothing_staged：取帧途中连续刷新 200 次。每次页面 load 之后 30–150ms 刷新——这时取帧循环
// 已经在跑、流式在推，刷新正好落在 next_frame 挂着或块交到一半。刷新时刻记在 sessionStorage（跨刷新保留），
// 最后一次加载后等界面收敛再报。每 20 次报一次进度（留出写标题的时间再刷新）。
// 存储在 load 之后才读：初始化脚本跑的时候文档还没建立，那时碰 sessionStorage 会抛错、整段脚本停掉。
// 刷新时刻分批带在进度报告里（每 20 次一批）：一次全塞进标题会被 WebKit 截断（实测），结尾的 done 就丢了。
const N = 200;
const BATCH = 20;
window.addEventListener("load", async () => {
  const n = Number(sessionStorage.getItem("e2e-reloads") || "0");
  const at = JSON.parse(sessionStorage.getItem("e2e-reload-at") || "[]");
  if (n < N) {
    if (n % BATCH === 0 && n > 0) {
      __e2e.report({ case: "reload", progress: n, reloadAt: at.slice(n - BATCH, n) });
      await __e2e.sleep(200);
    }
    setTimeout(() => {
      at.push(Date.now());
      sessionStorage.setItem("e2e-reload-at", JSON.stringify(at));
      sessionStorage.setItem("e2e-reloads", String(n + 1));
      location.reload();
    }, 30 + Math.random() * 120);
    return;
  }
  if (!(await __e2e.until("收敛后的侧栏", () => document.querySelectorAll(".iw-conv").length > 0, 30000))) return;
  await __e2e.sleep(3000);
  __e2e.report({
    case: "reload",
    reloads: n,
    reloadAt: at.slice(n - (n % BATCH || BATCH), n),
    diagnostic: Boolean(document.querySelector(".iw-transport-failure")),
    converged: document.querySelectorAll(".iw-conv").length > 0,
    done: true,
  });
});
