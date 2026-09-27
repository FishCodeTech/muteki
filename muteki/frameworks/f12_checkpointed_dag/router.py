"""F12 deterministic Worker Profile routing (design §7.3).

Each dispatch jointly selects:

    (Worker Profile, engine, model, reasoning effort, permission mode, runtime)

In Muteki these five axes are properties OF the Worker Profile, so the joint
choice reduces to picking one profile — the event payload records the full
tuple so eval reports can audit every axis.

Task capability tiers come from the F12 task metadata (never model-name string
guessing). Profile capability tiers are NOT a product profile field, so F12
carries explicit experiment-layer metadata: ``f12_profile_tiers`` constructor
kwarg or ``stage_policy.coordinator["f12"]["profile_tiers"]``. A profile
without metadata defaults to C1 (the middle tier) — deterministic and
documented, not inferred from the model string.

Scoring is deterministic and explainable (F05-style bidding stays a separate
ablation variable):

1. profile tier must satisfy the task's effective tier;
2. prefer the SMALLEST tier headroom (don't burn a C3 seat on C1 work);
3. prefer the operator-ranked profile (lower ``priority`` number first);
4. prefer the less-loaded profile (fewer running workers);
5. prefer a provider different from the source coordinator's model provider
   (independence);
6. final tie-break: profile name.
"""

from __future__ import annotations

from typing import Any

from muteki.frameworks.f12_checkpointed_dag.scheduler import TIER_ORDER

TIER_NAMES = ("C0", "C1", "C2", "C3")


def profile_tier(profile: dict[str, Any], tiers_meta: dict[str, str]) -> str:
    name = str(profile.get("name") or profile.get("id") or "")
    raw = str(tiers_meta.get(name) or tiers_meta.get("*") or "").strip().upper()
    if raw in TIER_NAMES:
        return raw
    return "C1"  # documented default; never guessed from the model name


def _provider_key(profile: dict[str, Any]) -> str:
    """Coarse provider identity for the diversity tie-break (no secrets)."""
    base_url = str(profile.get("base_url") or "")
    if base_url:
        host = base_url.split("://", 1)[-1].split("/", 1)[0].split(":")[0]
        if host:
            return host
    return str(profile.get("engine") or "")


def choose_profile(
    candidates: list[dict[str, Any]],
    *,
    required_tier: str,
    tiers_meta: dict[str, str],
    running_counts: dict[str, int] | None = None,
    source_provider: str = "",
    fixed_profile: str = "",
) -> dict[str, Any] | None:
    """Pick one profile for a task. Returns None when nothing qualifies.

    ``fixed_profile`` implements the A4 ablation (single pinned profile, joint
    routing removed): when set, the named profile wins whenever present.
    """
    if fixed_profile:
        for p in candidates:
            if str(p.get("name") or p.get("id") or "") == fixed_profile:
                return p
        return None
    need = TIER_ORDER.get(required_tier, 1)
    running_counts = running_counts or {}
    qualified: list[tuple[Any, ...]] = []
    for p in candidates:
        name = str(p.get("name") or p.get("id") or "")
        have = TIER_ORDER.get(profile_tier(p, tiers_meta), 1)
        if have < need:
            continue
        headroom = have - need
        try:
            prio = int(p.get("priority") or 100)
        except (TypeError, ValueError):
            prio = 100
        load = int(running_counts.get(name) or 0)
        # independence tie-break: same-provider-as-source ranks later
        same_provider = 1 if (source_provider and _provider_key(p) == source_provider) else 0
        qualified.append((headroom, prio, load, same_provider, name, p))
    if not qualified:
        return None
    qualified.sort(key=lambda row: (row[0], row[1], row[2], row[3], row[4]))
    return qualified[0][5]


def routing_event_payload(
    profile: dict[str, Any],
    *,
    task_id: str,
    intent_id: str,
    required_tier: str,
) -> dict[str, Any]:
    """The auditable joint-selection tuple for one dispatch."""
    return {
        "task_id": task_id,
        "intent_id": intent_id,
        "required_tier": required_tier,
        "profile": str(profile.get("name") or profile.get("id") or ""),
        "engine": str(profile.get("engine") or ""),
        "model": str(profile.get("model") or ""),
        "reasoning_effort": str(profile.get("reasoning_effort") or ""),
        "permission_mode": str(profile.get("credential_mode") or ""),
        "runtime": str(profile.get("runtime_instance_ref") or profile.get("transport") or ""),
        "credential_seat_ref": str(profile.get("credential_seat_ref") or ""),
    }
