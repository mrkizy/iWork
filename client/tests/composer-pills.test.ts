// 工具栏药丸防换行的样式规则（ui-m1 2a 第 12b 条，审查 7/F-1）。jsdom 不排版，宽度上的效果由 12b 的真机验收看。
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { declaration, rules, SRC } from "./css.ts";

const css = rules(readFileSync(join(SRC, "composer/Composer.css"), "utf8"));
const rule = (selector: string) => {
  const r = css.find((x) => x.selector === selector);
  if (!r) throw new Error(`Composer.css 里没有 ${selector}`);
  return (prop: string) => declaration(r.body, prop);
};

describe("药丸样式", () => {
  it("一律不换行、默认不被压缩（审批、附件药丸照此）", () => {
    const pill = rule(".iw-pill");
    expect(pill("white-space")).toBe("nowrap");
    expect(pill("flex")).toBe("none");
    expect(rule(".iw-send")("flex"), "发送按钮不被挤").toBe("none");
  });

  it("运行时与模型药丸可收窄到 0、最宽 240，文字超出用省略号", () => {
    const model = rule(".iw-pill--model");
    expect(model("flex")).toBe("0 1 auto");
    expect(model("min-width")).toBe("0");
    expect(model("max-width")).toBe("240px");
    const text = rule(".iw-pill__text");
    expect(text("overflow")).toBe("hidden");
    expect(text("text-overflow")).toBe("ellipsis");
  });
});

