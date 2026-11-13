// 真实 WebView 测试（T09 方案第 7 节「真实 WebView」；R7 的四道与传输路径的证据在这里取）。
// 用法：pnpm -C client e2e:webview [用例名…]（只在 macOS 跑；会开窗口，跑完自动关）。
//   1. 构建：带 e2e feature 的客户端（经 pnpm tauri build --debug --no-bundle，发布层配置与 CSP；包装脚本把
//      --debug 与 --no-bundle 同时在的 build 当测试构建，不装配、不出包，见 scripts/release-build.ts）、带 dev 的假后台；
//   2. 每条用例：起假后台跑场景 → 起客户端（初始化脚本 = prelude.js + 用例脚本）→ 等页面报 done 与壳的计数 → 断言。
// 测试构建与发布构建的差别只在 client/src-tauri/src/e2e.rs（初始化脚本、标题回报、可换 CSP）。
import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const CLIENT = fileURLToPath(new URL("../../", import.meta.url));
const ROOT = join(CLIENT, "..");
const TARGET = process.env.CARGO_TARGET_DIR ?? join(ROOT, "target");
const CASES_DIR = fileURLToPath(new URL("./cases/", import.meta.url));
const RELEASE_CSP: string = JSON.parse(readFileSync(join(CLIENT, "src-tauri/tauri.conf.json"), "utf8")).app.security.csp;
/** 不放行 Tauri 自定义协议的 CSP：invoke 只能退回 postMessage（forced_fallback）。 */
const NO_IPC_CSP = RELEASE_CSP.replace(/connect-src [^;]*/, "connect-src 'self'");
/** 视图声明类的操作（网页自己会发）；除此之外的操作在这些用例里都不该出现。 */
const VIEW_INTENTS = new Set(["SetVisible", "SetVisibleRows", "SetFocused", "LoadOlder"]);
const STEADY_SECONDS = Number(process.env.E2E_SECONDS ?? 60);

interface Run {
  reports: Record<string, unknown>[];
  counters?: {
    custom_protocol: number;
    fallback: number;
    fallback_at: [number, string][];
    intents: Record<string, number>;
    handed_bytes: number;
    frames_built: number;
  };
  rssKb: number[];
}

interface Case {
  scenario: string;
  /** 场景与客户端同时跑（流式用例）：假后台一开始监听就起客户端，并用 --pace 把流量摊长。 */
  live?: { pace: number };
  script: string;
  timeoutS: number;
  env?: Record<string, string>;
  extra?: string;
  /** 返回失败原因；空数组表示通过。 */
  check(run: Run): string[];
}

const done = (r: Run) => r.reports.find((x) => x.done === true) ?? {};
const nonViewIntents = (r: Run) => Object.keys(r.counters?.intents ?? {}).filter((k) => !VIEW_INTENTS.has(k));
const expect = (failures: string[], ok: boolean, what: string) => {
  if (!ok) failures.push(what);
};

