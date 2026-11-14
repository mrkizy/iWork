// anchor_list：真 WebKit 里展开锚点列表（g3b 第 5 轮）。列表按设计不显示滚动条（ui-m1 2a 第 11 条），这里证实
// 没有原生滚动条占位（也就没有「拖滚动条」这条输入路径），列表确实超出、滚得动；列表上的滚轮在事件那一刻
// 以 AnchorListInput 送到 Rust（壳的意图计数里看）。
history.replaceState(null, "", "#/c/c-anchors-long");
window.addEventListener("load", async () => {
  if (!(await __e2e.until("锚点导轨", () => document.querySelector(".iw-anchor") !== null, 30000))) return;
  document.querySelector(".iw-anchor").dispatchEvent(new MouseEvent("mouseenter"));
  if (!(await __e2e.until("展开的锚点列表", () => document.querySelector(".iw-anchor__list") !== null))) return;
  const list = document.querySelector(".iw-anchor__list");
  await __e2e.sleep(300);
  const scrollbarPx = list.offsetWidth - list.clientWidth;
  const overflow = list.scrollHeight > list.clientHeight;
  list.dispatchEvent(new WheelEvent("wheel", { deltaY: 40, bubbles: true }));
  await __e2e.sleep(500);
  __e2e.report({ case: "anchor_list", scrollbarPx, overflow, done: true });
});
