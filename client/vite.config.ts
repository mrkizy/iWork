import { defineConfig } from "vite";
import solid from "vite-plugin-solid";
import process from "node:process";

// tauri dev 在移动端调试时经 TAURI_DEV_HOST 告诉前端绑定哪个地址；桌面端为空。
const host = process.env.TAURI_DEV_HOST;

export default defineConfig(() => ({
  plugins: [solid()],
  // 不清屏，免得盖住 Rust 侧的编译错误
  clearScreen: false,
  server: {
    // tauri.conf.json 的 devUrl 写死了 1420，端口被占就直接失败，不悄悄换端口
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: {
      // Rust 侧由 tauri dev 自己监视
      ignored: ["**/src-tauri/**"],
    },
  },
}));