const CASES: Record<string, Case> = {
  render: {
    scenario: "xss-corpus",
    script: "render.js",
    timeoutS: 60,
    check(r) {
      const d = done(r);
      const f: string[] = [];
      expect(f, d.probe === null, `探针执行了：${String(d.probe)}`);
      expect(f, d.alerts === 0, `alert 被调用 ${String(d.alerts)} 次`);
      expect(f, d.executable === 0 && d.handlers === 0 && d.hrefs === 0, `DOM 里有可执行节点：${JSON.stringify(d)}`);
      expect(f, d.shownAsText === true, "对照：脚本文字应当当文字显示");
      expect(f, nonViewIntents(r).length === 0, `渲染后 Rust 收到了操作：${nonViewIntents(r).join(",")}`);
      expect(f, r.counters?.fallback === 0, `走了回退路径 ${r.counters?.fallback} 次`);
      return f;
    },
  },
  navigation: {
    scenario: "xss-corpus",
    script: "navigation.js",
    timeoutS: 60,
    check(r) {
      const d = done(r);
      const f: string[] = [];
      expect(f, d.sameOrigin === true, `主窗口来源变了：${JSON.stringify(d)}`);
      expect(f, d.opened === "null", `window.open 没被拒：${String(d.opened)}`);
      expect(f, nonViewIntents(r).length === 0, `收到了操作：${nonViewIntents(r).join(",")}`);
      return f;
    },
  },
  csp: {
    scenario: "empty",
    script: "csp.js",
    timeoutS: 60,
    check(r) {
      const d = done(r);
      const f: string[] = [];
      expect(f, d.inline === null, "内联脚本执行了");
      expect(f, d.remote === "blocked", `远程脚本：${String(d.remote)}`);
      expect(f, d.eval === "blocked", `eval：${String(d.eval)}`);
      return f;
    },
  },
  foreign: {
    scenario: "empty",
    script: "foreign.js",
    timeoutS: 60,
    env: { IWORK_E2E_OTHER: "1" },
    check(r) {
      const other = r.reports.find((x) => x.window === "other") as { results?: Record<string, string> } | undefined;
      const f: string[] = [];
      expect(f, other !== undefined, "副窗口没报告");
      for (const cmd of ["attach", "next_frame", "ui_intent"]) {
        expect(f, other?.results?.[cmd]?.startsWith("rejected") === true, `副窗口调 ${cmd}：${other?.results?.[cmd]}`);
      }
      return f;
    },
  },
  forged: {
    scenario: "empty",
    script: "forged.js",
    timeoutS: 60,
    check(r) {
      const d = done(r) as { forged?: { code?: string | null }; control?: { code?: string | null }; dialogOpen?: boolean };
      const f: string[] = [];
      expect(f, d.forged?.code === "invalid_intent", `伪造的 70 号意图没被拒：${JSON.stringify(d.forged)}`);
      expect(f, d.control?.code === null, `对照：合法的 clear_add_project 应被受理：${JSON.stringify(d.control)}`);
      expect(f, d.dialogOpen === false, "添加项目弹窗被打开了");
      const intents = r.counters?.intents ?? {};
      expect(f, intents["?"] === 1, `Rust 认作「认不出的操作」的次数：${JSON.stringify(intents)}`);
      expect(f, intents.ClearAddProject === 1, `对照：ClearAddProject 次数：${JSON.stringify(intents)}`);
      expect(f, r.counters?.fallback === 0, `走了回退路径 ${r.counters?.fallback} 次`);
      return f;
    },
  },
  reload: {
    scenario: "stream-16x100",
    live: { pace: 70 },
    script: "reload.js",
    timeoutS: 420,
    check(r) {
      const d = done(r) as { reloads?: number; diagnostic?: boolean; converged?: boolean };
      const c = r.counters;
      const f: string[] = [];
      expect(f, d.reloads === 200, `刷新次数 ${d.reloads}`);
      expect(f, d.diagnostic === false, "新页面显示了诊断页");
      expect(f, d.converged === true, "最后一次刷新后界面没收敛");
      expect(f, nonViewIntents(r).length === 0, `执行了操作：${nonViewIntents(r).join(",")}`);
      // E1 决定（supervisor 2026-11-10）：回退调用单独计数——每条都落在某次刷新之后的窗口里，总数不超过刷新次数 × 在途命令数
      // 刷新时刻分批带在各条报告里（标题太长会被截断）
      const at = r.reports.flatMap((x) => (Array.isArray(x.reloadAt) ? (x.reloadAt as number[]) : []));
      expect(f, at.length === d.reloads, `收到 ${at.length} 个刷新时刻，应为 ${d.reloads}`);
      const outside = (c?.fallback_at ?? []).filter(([t]) => !at.some((r0) => t >= r0 && t <= r0 + 3000));
      expect(f, outside.length === 0, `有 ${outside.length} 次回退调用不在任何刷新窗口里`);
      expect(f, (c?.fallback ?? 0) <= at.length * 3, `回退调用 ${c?.fallback} 次，超过刷新次数 × 在途命令数`);
      const growth = rssGrowthKb(r);
      expect(f, growth < 1024, `客户端进程常驻内存涨了 ${growth} KiB（上限 1 MiB）`);
      return f;
    },
  },
  forced_fallback: {
    scenario: "stream-16x100",
    live: { pace: 70 },
    script: "steady.js",
    timeoutS: STEADY_SECONDS + 60,
    env: { IWORK_E2E_CSP: NO_IPC_CSP },
    extra: `window.__E2E_SECONDS = ${STEADY_SECONDS};`,
    check(r) {
      const c = r.counters;
      const d = done(r);
      const f: string[] = [];
      expect(f, c?.custom_protocol === 0, `自定义协议路径被调用了 ${c?.custom_protocol} 次`);
      expect(f, (c?.fallback ?? 0) > 0, "对照：应当真的走了回退路径");
      expect(f, (c?.fallback ?? 0) <= 3, `收到 transport_fallback 后还在调用（${c?.fallback} 次）`);
      expect(f, c?.handed_bytes === 0 && c?.frames_built === 0, `回退路径上交出了帧数据：${JSON.stringify(c)}`);
      expect(f, d.diagnostic === true, "没有显示固定诊断页");
      const growth = rssGrowthKb(r);
      expect(f, growth < 1024, `这段时间客户端进程常驻内存涨了 ${growth} KiB`);
      return f;
    },
  },
  custom_protocol: {
    scenario: "stream-16x100",
    live: { pace: 70 },
    script: "steady.js",
    timeoutS: STEADY_SECONDS + 60,
    extra: `window.__E2E_SECONDS = ${STEADY_SECONDS};`,
    check(r) {
      const c = r.counters;
      const f: string[] = [];
      expect(f, c?.fallback === 0, `走了回退路径 ${c?.fallback} 次`);
      // 对照：流式期间持续交帧（场景摊到约 70 秒，这一分钟里帧数应当远多于一两帧）
      expect(f, (c?.handed_bytes ?? 0) > 0 && (c?.frames_built ?? 0) > 50, `对照：应当持续交帧（只算了 ${c?.frames_built} 帧）`);
      expect(f, done(r).diagnostic === false, "显示了诊断页");
      return f;
    },
  },
  once: {
    scenario: "empty",
    script: "once.js",
    timeoutS: 60,
    check(r) {
      const n = r.counters?.intents?.LoadMoreConversations ?? 0;
      return n <= 1 ? [] : [`同一个操作执行了 ${n} 次`];
    },
  },
};

