// 侧栏眼睛跟随（ui-m1.md 3.3）：只监听侧栏的 pointermove，rAF 合并成每帧最多一次，只改 .iw-eyes 的 transform，
// 偏移不超过 1.5px。鼠标离开侧栏、窗口失焦、减少动态效果、轻松元素关闭时清掉偏移（后两者 motion.css 也会强制）。

/** 形象 SVG 的 viewBox 宽（design-system/characters/*.svg 都是 44）。 */
const VIEWBOX = 44;
export const MAX_OFFSET_PX = 1.5;

/** 眼睛朝 (dx, dy) 方向偏移多少像素：方向不变，长度封顶。 */
export function eyeOffset(dx: number, dy: number, max = MAX_OFFSET_PX): [number, number] {
  const len = Math.hypot(dx, dy);
  if (len === 0) return [0, 0];
  const k = Math.min(len, max) / len;
  return [dx * k, dy * k];
}

export function followEyes(container: HTMLElement): () => void {
  let frame = 0;
  let last: PointerEvent | undefined;
  const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)");
  const disabled = () => reduced?.matches === true || document.documentElement.classList.contains("iw-playful-off");

  const eyes = () => [...container.querySelectorAll<SVGGElement>(".iw-avatar .iw-n .iw-eyes")];
  const reset = () => {
    if (frame !== 0) cancelAnimationFrame(frame);
    frame = 0;
    for (const e of eyes()) e.style.transform = "";
  };
  const update = () => {
    frame = 0;
    if (last === undefined || disabled()) return;
    for (const e of eyes()) {
      const svg = e.ownerSVGElement;
      if (!svg) continue;
      const r = svg.getBoundingClientRect();
      if (r.width === 0) continue;
      const [x, y] = eyeOffset(last.clientX - (r.left + r.width / 2), last.clientY - (r.top + r.height / 2));
      const unit = VIEWBOX / r.width; // 像素 → SVG 坐标
      e.style.transform = `translate(${x * unit}px, ${y * unit}px)`;
    }
  };
  const onMove = (ev: PointerEvent) => {
    last = ev;
    if (frame === 0) frame = requestAnimationFrame(update);
  };
  container.addEventListener("pointermove", onMove);
  container.addEventListener("pointerleave", reset);
  window.addEventListener("blur", reset);
  return () => {
    reset();
    container.removeEventListener("pointermove", onMove);
    container.removeEventListener("pointerleave", reset);
    window.removeEventListener("blur", reset);
  };
}
