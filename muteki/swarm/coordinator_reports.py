"""Pentest report pipeline. Moved from coordinator_flags.py."""

from __future__ import annotations

import json
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    pass

from muteki.core.prompt_assembly import (
    PromptBudgetExceeded, PromptPart, compile_prompt, estimate_host_tokens,
    resolve_prompt_budget,
)
from muteki.solver.vuln_report import (
    VALUE_JUDGE_SYSTEM,
    VALUE_OK,
    VALUE_REJECT_INCOMPLETE,
    VALUE_REJECT_TEMPLATE,
    heuristic_value_code,
    parse_value_judge_reply,
    render_repro_intent_goal,
    render_report_markdown,
    report_goal_decision,
    report_id_from_intent,
    report_sse_fields,
    repro_intent_id,
)

def _submitted_report_count(self) -> int:
    if self.shared_graph is None or not hasattr(self.shared_graph, "report_states"):
        return 0
    try:
        states = self.shared_graph.report_states() or {}
    except Exception:
        return 0
    terminal = {
        "submitted", "reproduced", "repro_failed",
        "value_accepted", "value_rejected", "accepted",
    }
    return sum(
        1 for item in states.values()
        if str(item.get("status") or "") in terminal
    )


def _pentest_race_submission_quota_met(self) -> bool:
    if not self._pentest_product():
        return False
    return self._submitted_report_count() >= self._expected_findings()


def _record_reports(self, *reports: dict | None) -> list[dict]:
    fresh: list[dict] = []
    seen = {
        str(item.get("report_id") or "")
        for item in getattr(self, "_found_reports", [])
    }
    if not hasattr(self, "_found_reports"):
        self._found_reports = []
    for item in reports:
        if not item:
            continue
        rid = str(item.get("report_id") or "").strip()
        if not rid or rid in seen:
            continue
        self._found_reports.append(dict(item))
        seen.add(rid)
        fresh.append(dict(item))
    return fresh


def _sync_reports_from_graph(self) -> list[dict]:
    if self.shared_graph is None or not hasattr(self.shared_graph, "accepted_reports"):
        return []
    try:
        rows = list(self.shared_graph.accepted_reports() or [])
    except Exception:
        return []
    return self._record_reports(*rows)


def _ensure_report_repro_intents(self) -> int:
    if getattr(self.challenge, "mode", "ctf") != "pentest":
        return 0
    if not self.verifier_policy.get("enabled", True):
        return 0
    if self.shared_graph is None or not hasattr(self.shared_graph, "pending_report_repros"):
        return 0
    try:
        pending = list(self.shared_graph.pending_report_repros() or [])
    except Exception:
        return 0
    n = 0
    for report in pending:
        rid = str(report.get("report_id") or "").strip()
        if not rid:
            continue
        iid = repro_intent_id(rid)
        state = {}
        if hasattr(self.shared_graph, "intent_claim_state"):
            try:
                state = self.shared_graph.intent_claim_state(iid) or {}
            except Exception:
                state = {}
        status = str(state.get("status") or "")
        if status in {"open", "claimed"}:
            continue
        if status == "done" and hasattr(self.shared_graph, "reopen_intent"):
            try:
                if self.shared_graph.reopen_intent(
                    actor="coordinator", intent_id=iid, reason="repro retry",
                ):
                    n += 1
            except Exception:
                pass
            continue
        try:
            seq = self.shared_graph.propose_intent(
                actor="coordinator",
                intent_id=iid,
                goal=render_repro_intent_goal(report),
                payload={
                    "worker_class": "verifier",
                    "report_id": rid,
                    "source": "report_repro",
                    "priority": "high",
                },
            )
        except Exception:
            continue
        if seq > 0:
            n += 1
    return n


async def _judge_pending_report_values(self) -> list[dict]:
    accepted: list[dict] = []
    if getattr(self.challenge, "mode", "ctf") != "pentest":
        return accepted
    if self.shared_graph is None or not hasattr(self.shared_graph, "pending_report_value_judges"):
        return accepted
    try:
        pending = list(self.shared_graph.pending_report_value_judges() or [])
    except Exception:
        return accepted
    for report in pending:
        rid = str(report.get("report_id") or "").strip()
        if not rid:
            continue
        heuristic = heuristic_value_code(report)
        ok = heuristic is None
        code = VALUE_OK if ok else heuristic
        detail = ""
        if ok and getattr(self, "llm", None) is not None:
            ok, code, detail = await self._llm_value_judge(report)
        elif not ok:
            detail = f"heuristic:{code}"
        if not ok:
            try:
                self.shared_graph.report_value_decision(
                    actor="coordinator", report_id=rid,
                    accepted=False, code=code, detail=detail)
            except Exception:
                pass
            try:
                await self._emit_bb_bus(
                    "report_value_rejected",
                    report_id=rid, code=code, reason=detail,
                    title=report.get("title", ""))
            except Exception:
                pass
            continue
        goal_ok, goal_code, goal_detail = report_goal_decision(
            self._engagement(), report)
        report["goal_qualified"] = bool(goal_ok)
        report["goal_code"] = goal_code
        report["goal_detail"] = goal_detail
        try:
            self.shared_graph.report_value_decision(
                actor="coordinator", report_id=rid,
                accepted=True, code=VALUE_OK, detail=detail)
            try:
                report["markdown"] = render_report_markdown(report)
            except Exception:
                report.setdefault("markdown", "")
            self.shared_graph.report_accepted(actor="coordinator", report=report)
        except Exception:
            pass
        self._persist_accepted_collection(report)
        accepted.extend(self._record_reports(report))
        try:
            await self._emit_bb_bus(
                "report_accepted",
                **report_sse_fields(report, include_markdown=True),
            )
            await self._emit_bb_bus(
                "report_goal_evaluated",
                report_id=rid,
                qualified=bool(goal_ok),
                code=goal_code,
                reason=goal_detail,
                completion_kind=self._engagement().completion_kind,
                outcome_predicate=self._engagement().outcome_predicate,
            )
        except Exception:
            pass
    return accepted


