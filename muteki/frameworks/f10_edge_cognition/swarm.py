"""SwarmF10 — edge cognition swarm (light central arbiter).

Design: docs/frameworks_2026/10_edge_cognition_swarm.md.

The central coordinator is deliberately degraded (§4.1/§6):

- the per-tick Reason hot path is REMOVED — `_run_reason` only fires a light
  meta-explore pass once every META_EXPLORE_INTERVAL_S (§6 保留 meta-explore);
- what remains central: budget ceilings + kill enforcement, the flag gate
  (untouched, in the worker), lane arbitration (existing graph locks), the
  intent queue, and ingestion of worker-reported checkpoint events;
- workers carry the cognition: each dispatched intent gets an IntentEnvelope
  (§2.4) via `framework_worker_guidance_for_intent`, which flips the CliSolver
  into its internal multi-step shell loop.
"""

from __future__ import annotations

import re
import time
from typing import Any, Optional

from muteki.frameworks.f04_qd_archive.vocabulary import normalize_category
from muteki.frameworks.f10_edge_cognition.schema import (
    F10_FEATURE_KINDS,
    ensure_f10_schema_on_graph,
)
from muteki.frameworks.f10_edge_cognition.shell import (
    ensure_run_budget,
    guidance_from_envelope,
    ingest_edge_events,
    pick_by_capability,
    record_capability,
    start_shell,
)
from muteki.frameworks.f10_edge_cognition.state import (
    META_EXPLORE_INTERVAL_S,
)
from muteki.solver.result_codes import (
    RESULT_DEAD_END,
    RESULT_EXPLORED,
    RESULT_SOLVED,
    normalize_result_code,
)
from muteki.swarm.swarm import Swarm


def _engine_of(actor: str) -> str:
    """cli-<engine>[-N] worker label → engine name for capability profiles."""
    eng = re.sub(r"^cli-", "", str(actor or ""))
    base = re.sub(r"-\d+$", "", eng)
    return base or eng


