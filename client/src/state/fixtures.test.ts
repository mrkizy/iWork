// applyPatch 对拍（T09 方案第 7 节「网页」第 1 条）：每个场景把第一代收到的帧逐帧折叠，
// 结果等于只应用重新 attach 后那一帧全量。录像由 `pnpm fixtures` 生成（`pnpm test` 会先跑它）。
import { existsSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { unwrap } from "solid-js/store";
import { describe, expect, it } from "vitest";
import { decodeFrames, type FixtureEntry } from "./fixtures";
import { createViewStore, type ViewState } from "./store";

// jsdom 环境里 import.meta.url 不是 file: 地址，用 vitest 给的文件路径
const DIR = `${resolve(dirname(import.meta.filename), "../../fixtures")}/`;
// 假后台只在 Unix 上监听，Windows 上不生成录像（M1 不支持 Windows，T03）。
const unsupported = process.platform === "win32";

function load(name: string, kind: "frames" | "full") {
  return decodeFrames(new Uint8Array(readFileSync(`${DIR}${name}.${kind}.binpb`)));
}

/** 对拍时只比视图内容：跳转请求是一次性的事件，全量帧不重发它。 */
function comparable(s: ViewState): ViewState {
  const { navigate: _navigate, ...rest } = structuredClone(unwrap(s));
  return rest;
}

describe.skipIf(unsupported)("录像对拍：逐帧折叠 == 一帧全量", () => {
  const indexFile = `${DIR}index.json`;
  if (!existsSync(indexFile)) {
    it("录像存在", () => {
      throw new Error(`没有录像：${indexFile}。先跑 pnpm -C client fixtures`);
    });
    return;
  }
  const entries = JSON.parse(readFileSync(indexFile, "utf8")) as FixtureEntry[];

  it("录像目录不为空", () => {
    expect(entries.length).toBeGreaterThan(0);
  });

  it.each(entries.map((e) => [e.name]))("%s", (name) => {
    const frames = load(name, "frames");
    const [full, ...extra] = load(name, "full");
    expect(extra, "全量文件只有一帧").toEqual([]);

    const folded = createViewStore();
    for (const f of frames) folded.applyPatch(f);
    const fresh = createViewStore();
    fresh.applyPatch(full);

    expect(comparable(folded.state)).toEqual(comparable(fresh.state));
    // 对照：真比了东西，不是两个空状态相等
    expect(fresh.state.sidebar, "全量帧带侧栏").toBeDefined();
  });
});
