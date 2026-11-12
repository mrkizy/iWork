import { describe, expect, it } from "vitest";
import { NARROW_BELOW, nextNarrow, WIDE_FROM } from "./layout";

describe("锚点形态的回差", () => {
  it.each([
    // 上一刻, 宽度, 下一刻是窄形态
    [false, 728, false],
    [false, NARROW_BELOW, false],
    [false, NARROW_BELOW - 1, true],
    [true, NARROW_BELOW, true],
    [true, WIDE_FROM - 1, true],
    [true, WIDE_FROM, false],
    [false, WIDE_FROM - 1, false],
  ])("上一刻窄=%s、主区域 %ipx → 窄=%s", (was, w, want) => {
    expect(nextNarrow(was, w)).toBe(want);
  });

  it("在 600～619 之间来回拖不闪：形态保持上一刻的", () => {
    let narrow = false;
    const seen: boolean[] = [];
    for (const w of [700, 599, 605, 615, 610, 619, 620, 612, 601, 600, 599]) {
      narrow = nextNarrow(narrow, w);
      seen.push(narrow);
    }
    expect(seen).toEqual([false, true, true, true, true, true, false, false, false, false, true]);
  });
});
