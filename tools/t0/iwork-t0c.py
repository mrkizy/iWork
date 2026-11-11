#!/usr/bin/env python3
"""iWork T0c 实测：用一个最小 ACP 客户端测试 claude-agent-acp、codex-acp、gemini。

测什么（每家都测）：
  T1 执行命令 + 拒绝：是否发 session/request_permission；拒绝后命令是否确实没执行
  T2 执行命令 + 允许：命令是交给客户端终端（terminal/create）执行，还是 Agent 自己执行
  T3 写文件 + 允许：是否经客户端 fs/write_text_file，还是 Agent 自己写
  T4 联网 + 拒绝；T5 联网审批长时间不答复（默认 720 秒）后再拒绝：是否被超时打断，打断后是放行还是拒绝
另外记录：会话模式、全部 session/update 类型、与用量/额度相关的字段、登录状态命令输出。

只用 Python 标准库。只执行无害的 echo 与写一个测试文件，均在临时目录里。
用法：python3 iwork-t0c.py            （完整，约 40 分钟）
      python3 iwork-t0c.py --quick    （跳过 T5，约 5 分钟）
      python3 iwork-t0c.py --only codex
结果：~/iwork-t0c-results/ 与 ~/iwork-t0c-results.tar.gz
"""
import json, os, queue, shutil, subprocess, sys, tarfile, tempfile, threading, time

HOME = os.path.expanduser("~")
OUT = os.path.join(HOME, "iwork-t0c-results")
LONG_WAIT = 720
PROMPT_TIMEOUT = 240


def log(msg):
    line = f"== [{time.strftime('%H:%M:%S')}] {msg}"
    print(line, flush=True)
    with open(os.path.join(OUT, "summary.txt"), "a") as f:
        f.write(line + "\n")


AUTH_FIELDS = ("loggedIn", "authMethod", "subscriptionType", "loginExpiresAt")


def claude_auth_fields():
    """claude auth status 只留四个字段；不是 JSON 时只记长度。原文不进日志。"""
    try:
        r = subprocess.run(["claude", "auth", "status"], capture_output=True, text=True, timeout=60)
    except Exception as e:
        return f"ERROR {type(e).__name__}"
    try:
        doc = json.loads(r.stdout)
        kept = {k: doc.get(k) for k in AUTH_FIELDS} if isinstance(doc, dict) else None
    except ValueError:
        kept = None
    if kept is None:
        return f"输出不是 JSON 对象（{len(r.stdout)} 字节，内容不记录）\n(exit={r.returncode})"
    return json.dumps(kept, ensure_ascii=False) + f"\n(exit={r.returncode})"


def redacted_status(cmd):
    """登录状态类命令：输出格式不固定、可能带账号，只记退出码与字节数。"""
    try:
        r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=60)
    except Exception as e:
        return f"ERROR {type(e).__name__}"
    return f"stdout {len(r.stdout)} 字节、stderr {len(r.stderr)} 字节（内容不记录）\n(exit={r.returncode})"


def sh(cmd):
    try:
        r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=60)
        return (r.stdout + r.stderr).strip() + f"\n(exit={r.returncode})"
    except Exception as e:
        return f"ERROR {e}"


