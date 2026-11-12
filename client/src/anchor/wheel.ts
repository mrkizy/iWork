// 锚点滚轮的手势（ui-m1.md 2a 第 11 条）：鼠标滚轮每格换一处；触控板的位移累计到阈值才换一处；
// 甩动时每秒最多换约 10 处；到第一处或最后一处时停住（回弹由组件显示），不循环。
// 纯函数：输入当前状态与一次滚动，输出新状态与要换几处，组件据此改预览位置。

/** 触控板（像素模式）累计到这么多像素换一处。 */
export const TRACKPAD_STEP_PX = 48;
/** 两次换处之间至少隔这么久（每秒最多约 10 处）。 */
export const MIN_STEP_INTERVAL_MS = 100;

export interface WheelState {
  /** 触控板还没用掉的累计位移（像素，有正负）。 */
  carry: number;
  /** 上一次换处的时刻（毫秒）。 */
  lastStepAt: number;
}

export const INITIAL_WHEEL: WheelState = { carry: 0, lastStepAt: -Infinity };

/** DOM WheelEvent.deltaMode：0 像素（触控板），1 行、2 页（鼠标滚轮的一格）。 */
export interface WheelInput {
  deltaY: number;
  deltaMode: number;
  now: number;
}

/** 一次滚动换几处（-1 往前、+1 往后、0 不换）。 */
export function wheelStep(s: WheelState, e: WheelInput): { state: WheelState; step: -1 | 0 | 1 } {
  if (e.deltaY === 0) return { state: s, step: 0 };
  const tooSoon = e.now - s.lastStepAt < MIN_STEP_INTERVAL_MS;
  if (e.deltaMode !== 0) {
    // 鼠标滚轮：一格一处，太快的丢掉
    if (tooSoon) return { state: s, step: 0 };
    return { state: { carry: 0, lastStepAt: e.now }, step: e.deltaY > 0 ? 1 : -1 };
  }
  // 换了方向就从头累计
  const carry = Math.sign(e.deltaY) === Math.sign(s.carry) ? s.carry + e.deltaY : e.deltaY;
  if (Math.abs(carry) < TRACKPAD_STEP_PX || tooSoon) return { state: { ...s, carry }, step: 0 };
  return { state: { carry: 0, lastStepAt: e.now }, step: carry > 0 ? 1 : -1 };
}

/** 换处之后的位置：越界就停在头上，并告诉组件往哪一头回弹。 */
export function moveWithin(index: number, step: number, length: number): { index: number; bounce?: "start" | "end" } {
  const next = index + step;
  if (next < 0) return { index: 0, bounce: "start" };
  if (next > length - 1) return { index: Math.max(0, length - 1), bounce: "end" };
  return { index: next };
}

/** 预览离窗口边缘这么近、那一侧还有，就请 Rust 以它为中心重取（方案 2.9）。 */
export const BROWSE_EDGE = 8;

export function nearEdge(index: number, length: number, hasBefore: boolean, hasAfter: boolean): boolean {
  return (hasBefore && index < BROWSE_EDGE) || (hasAfter && length - 1 - index < BROWSE_EDGE);
}