function rssGrowthKb(r: Run): number {
  // 第一次采样在启动 5 秒后（页面与字体都加载完），之后取最大值
  if (r.rssKb.length < 2) return 0;
  return Math.max(...r.rssKb.slice(1)) - r.rssKb[0];
}

function build() {
  const lock = ["/tmp/iwork-heavy-test.lock"];
  execFileSync("lockf", [...lock, "node", "scripts/tauri.ts", "build", "--debug", "--no-bundle", "--features", "e2e"], {
    cwd: CLIENT,
    stdio: "inherit",
  });
  execFileSync("lockf", [...lock, "cargo", "build", "-q", "-p", "iwork-fake-daemon", "--features", "dev"], { cwd: ROOT, stdio: "inherit" });
}

function waitFor<T>(what: string, ms: number, poll: () => T | undefined): Promise<T> {
  const start = Date.now();
  return new Promise((resolve, reject) => {
    const tick = () => {
      const v = poll();
      if (v !== undefined) return resolve(v);
      if (Date.now() - start > ms) return reject(new Error(`等不到：${what}`));
      setTimeout(tick, 200);
    };
    tick();
  });
}

function rss(pid: number): number | undefined {
  try {
    return Number(execFileSync("ps", ["-o", "rss=", "-p", String(pid)], { encoding: "utf8" }).trim()) || undefined;
  } catch {
    return undefined;
  }
}

