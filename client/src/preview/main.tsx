/* @refresh reload */
// 录像播放页（方案 2.6、第 8 节）：吃 client-core 对着假后台跑出的 ViewPatch 录像，用和正式入口同一套组件画出来。
// 只在开发服务器上（preview.html 不是发布入口）。地址：/preview.html 列出全部场景；
// #/<场景>?theme=light|dark 打开一个场景；&play=1 按 16ms 一帧回放第一代的帧，否则直接应用全量。
// 操作不发给任何后台：只在右下角显示最后一次操作，并回「受理」。
import { create } from "@bufbuild/protobuf";
import { createSignal, For, Show } from "solid-js";
import { render } from "solid-js/web";
import { IntentAckSchema } from "../gen/iwork/v1/view_pb";
import type { Host } from "../host";
import { App } from "../shell/App";
import { decodeFrames, type FixtureEntry } from "../state/fixtures";
import { createViewStore } from "../state/store";
import { createToaster } from "../ui/Toast";
import { applyTheme } from "../ui/theme";
import "../ui/theme";
import "./preview.css";

const FIXTURES = "/fixtures/";
const FRAME_MS = 16;
const VIEW_DECLARATIONS = new Set(["setVisible", "setVisibleRows", "setFocused", "loadOlder"]);

const root = document.getElementById("root");
if (root === null) throw new Error("preview.html 缺少 #root 挂载点");

async function fetchBytes(path: string): Promise<Uint8Array> {
  const r = await fetch(path);
  if (!r.ok) throw new Error(`取不到 ${path}（${r.status}）。先跑 pnpm -C client fixtures`);
  return new Uint8Array(await r.arrayBuffer());
}

function parseHash(): { name?: string; theme: "light" | "dark" | "system"; play: boolean } {
  const [path, query = ""] = location.hash.replace(/^#\/?/, "").split("?");
  const q = new URLSearchParams(query);
  const t = q.get("theme");
  return { name: path || undefined, theme: t === "light" || t === "dark" ? t : "system", play: q.get("play") === "1" };
}

async function main() {
  const { name, theme, play } = parseHash();
  applyTheme(theme);
  const index = JSON.parse(new TextDecoder().decode(await fetchBytes(`${FIXTURES}index.json`))) as FixtureEntry[];
  if (name === undefined) {
    render(() => <Index entries={index} />, root!);
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
  if (play) {
    const frames = decodeFrames(await fetchBytes(`${FIXTURES}${name}.frames.binpb`));
    let i = 0;
    const tick = () => {
      if (i < frames.length) {
        store.applyPatch(frames[i++]);
        setTimeout(tick, FRAME_MS);
      }
    };
    tick();
  } else {
    for (const p of decodeFrames(await fetchBytes(`${FIXTURES}${name}.full.binpb`))) store.applyPatch(p);
  }
  render(
    () => (
      <>
        <App
          host={host}
          toasts={toaster.View}
          initialRoute={entry.focus ? { page: "conversation", id: entry.focus } : { page: "home" }}
        />
        <Show when={last()}>
          <div class="iw-preview-log">{last()}</div>
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
      <p>每个场景是 client-core 对着假后台跑出的录像。浅色、深色各一个链接；加 &play=1 按帧回放。</p>
      <table>
        <tbody>
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
