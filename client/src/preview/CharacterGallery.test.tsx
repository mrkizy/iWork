// 形象一览（预览专用）：全部形象 × 全部表情都画出来；缺的标「缺」。期望值从素材目录的文件名独立数出来。
import { render } from "@solidjs/testing-library";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { catalogFrom, DESIGN_CATALOG } from "../ui/characters";
import { CharacterGallery } from "./CharacterGallery";

const HERE = dirname(import.meta.filename);
const DESIGN_DIR = resolve(HERE, "../../../docs/design/design-system/characters");
const INDEX = resolve(HERE, "../../fixtures/index.json");

describe("形象一览", () => {
  it("设计系统里的每个形象一行、每种表情一格，没有缺", () => {
    const files = readdirSync(DESIGN_DIR).filter((f) => f.endsWith(".svg"));
    const characters = new Set(files.map((f) => f.replace(/-[^-]+\.svg$/, "")));
    const moods = new Set(files.map((f) => /-([^-]+)\.svg$/.exec(f)![1]));
    expect(characters.size, "对照：13 个形象").toBe(13);
    expect(moods.size, "对照：15 种表情").toBe(15);
    const { container } = render(() => <CharacterGallery catalog={DESIGN_CATALOG} />);
    const rows = [...container.querySelectorAll("tbody tr")];
    expect(rows.map((r) => r.getAttribute("data-character")).sort()).toEqual([...characters].sort());
    for (const r of rows) {
      const c = r.getAttribute("data-character")!;
      // 每一格画的就是这个形象的这种表情（头像的读屏名是「形象 表情」）
      const drawn = [...r.querySelectorAll('td > [role="img"]')].map((el) => el.getAttribute("aria-label"));
      expect(drawn, c).toEqual([...moods].sort().map((m) => `${c} ${m}`));
    }
    expect(container.querySelectorAll(".iw-preview-gallery__missing")).toHaveLength(0);
  });

  it("对照：某个形象缺一种表情时标「缺」，不画兜底", () => {
    const catalog = catalogFrom({ "/a-idle.svg": "<svg/>", "/a-sad.svg": "<svg/>", "/b-idle.svg": "<svg/>" });
    const { container } = render(() => <CharacterGallery catalog={catalog} />);
    const b = container.querySelector('tr[data-character="b"]')!;
    expect(b.querySelector('td[data-mood="sad"] .iw-preview-gallery__missing')).toHaveTextContent("缺");
    expect(b.querySelector('td[data-mood="idle"] .iw-preview-gallery__missing')).toBeNull();
  });

  it("没有录像场景叫 characters（否则会被这一页挡住）", () => {
    if (!existsSync(INDEX)) return;
    const names = (JSON.parse(readFileSync(INDEX, "utf8")) as { name: string }[]).map((e) => e.name);
    expect(names.length).toBeGreaterThan(0);
    expect(names).not.toContain("characters");
  });
});
