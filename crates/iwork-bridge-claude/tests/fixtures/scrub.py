#!/usr/bin/env python3
"""录制样本的脱敏（T06 方案 15.4）。入库前的硬要求（supervisor 2026-11-12）：

1. 账号类的键 account、email、organization（不分大小写），不管在哪一层都删掉；解析不了、又带这几个键名的行，
   整行换成占位。与 tools/t0/iwork-t0g.py 的 `redact_event` / `redact_line` 同一套规则。
2. 列出用户本机环境的字段（init 与 initialize 响应里的插件、技能、命令、子代理、MCP、记忆路径……）换成条数，
   不留内容：它们是 `@`（`名字@市场`）与 `account`（命令说明）的来源，桥接也用不到。条数留着（隔离要看它）。
3. 家目录 → `~`，登录名 → `<user>`，主机名 → `<host>`，临时目录 → `<tmp>`；横线编码的路径（`-Users-<名字>-…`）
   靠登录名替换一起盖住。
4. 会话 id、消息 id、工具调用 id、请求 id → 固定占位（同一个 id 在一份样本里换成同一个占位，配对关系不变）。
5. 邮箱样式的串 → `<email>`；剩下的 `@` → ` at `（入库要求 `@` 零命中）。

`leak_paths()` 在脱敏后再查一遍：登录名、家目录、`account`、`@`、邮箱，只报键路径与种类，不报值。
用法：
  python3 -I scrub.py --selftest          反例自测
  python3 -I scrub.py <样本文件>…          只查不改，有残留以 1 退出（只打印键路径）
"""

import getpass
import json
import os
import re
import socket
import sys

# 与 iwork-t0g.py 同一份
ACCOUNT_KEYS = {"account", "email", "organization"}
ACCOUNT_KEY_TEXT = re.compile(r'"(?:account|email|organization)"', re.I)
REDACTED_LINE = json.dumps({"iwork_fixture_redacted": "这一行解析不了，又带账号类的键名，整行没存"}, ensure_ascii=False)

# 列出用户本机环境的字段：换成条数
ENV_LIST_KEYS = {"plugins", "skills", "slash_commands", "terminal_slash_commands", "agents", "mcp_servers",
                 "commands", "output_styles", "memory_paths", "additional_directories", "messaging_socket_path"}

UUID_RE = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
ID_RE = re.compile(r"\b(msg|toolu|req|srvtoolu|call)_[A-Za-z0-9]{6,}")
EMAIL_RE = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")


def redact_event(o):
    """去掉任意深度上的账号类键（不分大小写）；列环境的字段换成条数。"""
    if isinstance(o, dict):
        out = {}
        for k, v in o.items():
            if isinstance(k, str) and k.lower() in ACCOUNT_KEYS:
                continue
            if isinstance(k, str) and k in ENV_LIST_KEYS:
                out[k] = {"iwork_fixture_count": len(v) if isinstance(v, (list, dict)) else 1}
                continue
            out[k] = redact_event(v)
        return out
    if isinstance(o, list):
        return [redact_event(v) for v in o]
    return o


def redact_line(line):
    """解析得了：删账号键、收起环境字段后重新序列化。解析不了：带账号类键名就整行换成占位，否则原样。"""
    try:
        o = json.loads(line)
    except ValueError:
        return REDACTED_LINE if ACCOUNT_KEY_TEXT.search(line) else line
    return json.dumps(redact_event(o), ensure_ascii=False, separators=(",", ":"))


def _secrets(extra):
    """要换掉的本机串：(原文, 占位)，长的先换。"""
    home = os.path.expanduser("~")
    pairs = set()
    for path in extra:
        for p in (path, os.path.realpath(path)):
            pairs.add((p, "<tmp>"))
    for p in (home, os.path.realpath(home)):
        pairs.add((p, "~"))
    pairs.add((getpass.getuser(), "<user>"))
    host = socket.gethostname().split(".")[0]
    if len(host) >= 3:
        pairs.add((host, "<host>"))
    return sorted(pairs, key=lambda kv: -len(kv[0]))


class _Ids:
    """同一个 id 换成同一个占位。"""

    def __init__(self):
        self.map = {}
        self.counts = {}

    def _next(self, kind):
        self.counts[kind] = self.counts.get(kind, 0) + 1
        return self.counts[kind]

    def uuid(self, m):
        key = m.group(0).lower()
        if key not in self.map:
            self.map[key] = f"00000000-0000-4000-8000-{self._next('uuid'):012d}"
        return self.map[key]

    def ident(self, m):
        key = m.group(0)
        if key not in self.map:
            self.map[key] = f"{m.group(1)}_iwork{self._next(m.group(1)):04d}"
        return self.map[key]


def scrub_lines(lines, extra_secrets=()):
    pairs = _secrets(extra_secrets)
    ids = _Ids()
    out = []
    for line in lines:
        text = redact_line(line.rstrip("\n"))
        for raw, repl in pairs:
            text = text.replace(raw, repl)
        text = EMAIL_RE.sub("<email>", text)
        text = text.replace("@", " at ")
        text = UUID_RE.sub(ids.uuid, text)
        text = ID_RE.sub(ids.ident, text)
        out.append(text + "\n")
    return out


