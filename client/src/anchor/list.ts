// 锚点列表的几何与续载判据（ui-m1.md 2a 第 11 条，列表式）。纯函数：输入列表的滚动位置与条数，输出看得到哪几条、
// 要不要请 Rust 以某一条为中心重取、哪一侧渐隐。组件只负责量和画。

/** 每条刻度占的高度，与 tokens.css 的 --anchor-pitch 一致（改一处要两处一起改）。 */
export const PITCH = 11;
/** 列表上下的留白，与 AnchorRail.css 的 .iw-anchor__list padding 一致。 */
export const LIST_PAD = 8;
/** 离已载入的一端不到这么多条、那一侧还有，就请 Rust 以那一条为中心重取（方案 2.9）。 */
export const BROWSE_EDGE = 8;
/** 请了重取、这么久还没到，就在那一端出加载点。 */
export const LOADING_DOT_DELAY_MS = 300;

/** 滚动位置 → 看得到的第一条、最后一条（下标，含两端）。 */
export function visibleRange(scrollTop: number, clientHeight: number, count: number): { first: number; last: number } {
  if (count === 0) return { first: 0, last: -1 };
  const first = Math.min(count - 1, Math.max(0, Math.floor((scrollTop - LIST_PAD) / PITCH)));
  const last = Math.min(count - 1, Math.max(first, Math.ceil((scrollTop + clientHeight - LIST_PAD) / PITCH) - 1));
  return { first, last };
}

/** 看到的范围离某一端不到 BROWSE_EDGE 条、那一侧还有：要以哪一条为中心重取、是哪一端；不用就是 undefined。 */
export function browseTarget(
  range: { first: number; last: number },
  count: number,
  hasBefore: boolean,
  hasAfter: boolean,
): { index: number; side: "start" | "end" } | undefined {
  if (count === 0) return undefined;
  if (hasBefore && range.first < BROWSE_EDGE) return { index: range.first, side: "start" };
  if (hasAfter && count - 1 - range.last < BROWSE_EDGE) return { index: range.last, side: "end" };
  return undefined;
}

/** 哪一侧渐隐：那一侧还能滚、或者还有没载入的才渐隐；到头那一侧不渐隐。 */
export function fadeSides(
  scrollTop: number,
  clientHeight: number,
  scrollHeight: number,
  hasBefore: boolean,
  hasAfter: boolean,
): { top: boolean; bottom: boolean } {
  return {
    top: hasBefore || scrollTop > 0,
    bottom: hasAfter || scrollTop + clientHeight < scrollHeight - 1,
  };
}

/** 让第 `index` 条居中的滚动位置。 */
export function centeredScrollTop(index: number, clientHeight: number): number {
  return Math.max(0, LIST_PAD + index * PITCH + PITCH / 2 - clientHeight / 2);
}

/** 新列表到了，按 key 对回原位：原来顶上那一条（`topKey`，往下偏 `offset` 像素）在新列表里的滚动位置；对不上返回 undefined。 */
export function restoredScrollTop(keys: readonly string[], topKey: string | undefined, offset: number): number | undefined {
  if (topKey === undefined) return undefined;
  const i = keys.indexOf(topKey);
  return i < 0 ? undefined : Math.max(0, LIST_PAD + i * PITCH + offset);
}

/** 一处之外有几条（刻度画法按离鼠标所在那条的距离：0、1、2、更远）。 */
export function distanceClass(d: number): string {
  if (d === 0) return " iw-tick--hover";
  if (d === 1) return " iw-tick--near1";
  if (d === 2) return " iw-tick--near2";
  return "";
}
