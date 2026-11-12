/* @refresh reload */
// 录像播放页（方案 2.6、第 8 节）：吃 client-core 对着假后台跑出的 ViewPatch 录像，用和正式入口同一套组件画出来。
// 只在开发服务器上（preview.html 不是发布入口）。地址：/preview.html 列出全部场景；
// #/<场景>?theme=light|dark 打开一个场景；&play=1 按 16ms 一帧回放第一代的帧，否则直接应用全量；
// &frame=N 回放到第 N 帧停住（流式中间帧要截图时用）。#/characters 是全部形象 × 表情一览（不是录像）。回放时右上角有控制条：暂停 / 继续、下一帧、帧号。
// 操作不发给任何后台：只在右下角显示最后一次操作，并回「受理」。
import { create } from "@bufbuild/protobuf";
import { createSignal, For, Show } from "solid-js";
import { render } from "solid-js/web";
import { IntentAckSchema } from "../gen/iwork/v1/view_pb";
import type { Host } from "../host";
import { App } from "../shell/App";
import type { Route } from "../shell/route";
import { decodeFrames, type FixtureEntry } from "../state/fixtures";
import { createViewStore } from "../state/store";
import { DESIGN_CATALOG } from "../ui/characters";
import { createToaster } from "../ui/Toast";
import { applyTheme } from "../ui/theme";
import "../ui/theme";
import { CharacterGallery } from "./CharacterGallery";
import "./preview.css";

const FIXTURES = "/fixtures/";
const FRAME_MS = 16;
/** 形象一览的路由名。假后台没有叫这个名字的场景（CharacterGallery.test.tsx 守着），否则那个场景会被它挡住。 */
const GALLERY = "characters";
const VIEW_DECLARATIONS = new Set(["setVisible", "setVisibleRows", "setFocused", "loadOlder"]);

const root = document.getElementById("root");
if (root === null) throw new Error("preview.html 缺少 #root 挂载点");

async function fetchBytes(path: string): Promise<Uint8Array> {
  const r = await fetch(path);
  if (!r.ok) throw new Error(`取不到 ${path}（${r.status}）。先跑 pnpm -C client fixtures`);
  return new Uint8Array(await r.arrayBuffer());
}

/**
 * 地址里点名打开哪一页（审查用；没写就照场景的 focus 打开）：
 * - `?page=pending`、`?page=addProject`；`?page=settings&section=general|projectsRoot|runtimes|approval|about`
 * - `?page=allConversations&project=<项目 id>`
 * - `?page=assistant&assistant=<助手 id>&tab=activity|memory|config`：私聊用场景的 focus
 */
function pageRoute(q: URLSearchParams, focus: string | null): Route {
  const page = q.get("page");
  const section = q.get("section") as Extract<Route, { page: "settings" }>["section"] | null;
  const tab = (q.get("tab") ?? "activity") as Extract<Route, { page: "assistant" }>["tab"];
  switch (page) {
    case "pending":
      return { page: "pending" };
    case "addProject":
      return { page: "addProject" };
    case "settings":
      return { page: "settings", section: section ?? undefined };
    case "allConversations":
      return { page: "allConversations", projectId: q.get("project") ?? "" };
    case "assistant":
      if (focus) return { page: "assistant", id: q.get("assistant") ?? "", conversationId: focus, tab };
      break;
  }
  return focus ? { page: "conversation", id: focus } : { page: "home" };
}

