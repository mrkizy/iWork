// 助手形象素材的清单。构建时由 import.meta.glob 扫 design-system/characters/，文件名 `<形象>-<表情>.svg`
// （按最后一个「-」切开）。加形象、给形象加表情只要往目录里放文件，不改代码：网页按 Rust 给的「形象 id + 表情 id」
// 字符串查这份清单。单独放在 .ts 里：含 import.meta.glob 的 .tsx 会让 vite 的依赖预扫描按不认 JSX 的方式解析而失败（实测）。
// 这些是设计系统自带的静态素材，不是来自协议的内容，所以可以按 HTML 内联（Avatar.tsx）。

/** 兜底用的默认形象：清单里查不到的形象画它。 */
export const DEFAULT_CHARACTER = "blob";
/** 兜底用的表情：形象缺某种表情时画它的这一种。 */
export const FALLBACK_MOOD = "idle";
/** 网页本地的两种互动表情（ui-m1.md 3.3）：悬停、连戳三下假装生气。其余表情都由 Rust 随状态给。 */
export const HOVER_MOOD = "hover";
export const ANGRY_MOOD = "angry";

export interface CharacterCatalog {
  /** 形象 id → 它有的表情 id → SVG 文字。 */
  readonly characters: ReadonlyMap<string, ReadonlyMap<string, string>>;
}

/** 由「路径 → SVG 文字」建清单；路径只看文件名。 */
export function catalogFrom(files: Record<string, string>): CharacterCatalog {
  const characters = new Map<string, Map<string, string>>();
  for (const [path, svg] of Object.entries(files)) {
    const m = /([^/]+)-([^/-]+)\.svg$/.exec(path);
    if (m === null) throw new Error(`形象素材的文件名不是「<形象>-<表情>.svg」：${path}`);
    const [, character, mood] = m;
    let moods = characters.get(character);
    if (moods === undefined) characters.set(character, (moods = new Map()));
    moods.set(mood, svg);
  }
  return { characters };
}

/**
 * 查素材（兜底规则只写在这里）：查不到的形象用默认形象；形象缺这种表情用它的 idle；
 * 连 idle 都没有，用默认形象的 idle。都没有返回空串。
 */
export function svgFor(catalog: CharacterCatalog, character: string, mood: string): string {
  const own = catalog.characters.get(character) ?? catalog.characters.get(DEFAULT_CHARACTER);
  return (
    own?.get(mood) ?? own?.get(FALLBACK_MOOD) ?? catalog.characters.get(DEFAULT_CHARACTER)?.get(FALLBACK_MOOD) ?? ""
  );
}

/** 设计系统里的全部形象。 */
export const DESIGN_CATALOG: CharacterCatalog = catalogFrom(
  import.meta.glob<string>("@design/characters/*.svg", { query: "?raw", import: "default", eager: true }),
);
