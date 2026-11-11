// 首页发出第一条消息：先新建对话，Rust 在对话建好后发跳转请求（client-core 的 store::created），
// 跳过去之后再把这条消息发出去。草稿在等待期间留在网页本地（方案 2.5「输入框草稿」）。
import { createContext, createSignal, useContext } from "solid-js";
import type { Host } from "../host";
import type { Router } from "./route";

export interface Starter {
  /** 正在新建、等跳转的那一份草稿（首页显示「正在新建对话…」用）。 */
  pending(): string | undefined;
  start(projectId: string, text: string): Promise<void>;
  /** 跳转到了某个对话：有等着的草稿就发出去。 */
  arrived(conversationId: string): void;
}

export function createStarter(host: Host, router: Router): Starter {
  const [draft, setDraft] = createSignal<string>();
  return {
    pending: draft,
    async start(projectId, text) {
      setDraft(text);
      const ack = await host.act({ case: "createConversation", value: { projectId } });
      if (ack.error) setDraft(undefined);
    },
    arrived(conversationId) {
      const text = draft();
      if (text === undefined) return;
      setDraft(undefined);
      router.go({ page: "conversation", id: conversationId });
      void host.act({ case: "sendMessage", value: { conversationId, text, attachmentIds: [] } });
    },
  };
}

const StarterContext = createContext<Starter>();
export const StarterProvider = StarterContext.Provider;

export function useStarter(): Starter {
  const s = useContext(StarterContext);
  if (s === undefined) throw new Error("组件不在 StarterProvider 里");
  return s;
}
