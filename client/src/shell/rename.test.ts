import { describe, expect, it } from "vitest";
import { clampTitle, renameOutcome, TITLE_MAX } from "./rename";

describe("重命名规则（2a 第 6 条）", () => {
  it("去掉首尾空白后提交", () => {
    expect(renameOutcome("旧标题", "  新标题 ")).toEqual({ kind: "submit", title: "新标题" });
  });
  it("空内容还原，不提交", () => {
    expect(renameOutcome("旧标题", "   ")).toEqual({ kind: "restore" });
  });
  it("没变就直接退出，不发命令", () => {
    expect(renameOutcome("旧标题", " 旧标题")).toEqual({ kind: "restore" });
  });
  it("按字符数截到 50（中文与表情都算一个）", () => {
    const long = "字".repeat(60);
    expect(Array.from(clampTitle(long))).toHaveLength(TITLE_MAX);
    expect(Array.from(clampTitle("😀".repeat(51)))).toHaveLength(TITLE_MAX);
    expect(clampTitle("短")).toBe("短");
  });
});
