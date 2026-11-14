// 只给测试用的 feature 清单（scripts/test-only-features.txt，和 check-no-dev-trust.sh 读同一份），以及出包那一刻的检查：
// 编进包程序的那次 cargo 调用里，本地 crate 的产物不许带清单里的 feature。
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

/** 清单文件的位置（仓库里唯一的一份）。 */
export const LIST_PATH = fileURLToPath(new URL("../../scripts/test-only-features.txt", import.meta.url));

/** 一个 crate 的一个 feature。 */
export interface CrateFeature {
  crate: string;
  feature: string;
}

/** 清单里本检查要用的部分：只该在测试、开发构建里出现的 feature。 */
export interface TestOnlyFeatures {
  testFeatures: CrateFeature[];
}

const KINDS = new Set(["test-feature", "release-feature", "release-root"]);

/** 解析清单；写错的行抛错并点名行号（不跳过：跳过一行等于少查一项）。 */
export function parseTestOnlyFeatures(text: string): TestOnlyFeatures {
  const testFeatures: CrateFeature[] = [];
  text.split("\n").forEach((raw, i) => {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) return;
    const fields = line.split(/\s+/);
    if (fields.length !== 3 || !KINDS.has(fields[0])) {
      throw new Error(`test-only-features.txt 第 ${i + 1} 行写错了：${raw}（要 <种类> <crate> <名字> 三段）`);
    }
    if (fields[0] === "test-feature") testFeatures.push({ crate: fields[1], feature: fields[2] });
  });
  if (testFeatures.length === 0) throw new Error("test-only-features.txt 里一条 test-feature 都没有");
  return { testFeatures };
}

export function loadTestOnlyFeatures(path: string = LIST_PATH): TestOnlyFeatures {
  return parseTestOnlyFeatures(readFileSync(path, "utf8"));
}

/**
 * 本地包的名字；不是本地包（registry、git 来的）返回 undefined。cargo 的包 id：
 * `path+file:///…/crates/iwork-daemon#0.1.0`（目录名就是包名），或 `path+file:///…/client/src-tauri#iwork-client@0.1.0`。
 */
export function localPackageName(packageId: string): string | undefined {
  if (!packageId.startsWith("path+file://")) return undefined;
  const hash = packageId.lastIndexOf("#");
  if (hash < 0) throw new Error(`认不出的包 id：${packageId}`);
  const fragment = packageId.slice(hash + 1);
  const at = fragment.indexOf("@");
  if (at >= 0) return fragment.slice(0, at);
  const path = packageId.slice(0, hash).replace(/\/+$/, "");
  return path.slice(path.lastIndexOf("/") + 1);
}

/**
 * 一次 cargo 构建（`--message-format=json`）里带了测试 feature 的本地产物，一条一行，形如
 * `iwork-daemon（bin iwork-daemon）：test-hooks`。空数组 = 干净。
 */
export function testFeaturesInBuild(cargoJson: string, list: TestOnlyFeatures): string[] {
  const found = new Set<string>();
  for (const line of cargoJson.split("\n")) {
    if (!line.startsWith("{")) continue;
    const msg = JSON.parse(line) as {
      reason?: string;
      package_id?: string;
      target?: { name?: string; kind?: string[] };
      features?: string[];
    };
    if (msg.reason !== "compiler-artifact" || msg.package_id === undefined) continue;
    const crate = localPackageName(msg.package_id);
    if (crate === undefined) continue;
    for (const feature of msg.features ?? []) {
      if (list.testFeatures.some((f) => f.crate === crate && f.feature === feature)) {
        found.add(`${crate}（${(msg.target?.kind ?? []).join(",")} ${msg.target?.name ?? "?"}）：${feature}`);
      }
    }
  }
  return [...found].sort();
}
