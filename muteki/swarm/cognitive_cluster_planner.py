"""Cognitive cluster planner — ranks intents and matches heterogeneous engines.

North-star mechanism (not an eval toy): under fixed roster/budget, change *what*
gets dispatched next and *who* runs it, using only verified graph evidence.

Capabilities this targets:
- search different directions (route / goal diversity vs in-flight work)
- avoid dead ends and barren re-walks (barren + dead-end similarity penalties)
- long-chain continuity (boost intents linked to recent verified facts)
- heterogeneous complementarity (prefer idle engines with better fact/barren ratio)
- local recovery preference (prefer intents that continue from facts over blank restarts)

Default OFF. Enable via Swarm(cognitive_cluster_planner=True) or
MUTEKI_COGNITIVE_CLUSTER_PLANNER=1.

Does not widen the provenance gate. Does not invent flags. Does not require new
schemas on the wire — it reorders existing open intents and biases engine pick.
"""

from __future__ import annotations

from dataclasses import dataclass, field
import os
import re
from typing import Any, Iterable, Mapping, Sequence

from muteki.solver.worker_profiles import VALID_BASE_ENGINES


# Filler-stripped goal tokens for cheap semantic overlap (mirrors reason.py spirit).
_STOP = frozenset(
    "the a an to of for on in at and or with via then into from by it its this "
    "that these those please try attempt now next using use re examine check "
    "look see get find".split()
)

_VERIFY_CLASSES = frozenset({"verifier", "review"})


def planner_enabled_from_env() -> bool:
    raw = (os.environ.get("MUTEKI_COGNITIVE_CLUSTER_PLANNER") or "").strip().lower()
    return raw in {"1", "true", "yes", "on"}


# Minimal CTF/ops synonym expansion so "cross site scripting" hits barren "xss".
# Keep tiny and domain-local — not a general NLP stack.
_ALIASES: dict[str, tuple[str, ...]] = {
    "xss": ("cross", "site", "scripting"),
    "sqli": ("sql", "injection"),
    "rce": ("remote", "code", "execution"),
    "lfi": ("local", "file", "inclusion"),
    "ssrf": ("server", "side", "request", "forgery"),
    "jwt": ("json", "web", "token"),
    "rop": ("return", "oriented", "programming"),
    "uaf": ("use", "after", "free"),
}


def _tokens(text: str) -> frozenset[str]:
    toks = re.findall(r"[a-z0-9]+", (text or "").lower())
    base = {t for t in toks if t not in _STOP and len(t) > 1}
    expanded = set(base)
    # phrase-level: if all alias words present, add short tag
    for tag, words in _ALIASES.items():
        if set(words) <= expanded or tag in expanded:
            expanded.add(tag)
            expanded.update(words)
    return frozenset(expanded)


def _jaccard(a: frozenset[str], b: frozenset[str]) -> float:
    if not a or not b:
        return 0.0
    inter = len(a & b)
    if inter == 0:
        return 0.0
    return inter / float(len(a | b))


def _engine_from_actor(actor: str) -> str:
    """Best-effort map blackboard actor / solver_id → base engine name."""

    a = (actor or "").strip().lower()
    if not a:
        return ""
    for name in VALID_BASE_ENGINES:
        if a == name or a.startswith(f"{name}-") or f"-{name}-" in a or a.endswith(f"-{name}"):
            return name
        if name in a.split("-") or name in a.split("_"):
            return name
    # profile ids like "claude-deepseek" / "cursor-main"
    for name in VALID_BASE_ENGINES:
        if name in a:
            return name
    return ""


