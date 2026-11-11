// dev:fake 冒烟：conversation-basic 打开后，窗口里出现第一条消息。
history.replaceState(null, "", "#/c/c-basic");
window.addEventListener("load", async () => {
  if (!(await __e2e.until("第一条消息", () => document.querySelector(".iw-msg"), 30000))) return;
  __e2e.report({ case: "smoke", firstMessage: document.querySelector(".iw-msg").textContent.slice(0, 40), done: true });
});
