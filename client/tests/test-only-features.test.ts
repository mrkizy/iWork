// 只给测试用的 feature 清单的解析，与出包那一刻的检查（scripts/test-only-features.txt）。
import { describe, expect, it } from "vitest";
import {
  loadTestOnlyFeatures,
  localPackageName,
  parseTestOnlyFeatures,
  testFeaturesInBuild,
} from "../scripts/test-only-features.ts";

describe("清单", () => {
  it("仓库里那一份能解析，并且登记了 iwork-daemon 的 test-hooks", () => {
    const list = loadTestOnlyFeatures();
    expect(list.testFeatures).toContainEqual({ crate: "iwork-daemon", feature: "test-hooks" });
  });

  it.each([["test-feature iwork-daemon"], ["test-features iwork-daemon test-hooks"], ["test-feature a b c"]])(
    "写错的行 %j 报错并点名行号",
    (bad) => {
      expect(() => parseTestOnlyFeatures(`# 注释\n\n${bad}\n`)).toThrow(/第 3 行/);
    },
  );

  it("一条 test-feature 都没有也报错（空清单什么都挡不住）", () => {
    expect(() => parseTestOnlyFeatures("release-root iwork-daemon iwork-daemon\n")).toThrow(/一条 test-feature 都没有/);
  });
});

describe("本地包的名字", () => {
  it.each([
    ["path+file:///r/crates/iwork-daemon#0.1.0", "iwork-daemon"],
    ["path+file:///r/client/src-tauri#iwork-client@0.1.0", "iwork-client"],
    ["registry+https://github.com/rust-lang/crates.io-index#tokio@1.0.0", undefined],
  ])("%s → %s", (id, name) => {
    expect(localPackageName(id)).toBe(name);
  });
});

describe("一次构建里带了测试 feature 的本地产物", () => {
  const list = parseTestOnlyFeatures("test-feature iwork-store test-hooks\ntest-feature iwork-daemon dev\n");
  const artifact = (id: string, name: string, kind: string, features: string[]) =>
    JSON.stringify({ reason: "compiler-artifact", package_id: id, target: { name, kind: [kind] }, features });

  it("点名包、产物与 feature；同名 feature 只在登记的包上算", () => {
    const json = [
      artifact("path+file:///r/crates/iwork-store#0.1.0", "iwork_store", "lib", ["test-hooks"]),
      artifact("path+file:///r/crates/iwork-ipc#0.1.0", "iwork_ipc", "lib", ["test-hooks"]),
      artifact("registry+https://x#tokio@1.0.0", "tokio", "lib", ["dev"]),
      artifact("path+file:///r/crates/iwork-daemon#0.1.0", "iwork-daemon", "bin", ["default"]),
      JSON.stringify({ reason: "build-finished", success: true }),
    ].join("\n");
    expect(testFeaturesInBuild(json, list)).toEqual(["iwork-store（lib iwork_store）：test-hooks"]);
  });

  it("对照：都没带就是空的", () => {
    const json = artifact("path+file:///r/crates/iwork-store#0.1.0", "iwork_store", "lib", []);
    expect(testFeaturesInBuild(json, list)).toEqual([]);
  });
});
