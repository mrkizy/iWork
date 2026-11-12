// 助手头像（ui-m1.md 3.3）。画什么由 Rust 给（AvatarView：形象 id、表情 id、动效、是否画应用标志、颜色），
// 这里只按 id 去素材清单里查（ui/characters.ts，查不到的兜底也在那里），不认识任何具体的形象或表情。
// 悬停、戳一下、连戳生气是网页本地的纯展示状态（方案 2.5），戳头像不触发任何业务操作。
// 动效类由 motion.css 管：减少动态效果、轻松元素关闭时自动停用。
import iconUrl from "@design/icon/iwork-icon-white-256.png";
import { createContext, createSignal, onCleanup, Show, useContext } from "solid-js";
import type { AvatarView } from "../gen/iwork/v1/view_pb";
import { ANGRY_MOOD, DESIGN_CATALOG, HOVER_MOOD, svgFor, type CharacterCatalog } from "./characters";
import "./Avatar.css";

const HEX_COLOR = /^#[0-9a-fA-F]{3,8}$/;
const POKE_WINDOW_MS = 2000;
const POKES_TO_ANGRY = 3;
const ANGRY_MS = 1500;

/** 头像查素材用的清单；默认是设计系统的，测试可以换成夹具。 */
const CatalogContext = createContext<CharacterCatalog>(DESIGN_CATALOG);
export const CharacterCatalogProvider = CatalogContext.Provider;

export function Avatar(props: {
  avatar?: AvatarView;
  size?: number;
  /** 悬停蹦跳、戳一下、连戳生气（侧栏、对话里的助手头像）。 */
  interactive?: boolean;
  label?: string;
}) {
  const catalog = useContext(CatalogContext);
  const size = () => props.size ?? 28;
  const [pokes, setPokes] = createSignal<number[]>([]);
  const [angry, setAngry] = createSignal(false);
  const [poking, setPoking] = createSignal(false);
  let angryTimer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(angryTimer));

  const logo = () => props.avatar?.appLogo ?? false;
  const character = () => props.avatar?.character ?? "";
  const mood = () => (angry() ? ANGRY_MOOD : (props.avatar?.mood ?? ""));

  const poke = () => {
    if (!props.interactive || logo()) return;
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
    if (logo()) return [...c, "iw-avatar--logo"].join(" ");
    if (props.interactive) c.push("iw-ch");
    const motion = props.avatar?.motion;
    if (motion && !angry()) c.push(`iw-${motion}`);
    if (poking()) c.push("iw-poke");
    if (angry()) c.push("iw-angry");
    return c.join(" ");
  };
  // 配置里选的颜色经 --ch4 交给素材（只圆团子用它；Rust 对换过形象的不给颜色）
  const style = () => {
    const color = props.avatar?.color ?? "";
    return {
      width: `${size()}px`,
      height: `${size()}px`,
      ...(HEX_COLOR.test(color) ? { "--ch4": color } : {}),
    };
  };

  return (
    <span
      class={classes()}
      style={style()}
      role="img"
      aria-label={props.label ?? ""}
      onClick={poke}
      onAnimationEnd={(e) => e.animationName === "iw-poke" && setPoking(false)}
      data-character={logo() ? undefined : character()}
      data-mood={logo() ? undefined : mood()}
    >
      <Show when={!logo()} fallback={<img class="iw-avatar__logo" src={iconUrl} alt="" />}>
        <span class="iw-n" innerHTML={svgFor(catalog, character(), mood())} />
        <Show when={props.interactive}>
          <span class="iw-h" innerHTML={svgFor(catalog, character(), HOVER_MOOD)} />
        </Show>
      </Show>
    </span>
  );
}
