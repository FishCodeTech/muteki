"""Minimal Pi-backed Reason adapter for CTF runs."""

from __future__ import annotations

import asyncio
import hashlib
import json
import math
import os
import shutil
import signal
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from muteki.core.cost import CostController
    from muteki.solver.cli_engines.types import CliResult

from muteki.solver.cli_engines.engines.pi import PiDriver
from muteki.solver.cli_launch_check import check_process_launch
from muteki.solver.credential_accounts import runtime_env_for_engine
from muteki.solver.ctf_fgs import (
    cancelled_reopen_rows,
    stale_reopen_from_cancelled,
    stale_reopen_without_newer_fact,
)
from muteki.solver.reason import (
    Intent,
    PlannerFailure,
    PlannerFailureKind,
    ReasonDiagnostics,
    ReasonResult,
    VERDICT_COMPLETE,
    VERDICT_EXPLORE,
)


CTF_PI_DECIDE_SYSTEM = """在 Fact-Goal-Step 图上做面向目标的判断,不做任何执行。

下方 user 消息里是整张图的 YAML：facts 是已确认的客观事实，observations 是未获 Fact 身份的原始记录，deadEnds 是有边界的排除，resources 是已登记的文件资源，steps 是探索方向，goals 是达成标准。Observation 不能作为 from 引用；先读懂全图和结果，再判断。

判断两件事:
1. 已有事实是否已满足目标。满足就 satisfy_goal 置达成,引用支撑事实的 id,并在 reason 写清为何这些事实足以证明。
2. 未满足则据当前事实决定下一步。只规划眼前这一步,简单的通过 Step 铺开方向去并行,后续随新事实每轮再定,不预先铺开整条路线。

step:
- action 一句话点明眼下要取的信息，不写分步剧本；from 只引用真实 Fact id。
- expected_observable 说明可检验的预期结果；stop_condition 写有界停止边界；coverage_key 标识当前覆盖的问题，不拿不同 key 包装同一方向。
- 后继必须使用某个已登记 PoC 的精确版本时，在 requires 写该 Resource id；普通方向省略 requires。
- Goal 未满足且没有仍在途的 Step 时，至少开一个可执行 Step；已有工作在途时可以零个新 Step，不为填满槽位制造同义方向。

纪律:
- 目标分两类,判定方式不同:能出示见证的(拿到 flag、找到那个漏洞),见证摆在图上即达成;只能论证覆盖的(全面排查、普查),看 sub 覆盖是否齐、是否还有未探线索,不要去等一个能"证明穷尽"的产物。
- 只依据图中已确认的 Fact 判断目标是否满足；Observation 和 DeadEnd 帮助选择下一步，不冒充已证明的结论。
- 无 open step 时(冷启动或管道空转),开多条相邻但互补的侦察方向快速起量;已有事实可据分化时,各 step 覆盖不同维度、不重叠。互补指要取的信息不同,不是同一面换一种手段。已有范围内的阴性事实之后,换信息面或 drop;没有新的范围或上下文(新入口、新凭据)就不要加大同一问法。图上已写清怎么接着用、怎么确认还活着的入口或凭据,必须变成当前 step 去取其上的新信息,或 drop_step 并说明为什么现在不用;不要只写进摘要、继续在旧面上加手段。
- 方向失效用 drop_step 并说明原因。
- 汇总/报告类 step(把多个事实并成一份交付物)放到最后:仅当没有其他实质探索方向在途或待开时才开,避免边探边反复重写同一份产物。
- 可以质疑图上事实的可靠性,可以重新规划 step 重新尝试某些疑点。

图操作先记入草稿,最后必须调用 commit 一次性提交(不调用则本轮作废)。操作简单、合法性一眼可辨时直接 commit;仅当批次含判达成、退回或跨多事实等可能被剔除的操作时,先用 preview 复核叠加草稿后的投影(只出 step/goal 结构与事实标题,校验不过的条目会标注、提交时剔除)。"""

PENTEST_PI_DECIDE_SYSTEM = CTF_PI_DECIDE_SYSTEM + """

本任务的授权测试边界见 engagement。公开资料可以帮助推理，但不能替代目标环境的工具证据。你负责判断 Fact 是否已经满足用户原始测试目标；满足时用 satisfy_goal 的 from 引用支撑结论的 Fact，并在 reason 解释。若证据不足，继续规划当前最有信息增益的 Step。是否需要复核、复测或扩大授权范围内的探索，由你依据事实决定。

规划 Pentest Step 时，一条 Step 聚焦一个可独立核验的证据缺口；若指向潜在漏洞，让预期证据足以支持或排除一份独立漏洞报告。避免在一条 Step 中混合多个不同漏洞假设，也不把同一成因拆成多份报告。登录、会话等共用前置证据取得后，通过共享图中的 Fact 及其 sourceArtifacts 复用；后续 Step 的 from 引用相关 Fact，必须精确复用已登记 PoC 时才在 requires 引用 Resource。仅当证据不足、失效或目标上下文变化时再复核这些前置条件。Step 的具体划分和并行方向仍由你根据当前事实、信息增益与授权边界决定。

终局报告另由模型从共享图生成，不需要为撰写报告创建 Worker Step。"""
PENTEST_PI_DECIDE_SYSTEM += "\n当 engagement.reportGoalMode=count 时，宿主依据有证据且经过审阅的独立报告数量停止；不要用 satisfy_goal 抢先结束。继续规划有信息增益的漏洞验证 Step，直到 reportsAccepted 达到 reportsExpected。报告由 Worker 随每个 Step 实时提交；审阅中的报告仍可在共享图看到。"

