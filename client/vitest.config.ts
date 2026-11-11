import { defineConfig } from "vitest/config";
import solid from "vite-plugin-solid";
import { fileURLToPath } from "node:url";
import { DESIGN_SYSTEM_ALIAS, DESIGN_SYSTEM_DIR } from "./vite.config.ts";

// 两组测试分开跑：
// - node：只读文件、起子进程的配置检查与金样本解码（tests/、src/gen.test.ts），不需要 DOM；
// - web：网页层（src/**/*.test.ts(x)），jsdom + vite-plugin-solid（Solid 在 node 条件下会解析到没有响应式的服务端版本）。
export default defineConfig({
  test: {
    projects: [
      {
        test: {
          name: "node",
          environment: "node",
          include: ["src/gen.test.ts", "tests/**/*.test.ts"],
        },
      },
      {
        plugins: [solid()],
        resolve: { alias: { [DESIGN_SYSTEM_ALIAS]: DESIGN_SYSTEM_DIR } },
        // 与开发服务器相同：只多放行设计系统目录（组件内联形象 SVG）
        server: { fs: { allow: [fileURLToPath(new URL("./", import.meta.url)), DESIGN_SYSTEM_DIR] } },
        test: {
          name: "web",
          environment: "jsdom",
          include: ["src/**/*.test.{ts,tsx}"],
          exclude: ["src/gen.test.ts"],
          setupFiles: ["src/test-setup.ts"],
        },
      },
    ],
  },
});
