#!/usr/bin/env python3
"""录 Claude Code stream-json 样本（T06 方案 15.4；S5）。

用法：python3 -I record.py [--only <会话名>] <输出目录>     # 输出目录如 tests/fixtures/2.1.322；--only 只跑一个（诊断）

- 只用 Haiku（--model haiku），实例数由 SESSIONS 决定（产品负责人 2026-11-12 同意：≤ 10 个）。
- 每个会话在 mktemp 建的临时目录里跑（cwd 也在里面），录完删掉临时目录。
- 设置照选定配置（方案 3.4a）：dontAsk、PreToolUse 带 onFailure: block、SessionStart hook、沙箱开、
  autoAllowBashIfSandboxed false。录制时 allowUnsandboxedCommands 写 false（录样本用不着沙箱外这个出口）。
- hook 是同目录生成的小脚本：只放行、只拒绝登记好的无害命令，别的一律拒。
- 落盘前先过 scrub.py（account / email / organization 键、家目录、登录名、各种 id）；脱敏后再查一遍，
  有残留就不写文件、以非零退出。
- 录完照方案 5.5 的规矩删掉开过的会话在 CLI 配置目录里留下的记录：只删 `projects/<一层>/<编号>.jsonl`
  这一种普通文件，不读内容、不碰别的。删了哪几个打在 stdout。

不写死任何本机路径或用户名：claude 从 PATH 找，家目录与配置目录现取。
"""

import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import scrub  # noqa: E402  同目录

MODEL = "haiku"
WAIT = 180
ALLOWED_CMD = "echo iwork-sample-ok"
DENIED_CMD = "touch iwork-sample-denied"

HOOK_SRC = r'''#!/usr/bin/env python3
import json, os, sys
mode = os.environ.get("IWORK_REC_HOOK_MODE", "deny")
kind = sys.argv[1] if len(sys.argv) > 1 else ""
if kind == "session-start":
    sys.exit(0)
try:
    data = json.load(sys.stdin)
    cmd = (data.get("tool_input") or {}).get("command")
    allow = mode == "allow" and data.get("tool_name") == "Bash" and cmd == os.environ.get("IWORK_REC_ALLOWED")
except Exception:
    allow = False
out = {"hookSpecificOutput": {"hookEventName": "PreToolUse",
       "permissionDecision": "allow" if allow else "deny"}}
if not allow:
    out["hookSpecificOutput"]["permissionDecisionReason"] = "iWork 录制样本：没登记的调用一律拒绝"
sys.stdout.write(json.dumps(out) + "\n")
sys.stdout.flush()
sys.exit(0)
'''


def ask_bash(cmd):
    return (f"Use the Bash tool to run exactly this command, once, character for character, "
            f"and nothing else. Then reply DONE.\n{cmd}")


class Session:
    """一个流式输入的 CLI 进程；后台线程收 stdout 的每一行。"""

    def __init__(self, tmp, name, extra_args, hook_mode):
        self.name, self.lines, self.lock = name, [], threading.Lock()
        work = os.path.join(tmp, "work")
        argv = [shutil.which("claude"), "-p", "--input-format", "stream-json", "--output-format", "stream-json",
                "--verbose", "--include-partial-messages", "--setting-sources", "", "--strict-mcp-config",
                "--mcp-config", os.path.join(tmp, "mcp.json"), "--settings", os.path.join(tmp, "settings.json"),
                "--effort", "low"] + extra_args
        env = {**os.environ, "IWORK_REC_HOOK_MODE": hook_mode, "IWORK_REC_ALLOWED": ALLOWED_CMD}
        self.stderr = open(os.path.join(tmp, f"{name}.stderr"), "w")
        self.proc = subprocess.Popen(argv, cwd=work, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.stderr, text=True, env=env, start_new_session=True)
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.proc.stdout:
            with self.lock:
                self.lines.append(line)

    def events(self):
        with self.lock:
            out = []
            for line in self.lines:
                try:
                    out.append(json.loads(line))
                except ValueError:
                    pass
            return out

    def send(self, obj):
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def user(self, text):
        self.send({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}})

    def control(self, rid, request):
        self.send({"type": "control_request", "request_id": rid, "request": request})

    def wait(self, pred, count=1, timeout=WAIT):
        end = time.time() + timeout
        while time.time() < end:
            if sum(1 for o in self.events() if pred(o)) >= count:
                return True
            if self.proc.poll() is not None:
                time.sleep(0.3)
                return sum(1 for o in self.events() if pred(o)) >= count
            time.sleep(0.1)
        return False

    def results(self, n):
        return self.wait(lambda o: o.get("type") == "result", n)

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            code = self.proc.wait(timeout=60)
        except subprocess.TimeoutExpired:
            os.killpg(self.proc.pid, 9)
            code = self.proc.wait()
        self.stderr.close()
        time.sleep(0.2)
        return code