def _needles():
    home = os.path.expanduser("~")
    return {"登录名": getpass.getuser(), "家目录": home, "家目录真实路径": os.path.realpath(home),
            "account": "account", "@": "@"}


def leak_paths(lines):
    """残留出在哪：只给 JSON 键路径与命中的种类，不给值。"""
    needles = _needles()
    found = []

    def hit(text):
        low = text.lower()
        kinds = [w for w, n in needles.items() if n.lower() in low]
        if EMAIL_RE.search(text):
            kinds.append("邮箱")
        return kinds

    def walk(v, path, i):
        if isinstance(v, dict):
            for k, x in v.items():
                for kind in hit(k):
                    found.append(f"第 {i} 行 键名 {path}.{k}：{kind}")
                walk(x, f"{path}.{k}", i)
        elif isinstance(v, list):
            for j, x in enumerate(v):
                walk(x, f"{path}[{j}]", i)
        elif isinstance(v, str):
            for kind in hit(v):
                found.append(f"第 {i} 行 值 {path}：{kind}")

    for i, line in enumerate(lines, 1):
        try:
            walk(json.loads(line), "", i)
        except ValueError:
            for kind in hit(line):
                found.append(f"第 {i} 行（不是 JSON）：{kind}")
    return found


def selftest():
    user, home = getpass.getuser(), os.path.expanduser("~")
    dash = "-" + home.strip("/").replace("/", "-") + "-proj"
    raw = [
        # 1 initialize 响应：账号键在深处、大小写混着；命令说明里有 @ 与 account
        json.dumps({"type": "control_response", "response": {"response": {
            "Account": {"Email": "a@b.com", "organization": "org@x"},
            "commands": [{"name": "x", "description": "account @ thing"}], "current_permission_mode": "dontAsk"}}}),
        # 2 init：插件名带 @，路径带家目录与横线编码
        json.dumps({"type": "system", "subtype": "init", "plugins": [{"source": "p@m"}, {"source": "q@m"}],
                    "cwd": f"{home}/w", "memory_paths": {"auto": f"{home}/.claude/projects/{dash}/m"},
                    "session_id": "123e4567-e89b-42d3-a456-426614174000", "permissionMode": "dontAsk"}),
        # 3 解析不了、带账号键名：整行换掉
        '{"truncated": {"email": "x',
        # 4 解析不了、不带：原样（登录名照换）
        f"plain text {user}",
        # 5 工具调用 id 配对
        json.dumps({"message": {"id": "msg_01ABCdef123", "content": [{"type": "tool_use", "id": "toolu_01XYZabc999"}]}}),
        json.dumps({"tool_use_id": "toolu_01XYZabc999", "session_id": "123E4567-E89B-42D3-A456-426614174000",
                    "text": "mail me at someone@example.org or @here"}),
    ]
    out = scrub_lines(raw, extra_secrets=["/tmp/iwork-rec-x"])
    o1, o2, o5, o6 = (json.loads(out[i]) for i in (0, 1, 4, 5))
    checks = [
        ("account 键删掉（不分大小写）", "Account" not in o1["response"]["response"]),
        ("命令列表换成条数", o1["response"]["response"]["commands"] == {"iwork_fixture_count": 1}),
        ("对照：用得着的键留着", o1["response"]["response"]["current_permission_mode"] == "dontAsk"),
        ("插件换成条数", o2["plugins"] == {"iwork_fixture_count": 2}),
        ("家目录换掉", o2["cwd"] == "~/w"),
        ("记忆路径收起", o2["memory_paths"] == {"iwork_fixture_count": 1}),
        ("解析不了又带账号键的整行换掉", out[2].strip() == REDACTED_LINE),
        ("解析不了的普通行换登录名", out[3].strip() == "plain text <user>"),
        ("工具调用 id 配对不变", o5["message"]["content"][0]["id"] == o6["tool_use_id"] == "toolu_iwork0001"),
        ("同一个 uuid 大小写不同也是同一个占位", o2["session_id"] == o6["session_id"]),
        ("邮箱与 @ 都没了", "@" not in out[5] and "<email>" in out[5]),
        ("脱敏后查不出残留", leak_paths(out) == []),
        ("反例：没脱敏的查得出（且不带值）", leak_paths(raw) and all("a@b.com" not in f for f in leak_paths(raw))),
        ("反例：横线编码路径里的登录名查得出", any("登录名" in f for f in leak_paths([json.dumps({"d": dash})]))),
    ]
    bad = [name for name, ok in checks if not ok]
    for name, ok in checks:
        print(f"{'ok  ' if ok else 'FAIL'} {name}")
    return not bad


if __name__ == "__main__":
    if sys.argv[1:] == ["--selftest"]:
        sys.exit(0 if selftest() else 1)
    total = 0
    for path in sys.argv[1:]:
        with open(path, encoding="utf-8") as f:
            for found in leak_paths(f.readlines()):
                print(f"{path}：{found}")
                total += 1
    sys.exit(1 if total else 0)
