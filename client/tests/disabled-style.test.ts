// 禁用控件不整体降 opacity（UI 审查 4/F-1 之后的 4/F-2）：悬停提示是控件自己的 ::after（Button.css 的 [data-tip]），
// 控件的 opacity 会连带压到提示上，提示的对比度随之掉到约 2.9:1。禁用只给文字、图标、底色降色。
// 这条按样式规则查全部样式文件：以后新写的控件也挡得住，不靠逐个记得。
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { cssFiles, rules, SRC } from "./css.ts";

const DISABLED = /:disabled|\[aria-disabled/;

/** 选择器里带禁用状态、声明里设了 opacity 的规则。 */
function fadingDisabled(css: string): string[] {
  return rules(css)
    .filter((r) => DISABLED.test(r.selector) && /(^|[;\s])opacity\s*:/.test(r.body))
    .map((r) => r.selector);
}

describe("禁用控件上的提示不继承 opacity", () => {
  it("对照：查得出禁用规则里的 opacity（含 @media 里的）", () => {
    expect(fadingDisabled(".a:disabled { opacity: 0.4; cursor: not-allowed; }")).toEqual([".a:disabled"]);
    expect(fadingDisabled("@media (x) { .b[aria-disabled=\"true\"] { color: red; opacity: .5 } }")).toEqual([
      '.b[aria-disabled="true"]',
    ]);
    expect(fadingDisabled(".c:hover:not(:disabled) { opacity: 0.9 }")).toEqual([".c:hover:not(:disabled)"]);
    expect(fadingDisabled(".d:disabled { color: var(--tx3); }")).toEqual([]);
  });

  it("client/src 的样式文件里没有这样的规则", () => {
    const files = cssFiles(SRC);
    expect(files.length).toBeGreaterThan(5);
    const hits = files.flatMap((f) => fadingDisabled(readFileSync(f, "utf8")).map((s) => `${f.slice(SRC.length)}: ${s}`));
    expect(hits).toEqual([]);
  });
});
