// 发布构建的入口（T04p 方案 4.1）：走哪条路，以及六步在调用这一层拿到的路径与环境。
// 外界换成内存里的假文件系统与假命令：cargo 按自己的工作目录解析 CARGO_TARGET_DIR，tauri 在 client/src-tauri 里跑 cargo。
import { posix } from "node:path";
import { describe, expect, it } from "vitest";
import { type ReleaseDeps, clientProduct, planCommand, releaseBuild } from "../scripts/release-build.ts";

describe("走哪条路：只认 tauri 自己的 --help、--version，并且先拒禁用参数（审查 1/F-4）", () => {
  it.each([
    [["build", "--", "--help"]],
    [["build", "--features", "dev", "--", "--help"]],
    [["build", "--features", "dev", "--help"]],
    [["build", "--target", "x86_64-apple-darwin", "--version"]],
  ])("%j 被拒，不当成帮助", (args) => {
    expect(() => planCommand(args, {})).toThrow(/不接受/);
  });

  it.each([[["build", "--help"]], [["build", "-V"]]])("%j 原样交给 tauri CLI", (args) => {
    const plan = planCommand(args, {});
    expect(plan.kind).toBe("passthrough");
  });

  it("对照：普通的 build 走发布构建；dev 照常交给 tauri CLI", () => {
    expect(planCommand(["build", "--verbose"], {}).kind).toBe("release");
    expect(planCommand(["dev", "--help"], {}).kind).toBe("passthrough");
  });
});

const REPO = "/repo";
const TAURI_CWD = "/repo/client/src-tauri";

/** cargo 的输出根：按它所在的工作目录解析 CARGO_TARGET_DIR（相对路径就是相对工作目录）。 */
function cargoTarget(cwd: string, env: Readonly<Record<string, string | undefined>>): string {
  return posix.resolve(cwd, env.CARGO_TARGET_DIR ?? "target");
}

/** 一个假的外界：文件是内存里的一张表；`tauriHonorsEnv` 为 false 时 tauri 不照交给它的环境走（对照）。 */
function world(files: Map<string, string>, tauriHonorsEnv = true) {
  const assembled: string[][] = [];
  const deps: ReleaseDeps = {
    capture(command, args, env) {
      expect(command).toBe("cargo");
      const target = cargoTarget(REPO, env);
      if (args[0] === "metadata") return JSON.stringify({ target_directory: target });
      files.set(`${target}/release/iwork-daemon`, "FRESH_DAEMON");
      files.set(`${target}/release/examples/check-install`, "CHECK");
      return [
        JSON.stringify({
          reason: "compiler-artifact",
          target: { name: "iwork-daemon", kind: ["bin"] },
          executable: `${target}/release/iwork-daemon`,
        }),
        JSON.stringify({
          reason: "compiler-artifact",
          target: { name: "check-install", kind: ["example"] },
          executable: `${target}/release/examples/check-install`,
        }),
      ].join("\n");
    },
    remove(path) {
      for (const key of [...files.keys()]) {
        if (key === path || key.startsWith(`${path}/`)) files.delete(key);
      }
    },
    exists: (path) => files.has(path),
    async runTauri(_args, env) {
      const target = cargoTarget(TAURI_CWD, tauriHonorsEnv ? env : { CARGO_TARGET_DIR: "relative-output" });
      files.set(`${target}/release/iwork-client`, "FRESH_CLIENT");
      files.set(`${target}/release/bundle/macos/iWork.app/Contents/MacOS/iwork-client`, "FRESH_CLIENT");
    },
    runAssemble(args) {
      assembled.push(args);
    },
  };
  return { deps, assembled };
}

function argAfter(args: string[], flag: string): string {
  const i = args.indexOf(flag);
  expect(i, `${flag} 在参数里`).toBeGreaterThanOrEqual(0);
  return args[i + 1];
}

describe("六步共用 cargo 报的绝对输出根（审查 1/F-3）", () => {
  // 相对的 CARGO_TARGET_DIR：仓库根下这个目录里躺着上一次的客户端。tauri 在 client/src-tauri 里跑 cargo，
  // 要是没给它钉绝对的输出根，客户端会编到 client/src-tauri/relative-output，装配却读到仓库根那份旧的。
  it("相对的 CARGO_TARGET_DIR：装配拿到的是这一次编的客户端", async () => {
    const files = new Map<string, string>([["/repo/relative-output/release/iwork-client", "OLD_CLIENT"]]);
    const { deps, assembled } = world(files);
    const { targetDir } = await releaseBuild(["build"], { CARGO_TARGET_DIR: "relative-output" }, "linux", deps);
    expect(targetDir).toBe("/repo/relative-output");
    expect(assembled).toHaveLength(1);
    const client = argAfter(assembled[0], "--client");
    expect(client).toBe(clientProduct(targetDir, "linux"));
    expect(files.get(client), "装配读的是新客户端").toBe("FRESH_CLIENT");
    expect(files.get(argAfter(assembled[0], "--daemon"))).toBe("FRESH_DAEMON");
    expect(files.has("/repo/client/src-tauri/relative-output/release/iwork-client"), "没编到别处").toBe(false);
  });

  it("对照：tauri 不照钉好的输出根走 → 输出根里没有这一次的客户端，停下，不装配", async () => {
    const files = new Map<string, string>([["/repo/relative-output/release/iwork-client", "OLD_CLIENT"]]);
    const { deps, assembled } = world(files, false);
    await expect(
      releaseBuild(["build"], { CARGO_TARGET_DIR: "relative-output" }, "linux", deps),
    ).rejects.toThrow(/没有把客户端编到/);
    expect(assembled).toEqual([]);
  });

  it("macOS：装配拿到的 .app 与客户端都在同一个输出根下", async () => {
    const files = new Map<string, string>();
    const { deps, assembled } = world(files);
    const { targetDir } = await releaseBuild(["build"], {}, "macos", deps);
    expect(targetDir).toBe("/repo/target");
    expect(argAfter(assembled[0], "--app")).toBe("/repo/target/release/bundle/macos/iWork.app");
    expect(argAfter(assembled[0], "--client")).toBe("/repo/target/release/iwork-client");
  });
});
