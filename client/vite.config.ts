import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// 开发服务器只监听回环地址（AGENTS.md 安全不变量：本机不开对外 TCP 端口），不留用环境变量放宽的入口。
// src-tauri/tauri.dev.conf.json 的 devUrl 与这里一致，tests/dev-server.test.ts 对拍。
const DEV_HOST = "127.0.0.1";
const DEV_PORT = 1420;

/** 设计系统（令牌、动效、助手形象）：网页直接引用 docs/design/design-system/，不复制一份（T09 方案 2.5）。 */
export const DESIGN_SYSTEM_ALIAS = "@design";
export const DESIGN_SYSTEM_DIR = fileURLToPath(new URL("../docs/design/design-system/", import.meta.url));

export default defineConfig({
  plugins: [solid()],
  resolve: { alias: { [DESIGN_SYSTEM_ALIAS]: DESIGN_SYSTEM_DIR } },
  // 不清屏，免得盖住 Rust 侧的编译错误
  clearScreen: false,
  server: {
    host: DEV_HOST,
    // devUrl 写死了端口，被占就直接失败，不悄悄换端口
    port: DEV_PORT,
    strictPort: true,
    // HMR 不另开端口，与 HTTP 共用同一个监听
    hmr: { host: DEV_HOST },
    watch: {
      // Rust 侧由 tauri dev 自己监视
      ignored: ["**/src-tauri/**"],
    },
    fs: {
      // 默认只放行 client/；只多放行设计系统这一个目录（tests/web-boundaries.test.ts 检查）
      allow: [fileURLToPath(new URL("./", import.meta.url)), DESIGN_SYSTEM_DIR],
    },
  },
  build: {
    // 发布入口只有 index.html；录像播放页 preview.html 只在开发服务器上用（tests/web-boundaries.test.ts 检查）
    rollupOptions: { input: fileURLToPath(new URL("./index.html", import.meta.url)) },
  },
});