function parseHash(): { name?: string; theme: "light" | "dark" | "system"; play: boolean; stopAt?: number; query: URLSearchParams } {
  const [path, query = ""] = location.hash.replace(/^#\/?/, "").split("?");
  const q = new URLSearchParams(query);
  const t = q.get("theme");
  const f = q.get("frame");
  const stopAt = f !== null && /^\d+$/.test(f) ? Number(f) : undefined;
  return {
    name: path || undefined,
    theme: t === "light" || t === "dark" ? t : "system",
    play: q.get("play") === "1" || stopAt !== undefined,
    stopAt,
    query: q,
  };
}

/** 逐帧回放：16ms 一帧，可暂停、单步；到 stopAt 自动停。 */
function createPlayer(frames: ReturnType<typeof decodeFrames>, apply: (i: number) => void, stopAt?: number) {
  const [at, setAt] = createSignal(0);
  const [paused, setPaused] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  const end = () => Math.min(stopAt ?? frames.length, frames.length);
  const step = () => {
    if (at() >= frames.length) return;
    apply(at());
    setAt(at() + 1);
  };
  const tick = () => {
    timer = undefined;
    if (paused()) return;
    if (at() >= end()) {
      setPaused(stopAt !== undefined && at() < frames.length);
      return;
    }
    step();
    timer = setTimeout(tick, FRAME_MS);
  };
  return {
    at,
    total: frames.length,
    paused,
    start: tick,
    toggle() {
      setPaused(!paused());
      if (!paused() && timer === undefined) {
        stopAt = undefined;
        tick();
      }
    },
    next() {
      setPaused(true);
      step();
    },
  };
}

async function main() {
  const { name, theme, play, stopAt } = parseHash();
  applyTheme(theme);
  const index = JSON.parse(new TextDecoder().decode(await fetchBytes(`${FIXTURES}index.json`))) as FixtureEntry[];
  if (name === undefined) {
    render(() => <Index entries={index} />, root!);
    return;
  }
  if (name === GALLERY) {
    render(() => <CharacterGallery catalog={DESIGN_CATALOG} />, root!);
    return;
  }
  const entry = index.find((e) => e.name === name);
  if (entry === undefined) throw new Error(`录像里没有场景 ${name}`);
  const store = createViewStore();
  const toaster = createToaster();
  const [last, setLast] = createSignal("");
  const host: Host = {
    view: store.state,
    async act(kind) {
      // 视图声明（看哪儿、窗口在哪）是自动发的，不当成「最后一次操作」显示
      if (VIEW_DECLARATIONS.has(kind?.case ?? "")) return create(IntentAckSchema, {});
      setLast(`${kind?.case ?? "?"} ${JSON.stringify(kind?.value, (_k, v) => (typeof v === "bigint" ? String(v) : v))}`);
      return create(IntentAckSchema, {});
    },
  };
  let player: ReturnType<typeof createPlayer> | undefined;
  if (play) {
    const frames = decodeFrames(await fetchBytes(`${FIXTURES}${name}.frames.binpb`));
    player = createPlayer(frames, (i) => store.applyPatch(frames[i]), stopAt);
    player.start();
  } else {
    for (const p of decodeFrames(await fetchBytes(`${FIXTURES}${name}.full.binpb`))) store.applyPatch(p);
  }
  render(
    () => (
      <>
        <App
          host={host}
          toasts={toaster.View}
          initialRoute={pageRoute(parseHash().query, entry.focus)}
        />
        <Show when={last()}>
          <div class="iw-preview-log">{last()}</div>
        </Show>
        <Show when={player}>
          {(p) => (
            <div class="iw-preview-player" role="toolbar" aria-label="回放">
              <button type="button" onClick={() => p().toggle()}>
                {p().paused() ? "继续" : "暂停"}
              </button>
              <button type="button" onClick={() => p().next()} disabled={p().at() >= p().total}>
                下一帧
              </button>
              <span>
                帧 {p().at()} / {p().total}
              </span>
            </div>
          )}
        </Show>
      </>
    ),
    root!,
  );
}

function Index(props: { entries: FixtureEntry[] }) {
  return (
    <main class="iw-preview-index">
      <h1>iWork 录像播放</h1>
      <p>每个场景是 client-core 对着假后台跑出的录像。浅色、深色各一个链接；加 &play=1 按帧回放，&frame=N 回放到第 N 帧停住。</p>
      <table>
        <tbody>
          <tr>
            <td>{GALLERY}</td>
            <td>测试专用：全部形象 × 表情一览（直接读设计系统素材，不是录像）</td>
            <td>
              <a href={`#/${GALLERY}?theme=light`} onClick={() => setTimeout(() => location.reload(), 0)}>
                浅色
              </a>
            </td>
            <td>
              <a href={`#/${GALLERY}?theme=dark`} onClick={() => setTimeout(() => location.reload(), 0)}>
                深色
              </a>
            </td>
          </tr>
          <For each={props.entries}>
            {(e) => (
              <tr>
                <td>{e.name}</td>
                <td>{e.about}</td>
                <td>
                  <a href={`#/${e.name}?theme=light`} onClick={() => setTimeout(() => location.reload(), 0)}>
                    浅色
                  </a>
                </td>
                <td>
                  <a href={`#/${e.name}?theme=dark`} onClick={() => setTimeout(() => location.reload(), 0)}>
                    深色
                  </a>
                </td>
              </tr>
            )}
          </For>
        </tbody>
      </table>
    </main>
  );
}

main().catch((e: unknown) => {
  root!.textContent = String(e);
});
