// 网页层的边界（T09 方案 2.4 第 7 条、2.5、第 7 节配置检查）：
// - Tauri 的 invoke 只在 src/bridge/call.ts 里出现，且只传命令与载荷两个参数（不带选项、不带自定义头）；
// - 开发服务器能读的目录只比默认多了设计系统一个；
// - 发布构建的入口只有 index.html（录像播放页只在开发服务器上）；
// - 按 HTML 插入内容只有 ui/Avatar.tsx 一处（内联设计系统自带的形象 SVG，不是来自协议的内容）；
// - 形象名、表情名不写死在源码里：素材清单由目录生成，兜底常量只在 ui/characters.ts。
import { readdirSync, readFileSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { resolveConfig } from "vite";

const CLIENT_DIR = fileURLToPath(new URL("..", import.meta.url));
const SRC_DIR = `${CLIENT_DIR}src/`;
const CALL_FILE = "bridge/call.ts";

function sourceFiles(dir: string, base = ""): string[] {
  return readdirSync(dir).flatMap((name) => {
    const rel = `${base}${name}`;
    if (rel === "gen") return [];
    const full = `${dir}${name}`;
    if (statSync(full).isDirectory()) return sourceFiles(`${full}/`, `${rel}/`);
    return /\.(ts|tsx)$/.test(name) ? [rel] : [];
  });
}

const TAURI_API = /@tauri-apps\/api|__TAURI_INTERNALS__|__TAURI__/;

describe("Tauri invoke 只有一个出口", () => {
  const files = sourceFiles(SRC_DIR);

  it("只有 bridge/call.ts 引用 Tauri 的 API", () => {
    const offenders = files.filter((f) => f !== CALL_FILE && TAURI_API.test(readFileSync(`${SRC_DIR}${f}`, "utf8")));
    expect(offenders).toEqual([]);
    // 对照：出口文件确实引用了它，证明正则查得到
    expect(readFileSync(`${SRC_DIR}${CALL_FILE}`, "utf8")).toMatch(TAURI_API);
  });

  it("出口只传命令与载荷，不传选项", () => {
    const calls = readFileSync(`${SRC_DIR}${CALL_FILE}`, "utf8").match(/invoke\(([^)]*)\)/g) ?? [];
    expect(calls).toEqual(["invoke(command, body)"]);
  });
});

// 写法：JSX 属性 innerHTML={...}、赋值 .innerHTML = / outerHTML、insertAdjacentHTML。注释里提到这个词不算。
const HTML_SINK = /\binnerHTML\s*=|\bouterHTML\s*=|insertAdjacentHTML\s*\(/;
const HTML_SINK_ALLOWED = "ui/Avatar.tsx";

describe("按 HTML 插入内容只有一处", () => {
  it("只有 ui/Avatar.tsx 用 innerHTML", () => {
    const files = sourceFiles(SRC_DIR).filter((f) => !/\.test\.tsx?$/.test(f));
    const offenders = files.filter((f) => HTML_SINK.test(readFileSync(`${SRC_DIR}${f}`, "utf8")));
    expect(offenders).toEqual([HTML_SINK_ALLOWED]);
  });

  it("对照：正则认得出三种写法，不认注释里的词", () => {
    for (const code of ["<span innerHTML={x} />", "el.innerHTML = s", "el.outerHTML=s", "el.insertAdjacentHTML('beforeend', s)"]) {
      expect(code).toMatch(HTML_SINK);
    }
    expect("// 不用 innerHTML；").not.toMatch(HTML_SINK);
  });
});

// 设计系统的形象目录：名字从文件名现读，不在测试里再抄一份。
const CHARACTERS_DIR = fileURLToPath(new URL("../../docs/design/design-system/characters/", import.meta.url));
const CHARACTER_FILES_OWNER = "ui/characters.ts";

describe("形象与表情不写死", () => {
  const names = new Set<string>();
  for (const f of readdirSync(CHARACTERS_DIR)) {
    const m = /^(.+)-([^-]+)\.svg$/.exec(f);
    if (m) {
      names.add(m[1]);
      names.add(m[2]);
    }
  }
  const literal = new RegExp(`(["'\`])(${[...names].join("|")})\\1`);

  it("除了清单与兜底常量，源码里没有形象名、表情名的字面量", () => {
    const files = sourceFiles(SRC_DIR).filter((f) => !/\.test\.tsx?$/.test(f) && f !== CHARACTER_FILES_OWNER);
    const offenders = files.flatMap((f) => {
      const m = literal.exec(readFileSync(`${SRC_DIR}${f}`, "utf8"));
      return m ? [`${f}: ${m[0]}`] : [];
    });
    expect(offenders).toEqual([]);
  });

  it("对照：目录读到了名字，兜底常量所在的文件确实含字面量", () => {
    expect(names.has("blob") && names.has("idle")).toBe(true);
    expect(readFileSync(`${SRC_DIR}${CHARACTER_FILES_OWNER}`, "utf8")).toMatch(literal);
  });
});

describe("开发服务器与发布入口", () => {
  it("server.fs.allow 除了 client/ 以内，只多放行设计系统目录", async () => {
    const config = await resolveConfig({ configFile: `${CLIENT_DIR}vite.config.ts`, root: CLIENT_DIR }, "serve");
    // 统一写法再比：Windows 上 vite 给的是正斜杠，还会自己加上 client/node_modules/.pnpm（在 client/ 以内）
    const norm = (p: string) => p.replace(/\\/g, "/").replace(/\/$/, "");
    const client = norm(CLIENT_DIR);
    const design = norm(fileURLToPath(new URL("../../docs/design/design-system/", import.meta.url)));
    const allow = config.server.fs.allow.map(norm);
    const outside = allow.filter((p) => p !== client && !p.startsWith(`${client}/`));
    expect(outside).toEqual([design]);
    expect(allow).toContain(client);
  });

  it("发布构建只以 index.html 为入口", async () => {
    const config = await resolveConfig({ configFile: `${CLIENT_DIR}vite.config.ts`, root: CLIENT_DIR }, "build");
    expect(config.build.rollupOptions.input).toBe(`${CLIENT_DIR}index.html`);
  });
});
