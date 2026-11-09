// 开发服务器只能监听 127.0.0.1（AGENTS.md 安全不变量：本机不开对外 TCP 端口）。
// 真把 vite.config.ts 起一遍看实际监听地址，而不是读配置文本；并且先把 TAURI_DEV_HOST
// 设成 0.0.0.0，证明环境变量放宽不了。
import { fileURLToPath } from "node:url";
import { createServer } from "vite";
import { afterEach, expect, it, vi } from "vitest";
import { buildSection, effectiveConfig } from "./tauri-config.ts";

const LOOPBACK = "127.0.0.1";
const CLIENT_DIR = fileURLToPath(new URL("..", import.meta.url));
const VITE_CONFIG = `${CLIENT_DIR}vite.config.ts`;

afterEach(() => {
  vi.unstubAllEnvs();
});

async function startDevServer() {
  // 端口改成 0（系统分配），免得和本机正在跑的 tauri dev 抢 1420；监听地址仍取配置文件的
  const server = await createServer({
    configFile: VITE_CONFIG,
    root: CLIENT_DIR,
    logLevel: "silent",
    server: { port: 0, strictPort: false },
  });
  await server.listen();
  return server;
}

it("HTTP 与 HMR 只监听 127.0.0.1，TAURI_DEV_HOST 放宽不了", async () => {
  vi.stubEnv("TAURI_DEV_HOST", "0.0.0.0");
  const server = await startDevServer();
  try {
    const address = server.httpServer?.address();
    if (address === null || address === undefined || typeof address === "string") {
      throw new Error(`开发服务器没有在 TCP 上监听：${String(address)}`);
    }
    expect(address.address, "HTTP 监听地址").toBe(LOOPBACK);

    // HMR 不另开端口时跟 HTTP 共用同一个监听，上面已经覆盖；另开端口时它按 hmr.host 监听
    const hmr = server.config.server.hmr;
    if (typeof hmr === "object" && hmr.port !== undefined) {
      expect(hmr.host, `HMR 另开端口 ${hmr.port}，监听地址`).toBe(LOOPBACK);
    }
  } finally {
    await server.close();
  }
});

it("pnpm tauri dev 的 devUrl 指向开发服务器的回环地址与端口", async () => {
  const server = await createServer({ configFile: VITE_CONFIG, root: CLIENT_DIR, logLevel: "silent" });
  try {
    expect(buildSection(effectiveConfig("dev")).devUrl).toBe(`http://${LOOPBACK}:${server.config.server.port}`);
  } finally {
    await server.close();
  }
});
