// `pnpm tauri <子命令>` 要做什么（T04p 方案 4.1）。tauri.ts 把真的起进程、删文件、调 tauri CLI 的办法传进来；
// 测试传假的，在调用这一层验证每一步拿到的路径与环境。
import {
  type BuiltPrograms,
  type Platform,
  DAEMON_BUILD_ARGS,
  assembleArgs,
  builtPrograms,
  checkBuildArgs,
  isHelpOrVersion,
  isTestBuild,
  staleBundleDir,
  tauriBuildArgs,
  withConfigLayers,
} from "./tauri-layers.ts";

type Env = Readonly<Record<string, string | undefined>>;

/** 一次 `pnpm tauri …` 要走哪条路。 */
export type Plan = { kind: "passthrough"; args: string[] } | { kind: "release" };

/**
 * dev、测试构建（`--debug --no-bundle`，不出任何包）、以及只问 Tauri 自己的帮助或版本：原样（叠上配置层）交给
 * tauri CLI。别的 build：发布构建。
 * build 先查禁用的参数与环境，再看是不是只问帮助：`build --features dev --help` 被拒，`-- --help` 是转给构建程序
 * 的参数（`--` 本身就被拒），不算帮助（审查 1/F-4）。
 */
export function planCommand(args: readonly string[], env: Env): Plan {
  if (args[0] !== "build") {
    return { kind: "passthrough", args: withConfigLayers(args, env) };
  }
  if (isTestBuild(args)) {
    checkBuildArgs(args, env, "test");
    return { kind: "passthrough", args: withConfigLayers(args, env) };
  }
  checkBuildArgs(args, env);
  if (isHelpOrVersion(args)) {
    return { kind: "passthrough", args: withConfigLayers(args, env) };
  }
  return { kind: "release" };
}

/** 发布构建要碰外界的几件事。 */
export interface ReleaseDeps {
  /** 跑一个命令（在仓库根），返回 stdout；失败抛错。 */
  capture(command: string, args: readonly string[], env: Env): string;
  /** 删掉一个路径（不在也算成功）。 */
  remove(path: string): void;
  exists(path: string): boolean;
  /** 交给 tauri CLI（它在 client/src-tauri 里跑 cargo）。 */
  runTauri(args: string[], env: Env): Promise<void>;
  /** 跑装配脚本；非零退出抛错。 */
  runAssemble(args: string[]): void;
}

/** 这一次构建出来的客户端在输出根里的位置。 */
export function clientProduct(targetDir: string, platform: Platform): string {
  return `${targetDir}/release/iwork-client${platform === "windows" ? ".exe" : ""}`;
}

/**
 * 发布构建的六步。输出根 T 只问一次 cargo（`cargo metadata` 报的是绝对路径），之后后台、tauri、装配都用同一个 T：
 * 交给 tauri 的环境里显式钉 `CARGO_TARGET_DIR=T`——tauri 在 client/src-tauri 里跑 cargo，相对的 CARGO_TARGET_DIR 或
 * 那里的 cargo 配置会让客户端编到别处，装配却去读 T 里的旧客户端（审查 1/F-3）。交给 tauri 之前删掉 T 里的客户端，
 * 之后它必须在：在就只可能是这一次编的。
 */
export async function releaseBuild(
  args: readonly string[],
  env: Env,
  platform: Platform,
  deps: ReleaseDeps,
): Promise<{ targetDir: string; programs: BuiltPrograms }> {
  checkBuildArgs(args, env);
  tauriBuildArgs(args, platform, env);
  const metadata = JSON.parse(deps.capture("cargo", ["metadata", "--format-version", "1", "--no-deps"], env)) as {
    target_directory: string;
  };
  const targetDir = metadata.target_directory;
  const pinned: Env = { ...env, CARGO_TARGET_DIR: targetDir };
  deps.remove(staleBundleDir(targetDir));
  const client = clientProduct(targetDir, platform);
  deps.remove(client);
  const programs = builtPrograms(deps.capture("cargo", DAEMON_BUILD_ARGS, pinned), targetDir, platform);
  await deps.runTauri(tauriBuildArgs(args, platform, pinned), pinned);
  if (!deps.exists(client)) {
    throw new Error(`tauri 没有把客户端编到 ${client}（输出根被改到了别处），不装配`);
  }
  const assemble = assembleArgs(targetDir, platform, programs);
  if (assemble === null) {
    console.log("M1 不出 Windows 包：只编了客户端，没有装配");
  } else {
    deps.runAssemble(assemble);
  }
  return { targetDir, programs };
}
