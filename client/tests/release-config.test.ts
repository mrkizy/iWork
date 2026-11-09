// 发布配置里不得出现开发服务器（T01 审查 F-5、2/F-1、2/F-2）。「发布配置」= `pnpm tauri build`
// 实际叠加的那几层，层清单与包装脚本读同一张表（scripts/tauri-layers.ts）。
import { readdirSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { buildSection, effectiveConfig, TAURI_DIR, unregisteredConfigFiles } from "./tauri-config.ts";

const DEV_SERVER_KEYS = ["devUrl", "beforeDevCommand"];
const REFUSE_BARE_BUILD = "node scripts/refuse-bare-build.ts";
// 裸调 tauri CLI 时必须被拒绝脚本挡住的钩子：build 先跑 beforeBuildCommand，单独的 bundle 只跑 beforeBundleCommand
const GUARDED_HOOKS = ["beforeBuildCommand", "beforeBundleCommand"];
const LOCAL_ADDRESS = /localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\]/;

describe("src-tauri/ 里只有层清单登记过的 Tauri 配置", () => {
  it("实际目录里没有未登记的配置文件", () => {
    expect(unregisteredConfigFiles(readdirSync(TAURI_DIR))).toEqual([]);
  });

  it.each([
    "tauri.macos.conf.json",
    "tauri.macos.conf.json5",
    "Tauri.macos.toml",
    "tauri.linux.conf.json",
    "tauri.windows.conf.json5",
    "Tauri.ios.toml",
    "tauri.conf.json5",
    "Tauri.toml",
    "TAURI.android.CONF.JSON",
  ])("出现 %s 时判为未登记", (file) => {
    expect(unregisteredConfigFiles(["tauri.conf.json", "tauri.build.conf.json", file])).toEqual([file]);
  });

  // 对照：登记过的层与无关文件不误报
  it("登记过的层与无关文件不报", () => {
    expect(
      unregisteredConfigFiles(["tauri.conf.json", "tauri.dev.conf.json", "tauri.build.conf.json", "Cargo.toml", "build.rs"]),
    ).toEqual([]);
  });
});

describe("发布配置不含开发服务器", () => {
  it("pnpm tauri build 的配置里没有开发服务器入口与本机地址", () => {
    const config = effectiveConfig("build");
    for (const key of DEV_SERVER_KEYS) {
      expect(buildSection(config), `build.${key}`).not.toHaveProperty(key);
    }
    expect(JSON.stringify(config)).not.toMatch(LOCAL_ADDRESS);
    // 对照：构建层确实叠上了，证明查的是完整的发布配置
    expect(buildSection(config), "build.frontendDist").toHaveProperty("frontendDist");
  });

  // 发布入口只有 `pnpm tauri build`：不叠加构建层时 Tauri 照样能打出缺前端资源的空壳包，
  // 所以共用配置的两个钩子都是拒绝脚本，构建层再把它们换掉
  it.each(GUARDED_HOOKS)("裸调 tauri CLI 时 %s 是拒绝脚本，经 pnpm tauri build 时不是", (hook) => {
    expect(buildSection(effectiveConfig(null))[hook]).toBe(REFUSE_BARE_BUILD);
    expect(buildSection(effectiveConfig(null))).not.toHaveProperty("frontendDist");
    expect(buildSection(effectiveConfig("build"))[hook]).not.toBe(REFUSE_BARE_BUILD);
  });

  // 对照：开发层确实带着开发服务器入口，证明上面查的是真会出现的键
  it("pnpm tauri dev 的配置带开发服务器入口", () => {
    const build = buildSection(effectiveConfig("dev"));
    for (const key of DEV_SERVER_KEYS) {
      expect(build, `build.${key}`).toHaveProperty(key);
    }
  });
});
