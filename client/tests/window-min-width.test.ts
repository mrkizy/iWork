// 最小窗口宽度与侧栏上限只有一份数（ui-m1 2a 第 12b 条）：tokens.css 的令牌、网页的常量、Tauri 主窗口的
// min_inner_size 三处对拍，改一处忘了另一处会红。
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { MAIN_MIN, SIDEBAR_MAX, WINDOW_MIN } from "../src/shell/sidebarWidth";

const read = (rel: string) => readFileSync(new URL(rel, import.meta.url), "utf8");
const tokens = read("../../docs/design/design-system/tokens.css");
const mainRs = read("../src-tauri/src/main.rs");

function tokenPx(name: string): number {
  const m = tokens.match(new RegExp(`${name}\\s*:\\s*(\\d+)px`));
  if (!m) throw new Error(`tokens.css 里没有 ${name}`);
  return Number(m[1]);
}

describe("最小窗口宽度只有一份数", () => {
  it("网页常量 = tokens.css 令牌", () => {
    expect(WINDOW_MIN).toBe(tokenPx("--window-min-w"));
    expect(SIDEBAR_MAX).toBe(tokenPx("--sidebar-w-max"));
  });

  it("Tauri 主窗口的最小宽度 = --window-min-w", () => {
    const m = mainRs.match(/\.min_inner_size\(\s*([\d.]+)\s*,/);
    expect(m, "main.rs 里找到 min_inner_size").not.toBeNull();
    expect(Number(m![1])).toBe(WINDOW_MIN);
  });

  it("最小窗口、默认侧栏 296 时主区域不小于 568", () => {
    expect(WINDOW_MIN - tokenPx("--sidebar-w")).toBeGreaterThanOrEqual(MAIN_MIN);
  });
});
