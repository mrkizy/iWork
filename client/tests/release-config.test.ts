// 发布配置里不得出现开发服务器（T01 审查 F-5、2/F-1、2/F-2）。「发布配置」= `pnpm tauri build`
// 实际叠加的那几层，层清单与包装脚本读同一张表（scripts/tauri-layers.ts）。
import { readdirSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { buildSection, effectiveConfig, TAURI_DIR, unregisteredConfigFiles } from "./tauri-config.ts";

const DEV_SERVER_KEYS = ["devUrl", "beforeDevCommand"];
const REFUSE_BARE_BUILD = "node scripts/refuse-bare-build.ts";
// 裸调 tauri CLI 时必须被拒绝脚本挡住的钩子：build 先跑 beforeBuildCommand，单独的 bundle 只跑 beforeBundleCommand
const GUARDED_HOOKS = ["beforeBuildCommand", "beforeBundleCommand"];
// `ipc.localhost` 不算：它是 Tauri 自定义协议在 Linux / Windows 上的来源（发布 CSP 必须放行它，见下文
// 「发布层 CSP」），不是开发服务器。
const LOCAL_ADDRESS = /(?<!ipc\.)localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\]/;

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

describe("本机地址的判定", () => {
  it("ipc.localhost 不算开发服务器，其余本机地址都算", () => {
    expect("http://ipc.localhost").not.toMatch(LOCAL_ADDRESS);
    for (const a of ["http://localhost:1420", "http://127.0.0.1:1420", "http://0.0.0.0", "http://[::1]"]) {
      expect(a).toMatch(LOCAL_ADDRESS);
    }
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

// 发布层 CSP（T09 方案 2.4 第 7 条、R7、E4）：必须放行 Tauri 的自定义协议（否则 invoke 退回 postMessage，
// 帧拿不到）；不许内联脚本、eval 与远程来源。
describe("发布层 CSP", () => {
  const csp = (): Record<string, string[]> => {
    const security = (effectiveConfig("build") as { app?: { security?: { csp?: unknown } } }).app?.security;
    expect(typeof security?.csp, "发布配置必须写 CSP（不能是 null）").toBe("string");
    const out: Record<string, string[]> = {};
    for (const part of String(security?.csp).split(";")) {
      const [name, ...values] = part.trim().split(/\s+/);
      if (name) out[name] = values;
    }
    return out;
  };

  it("connect-src 放行 ipc: 与 http://ipc.localhost", () => {
    expect(csp()["connect-src"]).toEqual(expect.arrayContaining(["ipc:", "http://ipc.localhost"]));
  });

  it("脚本只许应用自身来源：没有内联、eval、远程地址", () => {
    const script = csp()["script-src"] ?? csp()["default-src"];
    expect(script).toEqual(["'self'"]);
    for (const [name, values] of Object.entries(csp())) {
      expect(values, name).not.toContain("'unsafe-eval'");
      for (const v of values) {
        expect(v, `${name} 不许远程来源`).not.toMatch(/^https?:\/\/(?!ipc\.localhost$)/);
      }
    }
  });

  it("不许内嵌框架与插件对象", () => {
    expect(csp()["frame-src"]).toEqual(["'none'"]);
    expect(csp()["object-src"]).toEqual(["'none'"]);
  });

  // 对照：开发层另有 devCsp，放开开发服务器，但不进发布配置。
  it("开发服务器只出现在开发层的 devCsp 里", () => {
    const dev = effectiveConfig("dev") as { app?: { security?: { devCsp?: string } } };
    expect(dev.app?.security?.devCsp).toMatch(/127\.0\.0\.1:1420/);
    expect(JSON.stringify(effectiveConfig("build"))).not.toMatch(/devCsp/);
  });
});
