// `pnpm tauri <子命令>` 的入口：只支持 dev 和 build，按 scripts/tauri-layers.ts 叠加各自的配置层，
// 然后交给 tauri CLI。不要绕过它直接跑 tauri CLI，默认配置单独用不了（见 tauri-layers.ts）。
//
// build 是发布构建（T04p 方案 4.1）：要做什么由 release-build.ts 定，这里只提供真的起进程、删文件、调 tauri CLI 的办法。
import { spawnSync } from "node:child_process";
import { existsSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { run } from "@tauri-apps/cli";
import { type ReleaseDeps, planCommand, releaseBuild } from "./release-build.ts";
import type { Platform } from "./tauri-layers.ts";

const REPO_ROOT = fileURLToPath(new URL("../../", import.meta.url));

function platform(): Platform {
  switch (process.platform) {
    case "darwin":
      return "macos";
    case "win32":
      return "windows";
    default:
      return "linux";
  }
}

const deps: ReleaseDeps = {
  capture(command, args, env) {
    const result = spawnSync(command, args, {
      cwd: REPO_ROOT,
      env: { ...env },
      encoding: "utf8",
      stdio: ["ignore", "pipe", "inherit"],
      maxBuffer: 256 * 1024 * 1024,
    });
    if (result.error) throw result.error;
    if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} 退出 ${result.status}`);
    return result.stdout;
  },
  remove(path) {
    rmSync(path, { recursive: true, force: true });
  },
  exists: existsSync,
  async runTauri(args, env) {
    // tauri CLI 在本进程里跑、用本进程的环境起 cargo：把钉好的环境放进 process.env
    for (const [k, v] of Object.entries(env)) {
      if (v !== undefined) process.env[k] = v;
    }
    await run(args, "pnpm tauri");
  },
  runAssemble(args) {
    const result = spawnSync(`${REPO_ROOT}scripts/assemble-release.sh`, args, { cwd: REPO_ROOT, stdio: "inherit" });
    if (result.error) throw result.error;
    if (result.status !== 0) throw new Error(`装配没过（退出 ${result.status}），没有发布新包`);
  },
};

const argv = process.argv.slice(2);
try {
  const plan = planCommand(argv, process.env);
  if (plan.kind === "release") {
    await releaseBuild(argv, process.env, platform(), deps);
  } else {
    await run(plan.args, "pnpm tauri");
  }
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exit(1);
}
