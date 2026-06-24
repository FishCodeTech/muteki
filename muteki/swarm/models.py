"""Model lineup for the solver swarm (§5.2), configurable, budget-trimmable.

The design's heterogeneous lineup is Opus/GPT-5/o-series for complementary blind
spots. For now we're unified on DeepSeek (per the current constraint), so the
"lineup" varies model + temperature + role label rather than vendor. The shape
is what matters: `default_lineup(n)` returns N ModelSpecs the swarm races.

When other vendors come back, add their ModelSpecs here — nothing else changes.
"""

from __future__ import annotations

from muteki.core.llm import ModelSpec

# Role preambles give each swarm seat a different STRATEGIC PRIOR so they explore
# disjoint hypothesis spaces (temperature alone perturbs sampling, not strategy —
# all seats tend to try the same thing first). The InsightBus then fuses
# COMPLEMENTARY discoveries instead of duplicate ones. One seat stays generalist
# (role="") so coverage is a SUPERSET, not a partition (§5.2 + audit caveat).
ROLE_PREAMBLES: dict[str, str] = {
    "recon": (
        "## Your strategic prior: RECON SPECIALIST\n"
        "Lead with discovery before exploitation. Prioritize: enumerate hidden "
        "endpoints/files/params, read ALL provided attachments + page source/JS/"
        "comments, decode encodings, fingerprint the backend/version, map the "
        "attack surface. Surface facts the others can build on. Broadcast what you "
        "find. Only after recon, pivot to the most-indicated attack.\n\n"
    ),
    "exploit": (
        "## Your strategic prior: EXPLOITATION SPECIALIST\n"
        "Lead with the highest-probability injection/exploit, fast. Prioritize: "
        "SQLi/NoSQL/SSTI/command injection, auth bypass, deserialization, "
        "memory/crypto-math exploitation. When sibling solvers broadcast a "
        "confirmed fact (endpoint, version, leaked value), immediately weaponize "
        "it. Go deep on one chain rather than wide.\n\n"
    ),
    "lateral": (
        "## Your strategic prior: LATERAL / UNCONVENTIONAL THINKER\n"
        "Assume the obvious path is a decoy. Prioritize: logic flaws, race "
        "conditions, type confusion, unusual encodings, protocol/header abuse, "
        "off-by-one in the challenge's own assumptions, and re-reading the source "
        "for the ONE weird line. When others are stuck on the mainstream attack, "
        "try the angle they wouldn't.\n\n"
    ),
}

# The full default lineup. Heterogeneity is model + temperature + ROLE PRIOR.
# Seat order matters: a 2-solver swarm gets a generalist + an exploit specialist
# (the highest-ROI complementary pair); 3+ adds recon then lateral.
_FULL_LINEUP: list[ModelSpec] = [
    ModelSpec(solver_id="pro-gen", model="deepseek-v4-pro", temperature=0.3,
              role="", label="pro/generalist — methodical all-rounder"),
    ModelSpec(solver_id="pro-exploit", model="deepseek-v4-pro", temperature=0.6,
              role="exploit", label="pro/exploit specialist"),
    ModelSpec(solver_id="flash-recon", model="deepseek-v4-flash", temperature=0.4,
              role="recon", label="flash/recon specialist — cheap broad discovery"),
    ModelSpec(solver_id="flash-lateral", model="deepseek-v4-flash", temperature=0.8,
              role="lateral", label="flash/lateral — high-variance unconventional"),
]


def default_lineup(n: int = 2) -> list[ModelSpec]:
    """First N specs of the default lineup (N trimmed by budget upstream)."""
    n = max(1, min(n, len(_FULL_LINEUP)))
    return [_clone(s) for s in _FULL_LINEUP[:n]]


def _clone(s: ModelSpec) -> ModelSpec:
    return ModelSpec(
        solver_id=s.solver_id,
        model=s.model,
        temperature=s.temperature,
        max_tokens=s.max_tokens,
        label=s.label,
        role=s.role,
    )
