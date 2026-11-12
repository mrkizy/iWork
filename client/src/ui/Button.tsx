// 按钮与图标按钮。图标按钮必须带 aria-label 与悬停提示（ui-m1.md 第 2 节第 7 条）。
import { splitProps, type JSX } from "solid-js";
import { Icon, type IconName } from "./Icon";
import "./Button.css";

export type ButtonVariant = "default" | "primary" | "accent" | "ghost" | "link";

type ButtonProps = JSX.ButtonHTMLAttributes<HTMLButtonElement> & { variant?: ButtonVariant };

export function Button(props: ButtonProps) {
  const [own, rest] = splitProps(props, ["variant", "class", "type"]);
  const variant = () => (own.variant && own.variant !== "default" ? ` iw-btn--${own.variant}` : "");
  return <button type={own.type ?? "button"} class={`iw-btn${variant()}${own.class ? ` ${own.class}` : ""}`} {...rest} />;
}

type IconButtonProps = Omit<JSX.ButtonHTMLAttributes<HTMLButtonElement>, "children"> & {
  icon: IconName;
  /** 同时是 aria-label 与悬停提示。 */
  label: string;
  tipSide?: "top" | "bottom";
  iconSize?: number;
};

export function IconButton(props: IconButtonProps) {
  const [own, rest] = splitProps(props, ["icon", "label", "tipSide", "iconSize", "class", "type"]);
  return (
    <button
      type={own.type ?? "button"}
      class={`iw-icon-btn${own.class ? ` ${own.class}` : ""}`}
      aria-label={own.label}
      data-tip={own.label}
      data-tip-side={own.tipSide}
      {...rest}
    >
      <Icon name={own.icon} size={own.iconSize ?? 16} />
    </button>
  );
}
