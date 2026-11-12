// 输入框对着谁：一个对话，或首页（还没有对话，发出时由 Rust 新建对话、设上首页选的值、发出首条消息再跳过去）。
// 同一个按钮在两处发的操作不同，分叉只在这里。
import type { IntentKind } from "../bridge/intent";

export type ComposerTarget = { kind: "conversation"; id: string } | { kind: "home"; projectId: string };

export function sendIntent(t: ComposerTarget, text: string, attachmentIds: string[]): IntentKind {
  return t.kind === "conversation"
    ? { case: "sendMessage", value: { conversationId: t.id, text, attachmentIds } }
    : { case: "startConversation", value: { projectId: t.projectId, text } };
}

export function modelIntent(t: ComposerTarget, modelId: string, effort: string | undefined): IntentKind {
  return t.kind === "conversation"
    ? { case: "setModel", value: { conversationId: t.id, modelId, effort } }
    : { case: "setHomeModel", value: { modelId, effort } };
}

export function modeIntent(t: ComposerTarget, mode: number): IntentKind {
  return t.kind === "conversation"
    ? { case: "setApprovalMode", value: { conversationId: t.id, mode } }
    : { case: "setHomeApprovalMode", value: { mode } };
}
