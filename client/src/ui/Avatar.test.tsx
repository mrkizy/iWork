// 助手头像（ui-m1.md 3.3、2a 第 13 条）：Rust 给形象 id 与表情 id，网页按 id 查素材清单；兜底规则在 characters.ts。
import { create } from "@bufbuild/protobuf";
import { render, screen } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { AvatarViewSchema } from "../gen/iwork/v1/view_pb";
import { Avatar, CharacterCatalogProvider } from "./Avatar";
import { catalogFrom, DESIGN_CATALOG, svgFor, type CharacterCatalog } from "./characters";

// 夹具目录里放的是设计系统里没有的形象 zorb（表情 giggle、idle）：不改任何代码就能画出来。
const FIXTURE_CATALOG = catalogFrom({
  ...import.meta.glob<string>("./__fixtures__/characters/*.svg", { query: "?raw", import: "default", eager: true }),
  // 兜底要用的默认形象 idle
  "blob-idle.svg": "<svg class='iw-mood--blob-idle'></svg>",
});

function shown(view: Partial<{ character: string; mood: string; motion: string; appLogo: boolean; color: string }>, catalog?: CharacterCatalog) {
  const avatar = create(AvatarViewSchema, view);
  render(() =>
    catalog ? (
      <CharacterCatalogProvider value={catalog}>
        <Avatar avatar={avatar} interactive label="助手" />
      </CharacterCatalogProvider>
    ) : (
      <Avatar avatar={avatar} interactive label="助手" />
    ),
  );
  const el = screen.getByRole("img", { name: "助手" });
  return { el, svgClass: el.querySelector(".iw-n svg")?.getAttribute("class") ?? "" };
}

describe("按 id 查素材", () => {
  it("清单里新放进的形象与表情不改代码就能画", () => {
    const { svgClass } = shown({ character: "zorb", mood: "giggle" }, FIXTURE_CATALOG);
    expect(svgClass).toContain("iw-mood--giggle");
  });

  it("设计系统的清单由目录生成：每个形象都有 idle（兜底要用）", () => {
    expect(DESIGN_CATALOG.characters.size).toBeGreaterThan(0);
    for (const [c, moods] of DESIGN_CATALOG.characters) expect(moods.has("idle"), c).toBe(true);
  });
});

describe("兜底", () => {
  const cat = catalogFrom({
    "blob-idle.svg": "B-IDLE",
    "blob-wait.svg": "B-WAIT",
    "zorb-idle.svg": "Z-IDLE",
    "zorb-wait.svg": "Z-WAIT",
    "nada-wait.svg": "N-WAIT",
  });
  it("有就用自己的", () => expect(svgFor(cat, "zorb", "wait")).toBe("Z-WAIT"));
  it("查不到的形象用默认形象的同一种表情", () => expect(svgFor(cat, "ghost", "wait")).toBe("B-WAIT"));
  it("缺这种表情用它自己的 idle", () => expect(svgFor(cat, "zorb", "sorry")).toBe("Z-IDLE"));
  it("连 idle 都没有用默认形象的 idle", () => expect(svgFor(cat, "nada", "sorry")).toBe("B-IDLE"));
  it("文件名不合规时点名报错", () => expect(() => catalogFrom({ "x/bad.svg": "" })).toThrow("bad.svg"));
});

describe("Rust 给的表情与动效", () => {
  it("回合失败：sorry 素材，不浮动", () => {
    const { el, svgClass } = shown({ character: "asen", mood: "sorry", motion: "" });
    expect(el.dataset.mood).toBe("sorry");
    expect(svgClass).toContain("iw-mood--sorry");
    expect(el.classList.contains("iw-float")).toBe(false);
  });

  it("对照：Rust 给了动效就挂上对应的类", () => {
    const { el, svgClass } = shown({ character: "asen", mood: "work", motion: "float" });
    expect(svgClass).toContain("iw-mood--work");
    expect(el.classList.contains("iw-float")).toBe(true);
  });

  it("内置 iWork 画应用标志，不画形象", () => {
    const { el } = shown({ appLogo: true, mood: "idle" });
    expect(el.querySelector("img")).not.toBeNull();
    expect(el.querySelector("svg")).toBeNull();
  });

  // 配置里的配色是令牌名，解析成 var(--令牌) 经 --ch4 交给素材；默认 ch4 不设（免得自引用）；不是令牌名不认
  it.each([
    ["ch1", "var(--ch1)"],
    ["ch4", ""],
    ["#E8A33D", ""],
  ])("配色 %s → --ch4 = %j", (color, expected) => {
    const { el } = shown({ character: "blob", mood: "idle", color });
    expect(el.style.getPropertyValue("--ch4")).toBe(expected);
  });
});
