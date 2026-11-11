// 组件测试用：读录像、建 store、渲染整套界面（与录像播放页同一套组件）。只给测试 import。
import { create } from "@bufbuild/protobuf";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { render } from "@solidjs/testing-library";
import { IntentAckSchema } from "./gen/iwork/v1/view_pb";
import type { IntentKind } from "./bridge/intent";
import type { Host } from "./host";
import type { Route } from "./shell/route";
import { App } from "./shell/App";
import { decodeFrames, type FixtureEntry } from "./state/fixtures";
import { createViewStore } from "./state/store";

const DIR = `${resolve(dirname(import.meta.filename), "../fixtures")}/`;

/** 假后台只在 Unix 上监听，Windows 上不生成录像（M1 不支持 Windows，T03）。 */
export const noFixtures = process.platform === "win32";

export function fixtureEntry(name: string): FixtureEntry {
  const index = JSON.parse(readFileSync(`${DIR}index.json`, "utf8")) as FixtureEntry[];
  const e = index.find((x) => x.name === name);
  if (e === undefined) throw new Error(`录像里没有场景 ${name}；先跑 pnpm -C client fixtures`);
  return e;
}

/** 渲染一个场景的全量帧；返回发出的操作记录。 */
export function renderScenario(name: string, route?: Route) {
  const entry = fixtureEntry(name);
  const store = createViewStore();
  for (const p of decodeFrames(new Uint8Array(readFileSync(`${DIR}${name}.full.binpb`)))) store.applyPatch(p);
  const acts: NonNullable<IntentKind>[] = [];
  const host: Host = {
    view: store.state,
    async act(kind) {
      if (kind) acts.push(kind);
      return create(IntentAckSchema, {});
    },
  };
  const initial: Route = route ?? (entry.focus ? { page: "conversation", id: entry.focus } : { page: "home" });
  const utils = render(() => <App host={host} toasts={() => null} initialRoute={initial} />);
  return { ...utils, acts, store };
}

/** 除了视图声明以外的操作。 */
export function userActs(acts: NonNullable<IntentKind>[]) {
  return acts.filter((a) => !["setVisible", "setVisibleRows", "setFocused", "loadOlder"].includes(a.case ?? ""));
}
