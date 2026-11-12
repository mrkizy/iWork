// 测试共用：读 client/src 下的样式文件、拆出规则。只认这个仓库自己写的 CSS 子集（没有嵌套规则之外的花样）。
import { readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

export const SRC = fileURLToPath(new URL("../src/", import.meta.url));

export function cssFiles(dir: string = SRC): string[] {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) return name === "gen" ? [] : cssFiles(p);
    return name.endsWith(".css") ? [p] : [];
  });
}

/** 最内层的「选择器 { 声明 }」：@media 里的规则也算（取到的是里面那一条）。 */
export function rules(css: string): { selector: string; body: string }[] {
  const text = css.replace(/\/\*[\s\S]*?\*\//g, "");
  return [...text.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((m) => ({ selector: m[1].trim(), body: m[2] }));
}

/** 规则里某个属性的值（最后一次声明为准）；没有为 undefined。 */
export function declaration(body: string, prop: string): string | undefined {
  const all = [...body.matchAll(new RegExp(`(?:^|[;\\s])${prop}\\s*:\\s*([^;]+)`, "g"))];
  return all.at(-1)?.[1].trim();
}
