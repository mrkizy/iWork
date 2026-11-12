// 主题与字体：随包带的字体、设计令牌、动效。主题跟随系统（tokens.css 的 prefers-color-scheme）；
// 只有录像播放页按 ?theme= 强制浅色或深色。
import "@fontsource-variable/inter";
import "@fontsource-variable/jetbrains-mono";
import "@fontsource-variable/noto-sans-sc";
import "./theme.css";

export type ThemeChoice = "light" | "dark" | "system";

export function applyTheme(choice: ThemeChoice, root: HTMLElement = document.documentElement) {
  if (choice === "system") delete root.dataset.theme;
  else root.dataset.theme = choice;
}

/** 轻松元素开关（ui-m1.md 3.4）：关掉时根元素加 .iw-playful-off，motion.css 统一停用动效。 */
export function applyPlayful(on: boolean, root: HTMLElement = document.documentElement) {
  root.classList.toggle("iw-playful-off", !on);
}