@dataclass(frozen=True, slots=True)
class ClusterEvidence:
    """Compact evidence bag extracted from the shared graph for ranking."""

    recent_fact_tokens: tuple[str, ...] = ()
    dead_end_token_sets: tuple[frozenset[str], ...] = ()
    barren_goal_token_sets: tuple[frozenset[str], ...] = ()
    productive_goal_token_sets: tuple[frozenset[str], ...] = ()
    in_flight_route_hashes: frozenset[str] = field(default_factory=frozenset)
    # engine -> (facts_added, dead_ends, barren_concludes)
    engine_stats: Mapping[str, tuple[int, int, int]] = field(default_factory=dict)

    @staticmethod
    def from_graph(shared_graph: Any) -> "ClusterEvidence":
        fact_tokens: list[str] = []
        dead_ends: list[frozenset[str]] = []
        engine_facts: dict[str, int] = {}
        engine_dead: dict[str, int] = {}
        engine_barren: dict[str, int] = {}

        events_fn = getattr(shared_graph, "events", None)
        events: list[dict[str, Any]] = []
        if callable(events_fn):
            try:
                events = list(events_fn() or [])
            except Exception:
                events = []

        for ev in events[-400:]:
            kind = str(ev.get("kind") or "")
            actor = str(ev.get("actor") or "")
            eng = _engine_from_actor(actor)
            payload = ev.get("payload") if isinstance(ev.get("payload"), dict) else {}
            if kind == "fact_added":
                fact = str(payload.get("fact") or "")
                fact_tokens.extend(sorted(_tokens(fact)))
                if eng:
                    engine_facts[eng] = engine_facts.get(eng, 0) + 1
            elif kind == "dead_end":
                reason = str(payload.get("reason") or "")
                dead_ends.append(_tokens(reason))
                if eng:
                    engine_dead[eng] = engine_dead.get(eng, 0) + 1
            elif kind == "intent_concluded":
                # barren conclude: no to_fact
                if payload.get("to_fact_seq") in (None, "", 0):
                    if eng:
                        engine_barren[eng] = engine_barren.get(eng, 0) + 1

        barren_goals: list[frozenset[str]] = []
        productive_goals: list[frozenset[str]] = []
        try:
            for g in shared_graph.barren_concluded_goal_texts() or []:
                barren_goals.append(_tokens(str(g)))
        except Exception:
            pass
        # productive: concluded intents that produced facts — best-effort from events
        for ev in events[-400:]:
            if str(ev.get("kind") or "") != "intent_concluded":
                continue
            payload = ev.get("payload") if isinstance(ev.get("payload"), dict) else {}
            if payload.get("to_fact_seq") not in (None, "", 0):
                goal = str(payload.get("goal") or "")
                if goal:
                    productive_goals.append(_tokens(goal))

        routes: set[str] = set()
        try:
            routes = set(shared_graph.open_route_hashes() or [])
        except Exception:
            routes = set()

        stats: dict[str, tuple[int, int, int]] = {}
        for eng in set(engine_facts) | set(engine_dead) | set(engine_barren):
            stats[eng] = (
                engine_facts.get(eng, 0),
                engine_dead.get(eng, 0),
                engine_barren.get(eng, 0),
            )

        # keep last ~80 fact tokens for continuity
        recent = tuple(fact_tokens[-80:])
        return ClusterEvidence(
            recent_fact_tokens=recent,
            dead_end_token_sets=tuple(dead_ends[-40:]),
            barren_goal_token_sets=tuple(barren_goals[-40:]),
            productive_goal_token_sets=tuple(productive_goals[-40:]),
            in_flight_route_hashes=frozenset(routes),
            engine_stats=stats,
        )


