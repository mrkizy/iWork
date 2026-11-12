import { describe, expect, it } from "vitest";
import { eyeOffset, MAX_OFFSET_PX } from "./eyes";

describe("眼睛偏移", () => {
  it("方向朝鼠标，长度不超过 1.5px", () => {
    const [x, y] = eyeOffset(30, 40);
    expect(Math.hypot(x, y)).toBeCloseTo(MAX_OFFSET_PX);
    expect(x / y).toBeCloseTo(30 / 40);
  });
  it("离得很近时按实际距离，不放大", () => {
    expect(eyeOffset(0.5, 0)).toEqual([0.5, 0]);
    expect(eyeOffset(0, 0)).toEqual([0, 0]);
  });
});
