"""Profile selection for auxiliary workers; never invent an unbound engine."""
from typing import Any
from copy import deepcopy

from muteki.solver.worker_profiles import base_engine_for_profile


def require_followup_profile(ref: str, profiles: list[dict], *, allow_engine: bool = False) -> dict:
    rows = [p for p in profiles if isinstance(p, dict)]
    exact = [p for p in rows if ref in {p.get('id'), p.get('name')}]
    if exact:
        if len(exact) == 1 and exact[0].get('enabled', True):
            return deepcopy(exact[0])
        raise ValueError('Selected follow-up Worker is disabled or ambiguous; select an enabled Worker Profile')
    if allow_engine:
        matches = [p for p in rows if p.get('enabled', True) and base_engine_for_profile(p) == ref]
        if len(matches) == 1:
            return deepcopy(matches[0])
    raise ValueError('Selected follow-up Worker is unavailable or ambiguous; select a Worker Profile')


def select_btw_profile(config: dict[str, Any], winner: dict[str, Any], requested: str = '') -> dict:
    profiles = config.get('worker_profiles') or []
    policy = (config.get('stage_policy') or {}).get('coordinator') or {}
    if requested:
        return require_followup_profile(requested, profiles, allow_engine=True)
    if policy.get('btw_profile'):
        return require_followup_profile(policy['btw_profile'], profiles)
    review_ref = str((policy.get('review') or {}).get('engine') or '')
    if review_ref and (policy.get("review") or {}).get("enabled"):
        return require_followup_profile(review_ref, profiles, allow_engine=True)
    # Legacy default engine names are not a selection of an actual Profile.
    if any(review_ref in {p.get('id'), p.get('name')} for p in profiles if isinstance(p, dict)):
        return require_followup_profile(review_ref, profiles)
    if winner.get('profile_id'):
        return require_followup_profile(winner['profile_id'], profiles)
    if review_ref:
        matches = [p for p in profiles if isinstance(p, dict) and p.get('enabled', True)
                   and base_engine_for_profile(p) == review_ref]
        if len(matches) > 1:
            raise ValueError('BTW Review engine matches multiple Profiles; select a BTW Worker')
        if len(matches) == 1:
            return deepcopy(matches[0])
    for p in profiles:
        if isinstance(p, dict) and p.get('enabled', True) and set(p.get('roles') or []) & {'respond', 'review'}:
            return deepcopy(p)
    for ref in config.get('engines') or []:
        return require_followup_profile(str(ref), profiles, allow_engine=True)
    raise ValueError('No BTW Worker Profile is configured; select one in Worker settings')


def configured_standby_identity(winner: dict, config: dict, action: str) -> dict | None:
    ref = ((config.get('stage_policy') or {}).get('coordinator') or {}).get('standby_profile')
    if not ref or action not in {'ask', 'writeup'}:
        return None
    profile = require_followup_profile(ref, config.get('worker_profiles') or [])
    # A dedicated worker gets task context, never another account's CLI session.
    return {**winner, 'profile_id': profile.get('id') or profile.get('name'),
            'engine': base_engine_for_profile(profile), 'worker_id': '',
            'session': '', 'workdir_rel': '', 'agent_state_rel': ''}
