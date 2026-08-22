"""Flag tracking, solver build, blackboard/bus emission, finalize, HITL drain.

Split out of ``swarm.py`` (code-health G1) as a mixin of ``Swarm``. Every method
body is byte-for-byte the original; the mixin is composed back into ``Swarm`` so
behavior and the public surface are unchanged. Instance state built in
``Swarm.__init__`` is resolved through the composed class at runtime.
"""

# ruff: noqa: F401
from __future__ import annotations

import asyncio
import hashlib
import json
import os
import re
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, Optional

if TYPE_CHECKING:
    from muteki.learning.distill import TemplateStore

from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.runtime_env import is_web_container
from muteki.core.events import Event, EventType, blackboard_delta_payload
from muteki.core.llm import LLMClient, ModelSpec
from muteki.models.solve_graph import (
    Challenge, engagement_goal_of, engagement_reports_complete,
)
from muteki.solver.gate import finding_key
from muteki.solver.vuln_report import (
    VALUE_JUDGE_SYSTEM,
    VALUE_OK,
    VALUE_REJECT_TEMPLATE,
    heuristic_value_code,
    parse_value_judge_reply,
    persist_report_collection,
    render_repro_intent_goal,
    render_report_markdown,
    report_id_from_intent,
    report_sse_fields,
    repro_intent_id,
    reports_dir_from_graph_db,
)
from muteki.sandbox.manager import SandboxManager
from muteki.solver.result import ArtifactStore
from muteki.solver.types import SolverConfig, SolveOutcome
from muteki.solver.credential_accounts import runtime_env_for_engine
from muteki.solver.worker_profiles import (
    base_engine_for_profile,
    coerce_nonneg_int,
    normalize_profile_roster,
    normalize_worker_profiles,
    profile_names,
    worker_identity_fields,
)
from muteki.solver.workspace import cleanup_worker_scratch, ensure_workspace
from muteki.swarm.insight_bus import InsightBus
from muteki.swarm.stage_policy import StagePolicy
from muteki.swarm.shared_graph import SharedGraph, SQLiteSharedGraph, canonicalize_lane
from muteki.swarm.swarm_support import (
    _STANDING_MAX,
    _PENDING_HELP_MAX,
    WorkerBudgetExhausted,
    WorkerSpawnRejected,
    ControlShutdownIncomplete,
    SwarmOutcome,
    _CONTAINER_BLACKBOARD_SKILL,
    _BLACKBOARD_SKILL_LINKS,
    _ensure_blackboard_skill_links,
    _HEALTH_PROBE_CACHE,
    _HEALTH_FAILURE_TTL_FRACTION,
    _health_cache_get,
    _health_cache_put,
    _health_cache_clear,
    _is_control_failure,
)


