// 包装脚本 `pnpm tauri` 是 tauri CLI 的唯一入口（T01 审查 2/F-1、2/F-2）：
// 只放行登记过的子命令，配置层只能来自层清单，调用方不能自己再叠 --config。
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import {
  assembleArgs,
  builtPrograms,
  checkBuildArgs,
  configLayersFor,
  isHelpOrVersion,
  staleBundleDir,
  tauriBuildArgs,
  withConfigLayers,
} from "../scripts/tauri-layers.ts";

const CLIENT_DIR = fileURLToPath(new URL("..", import.meta.url));

describe("包装脚本只放行登记过的子命令", () => {
  it.each(["dev", "build"])("%s 放行，并叠上层清单里它的配置层", (subcommand) => {
    const layers = configLayersFor(subcommand);
    expect(layers.length, "对照：登记过的子命令至少有一层").toBeGreaterThan(0);
    expect(withConfigLayers([subcommand, "--verbose"])).toEqual([
      subcommand,
      ...layers.flatMap((file) => ["--config", file]),
      "--verbose",
    ]);
  });

  it.each([["bundle"], ["android", "build"], ["ios", "build"], ["icon"], ["constructor"], ["--version"], []].map((args) => ({ args })))(
    "$args 被拒",
    ({ args }) => {
      expect(() => withConfigLayers(args)).toThrow(/只支持/);
    },
  );

  // 端到端：真跑一次包装脚本，确认拒绝发生在调用 tauri CLI 之前，退出码非 0
  it("pnpm tauri bundle 实跑退出码非 0，错误点名子命令", () => {
    const result = spawnSync(process.execPath, ["scripts/tauri.ts", "bundle"], { cwd: CLIENT_DIR, encoding: "utf8" });
    expect(result.status).not.toBe(0);
    expect(result.stderr).toMatch(/bundle/);
  });
});

describe("调用方不能自己叠加配置", () => {
  it.each([
    ["build", "--config", "src-tauri/tauri.dev.conf.json"],
    ["build", "--config=src-tauri/tauri.dev.conf.json"],
    ["build", "-c", "src-tauri/tauri.dev.conf.json"],
    ["build", "-csrc-tauri/tauri.dev.conf.json"],
    ["build", "-vc", "src-tauri/tauri.dev.conf.json"],
    ["dev", "--config", '{"build":{"devUrl":"http://0.0.0.0:1420"}}'],
  ].map((args) => ({ args })))("$args 被拒", ({ args }) => {
    expect(() => withConfigLayers(args)).toThrow(/--config/);
  });

  it("环境变量 TAURI_CONFIG 也被拒", () => {
    expect(() => withConfigLayers(["build"], { TAURI_CONFIG: "{}" })).toThrow(/TAURI_CONFIG/);
  });

  // 对照：形似但不是配置参数的写法照常放行（--target 只对 dev 放行，build 拒它，见下面「发布构建」）
  it("--ci、--target 等其他参数在 dev 照常放行", () => {
    expect(() => withConfigLayers(["dev", "--ci", "--target", "aarch64-apple-darwin"])).not.toThrow();
    expect(() => checkBuildArgs(["build", "--ci", "--verbose"], {})).not.toThrow();
  });
});

// T04p 方案 4.1、R5–R9
describe("发布构建 build 多拒的参数与环境变量", () => {
  it.each([
    ["--bundles", "dmg"],
    ["--bundles=dmg"],
    ["-b", "dmg"],
    ["--no-bundle"],
    ["--target", "x86_64-apple-darwin"],
    ["-t", "x86_64-apple-darwin"],
    ["--debug"],
    ["-d"],
    ["-vd"],
    ["--runner", "cross"],
    ["-r", "cross"],
    ["--features", "dev"],
    ["--features=dev"],
    ["-f", "dev"],
    ["-fdev"],
    ["--", "--locked"],
  ].map((rest) => ({ rest })))("build $rest 被拒", ({ rest }) => {
    expect(() => checkBuildArgs(["build", ...rest], {})).toThrow(/不接受/);
  });

  it.each(["APPLE_SIGNING_IDENTITY", "APPLE_CERTIFICATE"])("环境变量 %s 被拒", (name) => {
    expect(() => checkBuildArgs(["build"], { [name]: "x" })).toThrow(new RegExp(name));
  });

  it("build 自带 --config 或设了 TAURI_CONFIG 照样被拒", () => {
    expect(() => tauriBuildArgs(["build", "-c", "x.json"], "macos", {})).toThrow(/--config/);
    expect(() => tauriBuildArgs(["build"], "macos", { TAURI_CONFIG: "{}" })).toThrow(/TAURI_CONFIG/);
  });

  it("出哪种包按平台定：macOS 只出 .app，Linux、Windows 不打包", () => {
    const layers = configLayersFor("build").flatMap((file) => ["--config", file]);
    expect(tauriBuildArgs(["build", "--ci"], "macos", {})).toEqual(["build", ...layers, "--bundles", "app", "--ci"]);
    expect(tauriBuildArgs(["build"], "linux", {})).toEqual(["build", ...layers, "--no-bundle"]);
    expect(tauriBuildArgs(["build"], "windows", {})).toEqual(["build", ...layers, "--no-bundle"]);
  });

  // R8 help_and_version_do_not_build：只问帮助、版本时不走发布构建，原样交给 tauri CLI
  it.each([["build", "--help"], ["build", "-h"], ["build", "--version"], ["build", "-V"]])(
    "%s %s 只交给 tauri CLI",
    (...args) => {
      expect(isHelpOrVersion(args)).toBe(true);
      expect(withConfigLayers(args)).toContain(args[1]);
    },
  );
  it("对照：普通的 build 走发布构建", () => {
    expect(isHelpOrVersion(["build", "--verbose"])).toBe(false);
  });
});