class Client:
    """最小 ACP 客户端：换行分隔的 JSON-RPC over stdio。"""

    def __init__(self, name, argv, cwd):
        self.name, self.cwd = name, cwd
        self.trace = open(os.path.join(OUT, f"{name}-trace.jsonl"), "a")
        self.errf = open(os.path.join(OUT, f"{name}-stderr.txt"), "a")
        self.proc = subprocess.Popen(argv, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.errf, text=True, bufsize=1)
        self.next_id = 1000
        self.pending = {}
        self.lock = threading.Lock()
        self.events = []          # (time, direction, obj)
        self.policy = "allow"     # 对 request_permission 的处理：allow / reject / wait_then_reject
        self.terminals = {}
        threading.Thread(target=self._reader, daemon=True).start()

    def _rec(self, direction, obj):
        self.events.append((time.time(), direction, obj))
        self.trace.write(json.dumps({"t": time.time(), "dir": direction, "msg": obj}, ensure_ascii=False) + "\n")
        self.trace.flush()

    def _send(self, obj):
        self._rec("out", obj)
        with self.lock:
            self.proc.stdin.write(json.dumps(obj) + "\n")
            self.proc.stdin.flush()

    def request(self, method, params, timeout=PROMPT_TIMEOUT):
        self.next_id += 1
        rid = self.next_id
        q = queue.Queue()
        self.pending[rid] = q
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        try:
            return q.get(timeout=timeout)
        except queue.Empty:
            return {"error": {"message": f"client timeout after {timeout}s"}}

    def notify(self, method, params):
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def _reply(self, rid, result=None, error=None):
        msg = {"jsonrpc": "2.0", "id": rid}
        if error is not None:
            msg["error"] = error
        else:
            msg["result"] = result
        self._send(msg)

    def _reader(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except Exception:
                self._rec("in-raw", line)
                continue
            self._rec("in", obj)
            if "method" in obj and "id" in obj:
                threading.Thread(target=self._handle_request, args=(obj,), daemon=True).start()
            elif "id" in obj and obj["id"] in self.pending:
                self.pending.pop(obj["id"]).put(obj)

    # ---------- 客户端能力：权限、文件、终端 ----------
    def _handle_request(self, obj):
        m, p, rid = obj["method"], obj.get("params", {}), obj["id"]
        if m == "session/request_permission":
            opts = p.get("options", [])
            def pick(kinds):
                for k in kinds:
                    for o in opts:
                        if o.get("kind") == k:
                            return o["optionId"]
                return opts[0]["optionId"] if opts else None
            if self.policy == "allow":
                self._reply(rid, {"outcome": {"outcome": "selected", "optionId": pick(["allow_once", "allow_always"])}})
            elif self.policy == "reject":
                self._reply(rid, {"outcome": {"outcome": "selected", "optionId": pick(["reject_once", "reject_always"])}})
            else:  # wait_then_reject
                self.events.append((time.time(), "note", {"wait_started": True}))
                time.sleep(LONG_WAIT)
                self.events.append((time.time(), "note", {"wait_finished": True}))
                self._reply(rid, {"outcome": {"outcome": "selected", "optionId": pick(["reject_once", "reject_always"])}})
        elif m == "fs/read_text_file":
            try:
                with open(p["path"]) as f:
                    self._reply(rid, {"content": f.read()})
            except Exception as e:
                self._reply(rid, error={"code": -32000, "message": str(e)})
        elif m == "fs/write_text_file":
            try:
                with open(p["path"], "w") as f:
                    f.write(p.get("content", ""))
                self._reply(rid, None)
            except Exception as e:
                self._reply(rid, error={"code": -32000, "message": str(e)})
        elif m == "terminal/create":
            tid = f"term_{len(self.terminals) + 1}"
            env = dict(os.environ)
            for e in p.get("env", []) or []:
                env[e["name"]] = e["value"]
            argv = [p["command"]] + list(p.get("args", []) or [])
            try:
                proc = subprocess.Popen(argv if p.get("args") else p["command"], shell=not p.get("args"),
                                        cwd=p.get("cwd") or self.cwd, env=env,
                                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
                self.terminals[tid] = proc
                self._reply(rid, {"terminalId": tid})
            except Exception as e:
                self._reply(rid, error={"code": -32000, "message": str(e)})
        elif m in ("terminal/output", "terminal/wait_for_exit"):
            proc = self.terminals.get(p.get("terminalId"))
            if not proc:
                return self._reply(rid, error={"code": -32000, "message": "no such terminal"})
            out, _ = proc.communicate()
            proc._out = getattr(proc, "_out", "") + (out or "")
            if m == "terminal/output":
                self._reply(rid, {"output": proc._out, "truncated": False,
                                  "exitStatus": {"exitCode": proc.returncode, "signal": None}})
            else:
                self._reply(rid, {"exitCode": proc.returncode, "signal": None})
        elif m in ("terminal/release", "terminal/kill"):
            proc = self.terminals.get(p.get("terminalId"))
            if proc and proc.poll() is None:
                proc.kill()
            self._reply(rid, None)
        else:
            self._reply(rid, error={"code": -32601, "message": f"method not supported by test client: {m}"})

    def close(self):
        try:
            self.proc.terminate()
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()


def methods_since(c, t0):
    return [o.get("method") for (t, d, o) in c.events if t >= t0 and d == "in" and isinstance(o, dict) and "method" in o]


def text_since(c, t0):
    return json.dumps([o for (t, d, o) in c.events if t >= t0 and d == "in"], ensure_ascii=False)


def run_prompt(c, sid, text, policy, timeout=PROMPT_TIMEOUT):
    c.policy = policy
    t0 = time.time()
    res = c.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": text}]}, timeout=timeout)
    if "error" in res and "client timeout" in str(res["error"]):
        c.notify("session/cancel", {"sessionId": sid})
    return t0, res


def check(c, name, tag, t0, marker):
    ms = methods_since(c, t0)
    reqs = [m for m in ms if m != "session/update"]
    executed = False
    for (t, d, o) in c.events:
        if t >= t0 and d == "in" and isinstance(o, dict) and o.get("method") == "session/update":
            u = o["params"]["update"]
            if u.get("sessionUpdate") == "tool_call_update" and marker in json.dumps(u.get("rawOutput", ""), ensure_ascii=False):
                executed = True
    log(f"[{name}] {tag} 收到请求：{reqs}")
    log(f"[{name}] {tag} 权限请求：{'session/request_permission' in ms}；客户端终端：{'terminal/create' in ms}；"
        f"客户端写文件：{'fs/write_text_file' in ms}；命令输出出现：{executed}")


def setup(c, name, sid, cfg):
    for method, params in cfg:
        params = dict(params, sessionId=sid)
        r = c.request(method, params, timeout=60)
        log(f"[{name}] {method} {params} -> {json.dumps(r.get('result', r), ensure_ascii=False)[:300]}")


AGENT_SETUP = {
    # Claude：切到 Manual（default）模式，看每个动作是否都发权限请求
    "claude": [("session/set_mode", {"modeId": "default"})],
    # Codex：换成 ChatGPT 账户可用的模型；用 Default（auto）模式 = 工作区可写、越界才问
    "codex": [("session/set_config_option", {"configId": "model", "value": "gpt-5.5"}),
              ("session/set_mode", {"modeId": "auto"})],
    "codex-org": [("session/set_config_option", {"configId": "model", "value": "gpt-5.5"}),
                  ("session/set_mode", {"modeId": "auto"})],
    "agy": [],
    "gemini": [],
}


def test_agent(name, argv, quick):
    work = tempfile.mkdtemp(prefix=f"iwork-t0c-{name}-")
    log(f"[{name}] 启动：{' '.join(argv)}  工作目录 {work}")
    c = Client(name, argv, work)
    init = c.request("initialize", {
        "protocolVersion": 1,
        "clientCapabilities": {"fs": {"readTextFile": True, "writeTextFile": True}, "terminal": True},
        "clientInfo": {"name": "iwork-t0c", "title": "iWork T0c", "version": "0.2.0"}}, timeout=180)
    log(f"[{name}] initialize：{json.dumps(init.get('result', init), ensure_ascii=False)[:400]}")
    if "error" in init:
        c.close(); return
    new = c.request("session/new", {"cwd": work, "mcpServers": []}, timeout=120)
    log(f"[{name}] session/new：{json.dumps(new.get('result', new), ensure_ascii=False)[:300]}")
    if "error" in new:
        log(f"[{name}] 无法建会话（多半未登录）。跳过此家。")
        c.close(); return
    sid = new["result"]["sessionId"]
    setup(c, name, sid, AGENT_SETUP.get(name, []))

    t0, r = run_prompt(c, sid, "Use your shell/terminal tool to run exactly this command: echo hello-iwork-t1 . Then report its output.", "reject")
    check(c, name, "T1 执行+拒绝", t0, "hello-iwork-t1")
    t0, r = run_prompt(c, sid, "Use your shell/terminal tool to run exactly this command: echo hello-iwork-t2 . Then report its output.", "allow")
    check(c, name, "T2 执行+允许", t0, "hello-iwork-t2")
    t0, r = run_prompt(c, sid, "Using your file editing/writing tool (not the shell), create a file named iwork-t3.txt in the current working directory containing exactly: hi", "allow")
    check(c, name, "T3 写文件+允许", t0, "iwork-t3")
    log(f"[{name}] T3 文件存在：{os.path.exists(os.path.join(work, 'iwork-t3.txt'))}")
    t0, r = run_prompt(c, sid, "Use your shell/terminal tool to run exactly this command: curl -sI https://example.com | head -1 . Then report its output.", "reject")
    check(c, name, "T4 联网+拒绝", t0, "HTTP/")

    if not quick:
        log(f"[{name}] T5 开始：联网命令的审批请求将等待 {LONG_WAIT} 秒后才拒绝")
        t0, r = run_prompt(c, sid, "Use your shell/terminal tool to run exactly this command: curl -sI https://example.org | head -1 . Then report its output.",
                           "wait_then_reject", timeout=LONG_WAIT + 300)
        log(f"[{name}] T5 结果 {json.dumps(r.get('result', r), ensure_ascii=False)[:200]}；总耗时 {int(time.time() - t0)} 秒")
        check(c, name, "T5 长等待", t0, "HTTP/")

    kinds = {}
    for (t, d, o) in c.events:
        if d == "in" and isinstance(o, dict) and o.get("method") == "session/update":
            k = o.get("params", {}).get("update", {}).get("sessionUpdate")
            kinds[k] = kinds.get(k, 0) + 1
    log(f"[{name}] session/update 类型：{kinds}")
    c.close()
    shutil.rmtree(work, ignore_errors=True)


def main():
    quick = "--quick" in sys.argv
    only = sys.argv[sys.argv.index("--only") + 1] if "--only" in sys.argv else None
    shutil.rmtree(OUT, ignore_errors=True)
    os.makedirs(OUT)
    log(f"python {sys.version.split()[0]}；quick={quick}")
    for cmd in ("claude --version", "codex --version",
                "gemini --version", "agy --version", "agy acp --help", "node --version", "npx --version"):
        log(f"$ {cmd}\n{sh(cmd)}")
    # 登录状态的原文带邮箱等账号信息，不得落盘（AGENTS.md 安全不变量）：只记白名单字段或长度
    log(f"$ claude auth status（只留四个字段）\n{claude_auth_fields()}")
    log(f"$ codex login status（只记退出码与长度）\n{redacted_status('codex login status')}")
    if shutil.which("gemini") is None:
        log("未安装 gemini，跳过（安装：npm i -g @google/gemini-cli，然后运行 gemini 完成登录）")
    help_g = sh("gemini --help")
    gflag = "--acp" if "--acp" in help_g.replace("--experimental-acp", "") else "--experimental-acp"
    log(f"gemini ACP 参数：{gflag}")
    agents = [
        ("claude", ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]),
        ("codex", ["npx", "-y", "@zed-industries/codex-acp"]),
        ("codex-org", ["npx", "-y", "@agentclientprotocol/codex-acp"]),
        ("agy", ["agy", "acp"]),
        ("gemini", ["gemini", gflag]),
    ]
    for name, argv in agents:
        if only and name != only:
            continue
        if shutil.which(argv[0]) is None:
            continue
        try:
            test_agent(name, argv, quick)
        except Exception as e:
            log(f"[{name}] 异常：{e!r}")
    tar = os.path.join(HOME, "iwork-t0c-results.tar.gz")
    with tarfile.open(tar, "w:gz") as t:
        t.add(OUT, arcname="iwork-t0c-results")
    log(f"完成。请把 {tar} 发给 Claude。")


if __name__ == "__main__":
    main()
