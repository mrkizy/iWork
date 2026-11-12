// 混色底上的文字对比度（UI 审查 5/F-1）：组件里用 color-mix 调出来的底色不在设计令牌里，令牌的对比度检查管不到它。
// 这里从组件样式里读出这条规则实际的 background，按 tokens.css 的浅色、深色两套值算出底色，再和文字色比，要求 ≥ 4.5:1。
// client/src 里每一处 color-mix 都要登记：要么进 CHECKED（有文字压在上面），要么进 EXEMPT 并写明理由。
// 新加一处混色而没登记，这条测试就红。
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { cssFiles, declaration, rules, SRC } from "./css.ts";

const TOKENS = readFileSync(fileURLToPath(new URL("../../docs/design/design-system/tokens.css", import.meta.url)), "utf8");
const AA = 4.5;

type Theme = Record<string, string>;
function theme(start: string): Theme {
  const i = TOKENS.indexOf(start);
  const body = TOKENS.slice(TOKENS.indexOf("{", i) + 1, TOKENS.indexOf("}", i));
  return Object.fromEntries([...body.matchAll(/--([\w-]+)\s*:\s*(#[0-9A-Fa-f]{6})/g)].map((m) => [m[1], m[2]]));
}
const THEMES: [string, Theme][] = [
  ["浅色", theme(":root,")],
  ["深色", theme('[data-theme="dark"]')],
];

type Rgb = [number, number, number];
const rgb = (hex: string): Rgb => [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255) as Rgb;
const lum = (c: Rgb) => {
  const [r, g, b] = c.map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
};
export const contrast = (a: Rgb, b: Rgb) => {
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
};
const mix = (a: Rgb, p: number, b: Rgb): Rgb => a.map((v, i) => v * p + b[i] * (1 - p)) as Rgb;

function token(t: Theme, name: string): Rgb {
  const hex = t[name];
  if (hex === undefined) throw new Error(`令牌里没有 --${name}`);
  return rgb(hex);
}

/** `color-mix(in srgb, var(--a) N%, var(--b) | transparent)` 在某个主题下的颜色；transparent 时叠在 backdrop 上。 */
export function mixed(expr: string, t: Theme, backdrop: string): Rgb {
  const m = /^color-mix\(in srgb, var\(--([\w-]+)\) (\d+(?:\.\d+)?)%, (?:var\(--([\w-]+)\)|transparent)\)$/.exec(expr);
  if (m === null) throw new Error(`认不出的混色写法：${expr}`);
  return mix(token(t, m[1]), Number(m[2]) / 100, token(t, m[3] ?? backdrop));
}

interface Checked {
  file: string;
  selector: string;
  /** 压在这块底色上的文字色令牌。 */
  text: string;
  /** 混色里有 transparent 时，底下透出来的颜色。 */
  backdrop: string;
}
const CHECKED: Checked[] = [
  { file: "ui/Button.css", selector: ".iw-btn--primary:hover:not(:disabled)", text: "priTx", backdrop: "surf" },
  { file: "ui/Button.css", selector: ".iw-btn--accent:hover:not(:disabled)", text: "accTx", backdrop: "surf" },
  // 输入框框体是 --surf（Composer.css .iw-composer__box）
  { file: "composer/Composer.css", selector: ".iw-pill--full", text: "acc", backdrop: "surf" },
  // 对话页底色是 --bg
  { file: "pages/pages.css", selector: ".iw-drop", text: "tx2", backdrop: "bg" },
];
/** 不要求文字对比度的混色：禁用控件（WCAG 1.4.3 对不可用的控件不作要求）。 */
const EXEMPT: { file: string; selector: string; why: string }[] = [
  { file: "ui/Button.css", selector: ".iw-btn--primary:disabled", why: "禁用控件" },
  { file: "ui/Button.css", selector: ".iw-btn--accent:disabled", why: "禁用控件" },
  { file: "composer/Composer.css", selector: ".iw-send:disabled", why: "禁用控件" },
];

function background(file: string, selector: string): string {
  const r = rules(readFileSync(SRC + file, "utf8")).find((x) => x.selector === selector);
  if (r === undefined) throw new Error(`${file} 里没有 ${selector}`);
  const bg = declaration(r.body, "background");
  if (bg === undefined) throw new Error(`${file} ${selector} 没有 background`);
  return bg;
}

describe("混色底上的文字对比度 ≥ 4.5:1", () => {
  it("对照：算法与已知值一致", () => {
    expect(contrast(rgb("#FFFFFF"), rgb("#000000"))).toBeCloseTo(21, 5);
    // 5/F-1 报的那一处：浅色 --acc 混 90% --surf 上的白字约 4.35:1，必须判为不过
    const light = THEMES[0][1];
    expect(contrast(mixed("color-mix(in srgb, var(--acc) 90%, var(--surf))", light, "surf"), token(light, "accTx"))).toBeLessThan(AA);
  });

  for (const c of CHECKED) {
    for (const [name, t] of THEMES) {
      it(`${c.file} ${c.selector}（${name}）`, () => {
        const bg = mixed(background(c.file, c.selector), t, c.backdrop);
        const ratio = contrast(bg, token(t, c.text));
        expect(ratio, `${ratio.toFixed(2)}:1`).toBeGreaterThanOrEqual(AA);
      });
    }
  }

  it("每一处 color-mix 都登记了（检查或豁免）", () => {
    const known = new Set([...CHECKED, ...EXEMPT].map((x) => `${x.file} ${x.selector}`));
    const found = cssFiles().flatMap((f) =>
      rules(readFileSync(f, "utf8"))
        .filter((r) => r.body.includes("color-mix("))
        .map((r) => `${f.slice(SRC.length)} ${r.selector}`),
    );
    expect(found.length, "对照：确实扫到了混色").toBeGreaterThan(0);
    expect(found.filter((x) => !known.has(x))).toEqual([]);
    expect([...known].filter((x) => !found.includes(x)), "登记了但样式里已经没有").toEqual([]);
  });
});
