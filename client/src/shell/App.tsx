// 外壳：侧栏 + 主区域。路由（看哪一页）是网页本地状态；变了就整份声明可见视图。
// 传输故障时只显示固定诊断页；连接类整页（诊断、版本切换等）归步 7d，这里先给连接中的最小提示。
import { createEffect, createSignal, Match, on, Show, Switch, type Component, type JSX } from "solid-js";
import { HOME_DRAFT, setDraft } from "../composer/drafts";
import { ConnectionState } from "../gen/iwork/v1/view_pb";
import { HostProvider, type Host } from "../host";
import { ConversationPage } from "../pages/Conversation";
import { AddProjectDialog } from "../pages/AddProject";
import { AllConversationsPage } from "../pages/AllConversations";
import { AssistantPage } from "../pages/Assistant";
import { HomePage } from "../pages/Home";
import { PendingPage } from "../pages/Pending";
import { SettingsPage } from "../pages/settings/Settings";
import { Spinner } from "../ui/Badge";
import { applyPlayful } from "../ui/theme";
import { ConnectionPage, isFullPage, QuitConfirm } from "./ConnectionGate";
import { createRouter, visibleFor, type Route } from "./route";
import { Sidebar } from "./Sidebar";
import { TransportFailurePage } from "./TransportFailure";
import "./App.css";

export function App(props: { host: Host; toasts: Component; initialRoute?: Route }) {
  const host = props.host;
  const router = createRouter(props.initialRoute);

  // 路由变了：整份声明可见视图（方案 2.4 第 9 条：不可见的视图 Rust 不算增量）。
  createEffect(
    on(router.route, (r) => {
      const v = visibleFor(r);
      void host.act({
        case: "setVisible",
        value: {
          sidebar: v.sidebar,
          settings: v.settings,
          pending: v.pending,
          addProject: v.addProject,
          conversationId: v.conversationId,
          assistantId: v.assistantId,
          conversationList: v.conversationList,
          home: v.home,
        },
      });
    }),
  );

  // Rust 发来的跳转请求（新建对话后、点了系统通知后），按 seq 只跳一次。
  // 首页发出首条消息的收尾（startedProjectId）：那条草稿已经发出去了，清掉。
  const [navigated, setNavigated] = createSignal(0n);
  createEffect(() => {
    const n = host.view.navigate;
    if (n === undefined || n.seq <= navigated()) return;
    setNavigated(n.seq);
    if (n.startedProjectId !== undefined) setDraft(HOME_DRAFT + n.startedProjectId, "");
    // 被系统通知叫到前台、那一项没有对话归属：打开「待你处理」页（方案 2.8）
    if (n.pendingList) router.go({ page: "pending" });
    else if (n.addProject) router.go({ page: "addProject" });
    else if (n.projectId !== undefined) router.go({ page: "home", projectId: n.projectId });
    else router.go({ page: "conversation", id: n.conversationId, rowKey: n.rowKey || undefined });
  });

  createEffect(() => applyPlayful(host.view.settings?.playfulUi ?? true));

  const conn = () => host.view.connection;
  const connecting = () => {
    const s = conn()?.state;
    return s === undefined || s === ConnectionState.CONNECTING || s === ConnectionState.UNSPECIFIED;
  };

  return (
    <HostProvider value={host}>
      <Show when={host.view.transportFailure} fallback={<Body router={router} connecting={connecting()} />}>
        {(f) => <TransportFailurePage failure={f()} />}
      </Show>
      <QuitConfirm />
      <props.toasts />
    </HostProvider>
  );

  function Body(p: { router: typeof router; connecting: boolean }): JSX.Element {
    return (
      <Show when={!isFullPage(conn())} fallback={<ConnectionPage view={conn()!} />}>
      <Show
        when={host.view.sidebar !== undefined}
        fallback={
          <div class="iw-connecting" role="status">
            <Spinner />
            <span>{conn()?.title || "正在连接 iWork…"}</span>
          </div>
        }
      >
        <Show
          when={settings(p.router.route())}
          fallback={<Main router={p.router} />}
        >
          {(r) => (
            <SettingsPage
              section={r().section}
              go={(section) => p.router.go({ page: "settings", section })}
              back={p.router.back}
            />
          )}
        </Show>
      </Show>
      </Show>
    );
  }

  /** 侧栏 + 主区域（设置页以外的各页；设置页是整页，画布 12）。 */
  function Main(p: { router: typeof router }): JSX.Element {
    return (
        <div class="iw-app">
          <Sidebar router={p.router} />
          <main class="iw-main">
            <ConnectionBar />
            <Switch fallback={<div class="iw-placeholder">这一页在下一组实现</div>}>
              <Match when={home(p.router.route())}>
                <HomePage openRuntimeDetails={() => p.router.go({ page: "settings", section: "runtimes" })} />
              </Match>
              <Match when={conversation(p.router.route())}>{(r) => (
                  <ConversationPage id={r().id} rowKey={r().rowKey} openRuntimeDetails={() => p.router.go({ page: "settings", section: "runtimes" })} />
                )}</Match>
              <Match when={assistant(p.router.route())}>
                {(r) => (
                  <AssistantPage route={r()} go={p.router.go} openRuntimeDetails={() => p.router.go({ page: "settings", section: "runtimes" })} />
                )}
              </Match>
              <Match when={allConversations(p.router.route())}>
                {(r) => <AllConversationsPage route={r()} go={p.router.go} />}
              </Match>
              <Match when={p.router.route().page === "addProject"}>
                {/* 添加项目是弹窗：底下照常是首页 */}
                <HomePage openRuntimeDetails={() => p.router.go({ page: "settings", section: "runtimes" })} />
                <AddProjectDialog onClose={() => p.router.go({ page: "home" })} />
              </Match>
              <Match when={p.router.route().page === "pending"}>
                <PendingPage open={p.router.go} />
              </Match>
            </Switch>
          </main>
        </div>
    );
  }

  function ConnectionBar() {
    const text = () => {
      const c = conn();
      if (c === undefined) return undefined;
      if (c.state === ConnectionState.RECONNECTING) return c.title || "正在重新连接";
      return c.overloadNotice ?? c.queryNotice;
    };
    return (
      <Show when={text()}>
        <div class="iw-conn-bar" role="status">
          {text()}
        </div>
      </Show>
    );
  }
}

const home = (r: Route) => (r.page === "home" ? r : undefined);
const conversation = (r: Route) => (r.page === "conversation" ? r : undefined);
const assistant = (r: Route) => (r.page === "assistant" ? r : undefined);
const allConversations = (r: Route) => (r.page === "allConversations" ? r : undefined);
const settings = (r: Route) => (r.page === "settings" ? r : undefined);
