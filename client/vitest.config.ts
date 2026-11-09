import { defineConfig } from "vitest/config";

// 测试单独配置，不复用 vite.config.ts：vite-plugin-solid 会把测试环境改成 jsdom，
// 而现有测试只读文件、跑在 Node 里，不需要 DOM。
export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
