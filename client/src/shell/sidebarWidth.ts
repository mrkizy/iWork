// 侧栏宽度与窗口宽度的联动（ui-m1 2a 第 12、12b 条）。数值与 tokens.css 的 --sidebar-w-max、--window-min-w 一致，
// 由 client/tests/window-min-width.test.ts 对拍；侧栏拖动做好以前，这里只有上限的算法。

/** 完整侧栏的上限（外宽）。 */
export const SIDEBAR_MAX = 390;
/** 主区域至少留的宽度：窄形态内容列 520 + 行内边距 48。 */
export const MAIN_MIN = 568;
/** 窗口最小宽度（Tauri 主窗口的 min_inner_size 宽度同值）。 */
export const WINDOW_MIN = 880;

/** 完整侧栏在这个窗口宽度下的实际上限：min(390, 窗口宽 − 568)。窗口 880 → 312，≥ 958 → 390。 */
export function fullSidebarMax(windowWidth: number): number {
  return Math.min(SIDEBAR_MAX, windowWidth - MAIN_MIN);
}

/** 显示用的完整侧栏宽度：已保存的值压到当前上限内，不改写保存值（窗口放大后自动回到保存的宽度）。 */
export function shownSidebarWidth(saved: number, windowWidth: number): number {
  return Math.min(saved, fullSidebarMax(windowWidth));
}
