// 网页层的边界（T09 方案 2.4 第 7 条、2.5、第 7 节配置检查）：
// - Tauri 的 invoke 只在 src/bridge/call.ts 里出现，且只传命令与载荷两个参数（不带选项、不带自定义头）；
// - 开发服务器能读的目录只比默认多了设计系统一个；
// - 发布构建的入口只有 index.html（录像播放页只在开发服务器上）。
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

describe("开发服务器与发布入口", () => {
  it("server.fs.allow 只有 client/ 与设计系统目录", async () => {
    const config = await resolveConfig({ configFile: `${CLIENT_DIR}vite.config.ts`, root: CLIENT_DIR }, "serve");
    const design = fileURLToPath(new URL("../../docs/design/design-system/", import.meta.url));
    const norm = (p: string) => p.replace(/\/$/, "");
    expect(config.server.fs.allow.map(norm).sort()).toEqual([CLIENT_DIR, design].map(norm).sort());
  });

  it("发布构建只以 index.html 为入口", async () => {
    const config = await resolveConfig({ configFile: `${CLIENT_DIR}vite.config.ts`, root: CLIENT_DIR }, "build");
    expect(config.build.rollupOptions.input).toBe(`${CLIENT_DIR}index.html`);
  });
});
