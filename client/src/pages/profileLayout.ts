// 助手私聊页右侧资料栏放不放得下（ui-m1 2a 第 18 条 d）：对话区 = 主区域 − 资料栏，要保证对话区不小于 568px
// （窄形态内容列 520 + 行内边距 48，见 12b），资料栏宽 356px（--profile-w），所以主区域 ≥ 924px 时资料栏才能常驻。
// 有 20px 回差，拖动窗口或侧栏到临界处时不来回闪：降到 924 以下收起，回到 944 及以上才恢复。
// 这是布局事实，网页自己量（方案 2.5）；用户手动收起是另一回事，那是后台的全局偏好。

/** 主区域宽度低于它，资料栏放不下。 */
export const PROFILE_DOCK_BELOW = 924;
/** 放不下之后，主区域宽度到它及以上才又放得下。 */
export const PROFILE_DOCK_FROM = 944;

/** 下一刻资料栏放不放得下：`wasRoomy` 是上一刻的结论，`width` 是主区域现在的宽度。 */
export function nextRoomy(wasRoomy: boolean, width: number): boolean {
  return wasRoomy ? width >= PROFILE_DOCK_BELOW : width >= PROFILE_DOCK_FROM;
}
