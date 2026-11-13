import { describe, expect, it } from "vitest";
import {
  BROWSE_EDGE,
  browseTarget,
  centeredScrollTop,
  fadeSides,
  LIST_PAD,
  PITCH,
  restoredScrollTop,
  visibleRange,
} from "./list";

describe("锚点列表的几何（ui-m1 2a 第 11 条，列表式）", () => {
  it("看得到的范围：从留白之后按每条 11px 算，夹在 0 和条数之间", () => {
    expect(visibleRange(0, LIST_PAD + 3 * PITCH, 10), "正好三条").toEqual({ first: 0, last: 2 });
    expect(visibleRange(0, LIST_PAD + 3 * PITCH + 1, 10), "第四条露出 1px 也算").toEqual({ first: 0, last: 3 });
    expect(visibleRange(LIST_PAD + 5 * PITCH, 3 * PITCH, 100)).toEqual({ first: 5, last: 7 });
    expect(visibleRange(0, 1000, 4), "条数少：全看得到").toEqual({ first: 0, last: 3 });
    expect(visibleRange(0, 100, 0)).toEqual({ first: 0, last: -1 });
  });

  it("续载：离一端不到 8 条、那一侧还有才请；到了真正的头就不请", () => {
    expect(browseTarget({ first: BROWSE_EDGE - 1, last: 40 }, 120, true, true)).toEqual({ index: BROWSE_EDGE - 1, side: "start" });
    expect(browseTarget({ first: BROWSE_EDGE - 1, last: 40 }, 120, false, true), "前面没有了").toBeUndefined();
    expect(browseTarget({ first: BROWSE_EDGE, last: 40 }, 120, true, true), "离得够远").toBeUndefined();
    expect(browseTarget({ first: 60, last: 119 - (BROWSE_EDGE - 1) }, 120, true, true)).toEqual({ index: 112, side: "end" });
    expect(browseTarget({ first: 60, last: 119 }, 120, true, false), "后面没有了").toBeUndefined();
  });

  it("渐隐：还能滚或还有没载入的那一侧才渐隐，到头那一侧不渐隐", () => {
    expect(fadeSides(0, 100, 300, false, false)).toEqual({ top: false, bottom: true });
    expect(fadeSides(200, 100, 300, false, false)).toEqual({ top: true, bottom: false });
    expect(fadeSides(0, 100, 300, true, false), "顶上还有没载入的").toEqual({ top: true, bottom: true });
    expect(fadeSides(0, 300, 300, false, false), "不超出：都不渐隐").toEqual({ top: false, bottom: false });
  });

  it("展开时把当前位置那一条放到中间；新列表到了按 key 对回原位", () => {
    expect(centeredScrollTop(50, 200)).toBe(LIST_PAD + 50 * PITCH + PITCH / 2 - 100);
    expect(centeredScrollTop(0, 200), "不往负数滚").toBe(0);
    expect(restoredScrollTop(["a", "b", "c", "d"], "c", 3)).toBe(LIST_PAD + 2 * PITCH + 3);
    expect(restoredScrollTop(["a", "b"], "z", 3), "对不上").toBeUndefined();
  });
});