def score_intent(
    intent: Mapping[str, Any],
    evidence: ClusterEvidence,
    *,
    batch_routes: set[str] | None = None,
) -> float:
    """Higher score → dispatch sooner.

    Deterministic pure function of intent fields + evidence bag.
    """

    wc = str(intent.get("worker_class") or "code")
    if wc in _VERIFY_CLASSES:
        # Preserve historical verifier/review first-class urgency.
        return 10_000.0 + float(intent.get("priority") or 0)

    goal = str(intent.get("goal") or "")
    gtoks = _tokens(goal)
    route = str(intent.get("route_hash") or "").strip().lower()
    # Raw planner priority is a weak hint only.  Unbounded *10 let high-priority
    # barren paraphrases outrank fact-linked continuations (adversarial H1).
    raw_priority = max(0.0, float(intent.get("priority") or 0))
    score = min(raw_priority, 5.0) * 4.0

    # Continuity: overlap with recent verified facts (long-chain retention).
    if gtoks and evidence.recent_fact_tokens:
        fact_set = frozenset(evidence.recent_fact_tokens)
        score += 55.0 * _jaccard(gtoks, fact_set)

    # Productive lineage: similar to past goals that produced facts.
    best_prod = 0.0
    for p in evidence.productive_goal_token_sets:
        best_prod = max(best_prod, _jaccard(gtoks, p))
    score += 35.0 * best_prod

    # Dead-end / barren penalties (do not re-walk ruled-out neighborhoods).
    # Stronger than priority: a known-barren neighborhood must lose to weak
    # but novel / fact-linked work under fixed worker budgets.
    best_dead = 0.0
    for d in evidence.dead_end_token_sets:
        best_dead = max(best_dead, _jaccard(gtoks, d))
    best_barren = 0.0
    for b in evidence.barren_goal_token_sets:
        best_barren = max(best_barren, _jaccard(gtoks, b))
    # If new verified facts substantially overlap this goal, treat re-entry as
    # recovery under new evidence rather than barren re-walk (local recovery).
    fact_overlap = 0.0
    if gtoks and evidence.recent_fact_tokens:
        fact_overlap = _jaccard(gtoks, frozenset(evidence.recent_fact_tokens))
    # Negation / avoidance goals that *mention* a dead technique should not be
    # treated as re-walking it ("prove NOT sql injection").
    negating = bool(
        gtoks & {"not", "no", "avoid", "without", "disprove", "rule", "ruled", "out"}
    )
    dead_scale = 20.0 if (negating and best_dead >= 0.3) else 90.0
    score -= dead_scale * best_dead
    if best_dead >= 0.45 and not negating:
        score -= 25.0

    barren_scale = 100.0
    if fact_overlap >= 0.35 and best_barren >= 0.3:
        barren_scale = 35.0  # soft reopen under new facts
        score += 18.0  # explicit recovery bonus
    elif negating and best_barren >= 0.3:
        barren_scale = 20.0
        score += 6.0
    score -= barren_scale * best_barren
    if best_barren >= 0.45 and fact_overlap < 0.35 and not negating:
        score -= 30.0

    # Route diversity: penalize routes already in flight this tick / on graph.
    if route:
        if route in evidence.in_flight_route_hashes:
            score -= 40.0
        if batch_routes and route in batch_routes:
            score -= 45.0
        else:
            score += 12.0  # novel route boost

    # Blank restarts (no from_facts / empty goal) rank lower than fact-linked work.
    from_facts = intent.get("from_facts") or intent.get("from_fact_seqs") or []
    if from_facts:
        score += 22.0
    if not gtoks:
        score -= 20.0

    # Unknown-seeking language is only a mild boost when not barren-like.
    unknownish = bool(gtoks & {"unknown", "unexplored", "unexpected", "why", "root"})
    if unknownish and best_barren < 0.25 and best_dead < 0.25:
        score += 8.0

    return score


