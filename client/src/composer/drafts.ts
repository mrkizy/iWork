// 输入框草稿：按对话记在网页本地（方案 2.5）；断线时发送禁用，草稿留着（D8）。只在这次运行里记，不落盘。
const drafts = new Map<string, string>();

/** 首页（还没有对话）的草稿用这个键。 */
export const HOME_DRAFT = "";

export function getDraft(key: string): string {
  return drafts.get(key) ?? "";
}

export function setDraft(key: string, text: string) {
  if (text === "") drafts.delete(key);
  else drafts.set(key, text);
}
