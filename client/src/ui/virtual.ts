// 虚拟行的纯计算（VirtualRows 组件用）。Rust 侧只发网页声明的窗口前后各 30 行（方案 2.4 第 9 条），
// 网页只挂这些行；窗口外的行用占位高度撑开滚动条。

/** 一行在滚动容器里的位置（相对内容顶部）。 */
export interface RowBox {
  ordinal: bigint;
  top: number;
  height: number;
}

/** 视口里第一行与最后一行的 ordinal；视口里一行都没有时，取离视口最近的那一行（让 Rust 把窗口往那边挪）。 */
export function visibleOrdinals(rows: readonly RowBox[], viewTop: number, viewHeight: number): [bigint, bigint] | undefined {
  if (rows.length === 0) return undefined;
  const viewBottom = viewTop + viewHeight;
  const inside = rows.filter((r) => r.top + r.height > viewTop && r.top < viewBottom);
  if (inside.length > 0) return [inside[0].ordinal, inside[inside.length - 1].ordinal];
  const first = rows[0];
  const last = rows[rows.length - 1];
  if (viewBottom <= first.top) return [first.ordinal, first.ordinal];
  return [last.ordinal, last.ordinal];
}

/** 已测得的行高的平均值，用来估算窗口外的行；还没测到任何行时用默认值。 */
export function averageHeight(heights: Iterable<number>, fallback: number): number {
  let sum = 0;
  let n = 0;
  for (const h of heights) {
    if (h > 0) {
      sum += h;
      n++;
    }
  }
  return n > 0 ? sum / n : fallback;
}

/** 窗口上方、下方没收到的行数。 */
export function unsentAround(firstIndex: number, sent: number, total: number): { above: number; below: number } {
  const above = Math.max(0, Math.min(firstIndex, total));
  return { above, below: Math.max(0, total - above - sent) };
}

/** 离底部多近算「停在底部」（像素）。 */
export const STICK_THRESHOLD = 24;

export function isAtBottom(scrollTop: number, clientHeight: number, scrollHeight: number): boolean {
  return scrollHeight - scrollTop - clientHeight <= STICK_THRESHOLD;
}