/** 进包那次构建的 cargo `--message-format=json` 输出：后台，外加一个库（不是可执行文件）。 */
function daemonArtifacts(daemon: string): string {
  return [
    JSON.stringify({ reason: "compiler-artifact", target: { name: "iwork-daemon", kind: ["bin"] }, executable: daemon }),
    JSON.stringify({ reason: "compiler-artifact", target: { name: "iwork-ipc", kind: ["lib"] }, executable: null }),
    JSON.stringify({ reason: "build-finished", success: true }),
  ].join("\n");
}

/** 核对程序那次构建的输出。 */
function checkArtifacts(check: string): string {
  return [
    JSON.stringify({
      reason: "compiler-artifact",
      target: { name: "check-install", kind: ["example"] },
      executable: check,
    }),
    JSON.stringify({ reason: "build-finished", success: true }),
  ].join("\n");
}

describe("发布构建的路径全取自 cargo（R9）", () => {
  // build_plan_follows_cargo_target_dir：仓库默认的 target/ 里可能躺着旧的包与程序；CARGO_TARGET_DIR 指向另一个目录时
  // cargo metadata 报的是它。算出来的删除位置、装配的五个路径全在新目录下，没有一个落在旧目录里。
  it("build_plan_follows_cargo_target_dir", () => {
    const fresh = "/work/fresh-target";
    const stale = "/repo/target";
    const programs = builtPrograms(
      daemonArtifacts(`${fresh}/release/iwork-daemon`),
      checkArtifacts(`${fresh}/release/examples/check-install`),
      fresh,
      "macos",
    );
    const assemble = assembleArgs(fresh, "macos", programs)!;
    expect(staleBundleDir(fresh)).toBe(`${fresh}/release/bundle`);
    expect(assemble).toEqual([
      "--app",
      `${fresh}/release/bundle/macos/iWork.app`,
      "--client",
      `${fresh}/release/iwork-client`,
      "--daemon",
      `${fresh}/release/iwork-daemon`,
      "--check",
      `${fresh}/release/examples/check-install`,
      "--dist",
      `${fresh}/release/dist`,
    ]);
    expect(assemble.filter((a) => a.startsWith(stale))).toEqual([]);
    expect(staleBundleDir(fresh).startsWith(stale)).toBe(false);
  });

  it("daemon_path_outside_release_stops：cargo 把后台编到 <输出根>/<三元组>/release/ → 停", () => {
    const t = "/repo/target";
    expect(() =>
      builtPrograms(
        daemonArtifacts(`${t}/aarch64-apple-darwin/release/iwork-daemon`),
        checkArtifacts(`${t}/release/examples/check-install`),
        t,
        "macos",
      ),
    ).toThrow(/本机默认目标/);
  });

  it("cargo 没报出两个程序 → 停", () => {
    expect(() => builtPrograms("", "", "/t", "linux")).toThrow(/没有报出/);
  });

  it("后台只认进包那次构建报的：核对程序那次也报出后台，也不拿来进包", () => {
    expect(() =>
      builtPrograms(checkArtifacts("/t/release/examples/check-install"), daemonArtifacts("/t/release/iwork-daemon"), "/t", "linux"),
    ).toThrow(/没有报出/);
  });

  it("Linux 没有 --app；Windows 不装配", () => {
    const programs = { daemon: "/t/release/iwork-daemon", checkInstall: "/t/release/examples/check-install" };
    expect(assembleArgs("/t", "linux", programs)).not.toContain("--app");
    expect(assembleArgs("/t", "windows", programs)).toBeNull();
  });

  it("Windows 上 cargo 报的反斜杠路径照样认得", () => {
    const programs = builtPrograms(
      daemonArtifacts("C:\\t\\release\\iwork-daemon.exe"),
      checkArtifacts("C:\\t\\release\\examples\\check-install.exe"),
      "C:\\t",
      "windows",
    );
    expect(programs.daemon).toContain("iwork-daemon.exe");
  });
});
