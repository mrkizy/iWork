// 发布配置里不得出现开发服务器（T01 审查 F-5）。「发布配置」= `pnpm tauri build` 实际叠加的那几层，
// 层清单与包装脚本读同一张表（scripts/tauri-layers.ts）。
import { existsSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { buildSection, effectiveConfig, PLATFORMS, TAURI_DIR } from "./tauri-config.ts";

// Tauri 也认这两种格式，出现了这里的检查就会漏看，干脆不许出现
const UNCHECKED_FORMATS = ["tauri.conf.json5", "Tauri.toml"];

const DEV_SERVER_KEYS = ["devUrl", "beforeDevCommand"];
const REFUSE_BARE_BUILD = "node scripts/refuse-bare-build.ts";
const LOCAL_ADDRESS = /localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\]/;

describe("发布配置不含开发服务器", () => {
  it.each(UNCHECKED_FORMATS)("不存在本检查覆盖不到的配置格式 %s", (file) => {
    expect(existsSync(`${TAURI_DIR}${file}`), file).toBe(false);
  });

  it.each(PLATFORMS)("%s：pnpm tauri build 的配置里没有开发服务器入口与本机地址", (platform) => {
    const config = effectiveConfig("build", platform);
    for (const key of DEV_SERVER_KEYS) {
      expect(buildSection(config), `build.${key}`).not.toHaveProperty(key);
    }
    expect(JSON.stringify(config)).not.toMatch(LOCAL_ADDRESS);
    // 对照：构建层确实叠上了，证明查的是完整的发布配置
    expect(buildSection(config), "build.frontendDist").toHaveProperty("frontendDist");
  });

  // 发布入口只有 `pnpm tauri build`：不叠加构建层时 Tauri 照样能打出缺前端资源的空壳包，
  // 所以默认配置的 beforeBuildCommand 是拒绝脚本，构建层再把它换成真正的前端构建
  it.each(PLATFORMS)("%s：裸跑 tauri build 会被拒绝，经包装脚本不会", (platform) => {
    expect(buildSection(effectiveConfig("", platform)).beforeBuildCommand).toBe(REFUSE_BARE_BUILD);
    expect(buildSection(effectiveConfig("", platform))).not.toHaveProperty("frontendDist");
    expect(buildSection(effectiveConfig("build", platform)).beforeBuildCommand).not.toBe(REFUSE_BARE_BUILD);
  });

  // 对照：开发层确实带着开发服务器入口，证明上面查的是真会出现的键
  it("pnpm tauri dev 的配置带开发服务器入口", () => {
    const build = buildSection(effectiveConfig("dev", "linux"));
    for (const key of DEV_SERVER_KEYS) {
      expect(build, `build.${key}`).toHaveProperty(key);
    }
  });
});