_TOOLS = "open_step,drop_step,change_step_priority,satisfy_goal,preview,commit"
_REPO_ROOT = Path(__file__).resolve().parents[2]
_EXTENSION = Path(__file__).with_name("pi_decide_extension.ts")


def build_ctf_pi_decide_prompt(
    graph_summary: str,
    *,
    max_intents: int = 4,
    mode: str = "ctf",
) -> list[dict[str, str]]:
    """Build the same two-message prompt used by the Pi subprocess."""
    del max_intents
    return [
        {"role": "system", "content": PENTEST_PI_DECIDE_SYSTEM if mode == "pentest" else CTF_PI_DECIDE_SYSTEM},
        {"role": "user", "content": str(graph_summary)},
    ]


def _failure(
    kind: PlannerFailureKind,
    detail: str,
    *,
    status: str,
    timed_out: bool = False,
    raw_response: str = "",
) -> ReasonResult:
    return ReasonResult(
        goal_met=False,
        intents=[],
        audit_notes=[],
        semantic_dedupe_available=False,
        planner_failure=PlannerFailure(kind, detail),
        diagnostics=ReasonDiagnostics(
            response_status=status,
            timed_out=timed_out,
            finish_reason="timeout" if timed_out else "error",
            raw_response=raw_response,
            raw_response_sha256=hashlib.sha256(
                raw_response.encode("utf-8", errors="replace")
            ).hexdigest(),
            response_chars=len(raw_response),
            parse_status="not_run_timeout" if timed_out else "not_run_error",
            parse_detail=detail,
        ),
    )


def _positive_fact_ids(
    value: Any, *, fact_id_map: dict[str, int] | None = None,
) -> list[int]:
    if not isinstance(value, list):
        return []
    result: list[int] = []
    id_map = fact_id_map or {}
    for item in value:
        text = str(item or "").strip()
        if text in id_map:
            fact_id = int(id_map[text])
        else:
            try:
                fact_id = int(item)
            except (TypeError, ValueError):
                continue
        if fact_id > 0 and fact_id not in result:
            result.append(fact_id)
    return result


@dataclass
class DraftSimulation:
    intents: list[Intent] = field(default_factory=list)
    supersede: list[str] = field(default_factory=list)
    drop_whys: list[str] = field(default_factory=list)
    reprioritize: dict[str, str] = field(default_factory=dict)
    parse_rejections: list[dict[str, Any]] = field(default_factory=list)
    receipts: list[dict[str, Any]] = field(default_factory=list)
    goal_met: bool = False
    complete_why: str = ""
    goal_evidence_facts: list[int] = field(default_factory=list)
    progress_summary: str = ""
    raw_open_steps: int = 0
    draft_id: str = ""


def _yaml_scalar(line: str) -> str:
    value = line.split(":", 1)[1].strip() if ":" in line else ""
    if not value:
        return ""
    if value[0] in "\"'":
        try:
            return str(json.loads(value))
        except json.JSONDecodeError:
            return value.strip().strip("\"'")
    return value


def _parse_preview_goals(yaml_text: str) -> list[dict[str, str]]:
    goals: list[dict[str, str]] = []
    current: dict[str, str] | None = None
    in_goals = False
    for raw in str(yaml_text or "").splitlines():
        line = raw.rstrip()
        if line.startswith("goals:"):
            in_goals = True
            continue
        if in_goals and (
            line.startswith("steps:")
            or line.startswith("facts:")
            or line.startswith("findings:")
        ):
            break
        if not in_goals:
            continue
        if line.startswith("  - id:"):
            if current:
                goals.append(current)
            current = {
                "id": _yaml_scalar(line),
                "title": "",
                "state": "open",
            }
        elif current is not None and line.startswith("    criterion:"):
            current["title"] = _yaml_scalar(line)
    if current:
        goals.append(current)
    return goals or [{
        "id": "goal_final",
        "title": "goal",
        "state": "open",
    }]