async function runCase(name: string, c: Case): Promise<Run> {
  const dir = mkdtempSync(join(tmpdir(), "iwork-e2e-"));
  const sock = join(dir, "d.sock");
  const out = join(dir, "out.jsonl");
  const script = join(dir, "script.js");
  writeFileSync(script, [readFileSync(join(CASES_DIR, "prelude.js"), "utf8"), c.extra ?? "", readFileSync(join(CASES_DIR, c.script), "utf8")].join("\n"));
  const children: ChildProcess[] = [];
  try {
    let daemonErr = "";
    const pace = c.live ? ["--pace", String(c.live.pace)] : [];
    const daemon = spawn(join(TARGET, "debug/iwork-fake-daemon"), ["--endpoint", sock, "--scenario", c.scenario, ...pace], {
      stdio: ["ignore", "ignore", "pipe"],
    });
    children.push(daemon);
    daemon.stderr?.on("data", (b: Buffer) => (daemonErr += b.toString()));
    // 流式用例：一开始监听就起客户端，场景边跑边推；其余等场景跑完再起，结果确定
    const ready = c.live ? "监听" : "跑完";
    await waitFor(`假后台${ready}（${c.scenario}）`, 120_000, () => (daemonErr.includes(ready) ? true : undefined));
    const client = spawn(join(TARGET, "debug/iwork-client"), [], {
      stdio: "ignore",
      // IWORK_DATA_DIR：客户端日志（client.log）写进用例的临时目录，不写开发者真实的数据目录
      env: { ...process.env, IWORK_DEV_ENDPOINT: sock, IWORK_DATA_DIR: join(dir, "data"), IWORK_E2E_SCRIPT: script, IWORK_E2E_OUT: out, ...c.env },
    });
    children.push(client);
    const rssKb: number[] = [];
    const sampler = setInterval(() => {
      if (client.pid !== undefined && Date.now() - started > 5000) {
        const v = rss(client.pid);
        if (v !== undefined) rssKb.push(v);
      }
    }, 1000);
    const started = Date.now();
    const exited = new Promise<void>((r) => client.on("exit", () => r()));
    try {
      await Promise.race([
        exited,
        waitFor(`用例 ${name} 报 done`, c.timeoutS * 1000, () => (existsSync(out) && readFileSync(out, "utf8").includes('"counters"') ? true : undefined)),
      ]);
    } finally {
      clearInterval(sampler);
    }
    const lines = existsSync(out) ? readFileSync(out, "utf8").trim().split("\n").filter(Boolean) : [];
    const parsed = lines.map((l) => JSON.parse(l) as Record<string, unknown>);
    return {
      reports: parsed.filter((p) => "report" in p).map((p) => ({ ...(p.report as Record<string, unknown>), window: (p.report as { window?: string }).window ?? p.window })),
      counters: parsed.find((p) => "counters" in p)?.counters as Run["counters"],
      rssKb,
    };
  } finally {
    for (const ch of children) ch.kill("SIGTERM");
    rmSync(dir, { recursive: true, force: true });
  }
}

async function main() {
  if (process.platform !== "darwin") {
    console.error("真实 WebView 测试目前只在 macOS 上跑（Linux 的 webkit2gtk 归步 8 的 CI）");
    process.exit(2);
  }
  const names = process.argv.slice(2).filter((a) => a !== "--no-build");
  const picked = names.length > 0 ? names : Object.keys(CASES);
  for (const n of picked) if (!(n in CASES)) throw new Error(`没有用例 ${n}；可用：${Object.keys(CASES).join(", ")}`);
  if (!process.argv.includes("--no-build")) build();
  let failed = 0;
  for (const n of picked) {
    const t0 = Date.now();
    let failures: string[];
    let run: Run | undefined;
    try {
      run = await runCase(n, CASES[n]);
      failures = run.counters === undefined ? [`没有拿到计数（页面没报 done）：${JSON.stringify(run.reports)}`] : CASES[n].check(run);
    } catch (e) {
      failures = [String(e)];
    }
    const secs = ((Date.now() - t0) / 1000).toFixed(0);
    // 回退时刻可能上百个，只打条数
    const brief = run?.counters && { ...run.counters, fallback_at: run.counters.fallback_at.length };
    if (failures.length === 0) console.log(`✓ ${n}（${secs}s）${JSON.stringify(brief)}`);
    else {
      failed++;
      console.log(`✗ ${n}（${secs}s）\n  - ${failures.join("\n  - ")}\n  计数：${JSON.stringify(brief)}\n  报告：${JSON.stringify(run?.reports ?? []).slice(0, 600)}`);
    }
    if (run && run.rssKb.length > 1) console.log(`  客户端常驻内存：${run.rssKb[0]} → 最大 ${Math.max(...run.rssKb)} KiB`);
  }
  process.exit(failed === 0 ? 0 : 1);
}

void main();
