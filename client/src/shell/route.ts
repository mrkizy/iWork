// 网页本地的路由：主区域显示哪一页。只是「看哪儿」，不是业务状态；变了就向 Rust 整份声明可见视图（SetVisible）。
// 加一个页面 = Route 加一种 + 路由表（App.tsx 的 PAGES）一行。
import { createSignal } from "solid-js";

export type Route =
  | { page: "home"; projectId?: string }
  | { page: "conversation"; id: string; rowKey?: string }
  | { page: "pending" }
  | { page: "settings" }
  | { page: "allConversations"; projectId: string }
  | { page: "assistant"; id: string };

export function createRouter(initial: Route = { page: "home" }) {
  const [route, setRoute] = createSignal<Route>(initial);
  return { route, go: setRoute };
}

export type Router = ReturnType<typeof createRouter>;

/** 这个路由要 Rust 发哪些视图（侧栏与设置常驻：设置里带着轻松元素开关）。 */
export function visibleFor(route: Route) {
  return {
    sidebar: true,
    settings: true,
    pending: route.page === "pending",
    conversationId: route.page === "conversation" ? route.id : undefined,
    assistantId: route.page === "assistant" ? route.id : undefined,
    conversationList: route.page === "allConversations" ? { projectId: route.projectId, filter: "" } : undefined,
    // 首页对着哪个项目：路由里带了就用，没带由 Rust 挑。
    home: route.page === "home" ? { projectId: route.projectId ?? "" } : undefined,
  };
}

/** 深链：`#/c/<对话 id>` 直接打开某个对话（开发、录像对照与真实 WebView 测试用；没有就是首页）。 */
export function routeFromHash(hash: string): Route {
  const m = /^#\/c\/([^/?#]+)$/.exec(hash);
  return m ? { page: "conversation", id: decodeURIComponent(m[1]) } : { page: "home" };
}