def _parse_preview_open_steps(yaml_text: str) -> list[dict[str, Any]]:
    steps: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    in_steps = False
    in_from = False

    def _flush() -> None:
        nonlocal current
        if current and str(current.get("state") or "open") == "open":
            steps.append(current)
        current = None

    for raw in str(yaml_text or "").splitlines():
        line = raw.rstrip()
        if line.startswith("steps:"):
            in_steps = True
            continue
        if not in_steps:
            continue
        if line.startswith("  - id:"):
            _flush()
            current = {
                "id": _yaml_scalar(line),
                "action": "",
                "priority": "normal",
                "state": "open",
                "from": [],
            }
            in_from = False
            continue
        if current is None:
            continue
        if line.startswith("    action:"):
            current["action"] = _yaml_scalar(line)
            in_from = False
        elif line.strip() == "from:":
            in_from = True
        elif in_from and line.startswith("      - "):
            current["from"].append(line.strip()[2:].strip())
        elif in_from and line.strip() == "[]":
            in_from = False
        elif line.startswith("    priority:"):
            current["priority"] = _yaml_scalar(line) or "normal"
            in_from = False
        elif line.startswith("    state:"):
            current["state"] = _yaml_scalar(line) or "open"
            in_from = False
        elif line.startswith("    ") and not line.startswith("      "):
            in_from = False
    _flush()
    return steps


def _fact_catalog(shared_graph: Any) -> tuple[dict[str, int], list[dict[str, Any]], int]:
    fact_id_map: dict[str, int] = {}
    facts: list[dict[str, Any]] = []
    watermark = 0
    if shared_graph is None:
        return fact_id_map, facts, watermark
    next_fact_index = 0
    try:
        events = shared_graph.events()
    except Exception:
        return fact_id_map, facts, watermark
    for event in events:
        try:
            seq = int(event.get("seq") or 0)
        except (TypeError, ValueError):
            seq = 0
        watermark = max(watermark, seq)
        if str(event.get("kind") or "") != "fact_added":
            continue
        payload = event.get("payload") or {}
        text = str(payload.get("fact") or "").strip()
        source = str(payload.get("source") or "")
        if source == "origin":
            fact_id = "fact_origin"
        else:
            next_fact_index += 1
            fact_id = f"fact_{next_fact_index:03d}"
        fact_id_map[fact_id] = seq
        title = text.split("\n", 1)[0].strip()
        if title:
            facts.append({
                "id": fact_id,
                "seq": seq,
                "title": title,
            })
    return fact_id_map, facts, watermark


def build_decide_preview_graph(
    shared_graph: Any, *, max_intents: int,
) -> dict[str, Any]:
    fact_id_map, facts, watermark = _fact_catalog(shared_graph)
    yaml_text = ""
    to_yaml = getattr(shared_graph, "to_ctf_graph_yaml", None)
    if callable(to_yaml):
        try:
            yaml_text = str(to_yaml() or "")
        except Exception:
            yaml_text = ""
    resources = []
    if shared_graph is not None and callable(getattr(shared_graph, "pocs", None)):
        resources = [
            str(row.get("poc_id") or "")
            for row in shared_graph.pocs()
            if str(row.get("status") or "") in {"available", "directional", "wip"}
        ]
    mode = getattr(getattr(shared_graph, "challenge", None), "mode", "ctf")
    return {
        "schema": "muteki.pentest-decide-graph.v1" if mode == "pentest" else "muteki.ctf-decide-graph.v1",
        "max_intents": max(0, int(max_intents)),
        "graph_seq": watermark,
        "fact_id_map": fact_id_map,
        "facts": facts,
        "resource_ids": resources,
        "goals": _parse_preview_goals(yaml_text),
        "open_steps": _parse_preview_open_steps(yaml_text),
        "cancelled": cancelled_reopen_rows(shared_graph) if shared_graph is not None else [],
    }


