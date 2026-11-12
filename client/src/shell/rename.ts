// 就地重命名的输入规则（ui-m1.md 2a 第 6 条）。上限由 Rust 给（SidebarView.title_max_chars），
// Rust 侧（intent.rs）对提交的标题再校验一次。

/** 超过上限（字符数，按码点算）的截掉。输入法组字期间不要调它（组字结束后再截）。 */
export function clampTitle(v: string, max: number): string {
  const chars = Array.from(v);
  return chars.length > max ? chars.slice(0, max).join("") : v;
}

export type RenameOutcome = { kind: "submit"; title: string } | { kind: "restore" };

/** 回车或失焦时：去掉首尾空白后为空 → 还原不提交；和原标题相同 → 直接退出不发命令；否则提交。 */
export function renameOutcome(original: string, edited: string, max: number): RenameOutcome {
  const title = clampTitle(edited.trim(), max);
  if (title === "" || title === original) return { kind: "restore" };
  return { kind: "submit", title };
}
