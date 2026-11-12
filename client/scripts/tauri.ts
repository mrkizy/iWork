// `pnpm tauri <子命令>` 的入口：只支持 dev 和 build，按 scripts/tauri-layers.ts 叠加各自的配置层，
// 然后交给 tauri CLI。不要绕过它直接跑 tauri CLI，默认配置单独用不了（见 tauri-layers.ts）。
//
// build 是发布构建（T04p 方案 4.1），依次：查参数与环境 → 问 cargo 输出根在哪 → 删上一次打包器留下的包 →
// 编后台与核对程序 → 交给 tauri CLI → scripts/assemble-release.sh 装配、核对、发布到 <输出根>/release/dist/。
// 每一步的命令行由 tauri-layers.ts 里的纯函数算，这里只起进程；哪一步失败都以非零退出，后面的不做。
import { spawnSync } from "node:child_process";
import { rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { run } from "@tauri-apps/cli";
import {
  type Platform,
  DAEMON_BUILD_ARGS,
  assembleArgs,
  builtPrograms,
  checkBuildArgs,
  isHelpOrVersion,
  staleBundleDir,
  tauriBuildArgs,
  withConfigLayers,
} from "./tauri-layers.ts";

const REPO_ROOT = fileURLToPath(new URL("../../", import.meta.url));

function fail(error: unknown): never {
  console.error(error instanceof Error ? error.message : error);
  process.exit(1);
}

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

/** 跑一个命令，拿它的 stdout；失败（启动不了或非零退出）抛错。stderr 原样给用户看。 */
function capture(command: string, args: readonly string[]): string {
  const result = spawnSync(command, args, {
    cwd: REPO_ROOT,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
    maxBuffer: 256 * 1024 * 1024,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} ${args.join(" ")} 退出 ${result.status}`);
  return result.stdout;
}

async function releaseBuild(args: readonly string[]): Promise<void> {
  const here = platform();
  checkBuildArgs(args, process.env);
  // 自带的配置与 TAURI_CONFIG 也在动手之前拒（第 5 步还会再算一次，结果相同）
  tauriBuildArgs(args, here, process.env);
  const metadata = JSON.parse(capture("cargo", ["metadata", "--format-version", "1", "--no-deps"])) as {
    target_directory: string;
  };
  const targetDir = metadata.target_directory;
  rmSync(staleBundleDir(targetDir), { recursive: true, force: true });
  const programs = builtPrograms(capture("cargo", DAEMON_BUILD_ARGS), targetDir, here);
  await run(tauriBuildArgs(args, here, process.env), "pnpm tauri");
  const assemble = assembleArgs(targetDir, here, programs);
  if (assemble === null) {
    console.log("M1 不出 Windows 包：只编了客户端，没有装配");
    return;
  }
  const result = spawnSync(`${REPO_ROOT}scripts/assemble-release.sh`, assemble, { cwd: REPO_ROOT, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`装配没过（退出 ${result.status}），没有发布新包`);
}

const argv = process.argv.slice(2);
try {
  if (argv[0] === "build" && !isHelpOrVersion(argv)) {
    await releaseBuild(argv);
  } else {
    await run(withConfigLayers(argv, process.env), "pnpm tauri");
  }
} catch (error) {
  fail(error);
}
