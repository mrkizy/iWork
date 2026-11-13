import { describe, expect, it } from "vitest";
import { createRouter } from "./route";

describe("路由记住资料栏上次的页签（ui-m1 2a 第 18 条 b）", () => {
  it("没进过助手主页时是「动态」；进过之后是最后停的那个，离开助手主页也记着", () => {
    const r = createRouter();
    expect(r.assistantTab()).toBe("activity");
    r.go({ page: "assistant", id: "a", conversationId: "c", tab: "config" });
    r.go({ page: "assistant", id: "a", conversationId: "c", tab: "memory" });
    r.go({ page: "pending" });
    expect(r.assistantTab()).toBe("memory");
  });

  it("一开始就在助手主页：从那个页签算起", () => {
    expect(createRouter({ page: "assistant", id: "a", conversationId: "c", tab: "config" }).assistantTab()).toBe("config");
  });
});
