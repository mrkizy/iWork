// 助手形象素材（design-system/characters/<形象>-<表情>.svg）。单独放在 .ts 里：含 import.meta.glob 的 .tsx
// 会让 vite 的依赖预扫描按不认 JSX 的方式解析而失败（实测）。
// 形象素材：文件名 <形象>-<表情>.svg。设计系统自带的静态素材，不是来自协议的内容，所以可以按 HTML 内联。
const SVGS = import.meta.glob<string>("@design/characters/*.svg", { query: "?raw", import: "default", eager: true });

export type Mood = "idle" | "work" | "wait" | "done" | "hover" | "angry" | "sorry";

export function svgOf(character: string, mood: Mood): string | undefined {
  const suffix = `/characters/${character}-${mood}.svg`;
  for (const [path, svg] of Object.entries(SVGS)) if (path.endsWith(suffix)) return svg;
  return undefined;
}

/** 已有素材的形象名（Rust 给的 character 只有在这里面才内联，其余画字头像）。 */
export const CHARACTERS: readonly string[] = [
  ...new Set(Object.keys(SVGS).map((p) => /\/characters\/([a-z]+)-[a-z]+\.svg$/.exec(p)?.[1] ?? "")),
].filter(Boolean);

