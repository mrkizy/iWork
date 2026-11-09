// 包装脚本 `pnpm tauri` 是 tauri CLI 的唯一入口（T01 审查 2/F-1、2/F-2）：
// 只放行登记过的子命令，配置层只能来自层清单，调用方不能自己再叠 --config。
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { configLayersFor, withConfigLayers } from "../scripts/tauri-layers.ts";

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

  // 对照：形似但不是配置参数的写法照常放行
  it("--ci、--target 等其他参数照常放行", () => {
    expect(() => withConfigLayers(["build", "--ci", "--target", "aarch64-apple-darwin"])).not.toThrow();
  });
});
