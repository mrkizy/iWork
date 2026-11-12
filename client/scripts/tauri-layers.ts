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

// ---------------------------------------------------------------- 发布构建（T04p 方案 4.1）
// `pnpm tauri build` 不只是交给 tauri CLI：先编后台与核对程序，tauri 只把客户端打成 .app（Linux 不打包），之后由
// scripts/assemble-release.sh 放后台、写清单或签名、核对、发布到 <输出根>/release/dist/。下面是算命令行的纯函数，
// scripts/tauri.ts 只负责依次起进程。

/** 只问帮助或版本：原样交给 tauri CLI，不编、不装配。 */
export function isHelpOrVersion(args: readonly string[]): boolean {
  return args.some((a) => ["-h", "--help", "-V", "--version"].includes(a));
}

// build 多拒的参数：长名、短名（单字母；短参数可连写、可贴值，凡以单个 - 开头且含这个字母的都算）、为什么拒。
const REFUSED_BUILD_FLAGS: readonly { long: string; short: string | null; why: string }[] = [
  { long: "--bundles", short: "b", why: "出哪种包由打包命令按平台定；让调用方选 dmg 得到的是缺后台的安装包" },
  { long: "--no-bundle", short: null, why: "同上：出哪种包由打包命令定" },
  { long: "--target", short: "t", why: "构建会去 target/<三元组>/，装配只认这一次 cargo 报的位置" },
  { long: "--debug", short: "d", why: "构建会去 debug/，发布包只出 release" },
  { long: "--runner", short: "r", why: "换了构建程序，产物位置不再由 cargo 定" },
  { long: "--features", short: "f", why: "能给客户端开 dev，打出带 --dev-trust 的「发布包」" },
];

// 会让打包器自己签名的环境变量：签名只许在装配步骤做（封印之后不能再往包里放后台）
const REFUSED_BUILD_ENV = ["APPLE_SIGNING_IDENTITY", "APPLE_CERTIFICATE"];

/** build 的参数与环境不被接受时抛错，错误信息写明原因。`--` 之后的参数原样转给构建程序，一律不收。 */
export function checkBuildArgs(args: readonly string[], env: Readonly<Record<string, string | undefined>>): void {
  const rest = args.slice(1);
  if (rest.includes("--")) {
    throw new Error("pnpm tauri build 不接受 -- 之后转给构建程序的参数：构建去向只由打包命令定。");
  }
  for (const arg of rest) {
    for (const flag of REFUSED_BUILD_FLAGS) {
      const isLong = arg === flag.long || arg.startsWith(`${flag.long}=`);
      const isShort = flag.short !== null && new RegExp(`^-[^-]*${flag.short}`).test(arg);
      if (isLong || isShort) {
        throw new Error(`pnpm tauri build 不接受 ${arg}：${flag.why}。`);
      }
    }
  }
  for (const name of REFUSED_BUILD_ENV) {
    if (env[name] !== undefined) {
      throw new Error(`pnpm tauri build 不接受环境变量 ${name}：签名只在装配步骤里做（IWORK_SIGN_IDENTITY 选形态）。`);
    }
  }
}

export type Platform = "macos" | "linux" | "windows";

/** 第 5 步交给 tauri CLI 的参数：层清单的配置 + 按平台定的出包方式（同样不收自带的配置与 TAURI_CONFIG）。 */
export function tauriBuildArgs(
  args: readonly string[],
  platform: Platform,
  env: Readonly<Record<string, string | undefined>>,
): string[] {
  const bundling = platform === "macos" ? ["--bundles", "app"] : ["--no-bundle"];
  return withConfigLayers(["build", ...bundling, ...args.slice(1)], env);
}

/** 第 4 步：编后台与核对程序，让 cargo 用 JSON 报出产物位置。 */
export const DAEMON_BUILD_ARGS = [
  "build",
  "--release",
  "--locked",
  "-p",
  "iwork-daemon",
  "--bin",
  "iwork-daemon",
  "--example",
  "check-install",
  "--message-format=json",
];

/** 这一次构建出来的两个程序（位置是 cargo 报的）。 */
export interface BuiltPrograms {
  daemon: string;
  checkInstall: string;
}

/**
 * 从 cargo 的 JSON 输出里取后台与核对程序的位置，并核对后台恰好在 `<输出根>/release/iwork-daemon`：
 * 不是（构建目标被环境变量或 cargo 配置改了）就抛错——发布包只出本机默认目标。
 */
export function builtPrograms(cargoJson: string, targetDir: string, platform: Platform): BuiltPrograms {
  const exe = platform === "windows" ? ".exe" : "";
  let daemon: string | undefined;
  let checkInstall: string | undefined;
  for (const line of cargoJson.split("\n")) {
    if (!line.startsWith("{")) continue;
    const msg = JSON.parse(line) as {
      reason?: string;
      target?: { name?: string; kind?: string[] };
      executable?: string | null;
    };
    if (msg.reason !== "compiler-artifact" || !msg.executable) continue;
    if (msg.target?.name === "iwork-daemon" && msg.target.kind?.includes("bin")) daemon = msg.executable;
    if (msg.target?.name === "check-install" && msg.target.kind?.includes("example")) checkInstall = msg.executable;
  }
  if (daemon === undefined || checkInstall === undefined) {
    throw new Error("cargo 的输出里没有报出 iwork-daemon 或 check-install 的位置");
  }
  const expected = joinPath(targetDir, "release", `iwork-daemon${exe}`);
  if (joinPath(daemon) !== expected) {
    throw new Error(`发布包只出本机默认目标：后台编到了 ${daemon}，不是 ${expected}（环境变量或 cargo 配置改了构建目标）`);
  }
  return { daemon, checkInstall };
}

/** 第 3 步删掉的位置：上一次打包器留下的 .app。删了之后这里再出现的只可能是这一次做的。 */
export function staleBundleDir(targetDir: string): string {
  return joinPath(targetDir, "release", "bundle");
}

/** 第 6 步：装配脚本的参数。路径全来自 cargo（targetDir、programs），没有一个是写死的 target/。Windows 不装配。 */
export function assembleArgs(targetDir: string, platform: Platform, programs: BuiltPrograms): string[] | null {
  if (platform === "windows") return null;
  const release = joinPath(targetDir, "release");
  const app = platform === "macos" ? ["--app", joinPath(release, "bundle", "macos", "iWork.app")] : [];
  return [
    ...app,
    "--client",
    joinPath(release, "iwork-client"),
    "--daemon",
    programs.daemon,
    "--check",
    programs.checkInstall,
    "--dist",
    joinPath(release, "dist"),
  ];
}

// cargo 报的是本机平台的路径（Windows 上是反斜杠）；这里统一成正斜杠再拼、再比，不经 node:path，
// 纯函数在测试里给什么平台都一样
function joinPath(...parts: string[]): string {
  return parts.join("/").replace(/\\/g, "/").replace(/\/+/g, "/");
}
