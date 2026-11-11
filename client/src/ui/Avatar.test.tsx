// 助手头像的表情 → 素材（ui-m1.md 3.3、2a 第 13 条）。表情由 Rust 给，这里只验网页选对了素材与动效类。
import { create } from "@bufbuild/protobuf";
import { render, screen } from "@solidjs/testing-library";
import { describe, expect, it } from "vitest";
import { AvatarViewSchema, Expression } from "../gen/iwork/v1/view_pb";
import { Avatar } from "./Avatar";

function shown(character: string, expression: Expression) {
  render(() => <Avatar avatar={create(AvatarViewSchema, { character })} expression={expression} interactive label="阿森" />);
  const el = screen.getByRole("img", { name: "阿森" });
  return { el, svg: el.querySelector(".iw-n svg") };
}

describe("出错表情 sorry", () => {
  it("回合失败时换成 sorry 素材，不浮动", () => {
    const { el, svg } = shown("asen", Expression.ERROR);
    expect(el.dataset.mood).toBe("sorry");
    expect(svg?.getAttribute("class")).toContain("iw-mood--sorry");
    expect(el.classList.contains("iw-float")).toBe(false);
  });

  it("对照：工作中用 work 素材并浮动", () => {
    const { el, svg } = shown("asen", Expression.WORKING);
    expect(svg?.getAttribute("class")).toContain("iw-mood--work");
    expect(el.classList.contains("iw-float")).toBe(true);
  });
});
