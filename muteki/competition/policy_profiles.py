"""CompetitionPolicy 策略档：通用参数表，不绑定具体 platform_kind。"""

from __future__ import annotations

from typing import Any, Optional

from muteki.competition.models import AutomationMode, CompetitionPolicy

POLICY_PROFILE_DEFAULT = "default"
POLICY_PROFILE_TSEC_EVAL = "tsec_eval"

KNOWN_POLICY_PROFILES = frozenset({
    POLICY_PROFILE_DEFAULT,
    POLICY_PROFILE_TSEC_EVAL,
})

#: tsec_eval 完成态默认值（对齐 hxbai / Tsecbench v1 正式赛量级）。
TSEC_EVAL_DEFAULTS: dict[str, Any] = {
    "policy_profile": POLICY_PROFILE_TSEC_EVAL,
    "automation_mode": AutomationMode.AUTONOMOUS.value,
    "max_concurrent_runs": 3,
    "max_instances": 3,
    "submission_cooldown_seconds": 1.0,
    "round_timeboxes_s": [],
    "visit_floor_s": 0.0,
    "fill_idle_revisits": False,
    "terminal_phase_fill_idle": False,
    "overdue_mult": 1.25,
    "working_set": 3,
    "keepalive_max": 0,
    "keepalive_tail_s": 1800.0,
    "total_budget_s": 21300.0,
    "dry_defer_waves": 4,
    "deepchain_slots": 1,
    "per_challenge_seconds": 4000.0,
    "stuck_waves_cap": 2,
}


def resolve_policy_profile(name: str) -> dict[str, Any]:
    """返回策略档默认字段副本；未知名称回落 default（空覆盖）。"""
    key = str(name or "").strip() or POLICY_PROFILE_DEFAULT
    if key == POLICY_PROFILE_TSEC_EVAL:
        return dict(TSEC_EVAL_DEFAULTS)
    return {"policy_profile": POLICY_PROFILE_DEFAULT}


def merge_policy_hints(
    base: CompetitionPolicy,
    *,
    profile: Optional[str] = None,
    hints: Optional[dict[str, Any]] = None,
) -> CompetitionPolicy:
    """用策略档与 probe policy_hints 填充策略字段。

    当显式指定 ``profile``（或 hints 带 policy_profile）且当前仍是
    ``default``/空时，策略档默认值整表写入；调用方已设置的非默认字段保留。
    """
    updates: dict[str, Any] = {}
    profile_name = (
        str(profile or "").strip()
        or str(getattr(base, "policy_profile", "") or "").strip()
        or str((hints or {}).get("policy_profile") or "").strip()
    )
    applying_fresh_profile = bool(profile or (hints or {}).get("policy_profile")) and (
        not base.policy_profile or base.policy_profile == POLICY_PROFILE_DEFAULT
    )
    if profile_name:
        defaults = resolve_policy_profile(profile_name)
        for key, value in defaults.items():
            if applying_fresh_profile:
                updates[key] = value
            else:
                current = getattr(base, key, None)
                if current in (None, "", [], {}):
                    updates[key] = value
    if hints:
        for key, value in dict(hints).items():
            if key == "policy_profile":
                continue
            if hasattr(base, key) or key in TSEC_EVAL_DEFAULTS:
                # hints 覆盖档内同名字段（probe 建议优先于静态档）。
                updates[key] = value
    if not updates:
        return base
    return base.model_copy(update=updates)


def visit_timebox_seconds(policy: CompetitionPolicy, visit_index: int) -> int:
    """按 visit 序号取时间盒；超出序列时用最后一档。"""
    boxes = list(getattr(policy, "round_timeboxes_s", None) or [])
    if not boxes:
        per = float(getattr(policy, "per_challenge_seconds", 0) or 0)
        return int(per) if per > 0 else 600
    idx = max(0, int(visit_index))
    if idx >= len(boxes):
        return int(boxes[-1])
    return int(boxes[idx])


def difficulty_rank(value: str) -> int:
    return {"easy": 0, "medium": 1, "hard": 2}.get(str(value or "").lower(), 1)


def difficulty_of(category: str = "", name: str = "") -> str:
    """从 ``tsec/hard`` 这类 category 或题名里抽出 easy/medium/hard。"""
    blob = f"{category} {name}".lower()
    for token in ("easy", "medium", "hard"):
        if token in blob:
            return token
    return "medium"


__all__ = [
    "KNOWN_POLICY_PROFILES",
    "POLICY_PROFILE_DEFAULT",
    "POLICY_PROFILE_TSEC_EVAL",
    "TSEC_EVAL_DEFAULTS",
    "difficulty_of",
    "difficulty_rank",
    "merge_policy_hints",
    "resolve_policy_profile",
    "visit_timebox_seconds",
]
