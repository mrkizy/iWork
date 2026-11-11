// applyPatch 对拍（T09 方案第 7 节「网页」第 1 条）：每个场景把第一代收到的帧逐帧折叠，
// 结果等于只应用重新 attach 后那一帧全量。录像由 `pnpm fixtures` 生成（`pnpm test` 会先跑它）。
// 录像先打开对话、拿到快照，再跑场景剩下的事件：打开之后的帧是增量，对拍比的是增量折叠的结果。
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

/** 对拍时只比视图内容：跳转请求是一次性的事件，全量帧不重发它；reset 次数是网页本地的计数（让时间线回到底部）。 */
function comparable(s: ViewState): ViewState {
  const { navigate: _navigate, ...rest } = structuredClone(unwrap(s));
  if (rest.conversation) rest.conversation.resets = 0;
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

    const entry = entries.find((e) => e.name === name)!;
    const folded = createViewStore();
    frames.slice(0, entry.opened).forEach((f) => folded.applyPatch(f));
    const atOpen = comparable(folded.state);
    frames.slice(entry.opened).forEach((f) => folded.applyPatch(f));
    const fresh = createViewStore();
    fresh.applyPatch(full);

    expect(comparable(folded.state)).toEqual(comparable(fresh.state));
    // 对照：真比了东西，不是两个空状态相等
    expect(fresh.state.sidebar, "全量帧带侧栏").toBeDefined();
    // 打开之后的帧确实改了要看的视图：有 focus 比对话，首页比侧栏
    if (entry.focus) {
      expect(atOpen.conversation?.id, "打开时已有对话快照").toBe(entry.focus);
      expect(comparable(folded.state).conversation, "打开之后有对话增量").not.toEqual(atOpen.conversation);
    } else {
      expect(comparable(folded.state).sidebar, "连上之后有侧栏增量").not.toEqual(atOpen.sidebar);
    }
  });
});
