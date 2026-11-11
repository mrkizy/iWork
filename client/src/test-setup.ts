// 网页测试的公共准备：jest-dom 断言（toBeDisabled 等）；每个用例后卸载渲染的组件；
// jsdom 没有的两个浏览器接口给最小替身（jsdom 不排版，行高测不出来，虚拟行的计算另有纯函数测试）。
import "@testing-library/jest-dom/vitest";
import { cleanup } from "@solidjs/testing-library";
import { afterEach } from "vitest";

afterEach(cleanup);

if (globalThis.ResizeObserver === undefined) {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
}
if (globalThis.CSS?.escape === undefined) {
  (globalThis as { CSS?: { escape(s: string): string } }).CSS = { escape: (s: string) => s.replace(/["\\]/g, "\\$&") };
}
