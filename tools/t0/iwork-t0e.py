#!/usr/bin/env python3
"""iWork T0e 最后一轮实测（只用 Python 标准库；只执行无害命令，均在临时目录）。

A agy acp --sandbox：联网与工作区外写入是否被沙箱挡住；审批长等待（720 秒）
B Claude 自写适配器：PreToolUse hook 改写命令是否生效；hook 超时后是放行还是拒绝
C Codex app-server（本机 codex）：approvalPolicy=untrusted 时每个动作是否都请求审批、拒绝是否生效、
  联网越界是否请求、长等待、额度读取
用法：python3 iwork-t0e.py           （完整，约 30 分钟）
      python3 iwork-t0e.py --quick   （跳过长等待，约 6 分钟）
      python3 iwork-t0e.py --only A|B|C
结果：~/iwork-t0e-results.tar.gz
"""
import json, os, queue, shutil, subprocess, sys, tarfile, tempfile, threading, time

HOME = os.path.expanduser("~")
OUT = os.path.join(HOME, "iwork-t0e-results")
LONG_WAIT = 720
PROMPT_TIMEOUT = 240


def log(msg):
    line = f"== [{time.strftime('%H:%M:%S')}] {msg}"
    print(line, flush=True)
    with open(os.path.join(OUT, "summary.txt"), "a") as f:
        f.write(line + "\n")


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




def run_prompt(c, sid, text, policy, timeout=PROMPT_TIMEOUT):
    c.policy = policy
    t0 = time.time()
    res = c.request("session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": text}]}, timeout=timeout)
    if "error" in res and "client timeout" in str(res["error"]):
        c.notify("session/cancel", {"sessionId": sid})
    return t0, res


def ran(c, t0, marker):
    for (t, d, o) in c.events:
        if t >= t0 and d == "in" and isinstance(o, dict) and o.get("method") == "session/update":
            u = o["params"]["update"]
            if u.get("sessionUpdate") == "tool_call_update" and marker in json.dumps(u.get("rawOutput", ""), ensure_ascii=False):
                return True
    return False


# ---------------- A：agy ----------------
def part_a(quick):
    if shutil.which("agy") is None:
        return log("A 跳过：未安装 agy")
    work = tempfile.mkdtemp(prefix="iwork-t0e-agy-")
    outside = os.path.join(HOME, "iwork-t0e-outside.txt")
    if os.path.exists(outside):
        os.remove(outside)
    c = Client("agy", ["agy", "acp", "--sandbox"], work)
    c.request("initialize", {"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": True, "writeTextFile": True}, "terminal": True},
                             "clientInfo": {"name": "iwork-t0e", "version": "0.1.0"}}, timeout=180)
    new = c.request("session/new", {"cwd": work, "mcpServers": []}, timeout=120)
    if "error" in new:
        log(f"A 无法建会话：{new}"); c.close(); return
    sid = new["result"]["sessionId"]
    t0, r = run_prompt(c, sid, "Use your shell/terminal tool to run exactly: curl -sI https://example.com | head -1 . Then report the exact output or error.", "allow")
    log(f"A1 沙箱下联网（已批准）：命令输出含 HTTP/：{ran(c, t0, 'HTTP/')}；原始输出：{[json.dumps(o['params']['update'].get('rawOutput'), ensure_ascii=False)[:200] for (t,d,o) in c.events if t>=t0 and d=='in' and isinstance(o,dict) and o.get('method')=='session/update' and o['params']['update'].get('sessionUpdate')=='tool_call_update' and o['params']['update'].get('rawOutput')]}")
    t0, r = run_prompt(c, sid, f"Use your shell/terminal tool to run exactly: touch {outside} && echo touched-ok . Then report the exact output or error.", "allow")
    log(f"A2 沙箱下写工作区外（已批准）：文件被创建：{os.path.exists(outside)}；输出含 touched-ok：{ran(c, t0, 'touched-ok')}")
    if os.path.exists(outside):
        os.remove(outside)
    if not quick:
        log(f"A3 开始：审批等待 {LONG_WAIT} 秒后拒绝")
        t0, r = run_prompt(c, sid, "Use your shell/terminal tool to run exactly: echo hello-iwork-a3 . Then report its output.", "wait_then_reject", timeout=LONG_WAIT + 300)
        log(f"A3 结果：{json.dumps(r.get('result', r), ensure_ascii=False)[:200]}；耗时 {int(time.time()-t0)} 秒；命令被执行：{ran(c, t0, 'hello-iwork-a3')}")
    c.close()
    shutil.rmtree(work, ignore_errors=True)


