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

/** 不是本地包的来源（registry、sparse 索引、git）。 */
const NON_LOCAL_SOURCES = ["registry+", "sparse+", "git+"];

/**
 * 本地包的名字；不是本地包（registry、sparse、git 来的）返回 undefined；认不出的写法抛错——不当成「不是本地包」，
 * 否则 cargo 改了输出格式，出包检查会悄悄放行（1/F-1）。cargo 的包 id：
 * `path+file:///…/crates/iwork-daemon#0.1.0`（目录名就是包名），或 `path+file:///…/client/src-tauri#iwork-client@0.1.0`。
 */
export function localPackageName(packageId: string): string | undefined {
  if (NON_LOCAL_SOURCES.some((p) => packageId.startsWith(p))) return undefined;
  const hash = packageId.lastIndexOf("#");
  if (!packageId.startsWith("path+file://") || hash < 0) {
    throw new Error(`认不出的包 id：${packageId}（cargo 的输出格式变了？出包检查不放行）`);
  }
  const fragment = packageId.slice(hash + 1);
  const at = fragment.indexOf("@");
  if (at >= 0) return fragment.slice(0, at);
  const path = packageId.slice(0, hash).replace(/\/+$/, "");
  return path.slice(path.lastIndexOf("/") + 1);
}

/** 进包的程序：本检查必须在这次构建的输出里认出它，认不出就不算查过。 */
export const SHIPPED_DAEMON = { crate: "iwork-daemon", bin: "iwork-daemon" } as const;

/**
 * 一次 cargo 构建（`--message-format=json`）里带了测试 feature 的本地产物，一条一行，形如
 * `iwork-daemon（bin iwork-daemon）：test-hooks`。空数组 = 干净。
 *
 * 认不出输出就抛错，不返回空（1/F-1）：产物记录缺 `package_id` 或 `features`、包 id 写法认不出、或者没认出
 * `shipped` 那个本地包的那个 bin（它带着 features 数组），都说明这次没真查到进包的程序。
 */
export function testFeaturesInBuild(
  cargoJson: string,
  list: TestOnlyFeatures,
  shipped: { crate: string; bin: string } = SHIPPED_DAEMON,
): string[] {
  const found = new Set<string>();
  let sawShipped = false;
  for (const line of cargoJson.split("\n")) {
    if (!line.startsWith("{")) continue;
    const msg = JSON.parse(line) as {
      reason?: string;
      package_id?: unknown;
      target?: { name?: string; kind?: string[] };
      features?: unknown;
    };
    if (msg.reason !== "compiler-artifact") continue;
    const what = `${(msg.target?.kind ?? []).join(",")} ${msg.target?.name ?? "?"}`;
    if (typeof msg.package_id !== "string") throw new Error(`cargo 的产物记录缺 package_id（${what}），出包检查不放行`);
    if (!Array.isArray(msg.features)) throw new Error(`cargo 的产物记录缺 features（${what}），出包检查不放行`);
    const crate = localPackageName(msg.package_id);
    if (crate === undefined) continue;
    if (crate === shipped.crate && msg.target?.name === shipped.bin && msg.target.kind?.includes("bin")) {
      sawShipped = true;
    }
    for (const feature of msg.features as string[]) {
      if (list.testFeatures.some((f) => f.crate === crate && f.feature === feature)) {
        found.add(`${crate}（${what}）：${feature}`);
      }
    }
  }
  if (!sawShipped) {
    throw new Error(`这次构建的输出里没认出进包的后台（${shipped.crate} 的 bin ${shipped.bin}），出包检查不放行`);
  }
  return [...found].sort();
}
