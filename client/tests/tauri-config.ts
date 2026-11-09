// 按 Tauri v2 的规则拼出 `pnpm tauri <子命令>` 实际生效的配置：共用配置 → 各 --config 层，
// 依次用 JSON Merge Patch（RFC 7396，null 表示删除）合并。层清单取自包装脚本用的同一张表。
// 平台配置（tauri.<平台>.conf.*）不在这里合并：它们一律不许存在，见 unregisteredConfigFiles。
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { BASE_CONFIG, configLayersFor, registeredConfigFiles } from "../scripts/tauri-layers.ts";

export const TAURI_DIR = fileURLToPath(new URL("../src-tauri/", import.meta.url));

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

/** `pnpm tauri <subcommand>` 实际生效的配置；subcommand 为 null 表示裸调 tauri CLI（只有共用配置）。 */
export function effectiveConfig(subcommand: string | null): Json {
  const layers = subcommand === null ? [] : configLayersFor(subcommand);
  return [`${TAURI_DIR}${BASE_CONFIG}`, ...layers].map(readJson).reduce(mergePatch);
}

/** 配置里的 build 段；没有就是空对象。 */
export function buildSection(config: Json): { [key: string]: Json } {
  const build = isObject(config) ? config.build : undefined;
  return isObject(build) ? build : {};
}

// Tauri v2 自动读取的配置文件名（tauri-utils 2.10 config/parse.rs 的 ConfigFormat）：
// 共用 tauri.conf.json / tauri.conf.json5 / Tauri.toml，平台 tauri.<平台>.conf.json[5] / Tauri.<平台>.toml。
// 这里放宽成「tauri[.任意段].conf.json[5] 或 tauri[.任意段].toml」，不区分大小写
// （macOS、Windows 的文件系统不区分大小写，Tauri 按固定名字也能读到大小写不同的文件），
// 宁可多拦，不靠写死的平台清单。
const TAURI_CONFIG_NAME = /^tauri(\.[^.]+)?\.(conf\.json5?|toml)$/i;

/** 给定目录里的文件名，返回像 Tauri 配置、但层清单里没登记的那些。 */
export function unregisteredConfigFiles(fileNames: readonly string[]): string[] {
  const registered = new Set(registeredConfigFiles());
  return fileNames.filter((name) => TAURI_CONFIG_NAME.test(name) && !registered.has(name)).sort();
}