def rank_open_intents(
    intents: Sequence[Mapping[str, Any]],
    evidence: ClusterEvidence,
    *,
    drop_hopeless: bool = True,
) -> list[dict[str, Any]]:
    """Stable sort: higher score first; ties keep original relative order.

    When ``drop_hopeless`` is true and at least one intent scores non-negative,
    intents with a strongly negative score (barren/dead-end neighborhood) are
    moved after all non-negative work.  They are not deleted — only deprioritized
    — so a fully-barren queue still drains instead of starving (run-7349 lesson).
    """

    batch_routes: set[str] = set()
    scored: list[tuple[float, int, dict[str, Any]]] = []
    for idx, raw in enumerate(intents):
        item = dict(raw)
        s = score_intent(item, evidence, batch_routes=batch_routes)
        route = str(item.get("route_hash") or "").strip().lower()
        if route:
            batch_routes.add(route)
        scored.append((s, idx, item))
    scored.sort(key=lambda row: (-row[0], row[1]))
    if not drop_hopeless or not scored:
        return [row[2] for row in scored]
    has_hope = any(s >= 0.0 for s, _i, _it in scored)
    if not has_hope:
        return [row[2] for row in scored]
    hopeful = [row for row in scored if row[0] >= -5.0]
    hopeless = [row for row in scored if row[0] < -5.0]
    ordered = hopeful + hopeless
    return [row[2] for row in ordered]


def engine_productivity(stats: tuple[int, int, int]) -> float:
    facts, dead, barren = stats
    attempts = max(1, facts + dead + barren)
    return (facts + 0.25) / (attempts + 0.5) - 0.15 * (barren / attempts)


def select_engine(
    *,
    available: Sequence[str],
    running: Sequence[str],
    evidence: ClusterEvidence,
    intent: Mapping[str, Any] | None = None,
    avoid_engines: Sequence[str] = (),
) -> str:
    """Pick a healthy engine for this intent under complementarity pressure.

    Preference order:
    1. Not currently running (heterogeneous coverage)
    2. Not in avoid_engines (same-tick batch diversity)
    3. Higher historical productivity (facts vs barren/dead)
    4. Stable name order as final tie-break
    """

    if not available:
        raise ValueError("available engines required")
    running_set = {str(e) for e in running}
    avoid_set = {str(e) for e in avoid_engines}
    intent = intent or {}
    gtoks = _tokens(str(intent.get("goal") or ""))

    def key(name: str) -> tuple:
        base = name.split("-")[0].split("_")[0].lower()
        stats = evidence.engine_stats.get(base) or evidence.engine_stats.get(name) or (0, 0, 0)
        prod = engine_productivity(stats)
        idle = 0 if _base_running(name, running_set) else 1
        avoided = 0 if name in avoid_set or base in avoid_set else 1
        # slight affinity: if goal mentions an engine tool-family, tiny nudge only
        affinity = 0.0
        if gtoks:
            if base == "cursor" and ({"ui", "frontend", "web"} & gtoks):
                affinity = 0.05
            if base == "claude" and ({"recon", "read", "analyze", "review"} & gtoks):
                affinity = 0.05
            if base == "codex" and ({"patch", "fix", "implement", "code"} & gtoks):
                affinity = 0.05
        return (-idle, -avoided, -(prod + affinity), name)

    return sorted(available, key=key)[0]


def _base_running(name: str, running_set: set[str]) -> bool:
    base = name.split("-")[0].split("_")[0].lower()
    if name in running_set:
        return True
    return any(
        r == base or r.startswith(f"{base}-") or base in r.split("-")
        for r in running_set
    )


def plan_dispatch(
    intents: Sequence[Mapping[str, Any]],
    *,
    shared_graph: Any,
    running_engines: Sequence[str] = (),
) -> list[dict[str, Any]]:
    """End-to-end: evidence extract → rank intents. Engine pick is separate."""

    evidence = ClusterEvidence.from_graph(shared_graph)
    ranked = rank_open_intents(intents, evidence)
    # annotate scores for observability (coordinator may log; workers ignore)
    out: list[dict[str, Any]] = []
    for item in ranked:
        row = dict(item)
        row["cluster_planner_score"] = score_intent(item, evidence)
        out.append(row)
    return out


__all__ = (
    "ClusterEvidence",
    "engine_productivity",
    "plan_dispatch",
    "planner_enabled_from_env",
    "rank_open_intents",
    "score_intent",
    "select_engine",
)