def initialize(s):
    """开会话的次序（方案 3.5）：发第一条用户消息之前先发 initialize，等它的响应。"""
    s.control("init-1", {"subtype": "initialize"})
    return s.wait(lambda o: o.get("type") == "control_response", 1, 60)


def sess_text_multiturn(s, ctx):
    initialize(s)
    s.user("Reply with exactly: ONE")
    s.results(1)
    s.user("Reply with exactly: TWO")
    s.results(2)


def sess_tool_allowed(s, ctx):
    initialize(s)
    s.user(ask_bash(ALLOWED_CMD))
    s.results(1)


def sess_tool_denied(s, ctx):
    initialize(s)
    s.user(ask_bash(DENIED_CMD))
    s.results(1)


def sess_resume(s, ctx):
    initialize(s)
    s.user("What was the first word I asked you to reply with? Reply with that one word.")
    s.results(1)


def sess_bad_model(s, ctx):
    initialize(s)
    s.user("Reply with exactly: OK")
    s.results(1)


def sess_interrupt_then_set_model(s, ctx):
    initialize(s)
    s.user("Count from 1 to 300, one number per line, nothing else.")
    s.wait(lambda o: o.get("type") == "stream_event", 5)
    s.control("c-int", {"subtype": "interrupt"})
    s.results(1)
    # 切到 Haiku 系的别名（产品负责人：不切到更贵的模型）：要的是 set_model 之后那条 content 是字符串的 user 事件（N4）
    s.control("c-model", {"subtype": "set_model", "model": MODEL})
    s.wait(lambda o: o.get("type") == "control_response" and
           (o.get("response") or {}).get("request_id") == "c-model", 1, 60)
    s.user("Reply with exactly: AFTER")
    s.results(2)


# (名字, 会话函数, 额外参数函数, hook 模式)
SESSIONS = [
    ("text_multiturn", sess_text_multiturn, lambda c: ["--model", MODEL, "--session-id", c.new_id("text_multiturn")], "deny"),
    ("tool_allowed", sess_tool_allowed, lambda c: ["--model", MODEL, "--session-id", c.new_id("tool_allowed")], "allow"),
    ("tool_denied", sess_tool_denied, lambda c: ["--model", MODEL, "--session-id", c.new_id("tool_denied")], "deny"),
    ("resume", sess_resume, lambda c: ["--model", MODEL, "--resume", c.ids["text_multiturn"]], "deny"),
    ("bad_model", sess_bad_model, lambda c: ["--model", "claude-iwork-nonexistent", "--session-id", c.new_id("bad_model")], "deny"),
    ("interrupt_set_model", sess_interrupt_then_set_model,
     lambda c: ["--model", MODEL, "--session-id", c.new_id("interrupt_set_model")], "deny"),
]


class Ctx:
    def __init__(self):
        self.ids = {}

    def new_id(self, name):
        self.ids[name] = str(uuid.uuid4())
        return self.ids[name]


def write_setup(tmp):
    os.makedirs(os.path.join(tmp, "work"))
    hook = os.path.join(tmp, "hook.py")
    with open(hook, "w") as f:
        f.write(HOOK_SRC)
    os.chmod(hook, 0o700)
    home = os.path.expanduser("~")
    cmd = f"{shutil.which('python3')} -I '{hook}'"
    settings = {
        "disableAllHooks": False,
        "permissions": {"defaultMode": "dontAsk", "disableBypassPermissionsMode": "disable"},
        "hooks": {
            "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": f"{cmd} pre-tool-use",
                                                        "timeout": 60, "onFailure": "block"}]}],
            "SessionStart": [{"hooks": [{"type": "command", "command": f"{cmd} session-start", "timeout": 30}]}],
        },
        "sandbox": {"enabled": True, "autoAllowBashIfSandboxed": False, "allowUnsandboxedCommands": False,
                    "filesystem": {"denyRead": [home], "denyWrite": [home],
                                   "allowWrite": [os.path.join(tmp, "work")]}},
    }
    with open(os.path.join(tmp, "settings.json"), "w") as f:
        json.dump(settings, f)
    with open(os.path.join(tmp, "mcp.json"), "w") as f:
        json.dump({"mcpServers": {}}, f)