# ---------------- B：Claude 自写适配器（-p + hook） ----------------
def claude_run(work, settings_path, prompt, name):
    t0 = time.time()
    p = subprocess.run(["claude", "-p", prompt, "--settings", settings_path, "--output-format", "stream-json", "--verbose"],
                       cwd=work, capture_output=True, text=True, timeout=600)
    open(os.path.join(OUT, f"{name}-stream.jsonl"), "w").write(p.stdout)
    open(os.path.join(OUT, f"{name}-stderr.txt"), "w").write(p.stderr)
    results = []
    for line in p.stdout.splitlines():
        try:
            o = json.loads(line)
        except Exception:
            continue
        if o.get("type") == "user":
            for x in o["message"].get("content", []) if isinstance(o["message"].get("content"), list) else []:
                if x.get("type") == "tool_result":
                    results.append(json.dumps(x.get("content"), ensure_ascii=False)[:200])
    return int(time.time() - t0), results


def part_b(quick):
    if shutil.which("claude") is None:
        return log("B 跳过：未安装 claude")
    work = tempfile.mkdtemp(prefix="iwork-t0e-claude-")
    hook1 = os.path.join(work, "hook-rewrite.sh")
    open(hook1, "w").write("#!/usr/bin/env bash\ncat > /dev/null\n"
        "echo '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"allow\",\"updatedInput\":{\"command\":\"echo REWRITTEN-by-iwork\"}}}'\n")
    os.chmod(hook1, 0o755)
    s1 = os.path.join(work, "s1.json")
    json.dump({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": hook1}]}]}}, open(s1, "w"))
    secs, res = claude_run(work, s1, "Use the Bash tool to run exactly: echo original-iwork . Then report the output.", "B1")
    log(f"B1 hook 改写命令：耗时 {secs} 秒；工具结果：{res}；改写生效：{any('REWRITTEN-by-iwork' in r for r in res)}")

    hook2 = os.path.join(work, "hook-timeout.sh")
    open(hook2, "w").write("#!/usr/bin/env bash\ncat > /dev/null\nsleep 30\n"
        "echo '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"late deny\"}}'\n")
    os.chmod(hook2, 0o755)
    s2 = os.path.join(work, "s2.json")
    json.dump({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": hook2, "timeout": 5}]}]}}, open(s2, "w"))
    secs, res = claude_run(work, s2, "Use the Bash tool to run exactly: echo hello-iwork-b2 . Then report the output.", "B2")
    log(f"B2 hook 超时（5 秒）：耗时 {secs} 秒；工具结果：{res}；超时后命令被执行（失败即放行）：{any('hello-iwork-b2' in r and 'hook' not in r.lower() for r in res)}")
    shutil.rmtree(work, ignore_errors=True)


# ---------------- C：Codex app-server ----------------
class AppServer:
    def __init__(self, cwd):
        self.trace = open(os.path.join(OUT, "codex-appserver-trace.jsonl"), "a")
        self.errf = open(os.path.join(OUT, "codex-appserver-stderr.txt"), "a")
        self.proc = subprocess.Popen(["codex", "app-server"], cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.errf, text=True, bufsize=1)
        self.next_id, self.pending, self.lock = 100, {}, threading.Lock()
        self.events, self.decision, self.approvals = [], "decline", []
        threading.Thread(target=self._reader, daemon=True).start()

    def _send(self, obj):
        self.trace.write(json.dumps({"t": time.time(), "dir": "out", "msg": obj}, ensure_ascii=False) + "\n"); self.trace.flush()
        with self.lock:
            self.proc.stdin.write(json.dumps(obj) + "\n"); self.proc.stdin.flush()

    def request(self, method, params, timeout=120):
        self.next_id += 1; rid = self.next_id; q = queue.Queue(); self.pending[rid] = q
        self._send({"id": rid, "method": method, "params": params})
        try:
            return q.get(timeout=timeout)
        except queue.Empty:
            return {"error": {"message": "client timeout"}}

    def _reader(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                o = json.loads(line)
            except Exception:
                continue
            self.events.append((time.time(), o))
            self.trace.write(json.dumps({"t": time.time(), "dir": "in", "msg": o}, ensure_ascii=False) + "\n"); self.trace.flush()
            if "method" in o and "id" in o:
                threading.Thread(target=self._on_request, args=(o,), daemon=True).start()
            elif "id" in o and o["id"] in self.pending:
                self.pending.pop(o["id"]).put(o)

    def _on_request(self, o):
        m = o["method"]
        self.approvals.append((time.time(), m))
        if m in ("item/commandExecution/requestApproval", "item/fileChange/requestApproval", "execCommandApproval", "applyPatchApproval"):
            if self.decision == "wait_then_decline":
                time.sleep(LONG_WAIT); d = "decline"
            else:
                d = self.decision
            self._send({"id": o["id"], "result": {"decision": d}})
        elif m == "item/permissions/requestApproval":
            self._send({"id": o["id"], "error": {"code": -32000, "message": "declined by test client"}})
        else:
            self._send({"id": o["id"], "error": {"code": -32601, "message": "not supported by test client"}})

    def turn(self, tid, text, decision, timeout=PROMPT_TIMEOUT):
        self.decision = decision
        t0 = time.time()
        r = self.request("turn/start", {"threadId": tid, "input": [{"type": "text", "text": text}]}, timeout=60)
        end = time.time() + timeout
        while time.time() < end:
            if any(t >= t0 and o.get("method") == "turn/completed" for (t, o) in self.events):
                break
            time.sleep(0.5)
        appr = [m for (t, m) in self.approvals if t >= t0]
        out = ""
        for (t, o) in self.events:
            if t < t0:
                continue
            p = o.get("params") or {}
            if o.get("method") == "item/commandExecution/outputDelta":
                out += json.dumps(p, ensure_ascii=False)
            elif o.get("method") == "item/completed":
                item = p.get("item") or {}
                out += str(item.get("aggregatedOutput") or "")
        return t0, r, appr, out


def part_c(quick):
    if shutil.which("codex") is None:
        return log("C 跳过：未安装 codex")
    work = tempfile.mkdtemp(prefix="iwork-t0e-codex-")
    a = AppServer(work)
    init = a.request("initialize", {"clientInfo": {"name": "iwork-t0e", "title": "iWork T0e", "version": "0.1.0"}})
    log(f"C initialize：{json.dumps(init.get('result', init), ensure_ascii=False)[:300]}")
    a._send({"method": "initialized"})
    rl = a.request("account/rateLimits/read", {})
    log(f"C 额度：{json.dumps(rl.get('result', rl), ensure_ascii=False)[:600]}")
    th = a.request("thread/start", {"cwd": work, "approvalPolicy": "untrusted", "approvalsReviewer": "user", "sandbox": "workspace-write"})
    log(f"C thread/start：{json.dumps(th.get('result', th), ensure_ascii=False)[:400]}")
    if "error" in th:
        a.proc.terminate(); return
    tid = th["result"]["thread"]["id"]
    t0, r, appr, out = a.turn(tid, "Run exactly this shell command: echo hello-iwork-c1 . Then report its output.", "decline")
    log(f"C1 执行+拒绝：审批请求 {appr}；命令输出出现：{'hello-iwork-c1' in out}")
    t0, r, appr, out = a.turn(tid, "Run exactly this shell command: echo hello-iwork-c2 . Then report its output.", "accept")
    log(f"C2 执行+允许：审批请求 {appr}；命令输出出现：{'hello-iwork-c2' in out}")
    t0, r, appr, out = a.turn(tid, "Create a file named iwork-c3.txt in the current directory containing exactly: hi", "decline")
    log(f"C3 写文件+拒绝：审批请求 {appr}；文件存在：{os.path.exists(os.path.join(work, 'iwork-c3.txt'))}")
    t0, r, appr, out = a.turn(tid, "Run exactly this shell command: curl -sI https://example.com | head -1 . Then report its output.", "decline")
    log(f"C4 联网+拒绝：审批请求 {appr}；出现 HTTP/：{'HTTP/' in out}")
    if not quick:
        log(f"C5 开始：审批等待 {LONG_WAIT} 秒后拒绝")
        t0, r, appr, out = a.turn(tid, "Run exactly this shell command: echo hello-iwork-c5 . Then report its output.", "wait_then_decline", timeout=LONG_WAIT + 300)
        log(f"C5 长等待：审批请求 {appr}；耗时 {int(time.time()-t0)} 秒；命令被执行：{'hello-iwork-c5' in out}")
    kinds = {}
    for (t, o) in a.events:
        if "method" in o and "id" not in o:
            kinds[o["method"]] = kinds.get(o["method"], 0) + 1
    log(f"C 通知类型：{kinds}")
    a.proc.terminate()
    shutil.rmtree(work, ignore_errors=True)


def main():
    quick = "--quick" in sys.argv
    only = sys.argv[sys.argv.index("--only") + 1] if "--only" in sys.argv else None
    shutil.rmtree(OUT, ignore_errors=True); os.makedirs(OUT)
    log(f"quick={quick}")
    for name, fn in (("A", part_a), ("B", part_b), ("C", part_c)):
        if only and name != only:
            continue
        try:
            fn(quick)
        except Exception as e:
            log(f"{name} 异常：{e!r}")
    tar = os.path.join(HOME, "iwork-t0e-results.tar.gz")
    with tarfile.open(tar, "w:gz") as t:
        t.add(OUT, arcname="iwork-t0e-results")
    log(f"完成。请把 {tar} 发给 Claude。")


if __name__ == "__main__":
    main()