class SwarmF10(Swarm):
    """Central = budget/flag/lane arbiter; workers run the thick shell loop."""

    architecture_name = "f10-edge-cognition"
    framework_id = "f10"
    f10_enabled = True
    reason_declaration_mode = None

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self.f10_enabled = True
        self.reason_declaration_mode = None
        self._f10_ready = False
        self._f10_bootstrapped = False
        self._f10_mirror_seq = 0
        self._f10_ingest_seq = 0
        self._f10_envelopes: dict[str, dict[str, Any]] = {}
        self._f10_last_meta_explore = 0.0
        self._f10_meta_explore_count = 0
        self._f10_concluded_seq = 0

    def _f10_ensure(self) -> None:
        if self._f10_ready:
            return
        g = getattr(self, "shared_graph", None)
        if g is None:
            return
        if not ensure_f10_schema_on_graph(g):
            return
        run_id = str(getattr(self, "run_id", "") or getattr(self.challenge, "id", ""))
        ensure_run_budget(g, run_id=run_id)
        self._f10_ready = True
        self._f10_bootstrap_seed()

    def _f10_bootstrap_seed(self) -> None:
        """Prepare-stage intent source (§4.1a): one seed intent from challenge
        metadata — no central deep planning."""
        if self._f10_bootstrapped:
            return
        g = getattr(self, "shared_graph", None)
        if g is None:
            return
        cat = normalize_category(getattr(self.challenge, "category", None))
        try:
            envelope = start_shell(
                g,
                intent_id="f10-bootstrap-recon",
                goal="inventory challenge artifacts and map attack surface",
                category=cat,
                source="prepare",
            )
            self._f10_envelopes["f10-bootstrap-recon"] = envelope
        except Exception:
            pass
        self._f10_bootstrapped = True

    async def _f10_mirror_features(self) -> None:
        g = getattr(self, "shared_graph", None)
        emit = getattr(self, "_emit_coord_bb", None)
        if g is None or not callable(emit):
            return
        try:
            feature_events = g.events_since(
                self._f10_mirror_seq, kinds=list(F10_FEATURE_KINDS)
            )
        except Exception:
            feature_events = []
        for event in feature_events:
            payload = event.get("payload") or {}
            if not isinstance(payload, dict):
                payload = {}
            # event payloads may carry their own "kind"/"graph_seq" keys (the
            # EdgeMessage envelope does) — they must not collide with the emit
            # signature.
            payload = {k: v for k, v in payload.items()
                       if k not in ("kind", "graph_seq")}
            await emit(
                str(event.get("kind") or "edge_shell_started"),
                graph_seq=int(event.get("seq") or 0),
                **payload,
            )
            self._f10_mirror_seq = max(
                self._f10_mirror_seq, int(event.get("seq") or 0)
            )

    def framework_prepare_hook(self) -> None:
        self._f10_ensure()

    def framework_on_intents_proposed(self, proposed: list[dict]) -> None:
        self._f10_ensure()
        g = getattr(self, "shared_graph", None)
        if g is None:
            return
        cat = normalize_category(getattr(self.challenge, "category", None))
        for it in list(proposed or []):
            iid = str(it.get("intent_id") or "")
            goal = str(it.get("goal") or "")
            if not iid:
                continue
            envelope = start_shell(
                g,
                intent_id=iid,
                goal=goal,
                category=cat,
                predicted_effects=it.get("predicted_effects"),
                lane_key=str(it.get("lane_key") or ""),
                risk_class=str(it.get("risk_class") or ""),
                source="reason",
            )
            self._f10_envelopes[iid] = envelope

    def framework_worker_guidance_for_intent(self, intent_id: str) -> list[str]:
        """Hand the worker its IntentEnvelope; the marker line flips CliSolver
        into the shell loop. Unknown intents get nothing (default single-shot)."""
        envelope = self._f10_envelopes.get(str(intent_id) or "")
        if not envelope:
            return []
        return guidance_from_envelope(envelope)

    async def _run_reason(self) -> int:
        """Central slimming (§6 舍弃): the per-2s Reason hot path is gone. The
        only central planning left is one light meta-explore pass every
        META_EXPLORE_INTERVAL_S — a full-board scan that proposes fresh
        high-coverage intents so workers don't sink into local optima (§8.3)."""
        if self.shared_graph is None or getattr(self, "llm", None) is None:
            try:
                from muteki.solver.reason import (
                    PlannerFailure,
                    PlannerFailureKind,
                )
                self._last_reason = None
                self._last_planner_failure = PlannerFailure(
                    PlannerFailureKind.UNAVAILABLE,
                    "shared graph or planner client is unavailable",
                )
            except Exception:
                pass
            return 0
        now = time.monotonic()
        if now - self._f10_last_meta_explore < META_EXPLORE_INTERVAL_S:
            return 0
        self._f10_last_meta_explore = now
        self._f10_meta_explore_count += 1
        emit = getattr(self, "_emit_coord_bb", None)
        if callable(emit):
            try:
                await emit(
                    "edge_meta_explore",
                    trigger="interval",
                    interval_s=META_EXPLORE_INTERVAL_S,
                    count=self._f10_meta_explore_count,
                )
            except Exception:
                pass
        return await super()._run_reason()

    async def framework_after_workers(self, *_a: Any, **_k: Any) -> None:
        """End-of-tick: ingest worker-reported checkpoints/sub-intents (real
        counts only — no fabricated tokens, no global facts==0 stuck verdict)
        and cancel live workers whose budget tripped (Budget Enforcer, §3)."""
        self._f10_ensure()
        g = getattr(self, "shared_graph", None)
        if g is None:
            return
        report = ingest_edge_events(g, since_seq=self._f10_ingest_seq)
        try:
            self._f10_ingest_seq = max(
                self._f10_ingest_seq, int(report.get("since_seq") or 0)
            )
        except Exception:
            pass
        for shell_id in list(report.get("tripped") or []):
            self._f10_kill_shell(str(shell_id))
        self._f10_learn_from_concludes(g)
        await self._f10_mirror_features()

    def _f10_learn_from_concludes(self, g: Any) -> None:
        """Capability-based Dispatch (§3/§4.1) learns from REAL worker outcomes.

        The base Swarm never calls ``framework_record_worker_outcome`` (dormant
        hook upstream — same finding as f04/f05), so the profile is fed from
        ``intent_concluded`` events on the append-only graph instead. Only
        decisive outcomes count: a cancelled/steered/timed-out shell says
        nothing about the engine's capability on this category."""
        try:
            events = g.events_since(
                int(getattr(self, "_f10_concluded_seq", 0)),
                kinds=["intent_concluded"],
            )
        except Exception:
            return
        cat = normalize_category(getattr(self.challenge, "category", None))
        for event in events or []:
            try:
                seq = int(event.get("seq") or 0)
            except Exception:
                seq = 0
            self._f10_concluded_seq = max(
                getattr(self, "_f10_concluded_seq", 0), seq)
            payload = event.get("payload") or {}
            if not isinstance(payload, dict):
                continue
            iid = str(payload.get("intent_id") or "")
            if not iid or iid not in self._f10_envelopes:
                continue  # only f10-owned shell intents feed the edge profile
            result = normalize_result_code(str(payload.get("result") or ""))
            if result == RESULT_SOLVED:
                success = True
            elif result in (RESULT_DEAD_END, RESULT_EXPLORED):
                success = False
            else:
                continue  # transient/neutral: not a capability signal
            engine = _engine_of(str(event.get("actor") or ""))
            if not engine:
                continue
            try:
                record_capability(
                    g, engine=engine, category=cat, success=success)
            except Exception:
                pass

    def _f10_kill_shell(self, shell_id: str) -> None:
        """Budget Enforcer: cancel the live worker running a tripped shell."""
        if not shell_id:
            return
        solvers = getattr(self, "_live_solvers", None) or {}
        for worker in list(solvers.values()):
            try:
                if str(getattr(worker, "_f10_shell_id", "") or "") != shell_id:
                    continue
                cancel = getattr(worker, "cancel", None)
                if callable(cancel):
                    cancel()
            except Exception:
                pass

    def _effect_capability_pick_engine(
        self,
        running_engines: list[str],
        healthy: list[str],
        *,
        role: str = "bootstrap",
        intent_id: str = "",
        lane: str = "",
        intent: Optional[dict] = None,
        avoid_engines: Optional[list[str]] = None,
    ) -> str | None:
        del role, lane, intent, intent_id
        self._f10_ensure()
        g = getattr(self, "shared_graph", None)
        available = [e for e in (healthy or []) if e not in set(avoid_engines or ())]
        if g is None:
            return available[0] if available else None
        cat = normalize_category(getattr(self.challenge, "category", None))
        return pick_by_capability(
            g, available, category=cat, running=list(running_engines or [])
        )

    def framework_record_worker_outcome(
        self,
        *,
        engine: str,
        intent: Optional[dict],
        success: bool,
        cost_usd: float | None = None,
    ) -> None:
        """Direct outcome hook. Dormant upstream (the base Swarm never calls
        it); the live learning path is ``_f10_learn_from_concludes``."""
        del cost_usd, intent
        self._f10_ensure()
        g = getattr(self, "shared_graph", None)
        if g is None:
            return
        cat = normalize_category(getattr(self.challenge, "category", None))
        record_capability(g, engine=str(engine or ""), category=cat, success=bool(success))


Swarm = SwarmF10
