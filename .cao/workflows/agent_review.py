import hashlib
import json
import os
import re
import subprocess
from pathlib import Path

from cao_workflow import emit_output, get_inputs, step

INPUTS = {
    "task": {"type": "string", "required": True},
    "approved_plan": {"type": "string", "required": False, "default": ""},
}

CONFIG = json.loads("{\"selected_roles\":[\"code_supervisor\",\"architect\",\"developer\",\"code_reviewer\",\"ui_design_reviewer\",\"functional_reviewer\"],\"reviewers\":[\"code_reviewer\",\"ui_design_reviewer\",\"functional_reviewer\"],\"read_only_tools\":{\"code_supervisor\":[\"*\"],\"architect\":[\"@builtin\",\"fs_read\",\"fs_list\",\"fs_write\",\"@cao-mcp-server\",\"execute_bash\",\"web_fetch\"],\"code_reviewer\":[\"@builtin\",\"fs_read\",\"fs_list\",\"@cao-mcp-server\",\"execute_bash\"],\"ui_design_reviewer\":[\"@builtin\",\"fs_read\",\"fs_list\",\"@cao-mcp-server\",\"execute_bash\"],\"functional_reviewer\":[\"@builtin\",\"fs_read\",\"fs_list\",\"@cao-mcp-server\",\"execute_bash\"]},\"results_dir\":\".cao/tmp/workflow-results\",\"runtime_paths\":[\".cao/tmp/workflow-results\",\".cao/.env\",\".cao/db\",\".cao/logs\",\".cao/fifos\",\".cao/locks\",\".cao/server.lock\",\".cao/watchdog.pid\",\".cao/agent-context\",\".cao/memory\",\".cao/workflow-script-scratch\",\".cao/graph-exports\",\".cao/agent-plugin-data\",\".cao/skills\",\".cao/metrics\"],\"providers\":{\"code_supervisor\":\"claude_code\",\"architect\":\"claude_code\",\"developer\":\"claude_code\",\"code_reviewer\":\"codex\",\"ui_design_reviewer\":\"antigravity_cli\",\"functional_reviewer\":\"codex\"},\"models\":{\"code_supervisor\":\"claude-opus-5-5\",\"architect\":\"claude-fable-5-1\",\"developer\":\"claude-opus-5-5\",\"code_reviewer\":\"gpt-6-astra\",\"ui_design_reviewer\":\"gemini-3.8-flash-high\",\"functional_reviewer\":\"gpt-6.1-sol\"},\"provider_settings\":{\"code_supervisor\":{\"effort\":\"high\"},\"architect\":{\"effort\":\"high\"},\"developer\":{\"effort\":\"medium\"},\"code_reviewer\":{\"model_reasoning_effort\":\"high\"},\"functional_reviewer\":{\"model_reasoning_effort\":\"high\"}},\"severities\":[\"blocking\",\"important\",\"nit\"],\"outcome_severities\":[\"blocking\",\"important\"]}")
MAX_REVIEW_ROUNDS = 3
RESULTS_ROOT = Path(CONFIG["results_dir"])
# CAO cannot scope fs_write to one path, so read-only steps get the whole tool and
# worktree_fingerprint() checks afterwards that only the result file changed.
RESULT_WRITE_TOOL = "fs_write"
READ_ONLY_INSTRUCTION = (
    "除上面的结果文件外，不要创建、修改或删除任何文件。"
    "workflow 会在本步骤前后核对工作树，有其他改动时本步骤判为失败。"
)
# preview: how to see the change running (a preview address, how to start it, a test account), for
# the reviewers that have to look at the real thing; [] when there is nothing to run.
IMPLEMENTATION_REPORT_LIST_FIELDS = ("files_changed", "verification", "risks", "unresolved", "preview")
# The ledger's severities and the ones that need an outcome, rendered from SEVERITIES and
# OUTCOME_SEVERITIES in src/metrics/records.js. Where a finding came from is introduced_by_change.
SEVERITIES = tuple(CONFIG["severities"])
OUTCOME_SEVERITIES = tuple(CONFIG["outcome_severities"])
PREVIEW_INSTRUCTION = (
    "preview 写怎么看到改动后的界面或运行中的应用：已经在运行的预览地址、启动方式、测试账号。"
    "reviewer 要靠它看真实页面；没有界面或运行时的改动写 []。"
)
IMPLEMENTATION_REPORT_FORMAT = json.dumps(
    dict({"summary": "string"}, **{field: ["string"] for field in IMPLEMENTATION_REPORT_LIST_FIELDS}),
    ensure_ascii=False,
)


