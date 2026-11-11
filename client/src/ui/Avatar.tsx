// 助手头像（ui-m1.md 3.3）：有形象的内联 design-system/characters 里对应表情的 SVG，没有的画字头像。
// 表情由 Rust 侧给（Expression）；悬停、戳一下、连戳生气是网页本地的纯展示状态（方案 2.5）。
// 戳头像不触发任何业务操作。动效类由 motion.css 管：减少动态效果、轻松元素关闭时自动停用。
import { createSignal, onCleanup, Show } from "solid-js";
import { Expression, type AvatarView } from "../gen/iwork/v1/view_pb";
import { CHARACTERS, svgOf, type Mood } from "./characters";
import "./Avatar.css";

// 表情 → 素材（表情本身由 Rust 侧随状态给）。出错没有专门的素材，先用空闲。
const MOOD: Record<Expression, Mood> = {
  [Expression.UNSPECIFIED]: "idle",
  [Expression.IDLE]: "idle",
  [Expression.WORKING]: "work",
  [Expression.WAITING]: "wait",
  [Expression.ERROR]: "idle",
};

const HEX_COLOR = /^#[0-9a-fA-F]{3,8}$/;
const POKE_WINDOW_MS = 2000;
const POKES_TO_ANGRY = 3;
const ANGRY_MS = 1500;

export function Avatar(props: {
  avatar?: AvatarView;
  expression?: Expression;
  size?: number;
  /** 悬停蹦跳、戳一下、连戳生气（侧栏、对话里的助手头像）。 */
  interactive?: boolean;
  label?: string;
}) {
  const size = () => props.size ?? 28;
  const character = () => {
    const c = props.avatar?.character ?? "";
    return CHARACTERS.includes(c) ? c : undefined;
  };
  const mood = () => MOOD[props.expression ?? Expression.IDLE];
  const [pokes, setPokes] = createSignal<number[]>([]);
  const [angry, setAngry] = createSignal(false);
  const [poking, setPoking] = createSignal(false);
  let angryTimer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(angryTimer));

  const poke = () => {
    if (!props.interactive) return;
    setPoking(true);
    const now = Date.now();
    const recent = [...pokes().filter((t) => now - t < POKE_WINDOW_MS), now];
    setPokes(recent);
    if (recent.length >= POKES_TO_ANGRY && !angry()) {
      setPokes([]);
      setAngry(true);
      clearTimeout(angryTimer);
      angryTimer = setTimeout(() => setAngry(false), ANGRY_MS);
    }
  };

  const classes = () => {
    const c = ["iw-avatar"];
    if (props.interactive && character()) c.push("iw-ch");
    if (mood() === "work") c.push("iw-float");
    if (poking()) c.push("iw-poke");
    if (angry()) c.push("iw-angry");
    if (!character()) c.push("iw-avatar--initial");
    return c.join(" ");
  };
  const bg = () => {
    const color = props.avatar?.color ?? "";
    return HEX_COLOR.test(color) ? color : "var(--ch4)";
  };

  return (
    <span
      class={classes()}
      style={{ width: `${size()}px`, height: `${size()}px`, ...(character() ? {} : { background: bg(), "font-size": `${Math.round(size() * 0.45)}px` }) }}
      role="img"
      aria-label={props.label ?? props.avatar?.initial ?? ""}
      onClick={poke}
      onAnimationEnd={(e) => e.animationName === "iw-poke" && setPoking(false)}
      data-mood={angry() ? "angry" : mood()}
    >
      <Show when={character()} fallback={(props.avatar?.initial ?? "").slice(0, 1).toUpperCase()}>
        {(c) => (
          <>
            <span class="iw-n" innerHTML={svgOf(c(), angry() ? "angry" : mood()) ?? ""} />
            <Show when={props.interactive}>
              <span class="iw-h" innerHTML={svgOf(c(), "hover") ?? ""} />
            </Show>
          </>
        )}
      </Show>
    </span>
  );
}