async def _llm_value_judge(self, report: dict) -> tuple[bool, str, str]:
    try:
        body = json.dumps(report, ensure_ascii=False, indent=2)
        budget = resolve_prompt_budget(
            getattr(self, "reason_model", None) or "deepseek-v4-flash",
            role="judge",
        )
        user_budget = budget.input_budget_tokens - estimate_host_tokens(
            VALUE_JUDGE_SYSTEM)
        user = compile_prompt(
            "{context}",
            required_sections=(
                PromptPart(
                    "scope",
                    "Scope:\n" + str(
                        getattr(self.challenge, "scope", "") or "")),
                PromptPart("report", "Report:\n" + body),
            ),
            optional_sections=(),
            input_budget=user_budget,
        ).prompt
        messages = [
            {"role": "system", "content": VALUE_JUDGE_SYSTEM},
            {"role": "user", "content": user},
        ]
        resp = await self.llm.chat(
            model=getattr(self, "reason_model", None) or "deepseek-v4-flash",
            messages=messages,
            max_tokens=400,
            stream=False,
            run_id=getattr(self, "run_id", None),
            challenge_id=self.challenge.id,
            solver_id="report-value",
        )
        text = getattr(resp, "content", "") or ""
        accept, code, reason = parse_value_judge_reply(text)
        if accept:
            return True, VALUE_OK, reason
        parse_failed = code == VALUE_REJECT_TEMPLATE and reason.startswith("value judge")
        if parse_failed:
            heuristic = heuristic_value_code(report)
            if heuristic:
                return False, heuristic, reason
            return True, VALUE_OK, reason + "; heuristic accept"
        return False, code, reason
    except PromptBudgetExceeded:
        return False, VALUE_REJECT_INCOMPLETE, (
            "report exceeds the value-judge context budget; move raw evidence "
            "into artifacts and resubmit"
        )
    except Exception:
        heuristic = heuristic_value_code(report)
        if heuristic:
            return False, heuristic, "value judge unavailable; heuristic reject"
        return True, VALUE_OK, "value judge unavailable; heuristic did not reject"


def _verifier_dispatch_items(self, *, timeout: int = 240) -> list[dict[str, Any]]:
    """Open report-reproduction intents, verifier class first."""
    if getattr(self.challenge, "mode", "ctf") != "pentest":
        return []
    self._ensure_report_repro_intents()
    if self.shared_graph is None:
        return []
    try:
        rows = list(self.shared_graph.query_legacy_candidates())
    except Exception:
        return []
    cap = max(90, int(timeout))
    items: list[dict[str, Any]] = []
    for row in rows:
        if str(row.get("worker_class") or "") != "verifier":
            continue
        iid = str(row.get("intent_id") or "").strip()
        goal = str(row.get("goal") or "").strip()
        report_id = report_id_from_intent(iid)
        if not iid or not goal or not report_id:
            continue
        items.append({
            "id": iid,
            "intent_id": iid,
            "goal": goal,
            "mode": "report_reproducer",
            "timeout": cap,
            "from_facts": list(row.get("from_facts") or []),
            "expected_observable": str(row.get("expected_observable") or ""),
            "stop_condition": str(row.get("stop_condition") or ""),
            "coverage_key": str(row.get("coverage_key") or ""),
            "meta": {"source": "report_repro", "report_id": report_id},
        })
    return items


async def _drain_report_pipeline(self) -> list[dict]:
    n = self._ensure_report_repro_intents()
    if n:
        try:
            await self._emit_bb_bus("report_repro_queued", count=n)
        except Exception:
            pass
    accepted = await self._judge_pending_report_values()
    self._sync_reports_from_graph()
    return accepted


def _intent_matches_engagement(self, row: dict) -> bool:
    eg = self._engagement()
    cls = (eg.finding_class or "generic").strip().lower()
    blob = f"{row.get('goal') or ''} {row.get('route_hash') or ''}"
    low = blob.lower()
    if cls in {"", "generic"}:
        return True
    if cls == "idor":
        return any(
            h in blob or h in low
            for h in ("idor", "越权", "bola", "broken access", "bac", "未授权")
        )
    if cls == "rce":
        return any(
            h in blob or h in low
            for h in ("rce", "远程代码", "command", "cmdi", "exec", "注入")
        )
    if cls == "sqli":
        return any(h in blob or h in low for h in ("sqli", "sql", "注入"))
    if cls == "xss":
        return any(h in blob or h in low for h in ("xss", "跨站", "script"))
    if cls == "ssrf":
        return "ssrf" in low
    return cls in low


def _report_pipeline_pending(self) -> bool:
    if self.shared_graph is None:
        return False
    try:
        repros = []
        judges = []
        if hasattr(self.shared_graph, "pending_report_repros"):
            repros = list(self.shared_graph.pending_report_repros() or [])
        if hasattr(self.shared_graph, "pending_report_value_judges"):
            judges = list(self.shared_graph.pending_report_value_judges() or [])
        return bool(repros or judges)
    except Exception:
        return False
