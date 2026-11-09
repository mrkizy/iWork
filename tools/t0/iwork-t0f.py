#!/usr/bin/env python3
"""iWork T0f：Claude Code 配置隔离实测（只用 Python 标准库；不修改你的任何配置，全部在临时目录）。

要回答的问题（需求 8.4，评审意见 ❸）：
  V0 基线：不加任何隔离参数时，会加载哪些用户配置（hook、MCP、技能、插件、权限模式、全局 CLAUDE.md）
  V1 参数屏蔽：--setting-sources project,local + --strict-mcp-config 之后，上面这些还剩什么；登录是否仍可用
  V2 按会话加载：在 V1 基础上用 --mcp-config 注入一个测试 MCP、用 --plugin-dir 加载一个测试插件（含技能），
     看它们是否出现在本会话里
  V3 专用配置目录：CLAUDE_CONFIG_DIR 指向一个空的临时目录时，是否需要重新登录；用户配置是否完全不加载

每个变体只发一句 "Reply with exactly: OK"，限 1 轮，消耗很少。
用法：python3 iwork-t0f.py
结果：~/iwork-t0f-results.tar.gz（家目录路径已替换为 ~；不包含你的 CLAUDE.md 内容，只记录是否匹配）
"""
import json, os, shutil, subprocess, sys, tarfile, tempfile, time

HOME = os.path.expanduser("~")
OUT = os.path.join(HOME, "iwork-t0f-results")
PROMPT = "Reply with exactly: OK"
MD_PROMPT = ("Is there a user-level CLAUDE.md (from ~/.claude/CLAUDE.md) in your context? "
             "Answer on one line: either NONE, or the first 8 words of its first non-empty line.")


def log(msg):
    line = f"== [{time.strftime('%H:%M:%S')}] {msg}"
    print(line, flush=True)
    with open(os.path.join(OUT, "summary.txt"), "a") as f:
        f.write(line + "\n")


def scrub(text):
    return text.replace(HOME, "~")


def run(name, extra_args, prompt=PROMPT, env=None, cwd=None):
    argv = ["claude", "-p", prompt, "--output-format", "stream-json", "--verbose", "--max-turns", "1"] + extra_args
    t0 = time.time()
    try:
        p = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, timeout=300,
                           env={**os.environ, **(env or {})})
        out, err, code = p.stdout, p.stderr, p.returncode
    except subprocess.TimeoutExpired as e:
        out, err, code = (e.stdout or ""), (e.stderr or "") + "\nTIMEOUT", -1
    open(os.path.join(OUT, f"{name}-stream.jsonl"), "w").write(scrub(out if isinstance(out, str) else out.decode()))
    open(os.path.join(OUT, f"{name}-stderr.txt"), "w").write(scrub(err if isinstance(err, str) else err.decode()))
    info = {"exit": code, "secs": int(time.time() - t0), "hooks": [], "init": None, "result": None, "text": ""}
    for line in (out if isinstance(out, str) else out.decode()).splitlines():
        try:
            o = json.loads(line)
        except Exception:
            continue
        if o.get("type") == "system" and o.get("subtype") == "hook_started":
            info["hooks"].append(o.get("hook_name"))
        elif o.get("type") == "system" and o.get("subtype") == "init":
            info["init"] = o
        elif o.get("type") == "result":
            info["result"] = {"subtype": o.get("subtype"), "is_error": o.get("is_error")}
            info["text"] = (o.get("result") or "")[:300]
    return info


def describe(name, info):
    i = info["init"] or {}
    mcp = [f"{m.get('name')}({m.get('source')},{m.get('status')})" for m in i.get("mcp_servers", [])]
    plugins = [p.get("name") for p in i.get("plugins", [])]
    skills = i.get("skills", [])
    log(f"{name}: exit={info['exit']} result={info['result']} 用时={info['secs']}s")
    log(f"{name}:   触发的 hook（V1–V3 里会有 iWork 自己注入的那个）={info['hooks'] or '无'}")
    log(f"{name}:   permissionMode={i.get('permissionMode')} apiKeySource={i.get('apiKeySource')} version={i.get('claude_code_version')}")
    log(f"{name}:   MCP({len(mcp)})={mcp}")
    log(f"{name}:   插件({len(plugins)})={plugins}")
    log(f"{name}:   技能({len(skills)})={skills[:12]}{' …' if len(skills) > 12 else ''}")
    log(f"{name}:   回复={info['text']!r}")


def md_check(name, extra_args, env=None, cwd=None):
    path = os.path.join(HOME, ".claude", "CLAUDE.md")
    if not os.path.exists(path):
        log(f"{name}: 没有 ~/.claude/CLAUDE.md，跳过全局指令检查")
        return
    first = next((l.strip() for l in open(path, encoding="utf-8", errors="ignore") if l.strip()), "")
    words = first.lstrip("#").split()[:3]
    info = run(name, extra_args, prompt=MD_PROMPT, env=env, cwd=cwd)
    reply = info["text"]
    hit = bool(words) and all(w.lower() in reply.lower() for w in words)
    log(f"{name}: 全局 CLAUDE.md 是否进入上下文 = {'是' if hit else '否'}（回复是否为 NONE：{reply.strip().upper().startswith('NONE')}；不记录内容）")


PROBE_MCP = r'''
import json, sys
for line in sys.stdin:
    try: m = json.loads(line)
    except Exception: continue
    if "id" not in m: continue
    if m.get("method") == "initialize":
        r = {"protocolVersion": m["params"].get("protocolVersion", "2025-06-18"), "capabilities": {"tools": {}},
             "serverInfo": {"name": "iwork-probe", "version": "0.1"}}
    elif m.get("method") == "tools/list":
        r = {"tools": [{"name": "iwork_probe_ping", "description": "probe", "inputSchema": {"type": "object", "properties": {}}}]}
    else:
        r = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}) + "\n"); sys.stdout.flush()
'''