def run(tmp, ctx, spec, version):
    name, body, args, hook_mode = spec
    s = Session(tmp, name, args(ctx), hook_mode)
    try:
        body(s, ctx)
    finally:
        code = s.close()
    header = {"_fixture": {"cli_version": version, "recorded": time.strftime("%Y-%m-%d"), "session": name,
                           "model": MODEL, "exit_code": code}}
    return name, [json.dumps(header, ensure_ascii=False) + "\n"] + s.lines


def config_dir():
    """CLI 的配置目录：环境里有 CLAUDE_CONFIG_DIR 用它，否则 ~/.claude（方案 5.5 第 2 条）。"""
    return os.environ.get("CLAUDE_CONFIG_DIR") or os.path.join(os.path.expanduser("~"), ".claude")


def forget(ids):
    """方案 5.5：按编号删会话记录。只看 projects/ 的直接子目录、名字恰好是 <UUID>.jsonl 的普通文件，不读内容。"""
    root = os.path.realpath(os.path.join(config_dir(), "projects"))
    done = []
    if not os.path.isdir(root):
        return done
    subdirs = [e for e in os.scandir(root) if e.is_dir(follow_symlinks=False)]
    for sid in ids:
        str(uuid.UUID(sid))  # 不是 UUID 就抛错，不往下走
        for d in subdirs:
            path = os.path.join(d.path, f"{sid}.jsonl")
            try:
                st = os.lstat(path)
            except FileNotFoundError:
                continue
            if not stat.S_ISREG(st.st_mode) or os.path.realpath(os.path.dirname(path)) != os.path.realpath(d.path) \
                    or os.path.dirname(os.path.realpath(d.path)) != root:
                print(f"拒绝删除（不是普通文件或不在 projects 下）：{sid}")
                continue
            os.unlink(path)
            done.append(sid)
    return done


def main():
    args = sys.argv[1:]
    only = None
    if len(args) == 3 and args[0] == "--only":
        only, args = args[1], args[2:]
    if len(args) != 1:
        sys.exit("用法：record.py [--only <会话名>] <输出目录>")
    out_dir = args[0]
    version = subprocess.run([shutil.which("claude"), "--version"], capture_output=True, text=True).stdout.split()[0]
    tmp = tempfile.mkdtemp(prefix="iwork-rec-")
    ctx = Ctx()
    recorded = {}
    try:
        write_setup(tmp)
        for spec in SESSIONS:
            if only is not None and spec[0] != only:
                continue
            name, lines = run(tmp, ctx, spec, version)
            recorded[name] = lines
            print(f"录完 {name}：{len(lines) - 1} 行")
        # 只报有没有，不自动补跑（实例总数有上限：产品负责人批准 12 个）
        seen = any('"rate_limit_event"' in line for lines in recorded.values() for line in lines)
        print(f"rate_limit_event {'已出现' if seen else '没出现（要另录一个，先问 supervisor）'}")
        if os.path.exists(os.path.join(tmp, "work", "iwork-sample-denied")):
            sys.exit("被拒的命令执行了：录制环境不对，样本不入库")
        cleaned = {name: scrub.scrub_lines(lines, extra_secrets=[tmp]) for name, lines in recorded.items()}
        # 原来能解析的行，脱敏后必须仍然能解析（S5 审查 2/F-1）：不能就整个不写
        broken = {name: scrub.broken_json(recorded[name], lines) for name, lines in cleaned.items()}
        if any(broken.values()):
            for name, rows in broken.items():
                if rows:
                    print(f"脱敏后 JSON 坏了 {name}：第 {rows} 行")
            sys.exit("脱敏弄坏了 JSON，不写文件")
        leaks = {name: scrub.leak_paths(lines) for name, lines in cleaned.items()}
        if any(leaks.values()):
            # 只打印键路径与种类，不打印值
            for name, found in leaks.items():
                for f in found:
                    print(f"残留 {name}：{f}")
            sys.exit("脱敏后仍有残留，不写文件")
        os.makedirs(out_dir, exist_ok=True)
        for name, lines in cleaned.items():
            with open(os.path.join(out_dir, f"{name}.jsonl"), "w") as f:
                f.writelines(lines)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
        deleted = forget(list(ctx.ids.values()))
        print(f"开过的会话编号：{sorted(ctx.ids.values())}")
        print(f"删掉的会话记录（projects/<一层>/<编号>.jsonl）：{sorted(deleted)}")


if __name__ == "__main__":
    main()
