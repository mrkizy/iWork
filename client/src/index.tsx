/* @refresh reload */
// 正式入口：经 Tauri 的三个命令接 Rust 侧（方案 2.4）。
import { render } from "solid-js/web";
import { tauriCall } from "./bridge/call";
import { FrameBridge } from "./bridge/frames";
import { intentSender } from "./bridge/intent";
import { createViewStore } from "./state/store";
import { App } from "./shell/App";
import { routeFromHash } from "./shell/route";
import { createToaster } from "./ui/Toast";
import "./ui/theme";

const root = document.getElementById("root");
if (root === null) throw new Error("index.html 缺少 #root 挂载点");

const store = createViewStore();
const toaster = createToaster();
const send = intentSender(tauriCall, (f) => store.failTransport(f));
const bridge = new FrameBridge(tauriCall, {
  reset: () => store.reset(),
  apply: (p) => store.applyPatch(p),
  fail: (f) => store.failTransport(f),
});

render(
  () => (
    <App
      host={{
        view: store.state,
        async act(kind, opts) {
          const ack = await send(kind);
          if (ack.error && !opts?.quiet) toaster.show(ack.error.message);
          return ack;
        },
      }}
      toasts={toaster.View}
      initialRoute={routeFromHash(location.hash)}
    />
  ),
  root,
);
void bridge.attach();