class _FlagsBusMixin:
    @staticmethod
    def _clean_review_policy(value: Any) -> dict[str, Any]:
        configured = isinstance(value, dict)
        raw = value if configured else {}
        defaults = {
            "enabled": configured,
            "engine": "",
            "reasoning_effort": "inherit",
            "after_race": True,
            "after_fruitless_workers": 3,
            "after_duplicate_intents": 2,
            "on_course_correct": True,
            "on_reason_dry": True,
            "on_candidate_spike": True,
            "on_operator_hint": True,
            "every_completed_workers": 6,
            "candidate_spike_threshold": 5,
            "max_concurrent": 1,
            "allow_review_fallback": False,
            "cooldown_events": 8,
            "timeout": 420,
            "max_review_workers": 12,
            "max_challenges_per_cycle": 8,
        }
        out = dict(defaults)
        for key in ("enabled", "after_race", "on_course_correct", "on_reason_dry",
                    "on_candidate_spike", "on_operator_hint", "allow_review_fallback"):
            if key in raw:
                out[key] = bool(raw.get(key))
        if raw.get("engine"):
            out["engine"] = str(raw.get("engine")).strip()
        review_effort = str(raw.get("reasoning_effort") or "inherit").strip().lower()
        if review_effort in {
            "inherit", "default", "none", "minimal", "low", "medium",
            "high", "xhigh", "max",
        }:
            out["reasoning_effort"] = review_effort
        for key in ("after_fruitless_workers", "after_duplicate_intents",
                    "every_completed_workers", "candidate_spike_threshold",
                    "max_concurrent", "max_challenges_per_cycle",
                    "cooldown_events", "timeout", "max_review_workers"):
            if key in raw:
                try:
                    out[key] = max(0, int(raw.get(key)))
                except (TypeError, ValueError):
                    pass
        return out

    @staticmethod
    def _clean_verifier_policy(value: Any) -> dict[str, Any]:
        configured = isinstance(value, dict)
        raw = value if configured else {}
        defaults = {
            "enabled": True,
            "engine": "",
            "reasoning_effort": "inherit",
            "max_concurrent": 0,
            "allow_verifier_fallback": False,
            "timeout": 240,
            "max_verifier_workers": 24,
        }
        out = dict(defaults)
        for key in ("enabled", "allow_verifier_fallback"):
            if key in raw:
                out[key] = bool(raw.get(key))
        if raw.get("engine"):
            out["engine"] = str(raw.get("engine")).strip()
        verifier_effort = str(raw.get("reasoning_effort") or "inherit").strip().lower()
        if verifier_effort in {
            "inherit", "default", "none", "minimal", "low", "medium",
            "high", "xhigh", "max",
        }:
            out["reasoning_effort"] = verifier_effort
        for key in ("max_concurrent", "timeout", "max_verifier_workers"):
            if key in raw:
                try:
                    out[key] = max(0, int(raw.get(key)))
                except (TypeError, ValueError):
                    pass
        return out

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

    def _expected_flags(self) -> int:
        return max(1, getattr(self.challenge, "expected_flags", 1) or 1)

    def _multi_flag(self) -> bool:
        return bool(getattr(self.challenge, "multi_flag", False))

    def _flags_complete(self) -> bool:
        """Is the run's flag objective met? This is the SAVE-vs-FINISH decoupling
        (run-10070): saving a flag (_record_flags) must not finish a collect-mode run
        the way it finishes a single-flag run.

        - single-flag (multi_flag=False, the default): `len >= expected_flags`, which
          with expected_flags=1 finishes on the first gated flag — byte-identical to
          the old behavior.
        - collect mode with a known count (multi_flag=True, expected_flags>1): finish
          once N distinct flags are collected.
        - collect mode with UNKNOWN count (multi_flag=True, expected_flags<=1): NEVER
          finish by count. Flags still save + display; the run ends only on operator
          STOP or the coordinator's no-progress pause. A saved flag is not a finish."""
        if self._multi_flag() and self._expected_flags() <= 1:
            return False
        return len(self._found_flags) >= self._expected_flags()

    def _engagement(self):
        return engagement_goal_of(self.challenge)

    def _expected_findings(self) -> int:
        return max(1, int(self._engagement().expected_findings or 1))

    def _findings_complete(self) -> bool:
        """Pentest stop: accepted report collection reaches expected_findings."""
        if getattr(self.challenge, "mode", "ctf") != "pentest":
            return False
        return engagement_reports_complete(
            self._engagement(),
            len(getattr(self, "_found_reports", None) or []),
        )

    def _pentest_product(self) -> bool:
        """Product pentest: success is gated findings, not flags."""
        return (
            getattr(self.challenge, "mode", "ctf") == "pentest"
            and not self._pentest_flag_required()
        )

    def _goal_satisfied(self) -> bool:
        """Stop predicate for the live coordinator: accepted reports on product
        pentest, flags on CTF and flag-bearing pentest eval."""
        if self._pentest_product():
            return self._findings_complete()
        return self._flags_complete()

    def _record_findings(self, *findings: dict | None) -> list[dict]:
        fresh: list[dict] = []
        seen = {finding_key(f) for f in self._found_findings}
        for item in findings:
            if not item:
                continue
            key = finding_key(item)
            if not key or key in seen:
                continue
            self._found_findings.append(dict(item))
            seen.add(key)
            fresh.append(dict(item))
        return fresh

    def _sync_findings_from_graph(self) -> list[dict]:
        if self.shared_graph is None:
            return []
        try:
            snap = self.shared_graph.snapshot()
            graph_findings = list(getattr(snap, "findings", []) or [])
            invalidated = set()
            if hasattr(self.shared_graph, "invalidated_findings"):
                invalidated = self.shared_graph.invalidated_findings()
        except Exception:
            return []
        if invalidated:
            self._found_findings = [
                f for f in self._found_findings if finding_key(f) not in invalidated
            ]
        return self._record_findings(*(
            f for f in graph_findings if finding_key(f) not in invalidated
        ))

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
            except Exception:
                pass
        return accepted

    def _persist_accepted_collection(self, report: dict) -> None:
        directory = reports_dir_from_graph_db(
            getattr(self.shared_graph, "db_path", None) if self.shared_graph is not None else None)
        if directory is None:
            return
        try:
            rows: list[dict] = []
            if hasattr(self.shared_graph, "accepted_reports"):
                rows = [dict(item) for item in (self.shared_graph.accepted_reports() or [])]
            if not rows:
                rows = [dict(report)]
            name = str(getattr(self.challenge, "name", "") or "").strip()
            title = f"{name} 漏洞报告集" if name else "漏洞报告集"
            path = persist_report_collection(directory, rows, title=title)
            report["markdown_path"] = str(path)
        except Exception:
            pass

    async def _llm_value_judge(self, report: dict) -> tuple[bool, str, str]:
        try:
            body = json.dumps(report, ensure_ascii=False, indent=2)[:8000]
            messages = [
                {"role": "system", "content": VALUE_JUDGE_SYSTEM},
                {"role": "user", "content": (
                    "Scope:\n"
                    f"{getattr(self.challenge, 'scope', '') or ''}\n\n"
                    "Report:\n"
                    f"{body}\n"
                )},
            ]
            resp = await self.llm.chat(
                model=getattr(self, "reason_model", None) or "deepseek-v4-flash",
                messages=messages,
                temperature=0.0,
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
            rows = list(self.shared_graph.query_legacy_candidates(now=time.time()))
        except Exception:
            return []
        cap = max(90, int(timeout))
        items: list[dict[str, Any]] = []
        for row in rows:
            if str(row.get("worker_class") or "") != "verifier":
                continue
            iid = str(row.get("intent_id") or "").strip()
            goal = str(row.get("goal") or "").strip()
            if not iid or not goal:
                continue
            items.append({
                "id": iid,
                "intent_id": iid,
                "goal": goal,
                "mode": "verifier",
                "timeout": cap,
                "meta": {"source": "report_repro", "report_id": report_id_from_intent(iid)},
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

    def _coverage_complete(self) -> bool:
        """P2 pentest stop: matching intents are all concluded, none ACTIVE.

        Requires at least one matching intent so a cold start does not fire.
        Success bit stays false (coverage exhaustion is not goal_met).
        recon ends on matching-intent exhaustion. collect with
        collect_until_coverage also ends that way, but only after the report
        pipeline is empty.
        """
        if getattr(self.challenge, "mode", "ctf") != "pentest":
            return False
        engagement = self._engagement()
        quantity = engagement.quantity
        if quantity == "recon":
            pass
        elif quantity == "collect" and engagement.collect_until_coverage:
            if self._report_pipeline_pending():
                return False
        else:
            return False
        if self.shared_graph is None or not hasattr(self.shared_graph, "coverage_intent_rows"):
            return False
        try:
            rows = self.shared_graph.coverage_intent_rows()
        except Exception:
            return False
        matching = [r for r in rows if self._intent_matches_engagement(r)]
        if not matching:
            return False
        for row in matching:
            dispatch = str(row.get("dispatch_state") or "")
            status = str(row.get("status") or "")
            if dispatch == "active" and status in {"open", "claimed"}:
                return False
            if status not in {"done"} and dispatch not in {"closed", "retired"}:
                return False
        return True

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

    def _record_flags(self, *flags: Optional[str]) -> list[str]:
        """Add flags to the dedup set; return the ones that were NEW (so the caller
        can broadcast each exactly once)."""
        fresh: list[str] = []
        for f in flags:
            if f and f not in self._found_flags:
                self._found_flags.append(f)
                fresh.append(f)
        return fresh

    def _pentest_flag_required(self) -> bool:
        """tsec-f03 lesson: a pentest-MODE engagement that is flag-BEARING
        (tsecbench-style ranges — pentest prompt shape, but there is a flag and
        a judge) must not end on Reason's complete verdict alone: the brain
        judges from blackboard facts and can hallucinate a solve, or the flag
        can sit in evidence without ever passing the worker-side flag gate.
        Signalled explicitly via Challenge.pentest_flag_required — flag_format
        always has a non-empty default, so it cannot serve as the signal.
        Product default is False (eval bypass only; not a product success
        condition). Product pentest succeeds on gated findings."""
        return bool(getattr(self.challenge, "pentest_flag_required", False))

    async def _salvage_flags_from_evidence(self, complete_why: str = "") -> list[str]:
        """Compatibility hook retained for older coordinator call sites.

        Verified facts and verdict text are useful planning inputs, but they are
        not an acceptance channel.  Protocol 1 Flags enter the shared graph only
        after a Worker-owned ``submit-flag`` event passes CliSolver provenance
        validation.  Returning no values keeps that single entry point intact.
        """
        return []

    def _sync_flags_from_graph(self) -> list[str]:
        """Reconcile the in-memory flag set with the AUTHORITATIVE shared-graph
        snapshot, returning the flags that were newly absorbed (for one-time
        broadcast). This is the fix for the run-75379 split-brain (BUG②).

        Every worker writes each accepted flag to the shared graph via _accept_flag
        → shared_graph.flag_found, and the graph snapshot is what the UI / planner /
        finalize already trust. But _found_flags (the in-memory list _flags_complete
        reads) is fed ONLY from reaped `outcome.flags`, so a flag that reached the
        graph via a path that never delivered a clean outcome — a worker cancelled
        after it accepted a flag (reaped as CancelledError, line ~3615), an
        error-reaped worker, or the live-broadcast/DB-bridge path — stays invisible
        to the completion check. In run-75379 the graph held 4 valid flags (5 found,
        1 operator-invalidated) while _found_flags was stuck at 2, so _flags_complete()
        never fired and the run spawned ~55 post-solve waves until operator stop.

        Reconciling against snapshot().flags makes the graph the single source of
        truth for completion:
          - ADD any flag the graph holds but _found_flags is missing.
          - DROP any flag the operator explicitly INVALIDATED (snapshot already
            excludes it), so a blacklisted false positive (e.g. 090099b7) can never
            count toward expected_flags (BUG③ cross-check).
        Absent-from-snapshot-but-not-invalidated flags are LEFT in place: a silent
        flag_found DB-write failure (the `except: pass` in _accept_flag) must not
        let a genuinely-held flag vanish from the count."""
        if self.shared_graph is None:
            return []
        try:
            graph_flags = list(getattr(self.shared_graph.snapshot(), "flags", []) or [])
            invalidated = self.shared_graph.invalidated_flags()
        except Exception:
            return []
        # DROP operator-invalidated flags from the in-memory set (and never let one
        # back in below). reopen_after_false_positive removes it from the snapshot
        # too, so this only matters for a flag already absorbed before invalidation.
        if invalidated:
            self._found_flags = [f for f in self._found_flags if f not in invalidated]
        # ADD any authoritative flag the in-memory set is missing.
        fresh = self._record_flags(*(f for f in graph_flags if f not in invalidated))
        return fresh

    def _engine_healthcheck_cached(self, name: str, role: str) -> bool:
        """bool liveness for one engine, served from the shared health-probe cache
        (same TTL as _healthy_engines) so the race path doesn't re-shell a CLI we
        just verified on the coordinator path (or a prior dispatch). On a miss it
        probes once and caches the verdict."""
        startup_verdict = self._startup_health_verdict(name, role)
        if startup_verdict is not None:
            return startup_verdict[0]

        import time

        ttl = self._health_probe_ttl
        if ttl <= 0:
            return self._probe_engine_health(name, role)[0]
        now = time.monotonic()
        key = self._health_probe_key(name, role)
        cached = _health_cache_get(key, ttl, now)
        if cached is not None:
            return cached[0]
        ok, detail = self._probe_engine_health(name, role)
        _health_cache_put(key, ok, detail, now)
        return ok

    def _build_solvers(self) -> list:
        from muteki.solver.cli_driver import driver_for
        from muteki.solver.cli_solver import CliSolver

        def _healthy(name: str, role: str) -> bool:
            if getattr(self, "protocol2_session", None) is not None:
                # L0 forbids an unadmitted provider hello. Binary/profile/network
                # policy was checked statically; auth health is observed by the
                # first admitted attempt and receives ordinary UNKNOWN handling.
                return True
            return self._engine_healthcheck_cached(name, role)

        if self.cli_race:
            # race the configured engine roster (heterogeneous). Keep only the
            # engines whose healthcheck passes. ONE worker per healthy engine — independent of
            # the lineup size — so they genuinely race the same challenge (the
            # lineup specs only supply solver_id labels, cycled).
            engines = [e for e in self.engines if _healthy(e, "race")]
        else:
            # A failed selected engine is explicit; never substitute another CLI.
            if _healthy(self.cli_engine, "bootstrap"):
                engines = [self.cli_engine]
            else:
                engines = []

        # race mode → spec=None so each worker's id is cli-<engine> (distinct);
        # single mode → use the lineup spec so existing labels are preserved.
        specs = self.lineup or [None]
        # A race runs several solvers under one run; each solver's end is
        # worker-level (WORKER_FINISHED). _run_race emits the single run-level
        # RUN_FINISHED when the whole race settles, so 2 racers don't fire 2
        # run-level finishes (same conflation as the coordinator path).
        workers = []
        for i, engine in enumerate(engines):
            # resolve profile BEFORE charging budget (same #3 leak fix as
            # _make_cli_worker): a missing profile must `continue` WITHOUT having
            # incremented _spawned_total, else it leaks toward max_total_workers.
            role = "race" if self.cli_race else "bootstrap"
            profile = self._profile_for_engine(engine, role=role)
            if self.worker_profiles and profile is None:
                continue
            transport = base_engine_for_profile(profile or engine)
            if (self._context_requires_secure_prompt(engine=transport)
                    and not self._secure_prompt_candidate_ready(
                        engine, role=role)):
                # Secret delivery capability is a scheduling constraint, not a
                # failed spawn. Skip before charging the lifetime worker budget or
                # reserving one-shot context so a higher-priority Cursor profile
                # cannot starve a secure Claude/Codex racer.
                continue
            try:
                self._reserve_worker_spawn()
            except WorkerBudgetExhausted:
                break
            # solver_id labelling differs by mode:
            #  - race mode: spec=None, so without help every same-base-engine racer
            #    (e.g. 3 codex profiles each pinned to a different model) collapses
            #    onto one solver_id "cli-codex" and their event lanes /
            #    _active_profile_by_solver / account release maps overwrite each
            #    other. Apply the same _label_seq scheme as _make_cli_worker (the
            #    classic cli_race path bypasses it): first worker of a base engine
            #    keeps the bare "cli-<engine>" id (winner bookkeeping / existing
            #    tests), the rest get "-2", "-3", … . This is the bug fix.
            #  - single mode: the lineup spec already supplies a distinct solver_id
            #    label (preserved by passing solver_label=None → spec.solver_id wins
            #    in CliSolver). Don't override it.
            if self.cli_race:
                self._label_seq[transport] = self._label_seq.get(transport, 0) + 1
                n = self._label_seq[transport]
                label = f"cli-{transport}" if n == 1 else f"cli-{transport}-{n}"
                label += self._gen_suffix()
            else:
                label = f"cli-{transport}"
            expected_solver_id = (
                label if self.cli_race
                else str(getattr(specs[i % len(specs)], "solver_id", "") or label)
            )
            (typed_guidance, typed_context_reservations, typed_endpoint,
             typed_prompt_manifest) = (
                self._typed_context_for_worker(
                    worker_id=expected_solver_id, engine=transport))
            try:
                workdir = self._alloc_workdir(engine)
                container = self._container_for_engine(engine, profile)
                worker = CliSolver(
                    None if self.cli_race else specs[i % len(specs)],
                    self.challenge, bus=self.bus, cost=self.cost,
                    artifacts=self.artifacts, config=self.config, run_id=self.run_id,
                    insight=self.insight, knowledge=self.knowledge,
                    shared_graph=self.shared_graph, engine=transport,
                    driver=driver_for(profile or transport),
                    web_access=self.web_access, kb=self.kb,
                    workdir=workdir,
                    lifecycle_scope="worker",
                    solver_label=label if self.cli_race else None,
                    standing_guidance=typed_guidance,
                    hitl_cmd=({"action": "redirect", "url": typed_endpoint}
                              if typed_endpoint else None),
                    container=container,
                    worker_env=self._runtime_env_for(
                        transport, label, container=container, profile=profile),
                    identity=worker_identity_fields(profile),
                )
            except Exception as exc:
                rollback_ok = self._release_typed_context_reservations(
                    typed_context_reservations, expected_solver_id)
                for built in workers:
                    if not self._release_worker_account(built):
                        self._retain_worker_retirement_owner(
                            built, reason="race worker construction rollback")
                        rollback_ok = False
                if not rollback_ok:
                    raise ControlShutdownIncomplete(
                        "race worker construction rollback unconfirmed") from exc
                raise
            worker.engine = transport
            worker._pending_control_context_reservations = list(
                typed_context_reservations)
            worker._control_context_prompt_manifest = list(typed_prompt_manifest)
            worker._control_context_prompt_manifest_finalized = False
            worker._control_secret_values = self._take_context_secret_values(
                typed_context_reservations)
            worker._context_committer = getattr(self, "_context_committer", None)
            worker._context_releaser = getattr(self, "_context_releaser", None)
            worker._context_delivery_unknown_marker = getattr(
                self, "_context_delivery_unknown_marker", None)
            try:
                self._claim_worker_account(
                    worker.solver_id, transport, profile, role=role)
                self._register_control_worker(
                    worker, engine=transport, role=role,
                    intent_id=str(getattr(worker, "intent_id", "") or ""),
                )
            except Exception as exc:
                rollback_ok = self._release_worker_account(worker)
                if not rollback_ok:
                    self._retain_worker_retirement_owner(
                        worker, reason="race worker registration rollback")
                for built in workers:
                    if not self._release_worker_account(built):
                        self._retain_worker_retirement_owner(
                            built, reason="race worker registration rollback")
                        rollback_ok = False
                if not rollback_ok:
                    raise ControlShutdownIncomplete(
                        "race worker registration rollback unconfirmed") from exc
                raise
            workers.append(worker)
        return workers

    async def _reconcile_blackboard_skill(self) -> None:
        """Legacy hook kept for call-order compatibility.

        The skill is projected when each Worker builds its environment.  This hook
        deliberately performs no user-home writes.
        """
        return

    async def _emit_bb_bus(self, kind: str, **fields) -> None:
        """Emit one BLACKBOARD_DELTA from anywhere (finalize, resolve, etc.) — the
        coordinator loop has its own `_emit_bb` closure, but lifecycle transitions at
        run finish happen outside it and must still reach the JSONL/SSE stream the UI
        reads. Best-effort; a bus failure never masks the outcome."""
        if self.bus is None:
            return
        try:
            await self.bus.emit(Event(
                event_type=EventType.BLACKBOARD_DELTA, run_id=self.run_id,
                challenge_id=self.challenge.id,
                payload=blackboard_delta_payload(kind, actor="coordinator", **fields)))
        except Exception:
            pass

    async def _emit_finalize_lifecycle_deltas(self, result: dict, reason: str) -> None:
        """J/刀2: mirror release_claims_for_finalize's DB transitions onto the bus so
        the deck stops showing finalized intents as live. The DB write already
        happened (and was recorded as an event row in shared_graph); this re-emits it
        as a BLACKBOARD_DELTA the client reducer folds (intent_state_changed)."""
        if not isinstance(result, dict):
            return
        closed = [str(x) for x in (result.get("closed_intents") or []) if x]
        resumed = [str(x) for x in (result.get("resumed_intents") or []) if x]
        if closed:
            await self._emit_bb_bus(
                "intent_state_changed", intent_id=",".join(closed),
                dispatch_state="closed", close_reason="closed_by_solve",
                stop_reason="solved")
        if resumed:
            await self._emit_bb_bus(
                "intent_state_changed", intent_id=",".join(resumed),
                dispatch_state="resume", stop_reason=reason)

    _GRAPH_BRIDGE_KINDS = {
        "fact_added",
        "dead_end",
        "intent_proposed",
        "intent_claimed",
        "intent_concluded",
        "intent_state_changed",
        "flag_found",
        "poc_saved",
        "poc_claimed",
        "poc_concluded",
        "review_finding",
        "route_suppressed",
        "route_reopened",
        "branch_split",
        "branch_resolved",
        "coordinator_directive",
    }

    @staticmethod
    def _split_ids(value: Any) -> list[str]:
        return [x.strip() for x in str(value or "").split(",") if x.strip()]

    def _graph_event_to_bb(self, ev: dict) -> list[tuple[str, dict]]:
        seq = int(ev.get("seq") or 0)
        kind = str(ev.get("kind") or "")
        actor = str(ev.get("actor") or "")
        p = dict(ev.get("payload") or {})
        if kind == "fact_added":
            return [("fact_added", {
                "fact": p.get("fact", ""),
                "source": p.get("source", ""),
                "source_solver": p.get("source_solver") or actor,
                "verified": bool(ev.get("verified")),
                "confidence": ev.get("confidence", 1.0),
                "verifier": p.get("verifier", ""),
                "witness": p.get("witness", ""),
                "artifact_id": ev.get("artifact_id"),
                "fact_seq": seq,
                "route_hash": p.get("route_hash", ""),
                "intent_id": p.get("intent_id", ""),
            })]
        if kind == "dead_end":
            return [("dead_end", {
                "reason": p.get("reason", ""),
                "dead_end_seq": seq,
            })]
        if kind == "intent_proposed":
            fields = dict(p)
            fields["intent_id"] = p.get("intent_id", "")
            fields["goal"] = p.get("goal", "")
            fields["intent_seq"] = seq
            return [("intent_proposed", fields)]
        if kind == "intent_claimed":
            return [("intent_claimed", {
                "intent_id": p.get("intent_id", ""),
                "worker": actor,
                "intent_seq": seq,
            })]
        if kind == "intent_concluded":
            out = []
            for iid in self._split_ids(p.get("intent_id")):
                out.append(("intent_concluded", {
                    "intent_id": iid,
                    "worker": actor,
                    "result": p.get("result", ""),
                    "result_detail": p.get("result_detail", ""),
                    "to_fact_seq": p.get("to_fact_seq"),
                    "intent_seq": seq,
                }))
            return out
        if kind == "intent_state_changed":
            out = []
            for iid in self._split_ids(p.get("intent_id")):
                fields = dict(p)
                fields["intent_id"] = iid
                fields["intent_seq"] = seq
                out.append(("intent_state_changed", fields))
            return out
        if kind == "flag_found":
            fields = dict(p)
            fields["flag_seq"] = seq
            return [("flag_found", fields)]
        if kind in {"poc_saved", "poc_claimed", "poc_concluded"}:
            fields = dict(p)
            fields["seq"] = seq
            return [(kind, fields)]
        if kind == "review_finding":
            fields = dict(p)
            fields["seq"] = seq
            if "kind" in fields:
                fields["finding_kind"] = fields.pop("kind")
            return [("review_finding", fields)]
        if kind in {"route_suppressed", "route_reopened", "branch_split",
                    "branch_resolved", "coordinator_directive"}:
            fields = dict(p)
            fields["seq"] = seq
            return [(kind, fields)]
        return []

    async def _drain_graph_to_bus(self, *, emit_bb) -> None:
        if self.shared_graph is None:
            return
        try:
            events = self.shared_graph.events_since(
                self._last_graph_event_seq,
                kinds=sorted(self._GRAPH_BRIDGE_KINDS),
            )
        except Exception:
            return
        for ev in events:
            seq = int(ev.get("seq") or 0)
            emissions = self._graph_event_to_bb(ev)
            try:
                for kind, fields in emissions:
                    await emit_bb(kind, **fields)
            except Exception:
                fails = self._graph_bridge_failures.get(seq, 0) + 1
                self._graph_bridge_failures[seq] = fails
                if fails >= 3:
                    self._last_graph_event_seq = max(self._last_graph_event_seq, seq)
                    self._graph_bridge_failures.pop(seq, None)
                    continue
                return
            self._last_graph_event_seq = max(self._last_graph_event_seq, seq)
            self._graph_bridge_failures.pop(seq, None)

    async def _emit_run_finished(self, *, flag: "Optional[str]", solved: bool,
                                 reason: str = "finished") -> None:
        """Emit the ONE run-level RUN_FINISHED for this swarm run. Sub-workers emit
        WORKER_FINISHED (worker-level), so this is the single signal that flips the
        deck/rail to 'finished'. Best-effort: a bus failure must not mask the
        outcome the caller is about to return.

        Protocol 1 payloads carry `flag` (first, back-compat), `flags` (all
        collected), and `expected_flags`. Protocol 2 keeps those values private for
        finalization and publishes them only through typed reconciliation."""
        protocol2 = getattr(self, "protocol2_session", None)
        if protocol2 is not None:
            # V2 finalization is authoritative and happens before the legacy bus
            # projection. If receipts/rebuild/gate closure fail, RUN_FINISHED is
            # never emitted as solved and the Web wrapper surfaces a runtime error.
            await protocol2.finalize(solved=solved)
        self._cleanup_finished_worker_dirs()
        if self.bus is None:
            return
        try:
            runtime_meta = self._runtime_metadata_for()
            if protocol2 is None:
                # Protocol 1's aggregate projection is a compatibility contract.
                payload = {"flag": flag, "flags": list(self._found_flags),
                           "expected_flags": self._expected_flags(),
                           "multi_flag": self._multi_flag(),
                           "solved": solved,
                           "reason": reason,
                           **runtime_meta}
            else:
                # Protocol 2 accepted bytes are published only by the canonical
                # outbox/CAS reconciler as typed flag.accepted.  Finalization above
                # still consumes the private values; this legacy lifecycle projection
                # deliberately carries no flag aggregate.
                payload = {"expected_flags": self._expected_flags(),
                           "multi_flag": self._multi_flag(),
                           "solved": solved,
                           "reason": reason,
                           **runtime_meta}
            await self.bus.emit(Event(
                event_type=EventType.RUN_FINISHED, run_id=self.run_id,
                challenge_id=self.challenge.id,
                payload=payload))
        except Exception:
            pass

    async def _finalize_coordinator_run(
        self, *, winner: "Optional[str]", flag: "Optional[str]",
        goal_complete: bool, per_solver: "dict[str, SolveOutcome]",
        terminal_reason: str = "") -> None:
        """M11: persist the winner, close the shared graph (release the SQLite WAL/-shm
        handles), and emit the single run-level RUN_FINISHED (which also sweeps
        non-winner worker scratch dirs). Idempotent via _run_finalized — safe to call
        from BOTH the normal-return path and the coordinator's finally, so a cancelled
        / errored run still frees its DB handle and cleans scratch instead of leaking
        them (the cleanup used to sit AFTER the finally, on the normal path only)."""
        if self._run_finalized:
            return
        self._run_finalized = True
        # L3: detach the coordinator's bus sinks so a reused bus doesn't keep them.
        if self.bus is not None and self._coord_sinks:
            for sink in self._coord_sinks:
                try:
                    self.bus.remove_sink(sink)
                except Exception:
                    pass
            self._coord_sinks = []
        if winner is not None:
            self._persist_winner(
                per_solver.get(winner), flag, worker_id=str(winner or ""))
        pentest_product = (
            getattr(self.challenge, "mode", "ctf") == "pentest"
            and not self._pentest_flag_required()
        )
        if pentest_product:
            self._sync_findings_from_graph()
            self._sync_reports_from_graph()
            solved = bool(goal_complete) or self._findings_complete()
        else:
            solved = winner is not None or goal_complete or self._flags_complete()
        reason = (terminal_reason or "").strip()
        if not reason:
            if solved:
                if pentest_product:
                    reason = "goal_met"
                else:
                    reason = "solved" if winner is not None or self._flags_complete() else "goal_met"
            elif self._operator_stop:
                reason = "operator_stop"
            elif getattr(self, "_coverage_exhausted", False):
                reason = "coverage_complete"
            elif self._budget_exhausted_kind:
                reason = "budget_exhausted"
            else:
                reason = "runtime_failure"
        if self.shared_graph is not None:
            try:
                snap = self.shared_graph.snapshot()
                self._record_flags(*getattr(snap, "flags", []))
                self._record_findings(*list(getattr(snap, "findings", []) or []))
            except Exception:
                pass
            finalize_reason = (
                reason if reason in {
                    "solved", "goal_met", "operator_stop", "budget_exhausted",
                    "runtime_failure", "coverage_complete",
                }
                else ("solved" if solved else "runtime_failure"))
            try:
                fin = self.shared_graph.release_claims_for_finalize(  # type: ignore[attr-defined]
                    reason=finalize_reason)
                # 刀2: mirror the resume/closed transition onto the bus BEFORE close()
                # so the deck doesn't keep rendering these intents as live work.
                await self._emit_finalize_lifecycle_deltas(fin, finalize_reason)
                await self._drain_graph_to_bus(emit_bb=self._emit_bb_bus)
            except Exception:
                pass
            try:
                self.shared_graph.close()
            except Exception:
                pass
        if pentest_product:
            self._sync_findings_from_graph()
            solved = bool(goal_complete) or self._findings_complete()
        else:
            solved = winner is not None or goal_complete or self._flags_complete()
        if solved and (not terminal_reason or reason == "runtime_failure"):
            if pentest_product:
                reason = "goal_met"
            else:
                reason = "solved" if winner is not None or self._flags_complete() else "goal_met"
        finish_flag = self._found_flags[0] if self._found_flags else (
            flag if winner is not None else None)
        await self._emit_run_finished(flag=finish_flag, solved=solved,
                                      reason=reason)

    def _retain_control_shutdown_owner(
        self, *, winner: "Optional[str]", flag: "Optional[str]",
        goal_complete: bool, per_solver: "dict[str, SolveOutcome]",
    ) -> None:
        """Persist finalization inputs while a fenced control orphan still owns state."""
        self._deferred_control_finalization = {
            "winner": winner,
            "flag": flag,
            "goal_complete": bool(goal_complete),
            "per_solver": dict(per_solver),
        }

    async def settle_control_shutdown(self) -> None:
        """Wait for retained control owners, then perform the previously-forbidden teardown.

        This is intentionally separate from ``run()``: Web owns it as a durable
        cleanup task so the request loop remains available while a hostile callback
        takes an arbitrary amount of time to leave.
        """
        while True:
            context_pending = False
            for _key, owner in list(getattr(
                    self, "_context_cleanup_owners", {}).items()):
                reservations, worker_id = owner
                if not self._release_typed_context_reservations(
                        list(reservations), str(worker_id)):
                    context_pending = True
            control_owned = tuple(
                task for task in getattr(self, "_control_orphan_tasks", set())
                if not task.done())
            worker_owned: list[asyncio.Task[Any]] = []
            for _sid, owner in list(getattr(
                    self, "_worker_runtime_owners", {}).items()):
                solver, intent_id, reason, lane_key = owner
                if self._worker_runtime_exit_confirmed(solver):
                    if self._finish_worker_retirement(
                            solver, intent_id=intent_id, reason=reason,
                            lane_key=lane_key):
                        continue
                # A done/cancelled/failed task is not exit proof. Rebuild it from
                # the retained solver owner and wait for the real runtime fence.
                worker_owned.append(self._ensure_worker_runtime_reaper(
                    solver, intent_id=intent_id, reason=reason,
                    lane_key=lane_key))
            owned = (*control_owned, *worker_owned)
            if not owned and not context_pending:
                break
            if not owned:
                await asyncio.sleep(0.05)
                continue
            await asyncio.gather(
                *(asyncio.shield(task) for task in owned),
                return_exceptions=True)
        if getattr(self, "_worker_runtime_owners", {}):
            self._worker_runtime_incomplete = True
            self._mark_shutdown_incomplete("worker_runtime")
            raise ControlShutdownIncomplete(
                "worker runtime exit could not be proven")
        if getattr(self, "_context_cleanup_owners", {}):
            self._context_cleanup_incomplete = True
            self._mark_shutdown_incomplete("context_cleanup")
            raise ControlShutdownIncomplete(
                "context reservation release could not be proven")
        deferred = dict(getattr(
            self, "_deferred_control_finalization", {}) or {})
        # Container absence is part of the runtime exit proof, not post-finalize
        # housekeeping. Prove it before closing the graph or emitting RUN_FINISHED.
        if self.worker_backend == "container" or self._container_handle is not None:
            from muteki.solver.container_exec import teardown_container
            removed = await asyncio.to_thread(
                teardown_container, self.run_id, remove=True)
            if removed is not True:
                self._mark_shutdown_incomplete("container_absence")
                raise ControlShutdownIncomplete(
                    "container teardown could not be proven")
        self._shutdown_incomplete_causes.clear()
        self._control_shutdown_incomplete = False
        self._worker_runtime_incomplete = False
        self._context_cleanup_incomplete = False
        await self._finalize_coordinator_run(
            winner=deferred.get("winner"), flag=deferred.get("flag"),
            goal_complete=bool(deferred.get("goal_complete", False)),
            per_solver=dict(deferred.get("per_solver") or {}),
            terminal_reason="runtime_failure",
        )
        self._deferred_control_finalization = None

    def _cleanup_finished_worker_dirs(self) -> None:
        """Remove failed/finished worker scratch while preserving durable run data.

        The workspace root keeps shared/, inputs/, graph/, final/, manifest.json,
        and winner.json. Only non-winner worker cwd directories under workers/ are
        removed at run finish to avoid long coordinator runs accumulating hundreds
        of duplicate scratch trees.
        """
        if self.worker_root is None:
            return
        winner_workdir_name = str(
            getattr(self, "_winner_workdir_name", "") or ""
        ).strip()
        keep = [winner_workdir_name] if winner_workdir_name else []
        cleanup_worker_scratch(self.worker_root, keep=keep)

    @staticmethod
    def _control_scope_parts(target: str) -> tuple[str, str]:
        raw = str(target or "global").strip() or "global"
        if ":" not in raw:
            return ("global", "") if raw == "global" else ("worker", raw)
        kind, value = raw.split(":", 1)
        return kind.strip().lower(), value.strip()

    def _control_target_solvers(self, target: str) -> list[Any]:
        """Resolve a legacy target against the process-local live registry."""
        kind, value = self._control_scope_parts(target)
        workers = list(getattr(self, "_live_solvers", {}).values())
        if kind in {"global", "run"}:
            return workers
        if kind == "challenge":
            return workers if value == self.challenge.id else []
        if kind in {"worker", "solver"}:
            return [w for w in workers
                    if str(getattr(w, "solver_id", "")) == value]
        if kind == "engine":
            return [w for w in workers
                    if str(getattr(w, "engine", "")) == value]
        if kind == "intent":
            return [w for w in workers
                    if str(
                        getattr(w, "intent_id_assigned", "")
                        or getattr(w, "_intent_id", "")
                        or getattr(w, "intent_id", "")
                    ) == value]
        if kind == "lane":
            return [w for w in workers
                    if str(getattr(w, "lane", "")) == value]
        return []

    def _resolve_control_text(self, text: Any) -> str:
        """Materialise secret references transiently, at worker delivery only."""
        value = str(text or "")
        if not value.startswith("secret://"):
            return value
        resolver = getattr(self, "_secret_resolver", None)
        if not callable(resolver):
            return value
        try:
            return str(resolver(value))
        except Exception:
            # Never leak resolver details (which may contain the secret/path) into
            # a command receipt.  The unresolved opaque ref is safe to retain.
            return value

    def _materialize_reserved_control_text(self, text: Any) -> str:
        """Resolve a reserved prompt secret, failing closed on any ambiguity."""
        value = str(text or "")
        if not value.startswith("secret://"):
            return value
        resolver = getattr(self, "_secret_resolver", None)
        if not callable(resolver):
            raise RuntimeError("reserved secret material is unavailable")
        try:
            resolved = str(resolver(value) or "")
        except Exception:
            raise RuntimeError("reserved secret material is unavailable") from None
        if not resolved or resolved.startswith("secret://"):
            raise RuntimeError("reserved secret material is unavailable")
        return resolved

    @staticmethod
    def _ack_control(
        cmd: dict[str, Any],
        *,
        state: str,
        detail: str,
        target_ids: Optional[list[str]] = None,
        metadata: Optional[dict[str, Any]] = None,
    ) -> None:
        future = cmd.get("_control_ack")
        if future is None:
            return
        try:
            if future.done() or future.cancelled():
                return
            future.set_result({
                "state": state,
                "detail": detail,
                "target_ids": list(target_ids or []),
                "metadata": dict(metadata or {}),
            })
        except Exception:
            pass

    def _set_control_frozen(self, target: str, frozen: bool) -> tuple[list[str], int]:
        selected = self._control_target_solvers(target)
        confirmed: list[str] = []
        failures = 0
        for worker in selected:
            sid = str(getattr(worker, "solver_id", "") or "")
            setter = getattr(worker, "_set_paused", None)
            if not callable(setter):
                failures += 1
                continue
            try:
                signalled = setter(frozen)
                if (signalled is not False
                        and bool(getattr(worker, "_paused", False)) is frozen):
                    confirmed.append(sid)
                    self._update_control_worker_status(
                        sid, "frozen" if frozen else "running")
                else:
                    failures += 1
            except Exception:
                failures += 1
        return confirmed, failures

    def _control_paused_ids(self, target: str) -> list[str]:
        """Observed process-level freeze projection for truthful partial receipts."""
        return [
            str(getattr(worker, "solver_id", "") or "")
            for worker in self._control_target_solvers(target)
            if bool(getattr(worker, "_paused", False))
        ]

    def _contain_unfrozen_control_workers(self, target: str) -> list[str]:
        """Fail-closed fallback when signal compensation cannot be proven.

        A worker that cannot be returned to the canonical frozen state is asked to
        terminate through the normal runtime cancellation path.  The receipt still
        remains PARTIAL—cancellation request is not process-exit proof—but no such
        worker is knowingly allowed to continue consuming budget outside control.
        """
        requested: list[str] = []
        for worker in self._control_target_solvers(target):
            if bool(getattr(worker, "_paused", False)):
                continue
            sid = str(getattr(worker, "solver_id", "") or "")
            if self._cancel_solver(worker):
                requested.append(sid)
                # A successful cancel() call is a request, not process-exit proof.
                self._update_control_worker_status(sid, "cancel_requested")
        return requested

    def _lease_scope_for_control(self, target: str) -> tuple[str, str]:
        kind, value = self._control_scope_parts(target)
        if kind == "solver":
            kind = "worker"
        if kind == "global":
            return "challenge", self.challenge.id
        if kind == "run":
            return "run", value or self.run_id
        return kind, value

    def _begin_operator_help_freeze(self) -> bool:
        """Transactionally freeze a NEED_INPUT wait across process/lease/budget.

        Returns True only when this helper acquired the suspension. An existing
        explicit operator FREEZE remains owned by its original command and must not
        be thawed by the help-wait bracket.
        """
        if self._control_frozen:
            return False
        freeze_key = "__help__"
        if self._freeze_suspensions:
            raise RuntimeError("another freeze scope is already active")
        confirmed, failures = self._set_control_frozen("global", True)
        if failures:
            _rolled_back, rollback_failures = self._set_control_frozen(
                "global", False)
            if rollback_failures:
                self._contain_unfrozen_control_workers("global")
            raise RuntimeError("operator-help worker freeze was not confirmed")
        help_ids = [
            str(h.get("request_id") or h.get("id") or "")
            for h in self._pending_help
        ]
        help_digest = hashlib.sha256(str(help_ids).encode()).hexdigest()[:16]
        suspension_id = f"help:{self.run_id}:{help_digest}"
        if self.shared_graph is None:
            self._set_control_frozen("global", False)
            raise RuntimeError("operator-help lease graph is unavailable")
        try:
            self.shared_graph.suspend_active_leases(
                actor="control", suspension_id=suspension_id,
                scope_kind="challenge", scope_id=self.challenge.id,
                # Operational infinity: the append-only suspension remains the
                # owner until an explicit resume event, without a 7-day reclaim
                # cliff during an unattended operator wait.
                guard_s=1_000_000_000_000.0, reason="operator help wait")
        except Exception as exc:
            _rolled_back, rollback_failures = self._set_control_frozen(
                "global", False)
            if rollback_failures:
                self._contain_unfrozen_control_workers("global")
            raise RuntimeError("operator-help lease guard failed") from exc
        import time
        started = time.monotonic()
        self._freeze_suspensions[freeze_key] = suspension_id
        self._freeze_started_at[freeze_key] = started
        self._control_frozen = True
        self._operator_paused = True
        if self._budget_suspend_started is None:
            self._budget_suspend_started = started
        return True

    def _end_operator_help_freeze(self, *, reason: str) -> None:
        """Restore a help-owned suspension; retain it on any failed fence."""
        freeze_key = "__help__"
        suspension_id = self._freeze_suspensions.get(freeze_key, "")
        if not suspension_id:
            return
        confirmed, failures = self._set_control_frozen("global", False)
        if failures:
            _refrozen, refreeze_failures = self._set_control_frozen(
                "global", True)
            if refreeze_failures:
                self._contain_unfrozen_control_workers("global")
            raise RuntimeError("operator-help worker thaw was not confirmed")
        import time
        started = self._freeze_started_at.get(freeze_key)
        duration = max(0.0, time.monotonic() - started) if started else 0.0
        try:
            if self.shared_graph is None:
                raise RuntimeError("operator-help lease graph is unavailable")
            self.shared_graph.resume_suspended_leases(
                actor="control", suspension_id=suspension_id,
                duration_s=duration, reason=reason)
        except Exception as exc:
            _refrozen, refreeze_failures = self._set_control_frozen(
                "global", True)
            if refreeze_failures:
                self._contain_unfrozen_control_workers("global")
            raise RuntimeError("operator-help lease restoration failed") from exc
        self._freeze_suspensions.pop(freeze_key, None)
        self._freeze_started_at.pop(freeze_key, None)
        self._control_frozen = False
        self._operator_paused = False
        if self._budget_suspend_started is not None:
            self._budget_suspended_total += max(
                0.0, time.monotonic() - self._budget_suspend_started)
            self._budget_suspend_started = None

    def _control_continuation_id(
        self, command_id: str, *, required_when_unavailable: bool = False,
    ) -> str:
        """Return the exact context edge for a command without mutating the graph.

        ``required_when_unavailable`` is used only when command semantics already
        prove that this is an exact replacement edge (for example a decision
        answer or worker-scoped context).  A missing provider must then fail closed
        onto the deterministic id; it must never turn exact context into a global
        InsightBus broadcast.
        """
        if not command_id:
            return ""
        from muteki.control import (
            context_resource_id_for_command,
            continuation_intent_id_for_command,
        )
        context_id = context_resource_id_for_command(command_id)
        intent_id = continuation_intent_id_for_command(command_id)
        provider = getattr(self, "_context_provider", None)
        if not callable(provider):
            return intent_id if required_when_unavailable else ""
        try:
            try:
                resources = provider(active_only=False)
            except TypeError:
                resources = provider()
            resource = next(
                (row for row in resources
                 if str(getattr(row, "context_id", "")) == context_id),
                None,
            )
            if resource is None or str(
                    getattr(resource, "scope", "") or "") != f"intent:{intent_id}":
                return intent_id if required_when_unavailable else ""
            return intent_id
        except Exception:
            # A configured durable provider that is transiently unreadable cannot
            # prove the command is global. Fail closed onto its deterministic edge;
            # proposal/status checks below will return UNKNOWN, never broadcast.
            return intent_id

    def _propose_control_continuation(
        self, *, command_id: str, action: str, target: str,
    ) -> str:
        """Materialize the exact graph edge backing worker-scoped context.

        The context row is already durable when the runtime port is called.  This
        method deliberately inspects only its id/scope (never secret content) and
        creates the deterministic open intent that a replacement worker can claim.
        """
        intent_id = self._control_continuation_id(command_id)
        if not intent_id:
            return ""
        if self.shared_graph is None:
            return ""
        try:
            from muteki.control import context_resource_id_for_command
            context_id = context_resource_id_for_command(command_id)
            status_provider = getattr(self, "_context_status_provider", None)
            if callable(status_provider):
                if status_provider(context_id) != "active":
                    return ""
            else:
                provider = getattr(self, "_context_provider", None)
                if not callable(provider) or not any(
                    str(getattr(row, "context_id", "")) == context_id
                    for row in provider()
                ):
                    return ""
            self.shared_graph.propose_intent(
                actor="operator",
                intent_id=intent_id,
                goal=("Continue the blocked execution using the operator-provided "
                      f"context for control command {command_id}."),
                payload={
                    "source": "operator_continuation",
                    "source_command_id": command_id,
                    "action": action,
                    "requested_scope": target,
                    "priority": "operator",
                    "worker_class": "shell_agent",
                },
            )
            return intent_id
        except Exception:
            return ""

    async def _reconcile_standing_guidance(self) -> list[str]:
        """Drain durable clear/reset commands into the shared graph exactly once.

        Typed context and the evidence graph intentionally use separate SQLite
        stores. The control command is therefore an outbox record; the graph's
        ``apply_standing_clear`` transaction stores directive tombstones and its
        command-id marker together. Any crash point is repaired on the next Swarm
        start without replaying later operator guidance.
        """
        operation_provider = getattr(self, "_standing_clear_provider", None)
        if not callable(operation_provider):
            return []
        operations = list(operation_provider())
        if not operations:
            return []
        if self.shared_graph is None:
            raise RuntimeError("standing-clear reconciliation graph is unavailable")
        context_provider = getattr(self, "_context_provider", None)
        context_expirer = getattr(self, "_context_expirer", None)
        applied_commands: list[str] = []

        for operation in operations:
            def _value(name: str, default: Any = "") -> Any:
                if isinstance(operation, dict):
                    return operation.get(name, default)
                return getattr(operation, name, default)

            command_id = str(_value("command_id") or "").strip()
            if not command_id:
                raise RuntimeError("standing-clear outbox record has no command_id")
            action = str(_value("action", "clear_standing") or "clear_standing")
            actor = str(_value("actor", "operator") or "operator")
            exact_text = str(_value("text") or "").strip()
            cutoff_raw = _value("cutoff_before", None)
            cutoff_before = (
                float(cutoff_raw) if cutoff_raw is not None else None
            )
            eligible_ids = {
                str(value or "").strip()
                for value in (_value("eligible_standing_command_ids", ()) or ())
                if str(value or "").strip()
            }

            def _matching_context(resource: Any) -> bool:
                if not bool(getattr(resource, "standing", False)):
                    return False
                if exact_text and str(
                        getattr(resource, "content", "") or "") != exact_text:
                    return False
                metadata = dict(getattr(resource, "metadata", {}) or {})
                source_id = str(
                    metadata.get("source_command_id") or "").strip()
                if source_id:
                    # Closed-set eligibility is the concurrent recovery fence.
                    # Unknown ids may have committed after the outbox snapshot.
                    return source_id in eligible_ids
                return cutoff_before is None or float(
                    getattr(resource, "created_at", 0.0) or 0.0
                ) < cutoff_before

            # Context revocation is the first half of the absence guarantee. A
            # crash directly after PERSISTED (before ControlActor's companion
            # pass) is repaired here as well. The next-command cutoff protects
            # standing context added after this clear.
            if callable(context_provider):
                active_resources = list(context_provider())
                matching = [
                    resource for resource in active_resources
                    if _matching_context(resource)
                ]
                if matching and not callable(context_expirer):
                    raise RuntimeError(
                        "standing-clear context expirer is unavailable")
                for resource in matching:
                    context_expirer(
                        str(getattr(resource, "context_id", "") or ""),
                        actor=actor, reason=f"{action}:startup-reconcile")
                remaining = [
                    resource for resource in context_provider()
                    if _matching_context(resource)
                ]
                if remaining:
                    raise RuntimeError(
                        "standing-clear context expiration was not confirmed")

            result = self.shared_graph.apply_standing_clear(
                command_id=command_id,
                actor=actor,
                text="" if exact_text.startswith("secret://") else exact_text,
                cutoff_before=cutoff_before,
                eligible_command_ids=sorted(eligible_ids),
                match_by_source_ids=exact_text.startswith("secret://"),
            )
            if not bool(result.get("already_applied", False)):
                applied_commands.append(command_id)
                try:
                    await self._emit_coord_bb(
                        "operator_directive_changed",
                        action=action,
                        command_id=command_id,
                        recovered=True,
                        expired_directives=list(
                            result.get("expired_directives") or []),
                    )
                except Exception:
                    # Graph state is canonical and replayable; telemetry is only a
                    # projection and must never roll back or mask reconciliation.
                    pass

        # Rebuild the volatile prompt projection from the now-canonical graph so
        # a bounded replay retains standing guidance added by later commands.
        try:
            self._standing_guidance = [
                str(row.get("text") or "")
                for row in self.shared_graph.operator_directives(active_only=True)
                if row.get("standing") and row.get("text")
            ][-_STANDING_MAX:]
        except Exception:
            # Failure to rebuild a volatile projection is safe: new workers also
            # read the canonical graph, and startup teardown remains fenced by run().
            pass
        return applied_commands

    async def _reconcile_control_continuations(self) -> list[str]:
        """Repair the durable context -> graph outbox edge after crash/offline.

        Context lives in the control journal while intents live in the evidence
        graph, so they cannot share one SQLite transaction.  On every Swarm start we
        deterministically replay only the selector metadata (never context/secret
        content); graph dedupe makes this idempotent.
        """
        provider = getattr(self, "_context_provider", None)
        if not callable(provider) or self.shared_graph is None:
            return []
        repaired: list[str] = []
        retired: list[tuple[str, str]] = []
        try:
            active_resources = list(provider())
        except Exception:
            return []
        try:
            resources = list(provider(active_only=False))
        except TypeError:
            resources = active_resources
        except Exception:
            resources = active_resources
        active_ids = {
            str(getattr(resource, "context_id", "") or "")
            for resource in active_resources
        }
        status_provider = getattr(self, "_context_status_provider", None)
        try:
            open_ids = {
                str(row.get("intent_id") or "") for row in self._open_intents()
            }
        except Exception:
            open_ids = set()
        state_reader = getattr(self.shared_graph, "intent_claim_state", None)
        from muteki.control import continuation_intent_id_for_command
        for resource in resources:
            try:
                metadata = dict(getattr(resource, "metadata", {}) or {})
                command_id = str(metadata.get("source_command_id") or "")
                if not command_id:
                    continue
                intent_id = continuation_intent_id_for_command(command_id)
                if str(getattr(resource, "scope", "") or "") != f"intent:{intent_id}":
                    continue
                context_id = str(getattr(resource, "context_id", "") or "")
                if context_id not in active_ids:
                    status = "inactive"
                    if callable(status_provider):
                        try:
                            status = str(status_provider(context_id) or status)
                        except Exception:
                            pass
                    if intent_id in open_ids:
                        # A finite/expired/unknown edge can no longer be delivered.
                        # Close it append-only instead of leaving an operator-priority
                        # intent to churn through RequiredContextUnavailable forever.
                        self.shared_graph.conclude_intent(
                            actor="coordinator", intent_id=intent_id,
                            result=f"context_{status}",
                            result_detail=(
                                "operator continuation retired during recovery: "
                                f"context delivery status={status}"),
                        )
                        retired.append((intent_id, status))
                        open_ids.discard(intent_id)
                    continue
                # Reconciliation is a live outbox poll as well as a startup pass.
                # Do not emit a synthetic "recovered" event on every coordinator
                # tick for an edge that is already materialized (open, claimed, or
                # terminal). A missing row is the only repairable postcondition.
                if callable(state_reader):
                    try:
                        if dict(state_reader(intent_id) or {}):
                            continue
                    except Exception:
                        # An unreadable postcondition is not permission to claim a
                        # recovery. Let the next live tick retry the read.
                        continue
                elif intent_id in open_ids:
                    continue
                self.shared_graph.propose_intent(
                    actor="control-recovery",
                    intent_id=intent_id,
                    goal=("Continue the blocked execution using the operator-provided "
                          f"context for control command {command_id}."),
                    payload={
                        "source": "operator_continuation_recovery",
                        "source_command_id": command_id,
                        "action": str(metadata.get("action") or "context"),
                        "requested_scope": str(metadata.get("command_scope") or ""),
                        "priority": "operator",
                        "worker_class": "shell_agent",
                    },
                )
                if callable(state_reader):
                    try:
                        if not dict(state_reader(intent_id) or {}):
                            continue
                    except Exception:
                        continue
                repaired.append(intent_id)
                open_ids.add(intent_id)
            except Exception:
                continue
        for intent_id in repaired:
            try:
                await self._emit_coord_bb(
                    "intent_proposed", intent_id=intent_id,
                    goal="recovered operator-scoped continuation",
                    recovered=True)
            except Exception:
                # Graph writes above are canonical. Blackboard/UI projection is
                # replayable telemetry and cannot invalidate recovery.
                pass
        for intent_id, status in retired:
            try:
                await self._emit_coord_bb(
                    "intent_state_changed", intent_id=intent_id,
                    dispatch_state="closed", recovered=True,
                    reason=f"context_{status}")
            except Exception:
                pass
        return repaired

    async def _supervise_control_drain(self) -> None:
        """Keep the control inbox available after a command-level watchdog abort.

        A claimed command callback is allowed to be arbitrary runtime code and can
        suppress ``CancelledError``.  A watchdog therefore fences the old epoch,
        but deliberately does *not* run a replacement mutator concurrently with a
        still-live stale handler. Availability degrades to UNKNOWN while that owner
        drains; linearizability never degrades. Cancelling the supervisor retains
        ownership until all such coroutines actually exit.
        """
        restart_event = asyncio.Event()
        self._control_restart_event = restart_event
        self._control_consumer_epoch = int(
            getattr(self, "_control_consumer_epoch", 0) or 0)
        orphans: set[asyncio.Task[Any]] = set()
        self._control_orphan_tasks = orphans
        child: Optional[asyncio.Task[Any]] = None
        restart_wait: Optional[asyncio.Task[Any]] = None

        def _retire(task: asyncio.Task[Any]) -> None:
            orphans.discard(task)
            try:
                task.result()
            except BaseException:
                pass
            if not orphans:
                self._clear_shutdown_incomplete("hitl_orphan")

        try:
            while True:
                restart_event.clear()
                epoch = int(self._control_consumer_epoch)
                child = asyncio.create_task(
                    self._drain_hitl(consumer_epoch=epoch),
                    name=f"hitl-drain-envelope-{epoch}",
                )
                restart_wait = asyncio.create_task(
                    restart_event.wait(),
                    name=f"hitl-drain-restart-{epoch}",
                )
                done, _pending = await asyncio.wait(
                    {child, restart_wait},
                    return_when=asyncio.FIRST_COMPLETED,
                )
                if restart_wait in done:
                    # Publish the dequeue fence before cancelling the old child.
                    # Do not start a replacement until this handler exits: epoch
                    # checks cannot roll back mutations it performs after an await,
                    # so concurrent generations would allow stale PAUSE to land
                    # after a newer RESUME while both receipts looked successful.
                    self._control_consumer_epoch = epoch + 1
                    restart_event.clear()
                    # QueueControlPort normally issued the first cancellation before
                    # setting this event.  Do not issue a second one while the child
                    # is handling/suppressing that exception: a duplicate cancel
                    # would accidentally make the adversarial callback cooperative
                    # and hide the very failure mode this generation fence handles.
                    if not child.done() and child.cancelling() == 0:
                        child.cancel()
                    orphans.add(child)
                    child.add_done_callback(_retire)
                    restart_wait = None
                    try:
                        await asyncio.shield(child)
                    except asyncio.CancelledError:
                        current = asyncio.current_task()
                        if current is not None and current.cancelling():
                            raise
                        # Cooperative child cancellation; ownership is released.
                    except Exception:
                        pass
                    child = None
                    continue

                restart_wait.cancel()
                await asyncio.gather(restart_wait, return_exceptions=True)
                restart_wait = None
                try:
                    child.result()
                except asyncio.CancelledError:
                    # A child-only cancel from a legacy caller is treated like a
                    # restart.  Supervisor cancellation is handled by the outer
                    # cancellation branch below.
                    self._control_consumer_epoch = epoch + 1
                    child = None
                    await asyncio.sleep(0)
                    continue
                except Exception:
                    self._control_consumer_epoch = epoch + 1
                    child = None
                    await asyncio.sleep(0)
                    continue
                child = None
                return
        except asyncio.CancelledError:
            # Fence every consumer owned by this supervisor before asking it to
            # stop.  A shutdown-suppressing child can finish its current envelope,
            # but can never claim another one if this Swarm is resumed/restarted.
            self._control_consumer_epoch = int(
                getattr(self, "_control_consumer_epoch", 0) or 0) + 1
            if restart_wait is not None:
                restart_wait.cancel()
            if child is not None and not child.done():
                if child not in orphans:
                    orphans.add(child)
                    child.add_done_callback(_retire)
                child = None
            owned_orphans = tuple(orphans)
            for orphan in owned_orphans:
                if not orphan.done():
                    orphan.cancel()
            if restart_wait is not None:
                await asyncio.gather(restart_wait, return_exceptions=True)
                restart_wait = None
            # Epoch fencing restores availability, but it is not an exit proof. Wait
            # only a bounded interval so server shutdown cannot hang forever. If an
            # adversarial callback still suppresses cancellation, retain ownership
            # and surface an explicit incomplete state; the coordinator then refuses
            # to finalize the graph underneath it.
            pending: set[asyncio.Task[Any]] = set()
            if owned_orphans:
                _done, pending = await asyncio.wait(
                    owned_orphans,
                    timeout=max(0.0, float(getattr(
                        self, "control_shutdown_timeout", 2.0))),
                )
            if pending:
                self._mark_shutdown_incomplete("hitl_orphan")
                raise ControlShutdownIncomplete(
                    f"{len(pending)} control handler(s) still own runtime state")
            raise
        finally:
            if getattr(self, "_control_restart_event", None) is restart_event:
                self._control_restart_event = None

    async def _drain_hitl(self, *, consumer_epoch: Optional[int] = None) -> None:
        """Background: pull human commands off hitl_inbox and broadcast them to
        every solver via the InsightBus. Runs until cancelled. Each item is a
        dict {target, action, text} (the shape RunManager.post_hitl enqueues)."""
        if self.hitl_inbox is None:
            return
        if consumer_epoch is None:
            consumer_epoch = int(
                getattr(self, "_control_consumer_epoch", 0) or 0)
        while True:
            if consumer_epoch != int(
                    getattr(self, "_control_consumer_epoch", 0) or 0):
                return
            cmd = await self.hitl_inbox.get()
            if consumer_epoch != int(
                    getattr(self, "_control_consumer_epoch", 0) or 0):
                # Epoch changed while this consumer was blocked in Queue.get().
                # Return it before balancing the old claim so Queue.join never
                # observes a transient zero while the command is still pending;
                # never let a stale generation apply it.
                await self.hitl_inbox.put(cmd)
                try:
                    self.hitl_inbox.task_done()
                except Exception:
                    pass
                return
            try:
                if not isinstance(cmd, dict):
                    continue
                deadline = cmd.get("_control_deadline")
                expired = False
                try:
                    expired = deadline is not None and asyncio.get_running_loop().time() >= float(deadline)
                except (TypeError, ValueError):
                    expired = True
                if cmd.get("_control_cancel_requested") or expired:
                    self._ack_control(
                        cmd, state="unknown",
                        detail="control envelope expired before runtime application",
                        metadata={"code": "cancelled_before_apply"})
                    continue
                # Synchronous claim fence paired with QueueControlPort: once this
                # flips, the producer may no longer conclude UNKNOWN merely from a
                # timeout. It waits for the real terminal ACK, preventing a command
                # from executing after its journal was already closed unknown.
                cmd["_control_started"] = True
                cmd["_control_consumer_task"] = asyncio.current_task()
                restart = getattr(self, "_control_restart_event", None)
                if isinstance(restart, asyncio.Event):
                    cmd["_control_restart_event"] = restart
                cmd["_control_consumer_epoch"] = consumer_epoch
                payload = cmd.get("payload") if isinstance(cmd.get("payload"), dict) else {}
                # ``text`` remains the durable/safe representation (possibly a
                # secret:// reference). ``delivery_text`` exists transiently and is
                # used only for in-memory worker injection.
                text = (cmd.get("text") or cmd.get("hint")
                        or payload.get("text") or payload.get("hint") or "")
                # Keep secret:// opaque throughout the coordinator/bus. Plaintext is
                # materialised only after a context reservation, while constructing
                # the one worker prompt that is allowed to receive it.
                delivery_text = str(text or "")
                action = str(cmd.get("action") or "hint").strip().lower()
                original_action = action
                # A decision answer is ordinary operator guidance at the existing
                # single-shot runtime boundary, but remains typed as
                # answer_decision in the durable command journal.
                if action in ("answer_decision", "submit"):
                    action = "hint"
                elif action == "add_context":
                    raw_context = cmd.get("context")
                    if isinstance(raw_context, dict):
                        text = str(raw_context.get("content") or text)
                        cmd.setdefault("standing", bool(raw_context.get("standing", False)))
                    elif isinstance(raw_context, str):
                        text = raw_context
                    delivery_text = str(text or "")
                    action = "hint"
                elif action == "resume" and self._control_frozen:
                    # Back-compatible resume after an emergency freeze performs a
                    # real thaw; otherwise desired state would say ACTIVE while
                    # subprocess groups and guarded leases remained frozen.
                    action = "thaw"
                target = str(cmd.get("target") or "global").strip() or "global"
                request_id = str(cmd.get("request_id") or payload.get("request_id") or "")
                if original_action in (
                    "answer_decision", "submit", "dismiss", "dismiss_help"
                ) and request_id:
                    for pending in self._pending_help:
                        if str(pending.get("request_id") or pending.get("id") or "") != request_id:
                            continue
                        pending_worker = str(pending.get("worker") or "").strip()
                        if pending_worker:
                            target = f"solver:{pending_worker}"
                        break
                command_id = str(cmd.get("command_id") or "")
                # This Swarm instance owns exactly one challenge.  A challenge-scoped
                # command is therefore run-wide; solver/engine/intent scopes are not.
                scope_kind, scope_value = self._control_scope_parts(target)
                # A control envelope is already tied to this Swarm instance.  Do
                # not let a syntactically valid selector for another run/challenge
                # degrade into a run-wide command merely because its *kind* is
                # broad.  This check belongs at the final application boundary as
                # well as admission: legacy callers can bypass the typed HTTP API.
                if ((scope_kind == "run" and scope_value != self.run_id)
                        or (scope_kind == "challenge"
                            and scope_value != self.challenge.id)):
                    self._ack_control(
                        cmd, state="failed",
                        detail="control scope does not belong to this runtime",
                        metadata={"code": "scope_mismatch"})
                    continue
                context_actions = {
                    "ask", "hint", "focus", "redirect", "directive",
                    "correction", "add_context", "answer_decision", "submit",
                }
                semantic_exact_context = bool(
                    command_id and delivery_text
                    and original_action in context_actions
                    and (
                        original_action in {"answer_decision", "submit"}
                        or scope_kind in {"worker", "solver"}
                    )
                )
                required_continuation_id = self._control_continuation_id(
                    command_id,
                    required_when_unavailable=semantic_exact_context,
                )
                if required_continuation_id and not self.coordinator:
                    # A fixed non-coordinator race has no dispatcher after its
                    # initial worker batch. Persisting an exact intent there would
                    # strand the context while falsely reporting success. Keep the
                    # resource durable for a later coordinator resolve, but make the
                    # current effect explicitly unsupported/unknown.
                    self._ack_control(
                        cmd, state="unknown",
                        detail=(
                            "exact worker continuation requires coordinator mode"),
                        metadata={
                            "code": "exact_continuation_requires_coordinator",
                            "continuation_intent_id": required_continuation_id,
                        },
                    )
                    continue
                continuation_intent_id = self._propose_control_continuation(
                    command_id=command_id,
                    action=original_action,
                    target=target,
                )
                if required_continuation_id and not continuation_intent_id:
                    self._ack_control(
                        cmd, state="unknown",
                        detail="exact continuation could not be materialized",
                        metadata={
                            "code": "exact_continuation_unavailable",
                            "continuation_intent_id": required_continuation_id,
                        },
                    )
                    continue
                if continuation_intent_id:
                    # The durable ContextResource is authoritative.  A decision
                    # answer may arrive after restart with a globally-scoped wire
                    # command and no volatile _pending_help row; force every legacy
                    # path onto the exact continuation so it can never fall into
                    # _next_worker_guidance or an engine/global bus broadcast.
                    target = f"intent:{continuation_intent_id}"
                    scope_kind, scope_value = "intent", continuation_intent_id
                    try:
                        await self._emit_coord_bb(
                            "intent_proposed",
                            intent_id=continuation_intent_id,
                            goal="operator-scoped continuation",
                            source_command_id=command_id,
                        )
                    except Exception:
                        pass
                scope_is_global = (
                    target in {"global", self.challenge.id,
                               f"challenge:{self.challenge.id}"}
                    or (scope_kind == "run" and scope_value == self.run_id)
                    or (scope_kind == "challenge"
                        and scope_value == self.challenge.id)
                )
                if (action in {
                        "pause", "resume", "stop", "complete",
                        "graceful_drain", "clear_standing", "reset_guidance",
                        "mark_false",
                } and not scope_is_global):
                    self._ack_control(
                        cmd, state="failed",
                        detail=f"{action} requires a run-wide scope",
                        metadata={"code": "invalid_scope"})
                    continue
                # operator STOP/COMPLETE: end the run gracefully. Unlike a steer
                # (which only guides workers), this terminates the coordinator loop —
                # the lever for a challenge that never yields a gated flag. Wake the
                # coordinator so it checks the flag at its next iteration boundary.
                if action in ("stop", "complete"):
                    self._operator_stop = True
                    self._pending_help = []
                    if self._operator_event is not None:
                        self._operator_event.set()
                    self._ack_control(
                        cmd, state="effect_observed",
                        detail="coordinator termination latch observed",
                        metadata={"effect": "termination_requested"})
                    continue
                # operator PAUSE/RESUME (#5): soft-pause the coordinator's spawn loop.
                # pause sets a flag the loop checks at its top (no new workers until
                # resume); it does NOT kill running workers or end the run. resume
                # clears it and wakes the loop. This is the contract that actually fits
                # a single-shot swarm — see _operator_paused. Still broadcast on the
                # InsightBus below (the deck reflects pause/resume; a live standby
                # worker process signalling is exclusively FREEZE/THAW; PAUSE and
                # RESUME are dispatcher latches and never touch process state.
                if action == "pause":
                    self._operator_paused = True
                    # surface it on the board so the rail shows "paused"
                    try:
                        await self._emit_coord_bb(
                            "operator_paused",
                            reason="operator paused the swarm "
                                   "(no new workers until resume)")
                    except Exception:
                        pass
                    self._ack_control(
                        cmd, state="effect_observed",
                        detail="dispatcher quiesced; active single-shot workers continue",
                        metadata={"effect": "run_quiesced"})
                    continue
                if action == "resume":
                    self._operator_paused = False
                    self._operator_draining = False
                    if self._operator_event is not None:
                        self._operator_event.set()
                    self._ack_control(
                        cmd, state="effect_observed",
                        detail="dispatcher resumed",
                        metadata={"effect": "run_resumed"})
                    continue
                # FREEZE is stronger than pause: quiesce dispatch AND SIGSTOP the
                # selected live subprocess groups.  The graph receives a finite
                # lease guard in the same operation, so frozen owners cannot lose
                # claims merely because the operator paused wall-clock progress.
                if action == "freeze":
                    kind, _value = self._control_scope_parts(target)
                    run_wide = kind in {"global", "run", "challenge"}
                    freeze_key = "__run__" if run_wide else target
                    if (self._freeze_suspensions
                            and freeze_key not in self._freeze_suspensions):
                        self._ack_control(
                            cmd, state="failed",
                            detail="another freeze scope is active; thaw it before changing scope",
                            metadata={"code": "overlapping_freeze_scope"})
                        continue
                    if freeze_key in self._freeze_suspensions or self._control_frozen:
                        already = self._control_target_solvers(target)
                        target_ids = [str(getattr(w, "solver_id", "") or "")
                                      for w in already
                                      if bool(getattr(w, "_paused", False))]
                        self._ack_control(
                            cmd, state="effect_observed",
                            detail="requested scope is already frozen",
                            target_ids=target_ids,
                            metadata={"effect": "already_frozen"})
                        continue
                    if run_wide:
                        self._operator_paused = True
                        self._control_frozen = True
                        if self._budget_suspend_started is None:
                            import time
                            self._budget_suspend_started = time.monotonic()
                    confirmed, failures = self._set_control_frozen(target, True)
                    if not run_wide and not confirmed and not failures:
                        self._ack_control(
                            cmd, state="unknown",
                            detail="no matching live worker to freeze",
                            metadata={"effect": "no_effect"})
                        continue
                    if failures:
                        # Freeze is all-or-nothing. Never leave a hidden subset of
                        # workers stopped while the UI correctly refuses to call the
                        # command effect_observed.
                        _rolled_back, rollback_failures = self._set_control_frozen(
                            target, False)
                        if run_wide and not rollback_failures:
                            self._operator_paused = False
                            self._control_frozen = False
                            self._budget_suspend_started = None
                        if rollback_failures:
                            # POSIX/container signalling can itself fail during
                            # compensation. Fail closed (dispatcher remains paused)
                            # and report the split state explicitly; never claim the
                            # rollback succeeded when a process may still be stopped.
                            if run_wide:
                                self._operator_paused = True
                                self._control_frozen = True
                            containment_cancel_requested = (
                                self._contain_unfrozen_control_workers(target))
                            suspension_id = command_id or f"freeze:{target}"
                            import time
                            self._freeze_suspensions[freeze_key] = suspension_id
                            self._freeze_started_at[freeze_key] = time.monotonic()
                            lease_affected = 0
                            lease_guard_failed = False
                            if self.shared_graph is not None:
                                try:
                                    lease_scope, lease_scope_id = (
                                        self._lease_scope_for_control(target))
                                    lease_result = self.shared_graph.suspend_active_leases(
                                        actor="control", suspension_id=suspension_id,
                                        scope_kind=lease_scope,
                                        scope_id=lease_scope_id,
                                        guard_s=1_000_000_000_000.0,
                                        reason="operator freeze containment")
                                    lease_affected = int(
                                        lease_result.get("affected") or 0)
                                except Exception:
                                    lease_guard_failed = True
                            self._ack_control(
                                cmd, state="partial",
                                detail=("worker freeze failed and rollback could not "
                                        "be fully confirmed; dispatcher held and "
                                        "cancellation requested where possible; "
                                        "process exit unconfirmed"),
                                target_ids=self._control_paused_ids(target),
                                metadata={
                                    "code": "freeze_rollback_unconfirmed",
                                    "apply_failures": failures,
                                    "rollback_failures": rollback_failures,
                                    "lease_affected": lease_affected,
                                    "lease_guard_failed": lease_guard_failed,
                                    "containment_cancel_requested":
                                        containment_cancel_requested,
                                })
                            continue
                        self._ack_control(
                            cmd, state="failed",
                            detail="worker freeze could not be confirmed; rolled back",
                            target_ids=confirmed,
                            metadata={"code": "freeze_confirmation_failed"})
                        continue
                    suspension_id = command_id or f"freeze:{target}"
                    self._freeze_suspensions[freeze_key] = suspension_id
                    import time
                    self._freeze_started_at[freeze_key] = time.monotonic()
                    lease_info: dict[str, Any] = {"affected": 0}
                    lease_failed = False
                    if self.shared_graph is not None:
                        try:
                            scope_kind, scope_id = self._lease_scope_for_control(target)
                            try:
                                guard_s = float(os.environ.get(
                                    "MUTEKI_FREEZE_LEASE_GUARD_SECONDS",
                                    "1000000000000"))
                            except (TypeError, ValueError):
                                guard_s = 1_000_000_000_000.0
                            guard_s = max(
                                60.0, min(1_000_000_000_000.0, guard_s))
                            lease_info = self.shared_graph.suspend_active_leases(
                                actor="control", suspension_id=suspension_id,
                                scope_kind=scope_kind, scope_id=scope_id,
                                guard_s=guard_s, reason="operator freeze")
                        except Exception:
                            lease_failed = True
                    if lease_failed:
                        _rolled_back, rollback_failures = self._set_control_frozen(
                            target, False)
                        if not rollback_failures:
                            self._freeze_suspensions.pop(freeze_key, None)
                            self._freeze_started_at.pop(freeze_key, None)
                        if run_wide and not rollback_failures:
                            self._operator_paused = False
                            self._control_frozen = False
                            self._budget_suspend_started = None
                        if rollback_failures:
                            containment_cancel_requested = (
                                self._contain_unfrozen_control_workers(target))
                            self._ack_control(
                                cmd, state="partial",
                                detail=("lease guard failed and worker rollback was "
                                        "not fully confirmed; dispatcher held and "
                                        "cancellation requested where possible; "
                                        "process exit unconfirmed"),
                                target_ids=self._control_paused_ids(target),
                                metadata={
                                    "code": "lease_guard_rollback_unconfirmed",
                                    "rollback_failures": rollback_failures,
                                    "containment_cancel_requested":
                                        containment_cancel_requested,
                                })
                            continue
                        self._ack_control(
                            cmd, state="failed",
                            detail="lease guard failed; worker freeze rolled back",
                            target_ids=confirmed,
                            metadata={"code": "lease_guard_failed"})
                        continue
                    if self._operator_event is not None:
                        self._operator_event.set()
                    self._ack_control(
                        cmd, state="effect_observed",
                        detail=(f"froze {len(confirmed)} live worker(s); "
                                f"guarded {int(lease_info.get('affected') or 0)} lease(s)"),
                        target_ids=confirmed,
                        metadata={"effect": "run_frozen" if run_wide else "workers_frozen",
                                  "lease_affected": int(lease_info.get("affected") or 0),
                                  "failures": failures})
                    continue
                if action == "thaw":
                    kind, _value = self._control_scope_parts(target)
                    run_wide = kind in {"global", "run", "challenge"}
                    freeze_key = "__run__" if run_wide else target
                    suspension_id = self._freeze_suspensions.get(freeze_key, "")
                    started = self._freeze_started_at.get(freeze_key)
                    if not suspension_id:
                        self._ack_control(
                            cmd, state="unknown",
                            detail="requested scope has no active freeze suspension",
                            metadata={"effect": "no_effect"})
                        continue
                    confirmed, failures = self._set_control_frozen(target, False)
                    import time
                    duration = max(0.0, time.monotonic() - started) if started else 0.0
                    if failures:
                        # A partially resumed process set is more dangerous than a
                        # failed thaw receipt: compensate with SIGSTOP and preserve
                        # the canonical suspension/lease guard for an explicit retry.
                        _refrozen, refreeze_failures = self._set_control_frozen(
                            target, True)
                        if refreeze_failures:
                            containment_cancel_requested = (
                                self._contain_unfrozen_control_workers(target))
                            self._ack_control(
                                cmd, state="partial",
                                detail=("worker thaw and compensating re-freeze both "
                                        "failed; dispatcher held and cancellation "
                                        "requested where possible; process exit "
                                        "unconfirmed"),
                                target_ids=self._control_paused_ids(target),
                                metadata={
                                    "code": "thaw_compensation_unconfirmed",
                                    "thaw_failures": failures,
                                    "refreeze_failures": refreeze_failures,
                                    "containment_cancel_requested":
                                        containment_cancel_requested,
                                })
                            continue
                        self._ack_control(
                            cmd, state="failed",
                            detail="worker thaw could not be confirmed; freeze preserved",
                            target_ids=confirmed,
                            metadata={"code": "thaw_confirmation_failed"})
                        continue
                    lease_info: dict[str, Any] = {"affected": 0, "skipped": []}
                    lease_failed = False
                    if self.shared_graph is not None:
                        try:
                            lease_info = self.shared_graph.resume_suspended_leases(
                                actor="control", suspension_id=suspension_id,
                                duration_s=duration, reason="operator thaw")
                        except Exception:
                            lease_failed = True
                    if lease_failed:
                        _refrozen, refreeze_failures = self._set_control_frozen(
                            target, True)
                        if refreeze_failures:
                            containment_cancel_requested = (
                                self._contain_unfrozen_control_workers(target))
                            self._ack_control(
                                cmd, state="partial",
                                detail=("lease restoration failed and compensating "
                                        "re-freeze was not fully confirmed; dispatcher "
                                        "held and cancellation requested where possible; "
                                        "process exit unconfirmed"),
                                target_ids=self._control_paused_ids(target),
                                metadata={
                                    "code": "lease_restore_compensation_unconfirmed",
                                    "refreeze_failures": refreeze_failures,
                                    "containment_cancel_requested":
                                        containment_cancel_requested,
                                })
                            continue
                        self._ack_control(
                            cmd, state="failed",
                            detail="lease restoration failed; worker freeze preserved",
                            target_ids=confirmed,
                            metadata={"code": "lease_restore_failed"})
                        continue
                    # Commit the in-memory transition only after both OS process
                    # groups and the graph lease journal have confirmed the thaw.
                    self._freeze_started_at.pop(freeze_key, None)
                    self._freeze_suspensions.pop(freeze_key, None)
                    if run_wide:
                        self._operator_paused = False
                        self._control_frozen = False
                        if self._budget_suspend_started is not None:
                            self._budget_suspended_total += max(
                                0.0, time.monotonic() - self._budget_suspend_started)
                            self._budget_suspend_started = None
                    if self._operator_event is not None:
                        self._operator_event.set()
                    skipped = len(lease_info.get("skipped") or [])
                    # A skipped lease means ownership changed while frozen.  That is
                    # an audited, safe compare-and-swap outcome—not a partial thaw.
                    state = "effect_observed"
                    self._ack_control(
                        cmd, state=state,
                        detail=(f"thawed {len(confirmed)} live worker(s) after "
                                f"{duration:.3f}s; restored "
                                f"{int(lease_info.get('affected') or 0)} lease(s)"),
                        target_ids=confirmed,
                        metadata={"effect": "run_thawed" if run_wide else "workers_thawed",
                                  "duration_s": duration,
                                  "lease_affected": int(lease_info.get("affected") or 0),
                                  "lease_skipped": skipped,
                                  "failures": failures})
                    continue
                if action in ("cancel_worker", "force_cancel"):
                    cancel_target = target
                    worker_id = str(cmd.get("worker_id") or payload.get("worker_id") or "")
                    if worker_id and target == "global":
                        cancel_target = f"worker:{worker_id}"
                    selected = self._control_target_solvers(cancel_target)
                    requested: list[str] = []
                    failures: list[str] = []
                    for worker in selected:
                        sid = str(getattr(worker, "solver_id", "") or "")
                        if self._cancel_solver(worker):
                            requested.append(sid)
                            self._update_control_worker_status(
                                sid, "cancel_requested")
                        else:
                            failures.append(sid)
                    state = (
                        "effect_observed" if requested and not failures else
                        "partial" if requested else "unknown"
                    )
                    self._ack_control(
                        cmd, state=state,
                        detail=(
                            f"cancellation requested for {len(requested)} live worker(s)"
                            if requested else
                            "matching worker cancellation could not be delivered"
                            if selected else "no matching live worker to cancel"
                        ),
                        target_ids=requested,
                        metadata={
                            "effect": "worker_cancel_requested",
                            "cancel_failures": failures,
                            "process_exit_confirmed": False,
                        })
                    continue
                if original_action == "expire_context":
                    self._ack_control(
                        cmd, state="effect_observed",
                        detail="typed operator context expired in the durable journal",
                        metadata={"effect": "context_expired"})
                    continue
                if action == "graceful_drain":
                    self._operator_draining = True
                    if self._operator_event is not None:
                        self._operator_event.set()
                    self._ack_control(
                        cmd, state="effect_observed",
                        detail="new dispatch quiesced; in-flight workers are draining",
                        metadata={"effect": "graceful_drain"})
                    continue
                if action == "spawn_worker":
                    if self.worker_cmds is None:
                        self._ack_control(
                            cmd, state="unknown",
                            detail="worker dispatcher is unavailable",
                            metadata={"code": "worker_dispatcher_unavailable"})
                        continue
                    loop = asyncio.get_running_loop()
                    worker_ack = loop.create_future()
                    worker_envelope = {
                        "action": "spawn",
                        "engine": str(
                            cmd.get("engine") or payload.get("engine")
                            or cmd.get("profile") or payload.get("profile") or ""),
                        "_control_ack": worker_ack,
                        "command_id": command_id,
                        "_control_started": False,
                        "_control_cancel_requested": False,
                    }
                    await self.worker_cmds.put(worker_envelope)
                    if self._operator_event is not None:
                        self._operator_event.set()
                    try:
                        try:
                            spawn_timeout = float(os.environ.get(
                                "MUTEKI_WORKER_CONTROL_ACK_TIMEOUT", "10"))
                        except (TypeError, ValueError):
                            spawn_timeout = 10.0
                        worker_result = await asyncio.wait_for(
                            asyncio.shield(worker_ack),
                            timeout=max(0.1, spawn_timeout),
                        )
                    except asyncio.TimeoutError:
                        worker_envelope["_control_cancel_requested"] = True
                        removed = False
                        try:
                            pending = getattr(self.worker_cmds, "_queue")
                            pending.remove(worker_envelope)
                            self.worker_cmds.task_done()
                            removed = True
                        except (AttributeError, ValueError):
                            pass
                        if removed:
                            worker_result = {
                                "state": "unknown",
                                "detail": (
                                    "worker spawn retired before dispatcher claim"),
                                "target_ids": [],
                                "metadata": {
                                    "code": "worker_spawn_timeout_unclaimed",
                                    "late_effect_fenced": True,
                                },
                            }
                            if not worker_ack.done():
                                worker_ack.set_result(worker_result)
                        else:
                            # Claimed commands are no longer timeout-cancellable.
                            # Await the dispatcher's real terminal proof so a spawn
                            # can never occur after the durable command says UNKNOWN.
                            worker_result = await asyncio.shield(worker_ack)
                    self._ack_control(
                        cmd,
                        state=str(worker_result.get("state") or "unknown"),
                        detail=str(worker_result.get("detail") or ""),
                        target_ids=list(worker_result.get("target_ids") or []),
                        metadata=dict(worker_result.get("metadata") or {}),
                    )
                    continue
                if original_action == "writeup":
                    self._ack_control(
                        cmd, state="unknown",
                        detail="writeup requires a finished-run standby worker",
                        metadata={"code": "writeup_requires_standby"})
                    continue
                # DISMISS a worker's hand-raise (NEED_INPUT) WITHOUT supplying the
                # resource: the operator judges the ask a false alarm / not worth
                # answering. The swarm must NOT stay frozen waiting on a blocker the
                # operator won't clear. Clear the pending ask (scoped to target),
                # record a dead-end so a re-spawned worker doesn't immediately re-raise
                # the same thing, unfreeze the workers, and wake the coordinator. No
                # resource is injected (distinct from a hint/redirect that answers it).
                if action in ("dismiss", "dismiss_help"):
                    if request_id:
                        dismissed = [h for h in self._pending_help
                                     if str(h.get("request_id") or h.get("id") or "")
                                     == request_id]
                    elif target == "global":
                        dismissed = list(self._pending_help)
                    else:
                        scoped = target.split(":", 1)[-1] if ":" in target else target
                        dismissed = [h for h in self._pending_help
                                     if str(h.get("worker", "")) == scoped]
                    if not dismissed:
                        self._ack_control(
                            cmd, state="unknown",
                            detail="no matching decision request to dismiss",
                            metadata={"effect": "no_effect",
                                      "code": "decision_not_found"})
                        continue
                    dismissed_objects = {id(h) for h in dismissed}
                    self._pending_help = [
                        h for h in self._pending_help
                        if id(h) not in dismissed_objects
                    ]
                    for h in dismissed:
                        need = str(h.get("need", "")).strip()
                        if need:
                            try:
                                await self.insight.dead_end(
                                    "coordinator",
                                    f"operator dismissed the ask «{need[:160]}» — "
                                    f"not supplying it; do not re-raise")
                            except Exception:
                                pass
                    if not self._pending_help:
                        if not self._control_frozen:
                            self._operator_paused = False
                        if self._operator_event is not None:
                            self._operator_event.set()
                    # SIGCONT the workers we froze on the hand-raise so the swarm
                    # resumes instead of sitting paused on a dismissed blocker.
                    if ("__help__" not in self._freeze_suspensions
                            and not self._control_frozen):
                        try:
                            await self.insight.guidance(
                                "", action="resume", target=target, standing=False)
                        except Exception:
                            pass
                    try:
                        await self._emit_coord_bb(
                            "help_dismissed",
                            reason=f"operator dismissed {len(dismissed)} hand-raise(s)"
                                   f"{'' if target == 'global' else ' for ' + target}",
                            count=len(dismissed))
                    except Exception:
                        pass
                    self._ack_control(
                        cmd, state="effect_observed",
                        detail=f"dismissed {len(dismissed)} matching decision request(s)",
                        target_ids=[str(h.get("worker") or "") for h in dismissed
                                    if h.get("worker")],
                        metadata={"effect": "decision_dismissed"})
                    continue
                # P0 defect-4: clear standing guidance. The list is only-grew before,
                # so an operator who dropped several corrections could not retract a
                # stale one (and the cumulative text bloated every new worker's prompt
                # → claude 36k-token empty-exit). clear_standing wipes all, or one by
                # exact text match (cmd["text"]).
                if action in ("clear_standing", "reset_guidance"):
                    companion = (cmd.get("_control_companion")
                                 if isinstance(cmd.get("_control_companion"), dict)
                                 else {})
                    expired_context_count = int(
                        companion.get("expired_context_count") or 0)
                    matched_source_ids = sorted({
                        str(value or "").strip()
                        for value in (
                            companion.get("matched_source_command_ids") or [])
                        if str(value or "").strip()
                    })
                    if self.shared_graph is None:
                        self._ack_control(
                            cmd, state=("partial" if expired_context_count else "failed"),
                            detail="standing guidance graph is unavailable",
                            metadata={
                                "code": "guidance_graph_unavailable",
                                "expired_context_count": expired_context_count,
                            })
                        continue
                    try:
                        source_command_id = str(
                            cmd.get("command_id") or "").strip()
                        if source_command_id:
                            clear_result = self.shared_graph.apply_standing_clear(
                                command_id=source_command_id,
                                actor="operator",
                                text=("" if str(text or "").startswith("secret://")
                                      else str(text or "")),
                                eligible_command_ids=(
                                    matched_source_ids if text else None),
                                match_by_source_ids=str(text or "").startswith(
                                    "secret://"),
                            )
                            expired_directives = list(
                                clear_result.get("expired_directives") or [])
                        else:
                            # Compatibility for old in-process queue producers.
                            # Durable API commands always carry command_id and use
                            # the crash-replay marker above.
                            expired_directives = (
                                self.shared_graph.expire_standing_directives(
                                    actor="operator", text=str(text or "")))
                    except Exception:
                        self._ack_control(
                            cmd, state=("partial" if expired_context_count else "failed"),
                            detail="standing directive expiration failed",
                            metadata={
                                "code": "guidance_graph_expire_failed",
                                "expired_context_count": expired_context_count,
                            })
                        continue

                    # The ControlActor normally expired typed contexts before this
                    # runtime call. Keep the final boundary safe for legacy direct
                    # queue producers and verify absence before mutating memory.
                    provider = getattr(self, "_context_provider", None)
                    expirer = getattr(self, "_context_expirer", None)
                    try:
                        if callable(provider):
                            matching = [
                                resource for resource in provider()
                                if bool(getattr(resource, "standing", False))
                                and (not text or str(
                                    getattr(resource, "content", "") or "") == str(text))
                            ]
                            if matching and not callable(expirer):
                                raise RuntimeError("context expirer unavailable")
                            for resource in matching:
                                expirer(
                                    str(getattr(resource, "context_id", "")),
                                    actor="operator", reason="clear_standing")
                            remaining = [
                                resource for resource in provider()
                                if bool(getattr(resource, "standing", False))
                                and (not text or str(
                                    getattr(resource, "content", "") or "") == str(text))
                            ]
                            if remaining:
                                raise RuntimeError("standing context remains active")
                    except Exception:
                        self._ack_control(
                            cmd,
                            state=("partial" if (
                                expired_directives or expired_context_count) else "failed"),
                            detail="typed standing context expiration was not confirmed",
                            metadata={
                                "code": "guidance_context_expire_failed",
                                "expired_directives": expired_directives,
                                "expired_context_count": expired_context_count,
                            })
                        continue

                    if text:
                        self._standing_guidance = [
                            s for s in self._standing_guidance if s != text]
                    else:
                        self._standing_guidance = []
                    self._ack_control(
                        cmd, state="effect_observed",
                        detail="standing guidance expired",
                        metadata={
                            "effect": "guidance_cleared",
                            "expired_directives": expired_directives,
                            "expired_context_count": expired_context_count,
                        })
                    continue
                if action == "mark_false":
                    flag = str(cmd.get("flag") or "").strip()
                    if not flag and text:
                        m = re.search(r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}", str(text))
                        flag = m.group(0) if m else str(text).strip()
                    if not flag and self._found_flags:
                        flag = self._found_flags[0]
                    if not flag:
                        self._ack_control(
                            cmd, state="unknown",
                            detail="no flag was available to invalidate",
                            metadata={"effect": "no_effect"})
                        continue
                    if self.shared_graph is None:
                        self._ack_control(
                            cmd, state="failed",
                            detail="flag graph is unavailable",
                            metadata={"code": "flag_graph_unavailable"})
                        continue
                    try:
                        info = self.shared_graph.reopen_after_false_positive(
                            actor="operator", flag=flag)
                    except Exception:
                        self._ack_control(
                            cmd, state="failed",
                            detail="flag invalidation was not committed",
                            metadata={"code": "flag_invalidation_failed"})
                        continue

                    # Only project volatile state after the canonical graph write.
                    self._found_flags = [f for f in self._found_flags if f != flag]
                    try:
                        await self._emit_coord_bb(
                            "dead_end", reason=info.get("dead_end_reason")
                            or f"false positive: {flag}")
                        for iid in info.get("reopened", []) or []:
                            await self._emit_coord_bb("intent_reopened", intent_id=iid)
                        await self._emit_coord_bb("flag_invalidated", flag=flag)
                        if self.bus is not None:
                            try:
                                await self.bus.emit(Event(
                                    event_type=EventType.RUN_REOPENED,
                                    run_id=self.run_id,
                                    challenge_id=self.challenge.id,
                                    payload={"flag": flag},
                                ))
                            except Exception:
                                pass
                        if self._operator_event is not None:
                            self._operator_event.set()
                    except Exception:
                        # Telemetry is replayable from the graph and never weakens
                        # the already-committed invalidation receipt.
                        pass
                    self._ack_control(
                        cmd, state="effect_observed",
                        detail="flag invalidated and dependent intents reopened",
                        metadata={"effect": "flag_invalidated"})
                    continue
                # `url` is the NEW target a redirect carries (distinct from `target`,
                # which is the SCOPE: global / solver:<id>). `standing` marks
                # persistent background guidance (VPS/SSH creds) for all workers.
                url = cmd.get("url") or cmd.get("target_url") or ""
                delivery_url = str(url or "")
                secret_delivery = any(
                    str(value or "").startswith("secret://")
                    for value in (text, url)
                )
                if secret_delivery:
                    typed_secret_available = False
                    provider = getattr(self, "_context_provider", None)
                    status_provider = getattr(
                        self, "_context_status_provider", None)
                    if command_id and callable(provider):
                        try:
                            from muteki.control import context_resource_id_for_command
                            context_id = context_resource_id_for_command(command_id)
                            if callable(status_provider):
                                typed_secret_available = status_provider(context_id) in {
                                    "active", "reserved", "bound",
                                }
                            else:
                                # Safe compatibility fallback: default provider()
                                # exposes active rows only, never expired/consumed.
                                typed_secret_available = any(
                                    str(getattr(row, "context_id", "")) == context_id
                                    for row in provider()
                                )
                        except Exception:
                            typed_secret_available = False
                    if not typed_secret_available:
                        self._ack_control(
                            cmd, state="unknown",
                            detail=("opaque secret reference has no reserved typed "
                                    "context delivery boundary"),
                            metadata={"code": "secret_delivery_unavailable"},
                        )
                        continue
                standing = bool(cmd.get("standing", False))
                # persist standing guidance on the coordinator so workers spawned
                # LATER inherit it at turn-1 (live workers also get it via the
                # InsightBus broadcast below). Dedupe so re-sends don't pile up.
                if (scope_is_global and standing and text and not secret_delivery
                        and text not in self._standing_guidance):
                    self._standing_guidance.append(text)
                    # P0 defect-4: LRU cap — keep only the most recent N standing
                    # hints so the cumulative text can't bloat every new worker's
                    # prompt unbounded (the 36k-token claude empty-exit). The per-
                    # worker char budget (cli_solver _standing_block) is the second
                    # guard; this bounds the count at the source.
                    if len(self._standing_guidance) > _STANDING_MAX:
                        self._standing_guidance = self._standing_guidance[-_STANDING_MAX:]
                # M-3 (single-shot migration): a NON-standing hint/redirect can no
                # longer steer a live (single-shot) worker — route it to the NEXT
                # spawned worker. A redirect url becomes the new target for every
                # subsequent worker; hint/redirect text is one-shot guidance the next
                # spawn folds in. (standing already flows via _standing_guidance.)
                if scope_is_global and not standing and not secret_delivery:
                    if url:
                        self._target_redirect = delivery_url
                    if text and text not in self._next_worker_guidance:
                        self._next_worker_guidance.append(text)
                # B: record the steer as a FIRST-CLASS OperatorDirective (not a fake
                # low-confidence candidate + ordinary intent). The directive carries a
                # preemption policy; soft_rebind (default) supersedes unclaimed
                # conflicting intents so the next worker batch picks up the new
                # direction, without killing a live worker. graceful_drain / force_cancel
                # are honored where the operator explicitly asks for them.
                preempt = str(cmd.get("preempt_policy")
                              or cmd.get("preemption") or "").strip().lower()
                directive_id = ""
                is_decision_answer = original_action in ("answer_decision", "submit")
                if (text and not is_decision_answer
                        and not continuation_intent_id
                        and self.shared_graph is not None
                        and action in (
                            "hint", "focus", "redirect", "directive", "correction"
                        )):
                    try:
                        info = self.shared_graph.add_operator_directive(
                            actor="operator", action=action, text=text,
                            scope=target or "global", standing=standing,
                            preempt_policy=preempt or "soft_rebind",
                            source_command_id=command_id,
                        )
                        directive_id = info["directive_id"]
                        policy = info["preempt_policy"]
                        intent_id = ""
                        status = "queued"
                        # Only a run-wide, one-shot search directive becomes a globally
                        # claimable intent.  A solver-scoped hint is delivered to that
                        # live worker through InsightBus; turning it into a global open
                        # intent leaked the supposedly private direction to any worker.
                        # Standing context is background context, not a search task.
                        if scope_is_global and not standing:
                            intent_id = f"I-{directive_id}"
                            self.shared_graph.propose_intent(
                                actor="operator", intent_id=intent_id, goal=text,
                                payload={"source": "operator_directive", "action": action,
                                         "directive_id": directive_id,
                                         "scope": target,
                                         "priority": "operator"},
                            )
                            status = "bound"
                        elif scope_is_global and standing:
                            status = "bound"
                        self.shared_graph.update_directive_status(
                            directive_id=directive_id, status=status,
                            generated_intent_id=intent_id or None)
                        await self._emit_coord_bb(
                            "operator_directive_changed", directive_id=directive_id,
                            action=action, text=text, scope=target, status=status,
                            preemption=policy, intent_id=intent_id)
                        # soft_rebind / graceful_drain / force_cancel: retire UNCLAIMED
                        # conflicting "ask operator" directions (the redirect obsoletes
                        # them). Live workers are only touched on graceful_drain (a
                        # GUIDANCE drain signal) / force_cancel (handled via _drain below).
                        if (scope_is_global and policy in
                                ("soft_rebind", "graceful_drain", "force_cancel")):
                            for needle in ("operator", "ask", "request"):
                                try:
                                    self.shared_graph.supersede_open_intents(
                                        actor="coordinator", match=needle,
                                        reason=f"superseded by operator directive {directive_id}")
                                except Exception:
                                    pass
                        if policy == "graceful_drain" and not secret_delivery:
                            try:
                                await self.insight.guidance(
                                    delivery_text, action="graceful_drain", target=target,
                                    standing=False)
                            except Exception:
                                pass
                        if self.review_policy.get("on_operator_hint", True):
                            self._queue_review_request(
                                trigger="operator_hint",
                                directive=(
                                    f"Operator {action} directive was added: {text}. "
                                    "Audit whether this should become a route change, "
                                    "branch split, fact challenge, or focused worker directive."
                                ),
                            )
                    except Exception:
                        pass
                # still broadcast on the InsightBus: the deck's event log + a live
                # standby worker consume it. A racing single-shot worker ignores it
                # (it has no resume turn) — that's the accepted intent-level degrade.
                if (not secret_delivery and not continuation_intent_id
                        and (delivery_text or delivery_url)):
                    try:
                        await self.insight.guidance(
                            delivery_text, action=action, target=target,
                            url=delivery_url, standing=standing)
                    except Exception:
                        # The typed context/graph selector is the durable delivery
                        # contract. InsightBus is a live projection and may be
                        # replayed; its failure must not turn a committed binding
                        # into a terminal FAILED receipt.
                        pass
                # Scoped commands have now been routed to their one eligible live
                # worker.  Close the one-shot directive so it cannot later leak into a
                # differently-scoped spawn.  ``acted`` here means routed/consumed by the
                # legacy bus, not that the model semantically obeyed it.
                if directive_id and not scope_is_global and self.shared_graph is not None:
                    try:
                        bound_worker = (target.split(":", 1)[1]
                                        if target.startswith("solver:") else target)
                        self.shared_graph.update_directive_status(
                            directive_id=directive_id, status="acted",
                            bound_worker=bound_worker)
                        await self._emit_coord_bb(
                            "operator_directive_changed", directive_id=directive_id,
                            action=action, text=text, scope=target, status="acted",
                            bound_worker=bound_worker)
                    except Exception:
                        pass
                gave_resource = bool(url) or standing or bool(text)
                # Wake only for an actual answer/resource. Empty ASK/WRITEUP-style
                # commands must not release a pending blocker with an UNKNOWN ACK.
                if (self._operator_event is not None
                        and (gave_resource or is_decision_answer)):
                    self._operator_event.set()
                # M5: clear the "waiting for help" asks SCOPED to the command's target.
                # A global command answers every pending ask; a solver-scoped one
                # (target == "solver:<id>") only clears that worker's ask, so a hint
                # addressed to worker B no longer wipes worker A's still-unmet blocker
                # (which would resolve awaiting_operator with no real answer and resume
                # hurling workers at A's wall). Keep the rest pending.
                if (is_decision_answer and request_id):
                    self._pending_help = [
                        h for h in self._pending_help
                        if str(h.get("request_id") or h.get("id") or "") != request_id]
                elif gave_resource and scope_is_global and len(self._pending_help) <= 1:
                    self._pending_help = []
                elif gave_resource:
                    scoped = target.split(":", 1)[-1] if ":" in target else target
                    matching = [h for h in self._pending_help
                                if str(h.get("worker", "")) == scoped]
                    # A request-less legacy hint is only an answer when its scope
                    # identifies exactly one outstanding decision.  Otherwise keep
                    # every card open and require an explicit request_id.
                    if len(matching) == 1:
                        self._pending_help = [
                            h for h in self._pending_help
                            if str(h.get("worker", "")) != scoped]
                # M3: RETIRE the now-obsolete "ask the operator for X" intents ONLY when
                # the operator actually SUPPLIED A RESOURCE — a redirect url, standing
                # guidance, or hint text (run-11190: 238-worker loop re-asking for the
                # L2 SSH password after it was supplied). A bare default-action hint with
                # no content used to run this sweep too, and its broad substring needles
                # (operator/unlock/dashboard) could wrongly retire a legitimate in-flight
                # intent on a totally unrelated hint. Gate on a resource being present;
                # for a solver-scoped command, only retire that worker's blocked intents.
                if self.shared_graph is not None and gave_resource and scope_is_global:
                    superseded = 0
                    for needle in ("operator", "ssh password", "dashboard",
                                   "unlock"):
                        try:
                            superseded += len(self.shared_graph.supersede_open_intents(
                                actor="coordinator", match=needle,
                                reason=f"operator supplied input ({action})"))
                        except Exception:
                            pass
                    if superseded:
                        try:
                            await self.insight.dead_end(
                                "coordinator",
                                f"retired {superseded} obsolete 'ask-operator' "
                                f"intent(s) after operator input")
                        except Exception:
                            pass
                matched = self._control_target_solvers(target)
                target_ids = [str(getattr(w, "solver_id", "") or "")
                              for w in matched]
                # Context is durable even when no matching single-shot is currently
                # alive. ``effect_observed`` means the selector/next-spawn binding
                # exists, not that an already-running model changed its prompt.
                observed = bool(text or url)
                if not (text or url or action in ("focus", "directive", "correction")):
                    observed = False
                durable_selector = False
                if command_id:
                    try:
                        from muteki.control import context_resource_id_for_command
                        context_id = context_resource_id_for_command(command_id)
                        provider = getattr(self, "_context_provider", None)
                        if callable(provider):
                            resource = next(
                                (row for row in provider()
                                 if str(getattr(row, "context_id", "")) == context_id),
                                None)
                            if resource is not None:
                                resource_scope = str(
                                    getattr(resource, "scope", "global") or "global")
                                durable_selector = not (
                                    resource_scope.startswith(("worker:", "solver:"))
                                    and not target_ids)
                    except Exception:
                        durable_selector = False
                if not scope_is_global and not target_ids and not durable_selector:
                    observed = False
                # A live single-shot worker cannot acquire new argv/prompt context.
                # For an ephemeral worker selector, durable delivery is observed only
                # when its exact replacement intent was actually materialised.
                if (not scope_is_global
                        and target.startswith(("solver:", "worker:"))
                        and not continuation_intent_id):
                    observed = False
                self._ack_control(
                    cmd,
                    state="effect_observed" if observed else "unknown",
                    detail=("operator context durably bound to the single-shot selector"
                            if observed else "no matching live worker or bindable context"),
                    target_ids=target_ids,
                    metadata={
                        "effect": ("decision_answered" if is_decision_answer
                                   else "directive_bound"),
                        "directive_id": directive_id,
                        "delivery": "durable_selector",
                        "continuation_intent_id": continuation_intent_id,
                    },
                )
            except Exception:
                # a malformed command must never kill the drain loop
                if isinstance(cmd, dict):
                    self._ack_control(
                        cmd, state="failed",
                        detail="coordinator rejected malformed control command",
                        metadata={"code": "coordinator_apply_failure"})
                continue
            finally:
                if isinstance(cmd, dict):
                    self._ack_control(
                        cmd, state="unknown",
                        detail="control consumer exited before a terminal effect was confirmed",
                        metadata={"code": "consumer_interrupted"})
                    cmd.pop("_control_consumer_task", None)
                    cmd.pop("_control_restart_event", None)
                    cmd.pop("_control_consumer_epoch", None)
                try:
                    self.hitl_inbox.task_done()
                except Exception:
                    pass
