// 按 Tauri v2 的规则拼出某个子命令实际生效的配置：默认配置 → 平台配置 → 各 --config 层，
// 依次用 JSON Merge Patch（RFC 7396）合并。层清单取自包装脚本用的同一张表。
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { configLayersFor } from "../scripts/tauri-layers.ts";

export const TAURI_DIR = fileURLToPath(new URL("../src-tauri/", import.meta.url));

// Tauri v2 自动合并的平台配置
export const PLATFORMS = ["linux", "macos", "windows", "android", "ios"] as const;
export type Platform = (typeof PLATFORMS)[number];

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export function isObject(value: Json | undefined): value is { [key: string]: Json } {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function mergePatch(target: Json, patch: Json): Json {
  if (!isObject(patch)) return patch;
  const result: { [key: string]: Json } = isObject(target) ? { ...target } : {};
  for (const [key, value] of Object.entries(patch)) {
    if (value === null) delete result[key];
    else result[key] = mergePatch(result[key] ?? null, value);
  }
  return result;
}

function readJson(path: string): Json {
  return JSON.parse(readFileSync(path, "utf8")) as Json;
}

/** `pnpm tauri <subcommand>` 在某个平台上实际生效的配置。 */
export function effectiveConfig(subcommand: string, platform: Platform): Json {
  const platformFile = `${TAURI_DIR}tauri.${platform}.conf.json`;
  const files = [
    `${TAURI_DIR}tauri.conf.json`,
    ...(existsSync(platformFile) ? [platformFile] : []),
    ...configLayersFor(subcommand),
  ];
  return files.map(readJson).reduce(mergePatch);
}

/** 配置里的 build 段；没有就是空对象。 */
export function buildSection(config: Json): { [key: string]: Json } {
  const build = isObject(config) ? config.build : undefined;
  return isObject(build) ? build : {};
}
