// 网页的视图状态（T09 方案 2.5）：只存 Rust 侧算好的视图模型；`applyPatch` 是唯一写入口，组件只读。
// 语义与 crates/iwork-client-core/tests/support/web.rs 的网页替身一致：
// - 整体视图模型脏了就整份发：替换；
// - 对话：reset 或换了对话先清空行；removed_ids 删行；rows 按 key 整行替换或新增；appends 追加流式文字
//   （消息行追加到正文，工具行追加到输出：两种行都有流式增量，MessageDelta / ToolOutputDelta）；
//   标题栏、输入框、窗口信息、整页说明每次随 patch 整份给；锚点列表与当前位置变了才给（没给就保留上一份），
//   滚动指令只记最新一条（网页按 token 去重）；
// - 跳转请求只记最新一条，由路由层按 seq 去重。
import { createStore, produce, reconcile, type SetStoreFunction } from "solid-js/store";
import type {
  AddProjectView,
  AnchorsView,
  AssistantProfileView,
  ComposerView,
  ConnectionView,
  ConversationHeader,
  ConversationListView,
  ConversationPatch,
  ScrollTo,
  HomeView,
  NavigatePatch,
  PendingView,
  RowView,
  SettingsView,
  SidebarView,
  ViewPatch,
  WindowInfo,
} from "../gen/iwork/v1/view_pb";
import type { TransportFailure } from "../bridge/frames";

export interface ConversationState {
  id: string;
  header?: ConversationHeader;
  composer?: ComposerView;
  window?: WindowInfo;
  blockingNotice?: string;
  /** 消息锚点（方案 2.9）：列表、当前位置、最近一次滚动指令。 */
  anchors?: AnchorsView;
  anchorKey?: string;
  scrollTo?: ScrollTo;
  rows: Record<string, RowView>;
  /** 行 key 按 ordinal 排好的顺序；只在行增删时重算。 */
  order: string[];
  /** 这个对话被 reset 过几次（打开、时间线重建、回到最新）：时间线据此回到底部。 */
  resets: number;
}

export interface ViewState {
  sidebar?: SidebarView;
  pending?: PendingView;
  settings?: SettingsView;
  assistant?: AssistantProfileView;
  addProject?: AddProjectView;
  connection?: ConnectionView;
  conversationList?: ConversationListView;
  conversation?: ConversationState;
  home?: HomeView;
  navigate?: NavigatePatch;
  /** 传输层故障：设了就显示固定诊断页（不依赖帧）。 */
  transportFailure?: TransportFailure;
}

export interface ViewStore {
  readonly state: ViewState;
  applyPatch(patch: ViewPatch): void;
  /** 换代：清空全部视图（下一帧是全量）。 */
  reset(): void;
  failTransport(failure: TransportFailure): void;
}

const utf8 = new TextEncoder();
const byteLength = (s: string) => utf8.encode(s).length;

export function createViewStore(): ViewStore {
  const [state, setState] = createStore<ViewState>({});
  // 每个可追加行（消息正文、工具输出）的 UTF-8 字节数：追加时核对 offset（view.proto MessageTextAppend）。
  // 不是视图状态，放在 store 外。
  const textBytes = new Map<string, number>();

  const applyPatch = (patch: ViewPatch) => {
    for (const { change } of patch.changes) {
      switch (change.case) {
        case "sidebar":
          replace(setState, "sidebar", change.value.view);
          break;
        case "pending":
          replace(setState, "pending", change.value.view);
          break;
        case "settings":
          replace(setState, "settings", change.value.view);
          break;
        case "assistant":
          replace(setState, "assistant", change.value.view);
          break;
        case "addProject":
          replace(setState, "addProject", change.value.view);
          break;
        case "connection":
          replace(setState, "connection", change.value.view);
          break;
        case "conversationList":
          replace(setState, "conversationList", change.value.view);
          break;
        case "home":
          replace(setState, "home", change.value.view);
          break;
        case "navigate":
          setState("navigate", change.value);
          break;
        case "conversation":
          applyConversation(setState, state, textBytes, change.value);
          break;
        case undefined:
          throw new Error(`帧 ${patch.frame} 里有一项空的 ViewChange`);
        default:
          assertNever(change);
      }
    }
  };

  return {
    state,
    applyPatch,
    reset() {
      textBytes.clear();
      setState(reconcile({ transportFailure: state.transportFailure }));
    },
    failTransport(failure) {
      setState("transportFailure", failure);
    },
  };
}

type WholeKey = "sidebar" | "pending" | "settings" | "assistant" | "addProject" | "connection" | "conversationList" | "home";

function replace<K extends WholeKey>(set: SetStoreFunction<ViewState>, key: K, view: ViewState[K]) {
  // 按位置合并：数组里同一位置的对象保留身份，界面上的节点（悬停、动画）不因整份重发而重建。
  if (view === undefined) set(key, undefined);
  else set(key, reconcile(view as NonNullable<ViewState[K]>, { merge: true }) as never);
}

function applyConversation(
  set: SetStoreFunction<ViewState>,
  state: ViewState,
  textBytes: Map<string, number>,
  p: ConversationPatch,
) {
  if (p.reset || state.conversation?.id !== p.conversationId) {
    textBytes.clear();
    const resets = state.conversation?.id === p.conversationId ? state.conversation.resets + 1 : 0;
    set("conversation", { id: p.conversationId, rows: {}, order: [], resets });
  }
  set(
    "conversation",
    produce((c) => {
      if (c === undefined) return;
      let reorder = false;
      for (const key of p.removedIds) {
        if (key in c.rows) {
          delete c.rows[key];
          textBytes.delete(key);
          reorder = true;
        }
      }
      for (const row of p.rows) {
        if (!(row.key in c.rows)) reorder = true;
        c.rows[row.key] = row;
        const text = streamedText(row);
        if (text !== undefined) textBytes.set(row.key, byteLength(text));
      }
      for (const a of p.appends) {
        const row = c.rows[a.messageId];
        const have = textBytes.get(a.messageId);
        // 对不上（行不在、不是可追加的行、长度不同）就丢掉这次追加，等 Rust 整行重发（它在下一次变脏时整行发）。
        if (row === undefined || have !== Number(a.offset)) continue;
        if (row.row.case === "message") row.row.value.text += a.text;
        else if (row.row.case === "tool") row.row.value.output += a.text;
        else continue;
        textBytes.set(a.messageId, have + byteLength(a.text));
      }
      c.header = p.header;
      c.composer = p.composer;
      c.window = p.window;
      c.blockingNotice = p.blockingNotice;
      if (p.anchors !== undefined) c.anchors = p.anchors;
      if (p.anchorCursor !== undefined) c.anchorKey = p.anchorCursor.currentKey;
      if (p.scrollTo !== undefined) c.scrollTo = p.scrollTo;
      if (reorder) c.order = sortedKeys(c.rows);
    }),
  );
}

/** 行里会流式追加的那段文字：消息正文、工具输出；别的行没有。 */
function streamedText(row: RowView): string | undefined {
  if (row.row.case === "message") return row.row.value.text;
  if (row.row.case === "tool") return row.row.value.output;
  return undefined;
}

function sortedKeys(rows: Record<string, RowView>): string[] {
  return Object.values(rows)
    .sort((a, b) => (a.ordinal < b.ordinal ? -1 : a.ordinal > b.ordinal ? 1 : 0))
    .map((r) => r.key);
}

function assertNever(x: never): never {
  throw new Error(`没处理的 ViewChange：${JSON.stringify(x)}`);
}
