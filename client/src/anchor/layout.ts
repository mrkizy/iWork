// 消息锚点的两种形态怎么选（ui-m1.md 2a 第 11 条「窄窗口与让位」）：按主区域（对话区）宽度判断，不按窗口宽度——侧栏
// 宽度可调。有 20px 回差，拖动窗口或侧栏到临界处时不来回闪：降到 600px 以下切窄形态，回到 620px 及以上才切回宽形态。
// 这是布局事实，网页自己量（方案 2.5）。

/** 主区域宽度低于它切到窄形态。 */
export const NARROW_BELOW = 600;
/** 窄形态下主区域宽度到它及以上才切回宽形态。 */
export const WIDE_FROM = 620;

/** 下一刻是不是窄形态：`wasNarrow` 是上一刻的形态，`width` 是主区域现在的宽度。 */
export function nextNarrow(wasNarrow: boolean, width: number): boolean {
  return wasNarrow ? width < WIDE_FROM : width < NARROW_BELOW;
}
