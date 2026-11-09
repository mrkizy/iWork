// `pnpm tauri <子命令>` 的入口：只支持 dev 和 build，按 scripts/tauri-layers.ts 叠加各自的配置层，
// 然后交给 tauri CLI。不要绕过它直接跑 tauri CLI，默认配置单独用不了（见 tauri-layers.ts）。
import { run } from "@tauri-apps/cli";
import { withConfigLayers } from "./tauri-layers.ts";

function fail(error: unknown): never {
  console.error(error instanceof Error ? error.message : error);
  process.exit(1);
}

let args: string[];
try {
  args = withConfigLayers(process.argv.slice(2), process.env);
} catch (error) {
  fail(error);
}
run(args, "pnpm tauri").catch(fail);
