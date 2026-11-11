import { describe, expect, it } from "vitest";
import { averageHeight, isAtBottom, unsentAround, visibleOrdinals } from "./virtual";

const rows = [
  { ordinal: 10n, top: 0, height: 50 },
  { ordinal: 12n, top: 50, height: 100 },
  { ordinal: 14n, top: 150, height: 50 },
];

describe("虚拟行的计算", () => {
  it("视口里的首尾行", () => {
    expect(visibleOrdinals(rows, 60, 60)).toEqual([12n, 12n]);
    expect(visibleOrdinals(rows, 40, 120)).toEqual([10n, 14n]);
  });

  it("视口落在占位里：取最近的已收到行，让 Rust 把窗口挪过去", () => {
    expect(visibleOrdinals(rows.map((r) => ({ ...r, top: r.top + 1000 })), 0, 100)).toEqual([10n, 10n]);
    expect(visibleOrdinals(rows, 5000, 100)).toEqual([14n, 14n]);
    expect(visibleOrdinals([], 0, 100)).toBeUndefined();
  });

  it("平均行高只算测到的；没测到用默认值", () => {
    expect(averageHeight([40, 0, 80], 72)).toBe(60);
    expect(averageHeight([], 72)).toBe(72);
  });

  it("窗口上下没收到的行数", () => {
    expect(unsentAround(100, 60, 1000)).toEqual({ above: 100, below: 840 });
    expect(unsentAround(0, 60, 60)).toEqual({ above: 0, below: 0 });
  });

  it("离底部 24px 以内算停在底部", () => {
    expect(isAtBottom(976, 500, 1500)).toBe(true);
    expect(isAtBottom(900, 500, 1500)).toBe(false);
  });
});