def simulate_draft(
    operations: list[Any],
    *,
    max_intents: int,
    fact_id_map: dict[str, int] | None = None,
    shared_graph: Any = None,
    cancelled: list[dict[str, Any]] | None = None,
    draft_id: str = "",
) -> DraftSimulation:
    sim = DraftSimulation(draft_id=str(draft_id or "").strip())

    def _record(
        index: int,
        op: str,
        *,
        outcome: str,
        reason_code: str,
        goal: str = "",
        intent_id: str = "",
        why: str = "",
    ) -> None:
        row = {
            "raw_index": index,
            "op": op,
            "outcome": outcome,
            "stage": "parse",
            "reason_code": reason_code,
            "goal": " ".join(goal.split()),
            "model_intent_id": intent_id,
        }
        if why:
            row["why"] = " ".join(why.split())
        sim.receipts.append(row)
        if op == "open_step" and outcome != "accepted":
            sim.parse_rejections.append(row)

    def _is_stale(action: str, from_facts: list[int]) -> bool:
        if shared_graph is not None:
            return stale_reopen_without_newer_fact(
                action, from_facts, shared_graph
            )
        return stale_reopen_from_cancelled(action, from_facts, cancelled)

    for index, operation in enumerate(operations):
        if not isinstance(operation, dict):
            _record(index, "unknown", outcome="dropped", reason_code="op_not_object")
            continue
        op = str(operation.get("op") or "").strip()
        if op == "open_step":
            sim.raw_open_steps += 1
            action = str(operation.get("action") or "").strip()
            if not action:
                _record(index, op, outcome="dropped", reason_code="missing_action")
                continue
            if len(sim.intents) >= max(0, int(max_intents)):
                _record(
                    index, op, outcome="dropped",
                    reason_code="max_intents_exceeded", goal=action,
                )
                continue
            expected = str(operation.get("expected_observable") or "").strip()
            stop = str(operation.get("stop_condition") or "").strip()
            coverage = str(operation.get("coverage_key") or "").strip()
            if not expected or not stop or not coverage:
                _record(
                    index, op, outcome="dropped",
                    reason_code="missing_step_contract", goal=action,
                )
                continue
            raw_sources = operation.get("from")
            if not isinstance(raw_sources, list):
                _record(index, op, outcome="dropped", reason_code="invalid_from", goal=action)
                continue
            known_seqs = set((fact_id_map or {}).values())
            if fact_id_map and any(
                (str(item) not in fact_id_map)
                and (not str(item).isdigit() or int(item) not in known_seqs)
                for item in raw_sources
            ):
                _record(index, op, outcome="dropped", reason_code="unknown_fact", goal=action)
                continue
            raw_resources = operation.get("requires") or []
            if not isinstance(raw_resources, list) or any(
                not isinstance(item, str) or not item.strip()
                for item in raw_resources
            ):
                _record(index, op, outcome="dropped", reason_code="invalid_resource", goal=action)
                continue
            required_pocs = list(dict.fromkeys(item.strip() for item in raw_resources))
            if shared_graph is not None and callable(getattr(shared_graph, "pocs", None)):
                available_pocs = {
                    str(row.get("poc_id") or "") for row in shared_graph.pocs()
                    if str(row.get("status") or "") in {"available", "directional", "wip"}
                }
                if set(required_pocs) - available_pocs:
                    _record(index, op, outcome="dropped", reason_code="unknown_resource", goal=action)
                    continue
            from_facts = _positive_fact_ids(
                operation.get("from"), fact_id_map=fact_id_map
            )
            if _is_stale(action, from_facts):
                _record(
                    index, op, outcome="dropped",
                    reason_code="stale_reopen_without_newer_fact", goal=action,
                )
                continue
            priority = str(operation.get("priority") or "normal").strip().lower()
            if priority not in {"high", "normal", "low"}:
                priority = "normal"
            value_claim: dict[str, Any] = {}
            contract = getattr(getattr(shared_graph, "challenge", None), "pentest_contract", None)
            if contract is not None:
                from muteki.pentest.contract import in_scope_url
                asset = str(operation.get("asset") or contract.target).strip()
                try:
                    version = int(operation.get("authorization_version") or contract.version)
                except (TypeError, ValueError):
                    _record(index, op, outcome="dropped", reason_code="invalid_authorization_version", goal=action)
                    continue
                risk = str(operation.get("risk_tier") or "bounded_validation").strip()
                if not in_scope_url(asset, contract):
                    _record(index, op, outcome="dropped", reason_code="asset_out_of_scope", goal=action)
                    continue
                if version != contract.version:
                    _record(index, op, outcome="dropped", reason_code="authorization_version_mismatch", goal=action)
                    continue
                if risk not in {"passive", "bounded_validation"}:
                    _record(index, op, outcome="dropped", reason_code="risk_not_authorized", goal=action)
                    continue
                value_claim = {
                    "asset": asset,
                    "identity": str(operation.get("identity") or "anonymous").strip(),
                    "risk_tier": risk,
                    "evidence_requirement": str(operation.get("evidence_requirement") or expected).strip(),
                    "authorization_version": version,
                }
            intent_id = f"D{len(sim.intents) + 1}"
            sim.intents.append(
                Intent(
                    intent_id=intent_id,
                    goal=action,
                    from_facts=from_facts,
                    priority=priority,
                    requested_priority=priority,
                    expected_observable=expected,
                    stop_condition=stop,
                    coverage_key=coverage,
                    value_claim=value_claim,
                    required_pocs=required_pocs,
                )
            )
            _record(
                index, op, outcome="accepted", reason_code="accepted",
                goal=action, intent_id=intent_id,
            )
        elif op == "drop_step":
            intent_id = str(operation.get("intent_id") or "").strip()
            why = str(
                operation.get("reason") or operation.get("why") or ""
            ).strip()
            if not intent_id:
                _record(index, op, outcome="dropped", reason_code="missing_intent_id")
                continue
            if intent_id in sim.supersede:
                _record(
                    index, op, outcome="dropped",
                    reason_code="duplicate_drop", intent_id=intent_id, why=why,
                )
                continue
            sim.supersede.append(intent_id)
            if why:
                sim.drop_whys.append(f"{intent_id}: {why}")
            _record(
                index, op, outcome="accepted", reason_code="drop_step",
                intent_id=intent_id, why=why,
            )
        elif op == "change_step_priority":
            intent_id = str(operation.get("intent_id") or "").strip()
            priority = str(operation.get("priority") or "").strip().lower()
            if intent_id and priority in {"high", "normal", "low"}:
                sim.reprioritize[intent_id] = priority
                _record(
                    index, op, outcome="accepted",
                    reason_code="change_step_priority", intent_id=intent_id,
                )
            else:
                _record(
                    index, op, outcome="dropped",
                    reason_code="invalid_priority", intent_id=intent_id,
                )
        elif op == "satisfy_goal":
            contract = getattr(getattr(shared_graph, "challenge", None), "pentest_contract", None)
            if contract is not None:
                if contract.report_goal_mode == "count":
                    _record(index, op, outcome="dropped", reason_code="report_count_governed_by_host")
                    continue
                raw_sources = operation.get("from")
                known_seqs = set((fact_id_map or {}).values())
                if (not isinstance(raw_sources, list) or not raw_sources
                        or any(
                            (str(item) not in (fact_id_map or {}))
                            and (not str(item).isdigit() or int(item) not in known_seqs)
                            for item in raw_sources
                        )):
                    _record(index, op, outcome="dropped", reason_code="invalid_goal_evidence")
                    continue
                sources = _positive_fact_ids(raw_sources, fact_id_map=fact_id_map)
                from muteki.pentest.judgement import goal_evidence_valid
                if not goal_evidence_valid(shared_graph.events(), contract, sources):
                    _record(index, op, outcome="dropped", reason_code="unverified_goal_evidence")
                    continue
                sim.goal_evidence_facts = sources
            sim.goal_met = True
            sim.complete_why = str(
                operation.get("reason") or operation.get("why") or ""
            ).strip()
            _record(index, op, outcome="accepted", reason_code="satisfy_goal")
        elif op == "commit":
            sim.progress_summary = str(operation.get("summary") or "").strip()
            _record(index, op, outcome="accepted", reason_code="commit")
        elif op == "preview":
            _record(index, op, outcome="ignored", reason_code="preview_not_an_op")
        else:
            _record(index, op or "unknown", outcome="dropped", reason_code="unknown_op")
    return sim


