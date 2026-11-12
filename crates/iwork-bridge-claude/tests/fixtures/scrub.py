#!/usr/bin/env python3
"""录制样本的脱敏（T06 方案 15.4）。入库前的硬要求（supervisor 2026-11-12）：

1. 账号类的键 account、email、organization（不分大小写），不管在哪一层都删掉；解析不了、又带这几个键名的行，
   整行换成占位。与 tools/t0/iwork-t0g.py 的 `redact_event` / `redact_line` 同一套规则。
2. 列出用户本机环境的字段（init 与 initialize 响应里的插件、技能、命令、子代理、MCP、记忆路径……）换成条数，
   不留内容：它们是 `@`（`名字@市场`）与 `account`（命令说明）的来源，桥接也用不到。条数留着（隔离要看它）。
3. 家目录 → `~`，登录名 → `<user>`，主机名 → `<host>`；临时目录 → `<tmp>`：录制脚本传进来的那一个，加上
   通用的临时根（`/tmp/…`、`/private/tmp/…`、`/var/folders/…`、`/private/var/folders/…`，CLI 自己建的临时文件就在
   这些地方），整段路径换掉；横线编码的路径（`-Users-<名字>-…`）靠登录名替换一起盖住。
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
REDACTED_LINE = json.dumps({"iwork_fixture_redacted": "这一行解析不了，又带账号类的键名，整行没存"},
                           ensure_ascii=False, separators=(",", ":"))

# 列出用户本机环境的字段：换成条数
ENV_LIST_KEYS = {"plugins", "skills", "slash_commands", "terminal_slash_commands", "agents", "mcp_servers",
                 "commands", "output_styles", "memory_paths", "additional_directories", "messaging_socket_path"}

UUID_PLACEHOLDER = "00000000-0000-4000-8000-"
UUID_RE = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
ID_RE = re.compile(r"\b(msg|toolu|req|srvtoolu|call)_[A-Za-z0-9]{6,}")
# 通用的临时根：整段路径（到引号、空白为止）换成 <tmp>
TEMP_PATH_RE = re.compile(r"(?:/private)?/(?:tmp|var/folders)/[^\s\"'<>]*")
EMAIL_RE = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")


def redact_event(o):
    """去掉任意深度上的账号类键（不分大小写）；列环境的字段换成条数。"""
    if isinstance(o, dict):
        out = {}
        for k, v in o.items():
            if isinstance(k, str) and k.lower() in ACCOUNT_KEYS:
                continue
            if isinstance(k, str) and k in ENV_LIST_KEYS:
                collapsed = isinstance(v, dict) and list(v) == ["iwork_fixture_count"]
                out[k] = v if collapsed else {"iwork_fixture_count": len(v) if isinstance(v, (list, dict)) else 1}
                continue
            out[k] = redact_event(v)
        return out
    if isinstance(o, list):
        return [redact_event(v) for v in o]
    return o


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
        if key.startswith(UUID_PLACEHOLDER):
            return m.group(0)  # 已经是占位：再过一遍不变
        if key not in self.map:
            self.map[key] = f"{UUID_PLACEHOLDER}{self._next('uuid'):012d}"
        return self.map[key]

    def ident(self, m):
        key = m.group(0)
        if key.startswith(f"{m.group(1)}_iwork"):
            return key  # 已经是占位
        if key not in self.map:
            self.map[key] = f"{m.group(1)}_iwork{self._next(m.group(1)):04d}"
        return self.map[key]


def _clean_text(text, pairs, ids):
    """一段文字里的本机串、临时路径、邮箱、`@`、各种 id 换成占位。只对**解码后的**字符串用：在 JSON 原文上
    替换会把转义（结束引号前的反斜杠）一起吞掉，弄坏 JSON（S5 审查 2/F-1）。"""
    for raw, repl in pairs:
        text = text.replace(raw, repl)
    text = TEMP_PATH_RE.sub("<tmp>", text)
    text = EMAIL_RE.sub("<email>", text)
    text = text.replace("@", " at ")
    text = UUID_RE.sub(ids.uuid, text)
    text = ID_RE.sub(ids.ident, text)
    return text


def _clean_value(v, clean):
    """键和字符串值都过一遍 `clean`，别的原样。"""
    if isinstance(v, dict):
        return {clean(k): _clean_value(x, clean) for k, x in v.items()}
    if isinstance(v, list):
        return [_clean_value(x, clean) for x in v]
    if isinstance(v, str):
        return clean(v)
    return v


def scrub_lines(lines, extra_secrets=()):
    """能解析的行：删账号键、收起环境字段，在解码后的键和字符串值上换占位，再序列化。
    解析不了的行：带账号类键名就整行换成占位，否则按纯文字换占位。"""
    pairs = _secrets(extra_secrets)
    ids = _Ids()

    def clean(text):
        return _clean_text(text, pairs, ids)

    out = []
    for line in lines:
        raw = line.rstrip("\n")
        try:
            o = json.loads(raw)
        except ValueError:
            text = REDACTED_LINE if ACCOUNT_KEY_TEXT.search(raw) else clean(raw)
        else:
            text = json.dumps(_clean_value(redact_event(o), clean), ensure_ascii=False, separators=(",", ":"))
        out.append(text + "\n")
    return out


def broken_json(before, after):
    """原来能解析、脱敏后解析不了的行号（从 1 数）。落盘前必须为空。"""
    broken = []
    for i, (b, a) in enumerate(zip(before, after), 1):
        try:
            json.loads(b)
        except ValueError:
            continue
        try:
            json.loads(a)
        except ValueError:
            broken.append(i)
    if len(before) != len(after):
        broken.append(-1)
    return broken


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
        if TEMP_PATH_RE.search(text):
            kinds.append("临时路径")
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


def _properties(cases=500, seed=20261112):
    """两条性质（固定种子的随机生成）：① 合法 JSON 脱敏后仍然合法；② 不含任何占位对象（本机串、临时根、邮箱、
    `@`、UUID、id、账号键、环境字段）的文本，脱敏前后完全一样。返回（弄坏的例子，变了的例子）。"""
    import random
    rnd = random.Random(seed)
    alphabet = 'ab xyz/\\"\'\n\t{}[]:,é漢\u2028-_.0'
    temp_bits = ["/tmp/a b/c", "/private/tmp/q\"r", "/var/folders/x/T/y"]

    def text(with_paths):
        t = "".join(rnd.choice(alphabet) for _ in range(rnd.randint(0, 12)))
        if with_paths and rnd.random() < 0.5:
            t += rnd.choice(temp_bits) + rnd.choice(['"', "'", " ", "", "\\"])
        return t

    def value(depth, with_paths):
        r = rnd.random()
        if depth > 3 or r < 0.4:
            return rnd.choice([text(with_paths), rnd.randint(-5, 5), None, True, 1.5])
        if r < 0.7:
            return [value(depth + 1, with_paths) for _ in range(rnd.randint(0, 3))]
        return {text(with_paths): value(depth + 1, with_paths) for _ in range(rnd.randint(0, 3))}

    broken, changed = [], []
    for _ in range(cases):
        doc = value(0, True)
        line = json.dumps(doc, ensure_ascii=False)
        if broken_json([line], scrub_lines([line])):
            broken.append(line)
        plain = value(0, False)
        flat = json.dumps(plain, ensure_ascii=False)
        if json.loads(scrub_lines([flat])[0]) != plain:
            changed.append(flat)
    return broken[:3], changed[:3]


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
        # 4b CLI 自己建的临时文件路径（不是录制脚本传进来的那个根）
        json.dumps({"output_file": "/private/var/folders/2r/abc/T/claude-1/tasks/x.output",
                    "note": "see /tmp/claude-501/scratch/y.txt and /private/tmp/z"}),
        # 5 工具调用 id 配对
        json.dumps({"message": {"id": "msg_01ABCdef123", "content": [{"type": "tool_use", "id": "toolu_01XYZabc999"}]}}),
        json.dumps({"tool_use_id": "toolu_01XYZabc999", "session_id": "123E4567-E89B-42D3-A456-426614174000",
                    "text": "mail me at someone@example.org or @here"}),
    ]
    out = scrub_lines(raw, extra_secrets=["/tmp/iwork-rec-x"])
    one = lambda o: json.loads(scrub_lines([json.dumps(o)])[0])  # noqa: E731
    quoted = one({"type": "user", "message": {"content": 'saved to "/private/tmp/claude-501/out.txt" ok'}})
    keyed = one({"/private/tmp/x/y.txt": 1, "plain": "x"})
    escaped = one({"t": 'a\\b\n\t"c" é \u2028', "w": "C:\\dir\\f.txt"})
    prop_broken, prop_changed = _properties()
    o1, o2, o4b, o5, o6 = (json.loads(out[i]) for i in (0, 1, 4, 5, 6))
    checks = [
        ("account 键删掉（不分大小写）", "Account" not in o1["response"]["response"]),
        ("命令列表换成条数", o1["response"]["response"]["commands"] == {"iwork_fixture_count": 1}),
        ("对照：用得着的键留着", o1["response"]["response"]["current_permission_mode"] == "dontAsk"),
        ("插件换成条数", o2["plugins"] == {"iwork_fixture_count": 2}),
        ("家目录换掉", o2["cwd"] == "~/w"),
        ("记忆路径收起", o2["memory_paths"] == {"iwork_fixture_count": 1}),
        ("解析不了又带账号键的整行换掉", out[2].strip() == REDACTED_LINE),
        ("解析不了的普通行换登录名", out[3].strip() == "plain text <user>"),
        ("通用临时根整段换掉", o4b == {"output_file": "<tmp>", "note": "see <tmp> and <tmp>"}),
        ("对照：不是临时根的路径不动", scrub_lines(['{"p":"/usr/tmp-like/x"}'])[0].strip() == '{"p":"/usr/tmp-like/x"}'),
        ("反例：没脱敏的临时路径查得出", any("临时路径" in f for f in leak_paths([raw[4]]))),
        ("幂等：再过一遍不变", scrub_lines(out) == out),
        ("带引号的临时路径：JSON 仍然合法、只换路径", quoted == {"type": "user", "message": {"content": 'saved to "<tmp>" ok'}}),
        ("键里的临时路径换掉", keyed == {"<tmp>": 1, "plain": "x"}),
        ("转义字符原样", escaped == {"t": 'a\\b\n\t"c" é \u2028', "w": "C:\\dir\\f.txt"}),
        ("broken_json 查得出弄坏的行（反例）", broken_json(['{"a":1}', "x"], ['{"a":', "y"]) == [1]),
        ("性质：合法 JSON 脱敏后仍然合法", not prop_broken),
        ("性质：路径以外的文本不变", not prop_changed),
        ("工具调用 id 配对不变", o5["message"]["content"][0]["id"] == o6["tool_use_id"] == "toolu_iwork0001"),
        ("同一个 uuid 大小写不同也是同一个占位", o2["session_id"] == o6["session_id"]),
        ("邮箱与 @ 都没了", "@" not in out[6] and "<email>" in out[6]),
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
