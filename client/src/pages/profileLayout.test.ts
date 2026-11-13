import { describe, expect, it } from "vitest";
import { nextRoomy, PROFILE_DOCK_BELOW, PROFILE_DOCK_FROM } from "./profileLayout";

describe("资料栏常驻的回差（ui-m1 2a 第 18 条 d）", () => {
  it.each([
    // 上一刻放得下, 主区域宽度, 下一刻放得下
    [true, 1144, true],
    [true, PROFILE_DOCK_BELOW, true],
    [true, PROFILE_DOCK_BELOW - 1, false],
    [false, PROFILE_DOCK_BELOW, false],
    [false, PROFILE_DOCK_FROM - 1, false],
    [false, PROFILE_DOCK_FROM, true],
    // 窗口 880、侧栏 296：主区域 584，默认收起
    [true, 584, false],
  ])("上一刻放得下=%s、主区域 %ipx → 放得下=%s", (was, w, want) => {
    expect(nextRoomy(was, w)).toBe(want);
  });

  it("在 924～943 之间来回拖不闪：保持上一刻的", () => {
    let roomy = true;
    const seen: boolean[] = [];
    for (const w of [1000, 923, 930, 943, 944, 930, 924, 923]) {
      roomy = nextRoomy(roomy, w);
      seen.push(roomy);
    }
    expect(seen).toEqual([true, false, false, false, true, true, true, false]);
  });
});
