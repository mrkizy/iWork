// 连接类整页与「完全退出 iWork」确认框（方案 2.3 的表、2.7「连接类」；Q16）。
// 整页三种：版本不一致 / 正在切换版本的中性提示（WAITING，后台照常退避重连）、诊断页（DIAGNOSTICS：原因、几行诊断、
// 提示、「重试」）、环境说明（ENVIRONMENT，如 M1 不支持 Windows：说明、「重试」）。标题、说明、诊断行、可否重试都
// 来自 ConnectionView。出口按 Rust 给的 kind 接（exit_actions：重试、重新打开 iWork），不按文字猜动作；
// 没有实现的 kind（查看诊断，等 T04p）不画。诊断页的「重试」按 can_retry 画。
// 确认框：菜单点了「完全退出 iWork」，Rust 设 quit_requested，这里弹确认；确认发 Quit{confirm: true}，取消发 false。
import { For, Match, Show, Switch } from "solid-js";
import { ConnectionExitKind, ConnectionState, type ConnectionExit, type ConnectionView } from "../gen/iwork/v1/view_pb";
import type { IntentKind } from "../bridge/intent";
import { useHost } from "../host";
import { Spinner } from "../ui/Badge";
import { Button } from "../ui/Button";
import { ConfirmDialog } from "../ui/Dialog";
import "./ConnectionGate.css";

/** 这种连接状态要不要整页盖住界面。 */
export function isFullPage(c: ConnectionView | undefined): boolean {
  return (
    c !== undefined &&
    (c.state === ConnectionState.WAITING || c.state === ConnectionState.DIAGNOSTICS || c.state === ConnectionState.ENVIRONMENT)
  );
}

export function ConnectionPage(props: { view: ConnectionView }) {
  const host = useHost();
  const v = () => props.view;
  const retry = () => void host.act({ case: "retry", value: {} });
  return (
    <div class="iw-gate" role="alert" aria-label={v().title || "连接"}>
      <div class="iw-gate__box">
        <Show when={v().title}>
          <h1 class="iw-gate__title">{v().title}</h1>
        </Show>
        <Show when={v().message}>
          <p class="iw-gate__message">{v().message}</p>
        </Show>
        <Switch>
          <Match when={v().state === ConnectionState.WAITING}>
            <Spinner label="正在重新连接" />
          </Match>
          <Match when={v().diagnostics}>
            {(d) => (
              <>
                <dl class="iw-gate__lines">
                  <For each={d().lines}>
                    {(l) => (
                      <>
                        <dt>{l.label}</dt>
                        <dd>{l.value}</dd>
                      </>
                    )}
                  </For>
                </dl>
                <Show when={d().hint}>{(h) => <p class="iw-gate__hint">{h()}</p>}</Show>
                <Show when={d().canRetry}>
                  <Button variant="primary" onClick={retry}>
                    重试
                  </Button>
                </Show>
              </>
            )}
          </Match>
        </Switch>
        <Show when={v().exitActions.length > 0}>
          <div class="iw-gate__exits">
            <For each={v().exitActions}>{(e) => <Exit exit={e} />}</For>
          </div>
        </Show>
      </div>
    </div>
  );
}

/** 出口 kind → 发什么。表里没有的 kind 不画（查看诊断等 T04p 接上真诊断）。 */
const EXIT_INTENTS: Partial<Record<ConnectionExitKind, NonNullable<IntentKind>>> = {
  [ConnectionExitKind.RETRY]: { case: "retry", value: {} },
  [ConnectionExitKind.RELAUNCH]: { case: "relaunch", value: {} },
};

function Exit(props: { exit: ConnectionExit }) {
  const host = useHost();
  const intent = () => EXIT_INTENTS[props.exit.kind];
  return (
    <Show when={intent()}>
      {(i) => <Button onClick={() => void host.act(i())}>{props.exit.label}</Button>}
    </Show>
  );
}

export function QuitConfirm() {
  const host = useHost();
  const quit = (confirm: boolean) => void host.act({ case: "quit", value: { confirm } });
  return (
    <Show when={host.view.connection?.quitRequested}>
      <ConfirmDialog
        title="完全退出 iWork？"
        confirmLabel="完全退出"
        confirmVariant="accent"
        onCancel={() => quit(false)}
        onConfirm={() => quit(true)}
      >
        后台服务会一起停下，正在进行和排着的回合都会取消；重新打开 iWork 后可以再发。
      </ConfirmDialog>
    </Show>
  );
}