def render_decide_preview(
    draft: dict[str, Any],
    graph: dict[str, Any],
    *,
    shared_graph: Any = None,
) -> str:
    operations = draft.get("operations")
    if not isinstance(operations, list):
        operations = []
    fact_id_map = {
        str(key): int(value)
        for key, value in dict(graph.get("fact_id_map") or {}).items()
        if str(key).strip()
    }
    try:
        max_intents = int(graph.get("max_intents") or 4)
    except (TypeError, ValueError):
        max_intents = 4
    sim = simulate_draft(
        operations,
        max_intents=max_intents,
        fact_id_map=fact_id_map,
        shared_graph=shared_graph,
        cancelled=list(graph.get("cancelled") or []),
        draft_id=str(draft.get("draft_id") or ""),
    )
    seq_to_id = {int(seq): fact_id for fact_id, seq in fact_id_map.items()}
    remaining: list[dict[str, Any]] = []
    dropped_ids = set(sim.supersede)
    for row in list(graph.get("open_steps") or []):
        if not isinstance(row, dict):
            continue
        step_id = str(row.get("id") or "").strip()
        if not step_id or step_id in dropped_ids:
            continue
        remaining.append({
            "id": step_id,
            "action": str(row.get("action") or ""),
            "priority": sim.reprioritize.get(
                step_id, str(row.get("priority") or "normal")
            ),
            "from": list(row.get("from") or []),
            "new": False,
        })
    for intent in sim.intents:
        remaining.append({
            "id": intent.intent_id,
            "action": intent.goal,
            "priority": intent.priority,
            "from": [
                seq_to_id.get(int(seq), str(seq))
                for seq in (intent.from_facts or [])
            ],
            "new": True,
        })
    lines = [
        f"# advisory snapshot seq={graph.get('graph_seq') or 0} "
        f"draft_ops={len(operations)} "
        "(commit 以当时图重验为准)",
    ]
    if draft.get("committed"):
        lines.append("# draft already committed")
    if sim.goal_met:
        lines.append("# satisfy_goal: 按现有规则将接受达成标记，不是证据验证通过")
    lines.append("goals:")
    for goal in list(graph.get("goals") or []):
        if not isinstance(goal, dict):
            continue
        state = "satisfied" if sim.goal_met else str(goal.get("state") or "open")
        title = str(goal.get("title") or goal.get("id") or "goal")
        lines.append(f"  - {goal.get('id') or 'goal'}: {title} [{state}]")
    lines.append("facts:")
    facts = [row for row in list(graph.get("facts") or []) if isinstance(row, dict)]
    if facts:
        for fact in facts:
            lines.append(
                f"  - {fact.get('id')}: {str(fact.get('title') or '')}"
            )
    else:
        lines.append("  []")
    lines.append("steps:")
    if remaining:
        for step in remaining:
            mark = " new" if step.get("new") else ""
            from_ids = ",".join(str(item) for item in step.get("from") or []) or "-"
            lines.append(
                f"  - {step['id']} [{step['priority']}{mark}]: "
                f"{step['action']}  from={from_ids}"
            )
    else:
        lines.append("  []")
    lines.append("changes:")
    drop_ids = list(sim.supersede)
    lines.append(f"  drop: [{', '.join(drop_ids)}]" if drop_ids else "  drop: []")
    if sim.reprioritize:
        changed = ", ".join(
            f"{intent_id}->{priority}"
            for intent_id, priority in sim.reprioritize.items()
        )
        lines.append(f"  priority: [{changed}]")
    else:
        lines.append("  priority: []")
    opened = [intent.intent_id for intent in sim.intents]
    lines.append(f"  open: [{', '.join(opened)}]" if opened else "  open: []")
    lines.append("receipts:")
    if sim.receipts:
        for row in sim.receipts:
            label = str(row.get("goal") or row.get("model_intent_id") or "")
            suffix = f" {label}" if label else ""
            lines.append(
                f"  - op[{row.get('raw_index')}] {row.get('op')} "
                f"{row.get('outcome')} {row.get('reason_code')}{suffix}"
            )
    else:
        lines.append("  []")
    return "\n".join(lines)


