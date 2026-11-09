// tauri CLI 的配置层：唯一真值。包装脚本 scripts/tauri.ts 按它放行子命令、传 --config，
// tests/ 下的检查按它拼出各子命令实际生效的配置、判定 src-tauri/ 里有没有未登记的配置文件，
// 两边读的是同一张表（T01 审查 F-5、2/F-1、2/F-2）。
//
// 三层（主管裁定）：
//   src-tauri/tauri.conf.json         共用部分：窗口、打包、标识；不含任何前端入口。
//                                     beforeBuildCommand / beforeBundleCommand 指向拒绝脚本，
//                                     绕过包装脚本裸跑 `tauri build`、`tauri bundle` 都会失败
//   src-tauri/tauri.dev.conf.json     只给 `tauri dev`：开发服务器地址与启动命令
//   src-tauri/tauri.build.conf.json   只给 `tauri build`：静态资源目录与前端构建命令，并撤掉拒绝脚本
// 不用 Tauri 自动合并的平台配置（tauri.<平台>.conf.*）放这些，那会同时进开发和发布；
// 平台配置一律不许出现（tests/release-config.test.ts 检查）。
import { fileURLToPath } from "node:url";

const TAURI_DIR = fileURLToPath(new URL("../src-tauri/", import.meta.url));

/** Tauri 自己读取的共用配置。 */
export const BASE_CONFIG = "tauri.conf.json";

// 登记表：包装脚本放行的子命令 → 它要叠加的配置层。只支持这两个子命令，加一个就是加一行。
const LAYERS: Readonly<Record<string, readonly string[]>> = {
  dev: ["tauri.dev.conf.json"],
  build: ["tauri.build.conf.json"],
};

/** src-tauri/ 下允许存在的全部 Tauri 配置文件：共用配置加各层。 */
export function registeredConfigFiles(): string[] {
  return [BASE_CONFIG, ...Object.values(LAYERS).flat()];
}

/** 子命令要叠加的配置层（绝对路径，按合并顺序）；没登记的子命令返回空。 */
export function configLayersFor(subcommand: string): string[] {
  // 绝对路径：tauri CLI 运行中会切换工作目录，相对路径的解析基准不可靠
  // hasOwn 而不是 in / 直接取值：constructor、toString 这类原型上的名字不能算登记过
  const files = Object.hasOwn(LAYERS, subcommand) ? LAYERS[subcommand] : undefined;
  return (files ?? []).map((file) => `${TAURI_DIR}${file}`);
}

// 调用方自带的配置会叠在层清单之后、盖掉它（比如给 build 叠上开发层），所以一律不收。
// 短参数可以连写（-c FILE、-cFILE、-vc FILE），凡是以单个 - 开头且含 c 的都按配置参数处理。
function isConfigArg(arg: string): boolean {
  return arg === "--config" || arg.startsWith("--config=") || /^-[^-]*c/.test(arg);
}

/**
 * 把 `pnpm tauri` 的参数变成交给 tauri CLI 的参数：在子命令后插入它的 --config 层。
 * 子命令没登记、调用方自带 -c/--config、或设了环境变量 TAURI_CONFIG 时抛错，错误信息说明原因。
 */
export function withConfigLayers(
  args: readonly string[],
  env: Readonly<Record<string, string | undefined>> = {},
): string[] {
  const [subcommand, ...rest] = args;
  const allowed = Object.keys(LAYERS).join("、");
  if (subcommand === undefined || !Object.hasOwn(LAYERS, subcommand)) {
    throw new Error(
      `pnpm tauri 只支持 ${allowed}，收到 ${subcommand ?? "（无子命令）"}。` +
        "其他子命令（bundle、android、ios 等）不会叠加配置层，产物会缺前端资源或带上开发配置。",
    );
  }
  const configArg = rest.find(isConfigArg);
  if (configArg !== undefined) {
    throw new Error(`pnpm tauri 不接受自带的 -c/--config（收到 ${configArg}）：配置层只能来自 scripts/tauri-layers.ts。`);
  }
  if (env.TAURI_CONFIG !== undefined) {
    throw new Error("pnpm tauri 不接受环境变量 TAURI_CONFIG：配置层只能来自 scripts/tauri-layers.ts。");
  }
  const layers = configLayersFor(subcommand).flatMap((file) => ["--config", file]);
  return [subcommand, ...layers, ...rest];
}
