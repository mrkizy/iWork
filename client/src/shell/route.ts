// 网页本地的路由：主区域显示哪一页。只是「看哪儿」，不是业务状态；变了就向 Rust 整份声明可见视图（SetVisible）。
// 加一个页面 = Route 加一种 + 路由表（App.tsx 的 PAGES）一行。
import { createSignal } from "solid-js";

export type Route =
  | { page: "home"; projectId?: string }
  | { page: "conversation"; id: string; rowKey?: string }
  | { page: "pending" }
  | { page: "settings"; section?: SettingsSection }
  // 「全部对话」：筛选文字是看哪儿的一部分（变了就重新声明，Rust 去后台筛，2a 第 5 条）
  | { page: "allConversations"; projectId: string; filter?: string }
  // 助手主页 = 和它的私聊 + 右侧资料栏（画布 09、13；需求 FR03「点开助手默认进入该对话，右侧为助手资料」）
  | { page: "assistant"; id: string; conversationId: string; tab: AssistantTab }
  | { page: "addProject" };

/** 设置页左栏的几节（画布 12）。 */
export type SettingsSection = "general" | "projectsRoot" | "runtimes" | "approval" | "about";

/** 助手资料栏的三个页签。 */
export type AssistantTab = "activity" | "memory" | "config";

export function createRouter(initial: Route = { page: "home" }) {
  const [route, setRoute] = createSignal<Route>(initial);
  // 设置页是整页（盖住侧栏），「‹ 返回」回到进设置之前看的那一页
  let beforeSettings: Route = { page: "home" };
  const go = (r: Route) => {
    const now = route();
    if (r.page === "settings" && now.page !== "settings") beforeSettings = now;
    setRoute(r);
  };
  return { route, go, back: () => setRoute(beforeSettings) };
}

export type Router = ReturnType<typeof createRouter>;

/** 这个路由要 Rust 发哪些视图（侧栏与设置常驻：设置里带着轻松元素开关）。 */
export function visibleFor(route: Route) {
  return {
    sidebar: true,
    settings: true,
    pending: route.page === "pending",
    addProject: route.page === "addProject",
    conversationId:
      route.page === "conversation" ? route.id : route.page === "assistant" ? route.conversationId : undefined,
    assistantId: route.page === "assistant" ? route.id : undefined,
    conversationList:
      route.page === "allConversations" ? { projectId: route.projectId, filter: route.filter ?? "" } : undefined,
    // 首页对着哪个项目：路由里带了就用，没带由 Rust 挑。
    home: route.page === "home" ? { projectId: route.projectId ?? "" } : undefined,
  };
}

/** 深链：`#/c/<对话 id>` 直接打开某个对话（开发、录像对照与真实 WebView 测试用；没有就是首页）。 */
export function routeFromHash(hash: string): Route {
  const m = /^#\/c\/([^/?#]+)$/.exec(hash);
  return m ? { page: "conversation", id: decodeURIComponent(m[1]) } : { page: "home" };
}