def print_decide_preview() -> None:
    draft_path = Path(os.environ.get("MUTEKI_DECIDE_DRAFT_PATH") or "")
    graph_path = Path(os.environ.get("MUTEKI_DECIDE_GRAPH_PATH") or "")
    try:
        draft = json.loads(draft_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        draft = {"committed": False, "operations": []}
    try:
        graph = json.loads(graph_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        graph = {
            "schema": "muteki.ctf-decide-graph.v1",
            "max_intents": int(os.environ.get("MUTEKI_DECIDE_MAX_STEPS") or 4),
            "graph_seq": 0,
            "fact_id_map": {},
            "facts": [],
            "goals": [],
            "open_steps": [],
            "cancelled": [],
        }
    print(render_decide_preview(draft, graph), end="")


def _materialise_draft(
    draft: dict[str, Any], *, max_intents: int,
    fact_id_map: dict[str, int] | None = None,
    shared_graph: Any = None,
) -> ReasonResult:
    operations = draft.get("operations")
    if not draft.get("committed") or not isinstance(operations, list):
        return _failure(
            PlannerFailureKind.INVALID_PLAN,
            "Pi did not commit a valid planner draft",
            status="received",
        )

    sim = simulate_draft(
        operations,
        max_intents=max_intents,
        fact_id_map=fact_id_map,
        shared_graph=shared_graph,
        draft_id=str(draft.get("draft_id") or "").strip(),
    )
    accepted = sum(1 for row in sim.receipts if row.get("outcome") == "accepted")
    dropped = sum(1 for row in sim.receipts if row.get("outcome") == "dropped")
    if sim.drop_whys:
        supersede_why = "; ".join(sim.drop_whys)
    elif sim.supersede:
        supersede_why = "Reason removed obsolete open Steps"
    else:
        supersede_why = ""

    return ReasonResult(
        goal_met=sim.goal_met,
        intents=sim.intents,
        audit_notes=[{
            "kind": "draft_receipts",
            "schema": "muteki.decide-draft-receipts.v1",
            "draft_id": sim.draft_id,
            "operations": sim.receipts,
        }] if sim.receipts else [],
        verdict=VERDICT_COMPLETE if sim.goal_met else VERDICT_EXPLORE,
        complete_why=sim.complete_why,
        goal_evidence_facts=sim.goal_evidence_facts,
        progress_summary=sim.progress_summary,
        semantic_dedupe_available=False,
        supersede_intents=sim.supersede,
        supersede_why=supersede_why,
        reprioritize_intents=sim.reprioritize,
        planner_failure=None,
        diagnostics=ReasonDiagnostics(
            response_status="received",
            finish_reason="stop",
            parse_status="parsed",
            parse_detail=(
                f"Pi committed planner operations; draft_id={sim.draft_id or '-'}; "
                f"accepted={accepted} dropped={dropped}"
            )[:500],
            raw_intent_count=sim.raw_open_steps,
            parsed_intent_count=len(sim.intents),
        ),
        intent_parse_rejections=sim.parse_rejections,
    )


async def _record_pi_decide_usage(
    parsed: CliResult,
    *,
    cost: CostController | None,
    run_id: str,
    challenge_id: str,
    model: str,
    usage_id: str,
    generation: int | None,
    status: str,
) -> None:
    """Settle one actual Pi CLI invocation, including a failed or partial turn.

    The work directory identifies this invocation. A repeated settlement of the
    same invocation uses its same identity, while a new CLI launch has a new
    identity even if its prompt is identical.
    """
    if cost is None or not run_id:
        return
    usage: dict[str, Any] = {
        "source": "pi-decide",
        "status": status,
        "measurement_scope": "invocation",
        "input_includes_cache": True,
    }
    for key in (
        "input_tokens", "output_tokens", "cache_read_tokens",
        "cache_write_tokens", "reasoning_tokens",
    ):
        value = getattr(parsed, key, None)
        if value is not None:
            usage[key] = value
    input_tokens = parsed.input_tokens
    output_tokens = parsed.output_tokens
    native_cost = parsed.cost_usd
    if native_cost is not None and math.isfinite(native_cost) and native_cost > 0:
        # Pi's JSONL cost.total is calculated from its local provider model
        # configuration; it is an estimate, not a supplier billing receipt.
        usage["estimated_cost"] = native_cost
        await cost.add_external_usd(
            native_cost,
            run_id=run_id,
            challenge_id=challenge_id,
            solver_id="reason",
            input_tokens=int(input_tokens or 0),
            output_tokens=int(output_tokens or 0),
            usage=usage,
            usage_id=usage_id,
            model=model,
            actor_kind="coordinator",
            generation=generation,
            engine="pi",
        )
    elif input_tokens is not None and output_tokens is not None:
        await cost.record(
            model=model,
            input_tokens=int(input_tokens),
            output_tokens=int(output_tokens),
            run_id=run_id,
            challenge_id=challenge_id,
            solver_id="reason",
            usage=usage,
            usage_id=usage_id,
            actor_kind="coordinator",
            generation=generation,
            engine="pi",
        )
    else:
        # Keep the original partial/missing fields in the usage row, while the
        # live ledger marks this invocation unpriced. A dollar-budgeted run can
        # then stop dispatching immediately if coverage becomes incomplete.
        await cost.record(
            model=model,
            input_tokens=int(input_tokens or 0),
            output_tokens=int(output_tokens or 0),
            run_id=run_id,
            challenge_id=challenge_id,
            solver_id="reason",
            usage=usage,
            usage_id=usage_id,
            actor_kind="coordinator",
            generation=generation,
            engine="pi",
            estimate_cost=False,
        )


async def run_ctf_pi_reason(
    *,
    graph_summary: str,
    max_intents: int,
    model: str,
    account_root: str | Path | None,
    account_id: str,
    state_root: str | Path,
    shared_graph: Any,
    mode: str = "ctf",
    cost: CostController | None = None,
    run_id: str = "",
    challenge_id: str = "",
    generation: int | None = None,
    cost_budget_usd: float | None = None,
) -> ReasonResult:
    """Run one stateless Pi turn and materialise its committed operations."""
    state_dir = Path(state_root).expanduser().resolve()
    state_dir.mkdir(parents=True, exist_ok=True)
    work_dir = Path(tempfile.mkdtemp(prefix="decide-", dir=state_dir))
    usage_id = f"pi-decide:{run_id}:{work_dir.name}"
    try:
        draft_path = work_dir / "draft.json"
        graph_path = work_dir / "graph.json"
        graph_path.write_text(
            json.dumps(
                build_decide_preview_graph(
                    shared_graph, max_intents=max_intents
                ),
                ensure_ascii=False,
            ),
            encoding="utf-8",
        )
        driver = PiDriver()
        runtime = runtime_env_for_engine(
            "pi",
            account_root=account_root,
            account_id=account_id,
            env=os.environ,
            agent_state_dir=state_dir / "pi-agent",
            model=model,
        )
        env = os.environ.copy()
        env.update(driver.env_extra())
        env.update(runtime.env)
        pythonpath = os.pathsep.join(
            item for item in (_REPO_ROOT.as_posix(), env.get("PYTHONPATH") or "")
            if item
        )
        env["PYTHONPATH"] = pythonpath
        env["MUTEKI_DECIDE_DRAFT_PATH"] = str(draft_path)
        env["MUTEKI_DECIDE_GRAPH_PATH"] = str(graph_path)
        env["MUTEKI_DECIDE_PYTHON"] = sys.executable
        env["MUTEKI_DECIDE_MAX_STEPS"] = str(max(0, int(max_intents)))

        provider = str(env.get("MUTEKI_PI_PROVIDER") or "").strip()
        # A positive dollar budget preflights the explicit planner model. An
        # ambient Pi default must not replace that model after preflight.
        # Unlimited Runs retain their established environment-first behavior.
        selected_model = str(
            (model if cost_budget_usd is not None and cost_budget_usd > 0 else None)
            or env.get("MUTEKI_PI_MODEL") or model or ""
        ).strip()
        system_prompt = PENTEST_PI_DECIDE_SYSTEM if mode == "pentest" else CTF_PI_DECIDE_SYSTEM
        argv = [
            driver.bin,
            "-p",
            "--mode",
            "json",
            "--no-session",
            "--no-extensions",
            "--no-skills",
            "--no-context-files",
            "--no-builtin-tools",
            "--extension",
            str(_EXTENSION),
            "--tools",
            _TOOLS,
        ]
        if provider:
            argv.extend(["--provider", provider])
        if selected_model:
            argv.extend(["--model", selected_model])
        argv.extend(["--system-prompt", system_prompt])
        check_process_launch(
            argv, cwd=str(work_dir), env=env,
            stdin_text=str(graph_summary), source="pi-decide")

        proc = None
        communicate_task = None

        async def _stop_and_collect() -> tuple[bytes, bytes]:
            if proc is not None and proc.returncode is None:
                try:
                    os.killpg(proc.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            if communicate_task is None:
                return b"", b""
            try:
                return await asyncio.wait_for(
                    asyncio.shield(communicate_task), timeout=5.0
                )
            except asyncio.TimeoutError:
                if proc is not None and proc.returncode is None:
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                try:
                    return await asyncio.wait_for(
                        asyncio.shield(communicate_task), timeout=5.0
                    )
                except asyncio.TimeoutError:
                    communicate_task.cancel()
                    await asyncio.gather(communicate_task, return_exceptions=True)
                    return b"", b""

        try:
            proc = await asyncio.create_subprocess_exec(
                *argv,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                cwd=str(work_dir),
                env=env,
                start_new_session=True,
            )
            communicate_task = asyncio.create_task(
                proc.communicate(str(graph_summary).encode("utf-8"))
            )
            deadline = asyncio.get_running_loop().time() + 300.0
            while not communicate_task.done():
                committed = False
                try:
                    committed = bool(json.loads(
                        draft_path.read_text(encoding="utf-8")
                    ).get("committed"))
                except (OSError, json.JSONDecodeError):
                    pass
                if committed:
                    if proc.returncode is None:
                        try:
                            os.killpg(proc.pid, signal.SIGTERM)
                        except ProcessLookupError:
                            pass
                    break
                if asyncio.get_running_loop().time() >= deadline:
                    raise asyncio.TimeoutError
                await asyncio.sleep(0.05)
            remaining = max(
                1.0, deadline - asyncio.get_running_loop().time()
            )
            stdout_bytes, stderr_bytes = await asyncio.wait_for(
                asyncio.shield(communicate_task), timeout=remaining
            )
        except asyncio.TimeoutError:
            stdout_bytes, stderr_bytes = await _stop_and_collect()
            parsed = driver.parse(
                stdout_bytes.decode("utf-8", errors="replace"),
                stderr_bytes.decode("utf-8", errors="replace"),
            )
            await _record_pi_decide_usage(
                parsed, cost=cost, run_id=run_id,
                challenge_id=challenge_id, model=selected_model,
                usage_id=usage_id, generation=generation, status="timeout",
            )
            return _failure(
                PlannerFailureKind.TIMEOUT,
                "Pi Decide exceeded 300 seconds",
                status="timeout",
                timed_out=True,
                raw_response=parsed.text,
            )
        except Exception as exc:
            stdout_bytes, stderr_bytes = await _stop_and_collect()
            if proc is not None:
                parsed = driver.parse(
                    stdout_bytes.decode("utf-8", errors="replace"),
                    stderr_bytes.decode("utf-8", errors="replace"),
                )
                await _record_pi_decide_usage(
                    parsed, cost=cost, run_id=run_id,
                    challenge_id=challenge_id, model=selected_model,
                    usage_id=usage_id, generation=generation, status="error",
                )
            return _failure(
                PlannerFailureKind.EXCEPTION,
                f"{type(exc).__name__}: {exc}",
                status="error",
            )
        finally:
            if proc is not None and proc.returncode is None:
                try:
                    os.killpg(proc.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass

        stdout = stdout_bytes.decode("utf-8", errors="replace")
        stderr = stderr_bytes.decode("utf-8", errors="replace")
        parsed = driver.parse(stdout, stderr)
        try:
            draft = json.loads(draft_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            await _record_pi_decide_usage(
                parsed, cost=cost, run_id=run_id,
                challenge_id=challenge_id, model=selected_model,
                usage_id=usage_id, generation=generation, status="error",
            )
            if proc is not None and proc.returncode not in (None, 0):
                return _failure(
                    PlannerFailureKind.UNAVAILABLE,
                    f"Pi Decide process exited with code {proc.returncode}: "
                    f"{stderr.strip() or parsed.error or 'no diagnostic output'}",
                    status="error",
                    raw_response=parsed.text,
                )
            detail = parsed.error or f"{type(exc).__name__}: {exc}"
            return _failure(
                PlannerFailureKind.INVALID_PLAN,
                detail,
                status="received" if stdout.strip() else "empty",
                raw_response=parsed.text,
            )

        await _record_pi_decide_usage(
            parsed, cost=cost, run_id=run_id,
            challenge_id=challenge_id, model=selected_model,
            usage_id=usage_id, generation=generation,
            status=("observed" if draft.get("committed")
                    and isinstance(draft.get("operations"), list)
                    else "error"),
        )
        fact_id_map, _facts, _watermark = _fact_catalog(shared_graph)
        result = _materialise_draft(
            draft,
            max_intents=max_intents,
            fact_id_map=fact_id_map,
            shared_graph=shared_graph,
        )
        result.diagnostics.raw_response = parsed.text
        result.diagnostics.raw_response_sha256 = hashlib.sha256(
            parsed.text.encode("utf-8", errors="replace")
        ).hexdigest()
        result.diagnostics.response_chars = len(parsed.text)
        result.diagnostics.input_tokens = int(parsed.input_tokens or 0)
        result.diagnostics.output_tokens = int(parsed.output_tokens or 0)
        # `commit` is the transaction boundary. Pi may time out while producing
        # optional prose after the extension has durably committed the draft;
        # that transport error must not turn an already-applied transaction into
        # a failed Reason pass.
        if parsed.error:
            result.diagnostics.parse_detail = (
                f"Pi committed planner operations; post-commit runtime error: "
                f"{parsed.error}"
            )
        return result
    except Exception as exc:
        return _failure(
            PlannerFailureKind.EXCEPTION,
            f"{type(exc).__name__}: {exc}",
            status="error",
        )
    finally:
        shutil.rmtree(work_dir, ignore_errors=True)