def response_path(step_id, response_format):
    run_id = os.environ.get("CAO_WORKFLOW_RUN_ID", "")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", run_id):
        raise ValueError("CAO_WORKFLOW_RUN_ID is missing or invalid")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", step_id):
        raise ValueError("workflow step ID is invalid: " + step_id)
    if response_format not in ("json", "text"):
        raise ValueError("unsupported response format: " + response_format)

    directory = Path.cwd() / RESULTS_ROOT / run_id
    directory.mkdir(parents=True, exist_ok=True)
    return directory / (step_id + (".json" if response_format == "json" else ".md"))


def response_instruction(path, response_format):
    if response_format == "json":
        return (
            "结果必须写入这个文件：" + str(path) + "。"
            "文件中只放一个合法 JSON 对象，不要代码围栏或额外说明。"
            "终端里只需简短确认文件已写好。"
        )
    return (
        "完整结果必须写入这个文件：" + str(path) + "。"
        "保留完整 Markdown 或纯文本内容。终端里只需简短确认文件已写好。"
    )


def file_digest(path):
    if path.is_symlink():
        return b"link:" + os.fsencode(os.readlink(path))
    if path.is_file():
        return hashlib.sha256(path.read_bytes()).digest()
    return b"absent"


def worktree_fingerprint():
    """Hash every tracked change and untracked file outside CAO runtime paths.

    Files ignored by git are not covered.
    """
    try:
        top = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True, check=True, text=True,
        ).stdout.strip()
        listing = subprocess.run(
            ["git", "status", "--porcelain=v1", "-z", "--untracked-files=all", "--"]
            + [":(exclude)" + path for path in CONFIG["runtime_paths"]],
            capture_output=True, check=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        raise ValueError(
            "read-only steps need a git worktree to verify they changed nothing: " + str(error)
        ) from error

    fingerprint = hashlib.sha256()
    entries = iter(listing.split(b"\0"))
    for entry in entries:
        if not entry:
            continue
        if entry[:1] in (b"R", b"C"):
            # Renames and copies carry the original path as the next entry.
            next(entries, None)
        fingerprint.update(entry + b"\0")
        fingerprint.update(file_digest(Path(top) / os.fsdecode(entry[3:])))
    return fingerprint.hexdigest()


def read_response(path, stale_mtime, replayed):
    try:
        mtime = path.stat().st_mtime_ns
        response = path.read_text(encoding="utf-8").strip()
    except OSError as error:
        raise ValueError(
            "agent did not write its workflow result file: " + str(path)
        ) from error
    # A replayed step ran nothing, so its file is the one the original run wrote.
    if not replayed and mtime == stale_mtime:
        raise ValueError("agent left a stale workflow result file unchanged: " + str(path))
    if not response:
        raise ValueError("agent wrote an empty workflow result file: " + str(path))
    return response


def run_agent(profile, prompt, step_id, response_format, recovery, read_only):
    path = response_path(step_id, response_format)
    stale_mtime = path.stat().st_mtime_ns if path.exists() else None
    prompt += "\n\n" + response_instruction(path, response_format)
    options = {}
    if read_only:
        prompt += READ_ONLY_INSTRUCTION
        options["allowed_tools"] = CONFIG["read_only_tools"][profile] + [RESULT_WRITE_TOOL]
        before = worktree_fingerprint()
    handle = step(
        CONFIG["providers"][profile],
        profile,
        prompt,
        recovery=recovery,
        step_id=step_id,
        **options,
    )
    if read_only and worktree_fingerprint() != before:
        raise ValueError(
            "read-only step " + step_id + " changed project files besides its result file"
        )
    return read_response(path, stale_mtime, handle.replayed)


def run_readonly(profile, prompt, step_id, response_format="json"):
    return run_agent(
        profile, prompt, step_id, response_format, recovery="idempotent", read_only=True
    )


def run_developer(prompt, step_id):
    return run_agent("developer", prompt, step_id, "json", recovery="manual", read_only=False)


def developer_report(raw):
    """The developer's report in ``raw``, checked and serialised again."""
    report = parse_json(raw)
    validate_implementation_report(report)
    return json.dumps(report, ensure_ascii=False)


def parse_json(text):
    candidate = text.strip()
    if candidate.startswith("```"):
        lines = candidate.splitlines()
        if lines and lines[0].startswith("```"):
            lines = lines[1:]
        if lines and lines[-1].strip() == "```":
            lines = lines[:-1]
        candidate = "\n".join(lines).strip()
    start = candidate.find("{")
    end = candidate.rfind("}")
    if start < 0 or end < start:
        raise ValueError("agent did not return a JSON object")
    return json.loads(candidate[start : end + 1])


def validate_implementation_report(report):
    if not isinstance(report, dict):
        raise ValueError("developer report must be a JSON object")
    for field in ("summary",) + IMPLEMENTATION_REPORT_LIST_FIELDS:
        if field not in report:
            raise ValueError("developer report is missing field: " + field)
    if not isinstance(report["summary"], str) or not report["summary"].strip():
        raise ValueError("developer report summary must be a non-empty string")
    for field in IMPLEMENTATION_REPORT_LIST_FIELDS:
        values = report[field]
        if not isinstance(values, list) or any(not isinstance(value, str) for value in values):
            raise ValueError("developer report field must be an array of strings: " + field)


def reviewer_unverified(payload, reviewer):
    """What ``reviewer`` says it could not check, as [{reviewer, item, reason}].

    Every reviewer writes the list, [] when it checked everything: an empty findings list says
    nothing about what the reviewer never saw (no preview, no screenshot, no sandbox).
    """
    entries = payload.get("unverified")
    if not isinstance(entries, list):
        raise ValueError("unverified must be an array; write [] when everything was checked")
    unverified = []
    for entry in entries:
        item = entry.get("item") if isinstance(entry, dict) else None
        reason = entry.get("reason") if isinstance(entry, dict) else None
        if not (isinstance(item, str) and item.strip() and isinstance(reason, str) and reason.strip()):
            raise ValueError(
                "each unverified entry needs a non-empty item and reason: " + json.dumps(entry, ensure_ascii=False)
            )
        unverified.append({"reviewer": reviewer, "item": item.strip(), "reason": reason.strip()})
    return unverified


def unverified_reason(unverified):
    """One line per entry: who could not check what, and why. The UI reviewer's is 「UI 未验证」."""
    return "；".join(
        "{} 未验证：{}（{}）".format(
            "UI" if entry["reviewer"] == "ui_design_reviewer" else entry["reviewer"], entry["item"], entry["reason"]
        )
        for entry in unverified
    )


def result(state, table, **fields):
    """Emit the workflow's output. Every exit carries prior_findings, so a stop never drops them."""
    payload = {"state": state}
    payload.update(fields)
    listed, unscreened = table.prior_listing()
    payload["prior_findings"] = listed
    if listed:
        note = "另有 {} 条改动前就有的 blocking / important，等用户决定".format(len(listed))
        if unscreened:
            note += "，其中 {} 条本轮还没经 supervisor 复核".format(unscreened)
        payload["message"] = payload.get("message", "") + note + "。"
    emit_output(payload)


def fail_closed(stage, raw_output, reason, table):
    result(
        "needs_user_decision",
        table,
        stage=stage,
        reason=reason,
        raw_output=raw_output,
        message="工作流无法安全解析 agent 结果，未继续派发写入或宣称通过。",
    )


def ask_supervisor(profile, task, instruction, step_id):
    prompt = "\n\n".join(
        [
            "你是项目 code_supervisor。只分析和协调，不直接改代码；本流程会负责调用角色，不要调用 assign 或 handoff。",
            "先读项目 AGENTS.md 和 .cao/project-prompts/code_supervisor.md。",
            "任务：\n" + task,
            instruction,
        ]
    )
    return run_readonly(profile, prompt, step_id, response_format="json")


def finding_key(finding):
    return str(finding.get("id", "")).strip()


def severity_problem(finding, source):
    """Why ``finding``'s severity cannot be read, or None when it can."""
    severity = finding.get("severity")
    if severity in SEVERITIES:
        return None
    return (
        "{} finding {} has severity {}; it must be one of {}. A problem that predates the change "
        "keeps its real severity and sets introduced_by_change to false".format(
            source, finding_key(finding) or "(no id)", json.dumps(severity, ensure_ascii=False),
            ", ".join(SEVERITIES),
        )
    )


def is_prior(finding):
    # As in records.js: only an explicit false means the problem predates the change (awaitsUser);
    # a finding that does not say where it came from blocks the change (blocksChange).
    return finding.get("severity") in OUTCOME_SEVERITIES and finding.get("introduced_by_change") is False


def valid_actionable_findings(payload, refuted_ids):
    findings = payload.get("actionable_findings")
    if not isinstance(findings, list):
        raise ValueError("supervisor result must include actionable_findings array")
    accepted = []
    seen = set()
    for finding in findings:
        if not isinstance(finding, dict):
            continue
        problem = severity_problem(finding, "supervisor actionable")
        if problem:
            raise ValueError(problem)
        if finding["severity"] not in OUTCOME_SEVERITIES:
            continue
        finding_id = finding_key(finding)
        if not finding_id or not finding.get("evidence"):
            raise ValueError("actionable finding needs a stable id and evidence")
        if is_prior(finding):
            continue
        if finding_id in refuted_ids or finding_id in seen:
            continue
        seen.add(finding_id)
        accepted.append(finding)
    return accepted


def reviewer_candidates(findings):
    """Read one reviewer's findings: (candidates, problems).

    ``candidates`` maps ID -> each blocking / important finding with a valid severity, the ones the
    round's screening decides on. ``problems`` says why the others cannot be read; the round fails
    on them once the supervisor has screened it.
    """
    candidates = {}
    problems = []
    for finding in findings:
        problem = severity_problem(finding, "reviewer")
        if problem:
            problems.append(problem)
            continue
        if finding["severity"] not in OUTCOME_SEVERITIES:
            continue
        finding_id = finding_key(finding)
        if finding_id:
            candidates.setdefault(finding_id, finding)
        elif is_prior(finding):
            # Without an ID it cannot be screened, kept across rounds or decided on by the user.
            problems.append("reviewer finding that predates the change needs a stable id: " + json.dumps(finding, ensure_ascii=False))
    return candidates, problems


def read_screening(screen, refuted_ids):
    """Check the supervisor's screening of one round and read what it concluded.

    Returns (actionable, closed_findings, copies, status): the findings it sends to the developer,
    its closed list as written, its own copy of each finding it listed as actionable (ID -> finding,
    whatever the severity and origin it gives them) and the status it closed each ID with.
    """
    actionable = valid_actionable_findings(screen, refuted_ids)
    closed_findings = screen.get("closed_findings", [])
    if not isinstance(closed_findings, list):
        raise ValueError("closed_findings must be an array")
    status = {
        finding_key(closed): closed.get("status")
        for closed in closed_findings if isinstance(closed, dict) and finding_key(closed)
    }
    copies = {
        finding_key(finding): finding
        for finding in screen["actionable_findings"] if isinstance(finding, dict) and finding_key(finding)
    }
    return actionable, closed_findings, copies, status


# Where a finding stands. Only REFUTED is final: PRIOR waits for the user and may still be sent to the
# developer; FIXED was sent and the next screening did not send it again; CLOSED is any other closing.
ACTIONABLE, PRIOR, REFUTED, FIXED, CLOSED = "actionable", "prior", "refuted", "fixed", "closed_other"


def screened_state(previous_state, reading, status, sent_to_developer):
    """Where a finding stands after a screened round, or None when the round changes nothing about it.

    In order: a refutation in an earlier round is final, so nothing later changes it. Sending it to
    the developer wins over anything else the supervisor says about it this round. Then a refutation;
    then predating (closed as pre_existing, or read as introduced_by_change false), which waits for
    the user; then any other closing. A finding the supervisor neither sends nor closes keeps the
    state it had.
    """
    if previous_state == REFUTED:
        return None
    if sent_to_developer:
        return ACTIONABLE
    if status == "refuted":
        return REFUTED
    if reading is not None and (status == "pre_existing" or is_prior(reading)):
        return PRIOR
    if status is not None:
        return CLOSED
    return None


class FindingTable:
    """Every finding the workflow has screened or is screening, one record per ID.

    A record holds the finding's latest content, its state and whether the supervisor has screened
    that content. What goes to the developer, what the reviewers are told is closed, and what the
    user is asked to decide all come from this table, so they cannot disagree.

    A screened round decides each ID it mentions; IDs it does not mention stay as they were. Until
    then, the reviewers' findings are held: their content replaces the record's, marked unscreened
    with the screened content kept as ``previous``, and the state does not change. So a stop in the
    middle of a round shows what the reviewers reported, and what was concluded before.
    """

    def __init__(self):
        self._records = {}

    def hold(self, candidates):
        for finding_id, finding in candidates.items():
            record = self._records.get(finding_id)
            if record is None:
                # A new finding has no state yet; one that predates the change is the user's to see.
                if is_prior(finding):
                    self._records[finding_id] = {"finding": finding, "state": PRIOR, "screened": False, "previous": None}
            elif record["screened"]:
                record.update(previous=record["finding"], finding=finding, screened=False)

    def apply_screening(self, candidates, copies, actionable_ids, status):
        """Record one screened round: the reviewers' ``candidates``, the supervisor's ``copies``, the
        IDs it sends to the developer and the ``status`` it closed IDs with (see read_screening)."""
        for finding_id in dict.fromkeys([*candidates, *copies, *status]):
            record = self._records.get(finding_id)
            reading = copies.get(finding_id) or candidates.get(finding_id) or (record or {}).get("finding")
            state = screened_state(
                record and record["state"], reading, status.get(finding_id), finding_id in actionable_ids
            )
            if state is None:
                if record:
                    # Nothing new was concluded: back to the content last screened.
                    if not record["screened"] and record["previous"] is not None:
                        record["finding"] = record["previous"]
                    record.update(screened=True, previous=None)
                continue
            if state == PRIOR and status.get(finding_id) == "pre_existing":
                reading = dict(reading, introduced_by_change=False)
            self._records[finding_id] = {"finding": reading, "state": state, "screened": True, "previous": None}
        for finding_id, record in self._records.items():
            if record["state"] == ACTIONABLE and finding_id not in actionable_ids:
                record["state"] = FIXED

    def findings(self, state):
        return [record["finding"] for record in self._records.values() if record["state"] == state]

    def ids(self, *states):
        return sorted(finding_id for finding_id, record in self._records.items() if record["state"] in states)

    def prior_listing(self):
        """(findings for the user, unscreened count).

        A screened finding is listed when its state is PRIOR. An unscreened one is listed also when
        this round's report says it predates the change, whatever its earlier state, unless it was
        refuted: the round has not been screened, so only the report can say. It is marked
        screened: false and carries the content last screened, if any, as previous.
        """
        listed = []
        unscreened = 0
        for record in self._records.values():
            if record["screened"]:
                if record["state"] == PRIOR:
                    listed.append(record["finding"])
                continue
            if record["state"] == REFUTED or (record["state"] != PRIOR and not is_prior(record["finding"])):
                continue
            marks = {"screened": False}
            if record["previous"] is not None:
                marks["previous"] = record["previous"]
            listed.append(dict(record["finding"], **marks))
            unscreened += 1
        return listed, unscreened


def main():
    table = FindingTable()
    inputs = get_inputs()
    task = inputs["task"].strip()
    if not task:
        result("needs_user_decision", table, stage="input", reason="task is empty")
        return

    supervisor = "code_supervisor"
    selected = set(CONFIG["selected_roles"])
    available_reviewers = [role for role in CONFIG["reviewers"] if role in selected]
    route_instruction = (
        "判断任务是否需要实现。输出 JSON："
        "{\"mode\":\"implementation\"|\"answer\",\"risk\":\"low\"|\"medium\"|\"high\","
        "\"acceptance_criteria\":[...],\"use_architect\":true|false,"
        "\"reviewers\":[role ids],\"requires_user_decision\":true|false,\"decision_reason\":\"...\","
        "\"answer\":\"...\"}。reviewers 只能从以下已创建的 reviewer 中选择："
        + json.dumps(available_reviewers, ensure_ascii=False)
        + "。"
        + ("use_architect 可用：跨模块、高风险或会建立新范例时设为 true。" if "architect" in selected
           else "本项目没有创建 architect，use_architect 必须为 false。")
    )
    # A UI designer's drafts wait for the user to pick one, which a workflow cannot do.
    has_designer = "ui_designer" in selected
    if has_designer:
        route_instruction += (
            "JSON 里再加一个字段 \"needs_design_draft\":true|false。本项目有 ui_designer。"
            "任务是新页面或明显的界面改动时，needs_design_draft 设为 true：要先出稿让用户挑，workflow 里做不了。"
            "免出稿的小改动设为 false。"
        )
    raw_route = ""
    try:
        raw_route = ask_supervisor(
            supervisor,
            task,
            route_instruction,
            "supervisor_triage",
        )
        route = parse_json(raw_route)
    except (ValueError, TypeError, json.JSONDecodeError) as error:
        fail_closed("supervisor:triage", raw_route, str(error), table)
        return
    if route.get("mode") == "answer":
        result("answered_without_changes", table, answer=route.get("answer", ""))
        return
    if route.get("mode") != "implementation":
        fail_closed("supervisor:triage", raw_route, "invalid mode", table)
        return
    if route.get("risk") not in ("low", "medium", "high"):
        fail_closed("supervisor:triage", raw_route, "risk must be low, medium, or high", table)
        return
    if not isinstance(route.get("requires_user_decision", False), bool):
        fail_closed("supervisor:triage", raw_route, "requires_user_decision must be a boolean", table)
        return
    if has_designer and not isinstance(route.get("needs_design_draft"), bool):
        fail_closed("supervisor:triage", raw_route, "needs_design_draft must be a boolean", table)
        return

    criteria = route.get("acceptance_criteria", [])
    if not isinstance(criteria, list) or not criteria:
        fail_closed("supervisor:triage", raw_route, "acceptance criteria are missing", table)
        return

    requested_reviewers = route.get("reviewers", [])
    if not isinstance(requested_reviewers, list):
        fail_closed("supervisor:triage", raw_route, "reviewers must be an array", table)
        return
    reviewers = [role for role in requested_reviewers if role in available_reviewers]
    if not reviewers:
        reviewers = available_reviewers

    # Before the architect and before approved_plan: an approved plan clears a design review, not
    # the user's pick of a draft. The UI reviewer needs no stop: it takes its basis from what the
    # project has, and with no final draft named it reviews against the acceptance criteria.
    if has_designer and route["needs_design_draft"]:
        result("needs_user_decision", table, stage="design", reason="要先出稿让用户挑，改用交互模式派 ui_designer")
        return

    approved_plan = inputs.get("approved_plan", "").strip()
    architecture = ""
    if route.get("use_architect") and "architect" in selected:
        prompt = "\n\n".join(
            [
                "先读项目 AGENTS.md 和 .cao/project-prompts/architect.md。只读分析。",
                "任务：\n" + task,
                "验收条件：\n" + json.dumps(criteria, ensure_ascii=False),
                "给出推荐方案、模块边界、风险和待用户决定的问题，最后给出实施计划：按依赖顺序列出每一步的目标、文件范围和验收方式。",
            ]
        )
        try:
            architecture = run_readonly(
                "architect", prompt, "architect_design", response_format="text"
            )
        except ValueError as error:
            fail_closed("architect_design", "", str(error), table)
            return
        if (route.get("risk") == "high" or route.get("requires_user_decision")) and not approved_plan:
            result(
                "needs_user_approval",
                table,
                risk=route.get("risk"),
                reason=route.get("decision_reason", "high-risk design needs approval"),
                architecture=architecture,
                acceptance_criteria=criteria,
                message="先审核方案，再用 approved_plan 输入重新运行工作流。",
            )
            return
    elif (route.get("requires_user_decision") or route.get("risk") == "high") and not approved_plan:
        result(
            "needs_user_decision",
            table,
            reason=route.get("decision_reason", "high-risk task needs a reviewed plan"),
            acceptance_criteria=criteria,
        )
        return

    if approved_plan:
        architecture = approved_plan

    developer_prompt = "\n\n".join(
        [
            "先读项目 AGENTS.md 和 .cao/project-prompts/developer.md。按验收条件实现，不要改范围外文件。",
            "任务：\n" + task,
            "验收条件：\n" + json.dumps(criteria, ensure_ascii=False),
            "架构方案和实施计划：\n" + (
                architecture
                or ("无；沿用项目现有结构，直接实现。" if "architect" in selected
                    else "本项目没有 architect。跨模块或高风险时先想清楚方案再实现，方案要点写进 summary。")
            ),
            "完成后的 JSON 报告格式（每个字段都必填）：" + IMPLEMENTATION_REPORT_FORMAT,
            PREVIEW_INSTRUCTION,
        ]
    )
    raw_developer = ""
    try:
        raw_developer = run_developer(developer_prompt, "developer_initial")
        raw_implementation = developer_report(raw_developer)
    except (ValueError, TypeError, json.JSONDecodeError) as error:
        fail_closed("developer_initial", raw_developer, str(error), table)
        return

    history = []
    for round_number in range(1, MAX_REVIEW_ROUNDS + 1):
        round_findings = []
        round_unverified = []
        candidates = {}
        problems = []
        for reviewer in reviewers:
            prompt = "\n\n".join(
                [
                    "先读项目 AGENTS.md、.cao/project-prompts/review-standards.md 和对应 reviewer 项目提示词。只读审查。",
                    "任务：\n" + task,
                    "验收条件：\n" + json.dumps(criteria, ensure_ascii=False),
                    "本轮实现报告：\n" + raw_implementation,
                    "要看真实页面或运行中的应用时，从实现报告的 preview 取预览地址或启动方式。没看到、没跑成的页面、状态、交互或验收条件写进 unverified，写明缺什么；workflow 不会把有未验证项的审查算作通过。",
                    "已关闭 finding ID（不得重复）：\n" + json.dumps(table.ids(REFUTED, CLOSED)),
                    "结果 JSON 格式：{\"findings\":[{\"id\":\"...\",\"severity\":\"" + "|".join(SEVERITIES) + "\",\"confidence\":\"confirmed|plausible\",\"summary\":\"...\",\"failure_scenario\":\"什么输入或状态得到什么错误结果\",\"evidence\":\"...\",\"location\":\"file:line or reproduction\",\"impact\":\"...\",\"introduced_by_change\":true|false}],\"unverified\":[{\"item\":\"没验证的页面、状态、交互或验收条件\",\"reason\":\"为什么没验证、缺什么\"}]}。没有问题时 findings=[]；都验证了时 unverified=[]。",
                ]
            )
            raw_review = ""
            try:
                raw_review = run_readonly(
                    reviewer,
                    prompt,
                    "review_{}_{}".format(reviewer, round_number),
                    response_format="json",
                )
                review_payload = parse_json(raw_review)
            except (ValueError, json.JSONDecodeError) as error:
                fail_closed("review_{}_{}".format(reviewer, round_number), raw_review, str(error), table)
                return
            findings = review_payload.get("findings")
            if not isinstance(findings, list):
                fail_closed("review_{}_{}".format(reviewer, round_number), raw_review, "findings must be an array", table)
                return
            try:
                round_unverified.extend(reviewer_unverified(review_payload, reviewer))
            except ValueError as error:
                fail_closed("review_{}_{}".format(reviewer, round_number), raw_review, str(error), table)
                return
            findings = [finding for finding in findings if isinstance(finding, dict)]
            found, unreadable = reviewer_candidates(findings)
            for finding_id, finding in found.items():
                candidates.setdefault(finding_id, finding)
            problems.extend(unreadable)
            # Held before anything else in the round can fail, so every exit shows them.
            table.hold(found)
            round_findings.extend(findings)

        screening_prompt = "\n\n".join(
            [
                "你是 code_supervisor。核对 findings 是否由本次改动引入，去重，并逐条复核 blocking / important：打开引用的代码行，判为成立、可能成立或不成立。可能成立的照样保留；只有代码不是这么写、别处已经处理或只是风格时才判不成立，不要只因为「依赖运行时状态」就否掉。不要扩大任务范围。",
                "任务：\n" + task,
                "验收条件：\n" + json.dumps(criteria, ensure_ascii=False),
                "本轮 reviewer findings：\n" + json.dumps(round_findings, ensure_ascii=False),
                "本轮 reviewer 没验证的项（不用你筛，workflow 会交给用户，不算审过）：\n" + json.dumps(round_unverified, ensure_ascii=False),
                "已关闭 ID：\n" + json.dumps(table.ids(REFUTED, CLOSED)),
                "结果 JSON 格式：{\"actionable_findings\":[...],\"closed_findings\":[{\"id\":\"...\",\"status\":\"nit|pre_existing|refuted\",\"reason\":\"...\"}]}。actionable_findings 只含成立或可能成立的 blocking / important 项；refuted 的 reason 要写明证据所在的 file:line。改动前就有的 blocking / important 不派给 developer，工作流会把它们列给用户决定；不成立的照样判 refuted。",
            ]
        )
        raw_screen = ""
        try:
            raw_screen = ask_supervisor(
                supervisor,
                task,
                screening_prompt,
                "supervisor_screen_{}".format(round_number),
            )
            screen = parse_json(raw_screen)
            if problems:
                raise ValueError("; ".join(problems))
            sent, closed_findings, copies, status = read_screening(screen, table.ids(REFUTED))
        except (ValueError, TypeError, json.JSONDecodeError) as error:
            fail_closed("supervisor_screen_{}".format(round_number), raw_screen, str(error), table)
            return
        table.apply_screening(candidates, copies, {finding_key(finding) for finding in sent}, status)
        actionable = table.findings(ACTIONABLE)

        history.append({
            "round": round_number,
            "reviewers": reviewers,
            "actionable_findings": actionable,
            "closed_findings": closed_findings,
            "unverified": round_unverified,
        })

        if not actionable and round_unverified:
            result(
                "needs_user_decision",
                table,
                stage="review_unverified",
                reason=unverified_reason(round_unverified),
                unverified=round_unverified,
                task=task,
                risk=route.get("risk", "unknown"),
                acceptance_criteria=criteria,
                implementation=raw_implementation,
                review_history=history,
                closed_findings=table.ids(REFUTED, CLOSED),
                message="没有开放的 blocking 或 important finding，但有 reviewer 没验证的项，不能算审过。",
            )
            return

        if not actionable:
            state = "reviewed" if reviewers else "implemented_unreviewed"
            result(
                state,
                table,
                task=task,
                risk=route.get("risk", "unknown"),
                acceptance_criteria=criteria,
                implementation=raw_implementation,
                review_history=history,
                closed_findings=table.ids(REFUTED, CLOSED),
                message="没有开放的 blocking 或 important finding。",
            )
            return

        if round_number == MAX_REVIEW_ROUNDS:
            reassessor = "architect" if "architect" in selected else supervisor
            replan_prompt = "\n\n".join(
                [
                    "重新审视当前方案为何未能收敛。",
                    "任务：\n" + task,
                    "验收条件：\n" + json.dumps(criteria, ensure_ascii=False),
                    "实现报告：\n" + raw_implementation,
                    "全部开放 finding：\n" + json.dumps(actionable, ensure_ascii=False),
                    "给出根因、替代方案和建议给用户决定的事项。",
                ]
            )
            try:
                reassessment = run_readonly(
                    reassessor,
                    replan_prompt,
                    "reassess_after_round_3",
                    response_format="text",
                )
            except ValueError as error:
                fail_closed("reassess_after_round_3", "", str(error), table)
                return
            result(
                "needs_user_decision",
                table,
                task=task,
                risk=route.get("risk", "unknown"),
                open_findings=actionable,
                unverified=round_unverified,
                review_history=history,
                plan_reassessment=reassessment,
                message="三轮后仍有开放 finding，工作流已停止自动返工。",
            )
            return

        fix_prompt = "\n\n".join(
            [
                "先读项目 AGENTS.md 和 .cao/project-prompts/developer.md。只修下面的开放 finding 和同类实例，不扩大范围。",
                "任务：\n" + task,
                "验收条件：\n" + json.dumps(criteria, ensure_ascii=False),
                "开放 finding：\n" + json.dumps(actionable, ensure_ascii=False),
                "修复后的 JSON 报告格式（每个字段都必填）：" + IMPLEMENTATION_REPORT_FORMAT,
                PREVIEW_INSTRUCTION,
            ]
        )
        raw_developer = ""
        try:
            raw_developer = run_developer(
                fix_prompt,
                "developer_fix_{}".format(round_number),
            )
            raw_implementation = developer_report(raw_developer)
        except (ValueError, TypeError, json.JSONDecodeError) as error:
            fail_closed("developer_fix_{}".format(round_number), raw_developer, str(error), table)
            return


main()
