// 完整侧栏上限随窗口收缩（ui-m1 2a 第 12b 条）。
import { describe, expect, it } from "vitest";
import { fullSidebarMax, MAIN_MIN, shownSidebarWidth, WINDOW_MIN } from "./sidebarWidth";

describe("完整侧栏上限 = min(390, 窗口宽 − 568)", () => {
  it("规格里的几个点", () => {
    expect(fullSidebarMax(880)).toBe(312);
    expect(fullSidebarMax(957)).toBe(389);
    expect(fullSidebarMax(958)).toBe(390);
    expect(fullSidebarMax(960)).toBe(390);
    expect(fullSidebarMax(1440)).toBe(390);
  });

  it("任何不小于最小窗口的宽度、侧栏取上限时，主区域都不小于 568", () => {
    for (let w = WINDOW_MIN; w <= 2000; w++) {
      expect(w - fullSidebarMax(w), `窗口 ${w}`).toBeGreaterThanOrEqual(MAIN_MIN);
    }
  });

  it("窗口缩小只压显示值，保存值不变；放大后回到保存值", () => {
    const saved = 390;
    expect(shownSidebarWidth(saved, 1200)).toBe(390);
    expect(shownSidebarWidth(saved, 880)).toBe(312);
    expect(saved, "保存值没被改写").toBe(390);
    expect(shownSidebarWidth(saved, 1200)).toBe(390);
    expect(shownSidebarWidth(296, 880), "默认宽度在上限内，不动").toBe(296);
  });
});
