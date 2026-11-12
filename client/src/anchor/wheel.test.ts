// 锚点滚轮的手势规则（ui-m1.md 2a 第 11 条）。
import { describe, expect, it } from "vitest";
import { INITIAL_WHEEL, MIN_STEP_INTERVAL_MS, moveWithin, nearEdge, TRACKPAD_STEP_PX, wheelStep } from "./wheel";

describe("滚轮", () => {
  it("鼠标滚轮一格一处，方向按 deltaY", () => {
    expect(wheelStep(INITIAL_WHEEL, { deltaY: 3, deltaMode: 1, now: 0 }).step).toBe(1);
    expect(wheelStep(INITIAL_WHEEL, { deltaY: -3, deltaMode: 1, now: 0 }).step).toBe(-1);
  });

  it("触控板累计到阈值才换一处；换方向从头累计", () => {
    let s = INITIAL_WHEEL;
    let r = wheelStep(s, { deltaY: TRACKPAD_STEP_PX - 1, deltaMode: 0, now: 0 });
    expect(r.step, "没到阈值").toBe(0);
    s = r.state;
    r = wheelStep(s, { deltaY: -5, deltaMode: 0, now: 1 });
    expect(r.state.carry, "换方向从头算").toBe(-5);
    r = wheelStep(s, { deltaY: 1, deltaMode: 0, now: 1 });
    expect(r.step, "累计到阈值").toBe(1);
    expect(r.state.carry).toBe(0);
  });

  it("甩动：一秒内换的处数不超过 10", () => {
    let s = INITIAL_WHEEL;
    let steps = 0;
    for (let t = 0; t < 1000; t += 5) {
      const r = wheelStep(s, { deltaY: 200, deltaMode: 0, now: t });
      s = r.state;
      steps += Math.abs(r.step);
    }
    expect(steps).toBeLessThanOrEqual(1000 / MIN_STEP_INTERVAL_MS);
    expect(steps, "对照：确实在换").toBeGreaterThan(5);
  });
});

describe("位置", () => {
  it("到头停住并回弹，不循环", () => {
    expect(moveWithin(0, -1, 5)).toEqual({ index: 0, bounce: "start" });
    expect(moveWithin(4, 1, 5)).toEqual({ index: 4, bounce: "end" });
    expect(moveWithin(2, 1, 5)).toEqual({ index: 3 });
  });

  it("离边缘不到 8 处、那一侧还有才算近边缘", () => {
    expect(nearEdge(3, 100, true, false)).toBe(true);
    expect(nearEdge(3, 100, false, false), "前面没有了").toBe(false);
    expect(nearEdge(95, 100, false, true)).toBe(true);
    expect(nearEdge(50, 100, true, true)).toBe(false);
  });
});
