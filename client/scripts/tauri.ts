// `pnpm tauri <子命令>` 的入口：按 scripts/tauri-layers.ts 给 dev / build 叠加各自的配置层，
// 然后交给 tauri CLI。不要绕过它直接跑 tauri CLI，默认配置单独用不了（见 tauri-layers.ts）。
import { run } from "@tauri-apps/cli";
import { withConfigLayers } from "./tauri-layers.ts";

run(withConfigLayers(process.argv.slice(2)), "pnpm tauri").catch((error: unknown) => {
  console.error(error instanceof Error ? error.message : error);
  process.exit(1);
});
