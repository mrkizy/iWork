// pnpm -C client dev:fake [场景] [-- 传给 tauri dev 的参数]：起假后台跑一个场景，再起 `pnpm tauri dev` 连它（方案 2.6）。
// 客户端在 dev feature 下按 IWORK_DEV_ENDPOINT 连假后台（发布构建不认识这个变量）。退出时收掉假后台与临时目录。
// 场景列表：cargo run -p iwork-fake-daemon --features dev -- --list
import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const CLIENT = fileURLToPath(new URL("../", import.meta.url));
const ROOT = join(CLIENT, "..");
const TARGET = process.env.CARGO_TARGET_DIR ?? join(ROOT, "target");

const args = process.argv.slice(2);
const split = args.indexOf("--");
const own = split < 0 ? args : args.slice(0, split);
const tauriArgs = split < 0 ? [] : args.slice(split + 1);
const scenario = own[0] ?? "conversation-basic";

execFileSync("cargo", ["build", "-q", "-p", "iwork-fake-daemon", "--features", "dev"], { cwd: ROOT, stdio: "inherit" });
const dir = mkdtempSync(join(tmpdir(), "iwork-fake-"));
const sock = join(dir, "d.sock");
const daemon = spawn(join(TARGET, "debug/iwork-fake-daemon"), ["--endpoint", sock, "--scenario", scenario], { stdio: ["ignore", "inherit", "pipe"] });
let cleaned = false;
const cleanup = () => {
  if (cleaned) return;
  cleaned = true;
  daemon.kill("SIGTERM");
  rmSync(dir, { recursive: true, force: true });
};
process.on("exit", cleanup);
for (const sig of ["SIGINT", "SIGTERM"] as const) process.on(sig, () => process.exit(130));

let err = "";
daemon.stderr?.on("data", (b: Buffer) => {
  err += b.toString();
  process.stderr.write(b);
  if (err.includes("跑完") && !started) start();
});
daemon.on("exit", (code) => {
  if (!started) {
    console.error(`假后台没跑起来（退出码 ${code}）`);
    process.exit(1);
  }
});

let started = false;
function start() {
  started = true;
  const tauri = spawn("node", ["scripts/tauri.ts", "dev", ...tauriArgs], {
    cwd: CLIENT,
    stdio: "inherit",
    env: { ...process.env, IWORK_DEV_ENDPOINT: sock },
  });
  tauri.on("exit", (code) => process.exit(code ?? 0));
}
