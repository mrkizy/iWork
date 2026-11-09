// tauri CLI 各子命令要叠加的配置层：唯一真值。包装脚本 scripts/tauri.ts 按它传 --config，
// tests/release-config.test.ts 按它拼出发布配置再检查，两边读的是同一张表（T01 审查 F-5）。
//
// 三层（主管裁定）：
//   src-tauri/tauri.conf.json         共用部分：窗口、打包、标识；不含任何前端入口，
//                                     所以裸跑 `tauri build` 会因缺 frontendDist 失败，发布入口只剩 `pnpm tauri build`
//   src-tauri/tauri.dev.conf.json     只给 `tauri dev`：开发服务器地址与启动命令
//   src-tauri/tauri.build.conf.json   只给 `tauri build`：静态资源目录与前端构建命令
// 不用 Tauri 自动合并的平台配置（tauri.<平台>.conf.json）放这些，那会同时进开发和发布。
import { fileURLToPath } from "node:url";

const TAURI_DIR = fileURLToPath(new URL("../src-tauri/", import.meta.url));

const LAYERS: Readonly<Record<string, readonly string[]>> = {
  dev: ["tauri.dev.conf.json"],
  build: ["tauri.build.conf.json"],
};

/** 子命令要叠加的配置层（绝对路径，按合并顺序）；没登记的子命令返回空。 */
export function configLayersFor(subcommand: string | undefined): string[] {
  const files = subcommand === undefined ? undefined : LAYERS[subcommand];
  // 绝对路径：tauri CLI 运行中会切换工作目录，相对路径的解析基准不可靠
  return (files ?? []).map((file) => `${TAURI_DIR}${file}`);
}

/** 在子命令后插入它的 --config 层，其余参数原样保留。 */
export function withConfigLayers(args: readonly string[]): string[] {
  const [subcommand, ...rest] = args;
  if (subcommand === undefined) return [];
  const layers = configLayersFor(subcommand).flatMap((file) => ["--config", file]);
  return [subcommand, ...layers, ...rest];
}