def main():
    shutil.rmtree(OUT, ignore_errors=True)
    os.makedirs(OUT)
    if not shutil.which("claude"):
        log("找不到 claude 命令，停止")
        return
    work = tempfile.mkdtemp(prefix="iwork-t0f-")
    subprocess.run(["git", "init", "-q"], cwd=work)
    ver = subprocess.run(["claude", "--version"], capture_output=True, text=True).stdout.strip()
    log(f"Claude Code 版本：{ver}；工作目录：{scrub(work)}")
    helptext = subprocess.run(["claude", "--help"], capture_output=True, text=True).stdout
    open(os.path.join(OUT, "claude-help.txt"), "w").write(scrub(helptext))
    for flag in ["--setting-sources", "--strict-mcp-config", "--mcp-config", "--plugin-dir", "--settings",
                 "--disable-slash-commands", "--add-dir"]:
        log(f"参数 {flag}：{'有' if flag in helptext else '帮助里没有'}")

    # iWork 会注入的设置：一个总是放行的 PreToolUse hook（只用来确认 --settings 在屏蔽后仍生效）+ 权限模式 default
    hook = os.path.join(work, "iwork-hook.sh")
    open(hook, "w").write("#!/bin/sh\necho iwork-hook-ran >> \"$(dirname \"$0\")/hook-ran.txt\"\necho '{}'\n")
    os.chmod(hook, 0o755)
    settings = os.path.join(work, "iwork-settings.json")
    json.dump({"permissions": {"defaultMode": "default"},
               "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": hook}]}]}}, open(settings, "w"))
    empty_mcp = os.path.join(work, "empty-mcp.json")
    json.dump({"mcpServers": {}}, open(empty_mcp, "w"))
    probe_py = os.path.join(work, "probe_mcp.py")
    open(probe_py, "w").write(PROBE_MCP)
    probe_mcp = os.path.join(work, "probe-mcp.json")
    json.dump({"mcpServers": {"iwork-probe": {"command": sys.executable, "args": [probe_py]}}}, open(probe_mcp, "w"))
    plugin = os.path.join(work, "iwork-probe-plugin")
    os.makedirs(os.path.join(plugin, ".claude-plugin"))
    os.makedirs(os.path.join(plugin, "skills", "iwork-probe-skill"))
    json.dump({"name": "iwork-probe-plugin", "version": "0.1.0", "description": "iWork T0f probe"},
              open(os.path.join(plugin, ".claude-plugin", "plugin.json"), "w"))
    open(os.path.join(plugin, "skills", "iwork-probe-skill", "SKILL.md"), "w").write(
        "---\nname: iwork-probe-skill\ndescription: iWork T0f probe skill. Never use it.\n---\nDo nothing.\n")

    iso = ["--setting-sources", "project,local", "--strict-mcp-config", "--mcp-config", empty_mcp, "--settings", settings]

    log("—— V0 基线（不加隔离参数）")
    v0 = run("V0", [], cwd=work); describe("V0", v0)
    md_check("V0-md", [], cwd=work)

    log("—— V1 参数屏蔽")
    if os.path.exists(os.path.join(work, "hook-ran.txt")): os.remove(os.path.join(work, "hook-ran.txt"))
    v1 = run("V1", iso, cwd=work); describe("V1", v1)
    log(f"V1:   iWork 经 --settings 注入的 hook 是否运行 = {os.path.exists(os.path.join(work, 'hook-ran.txt'))}")
    md_check("V1-md", iso, cwd=work)

    log("—— V2 参数屏蔽 + 按会话注入测试 MCP 与测试插件")
    iso2 = ["--setting-sources", "project,local", "--strict-mcp-config", "--mcp-config", probe_mcp,
            "--settings", settings, "--plugin-dir", plugin]
    v2 = run("V2", iso2, cwd=work); describe("V2", v2)
    i2 = v2["init"] or {}
    log(f"V2:   测试 MCP 出现 = {any(m.get('name') == 'iwork-probe' for m in i2.get('mcp_servers', []))}；"
        f"测试插件出现 = {any(p.get('name') == 'iwork-probe-plugin' for p in i2.get('plugins', []))}；"
        f"测试技能出现 = {any('iwork-probe-skill' in s for s in i2.get('skills', []))}")

    log("—— V3 专用配置目录（空的临时 CLAUDE_CONFIG_DIR）")
    cfg = tempfile.mkdtemp(prefix="iwork-t0f-cfg-")
    st = subprocess.run(["claude", "auth", "status"], capture_output=True, text=True, env={**os.environ, "CLAUDE_CONFIG_DIR": cfg})
    try:
        s = json.loads(st.stdout)
        log(f"V3: auth status 退出码={st.returncode} loggedIn={s.get('loggedIn')} authMethod={s.get('authMethod')}（其余字段不记录）")
    except Exception:
        log(f"V3: auth status 退出码={st.returncode}，输出不是 JSON：{scrub(st.stdout[:200])!r} {scrub(st.stderr[:200])!r}")
    v3 = run("V3", ["--settings", settings], env={"CLAUDE_CONFIG_DIR": cfg}, cwd=work); describe("V3", v3)
    log(f"V3: 临时配置目录里生成的文件：{sorted(os.listdir(cfg))[:20]}")
    shutil.rmtree(cfg, ignore_errors=True)
    shutil.rmtree(work, ignore_errors=True)

    tar = OUT + ".tar.gz"
    with tarfile.open(tar, "w:gz") as t:
        t.add(OUT, arcname="iwork-t0f-results")
    log(f"完成，结果包：{scrub(tar)}")


if __name__ == "__main__":
    main()
