"""Default worker-roster configuration — which engines launch per challenge.

An OPERATOR preference (like the rail meta side-table), not part of the
event-sourced solve: a single small JSON file under the sessions root, loaded on
startup and rewritten on each mutation. It answers "when a challenge is
dispatched and the request doesn't say otherwise, which engines run, and how
many bootstrap workers?" — with an optional per-category override (e.g. give pwn
only claude+codex, give web all three).

The dispatch path (apps/web/drivers.py) reads `resolve(category)` as the FALLBACK
when the request body carries no explicit engines/start_workers; an explicit body
always wins, so this never overrides an intentional per-run choice.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Optional

from muteki.core.runtime_env import is_web_container
from muteki.solver.worker_profiles import (
    VALID_BASE_ENGINES,
    normalize_profile_roster,
    normalize_worker_profiles,
    resolve_seat_ref,
)
from muteki.solver.identity_model import (
    migrate_legacy_config,
    seats_to_legacy_profiles,
    is_legal_combo,
)

VALID_ENGINES = VALID_BASE_ENGINES
VALID_BACKENDS = ("local", "container")
DEFAULT_MAX_WORKERS = 10
DEFAULT_WORKER_BACKEND = "container"
DEFAULT_RACE_TIMEOUT = 720
DEFAULT_WALL_CLOCK_BUDGET = 0
DEFAULT_MAX_TOTAL_WORKERS = 0
DEFAULT_COST_BUDGET_USD = 0.0
DEFAULT_REVIEW_POLICY = {
    "enabled": True,
    "engine": "claude-sub-container",
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
}
DEFAULT_LLM_PROFILES = {
    "planner": {"provider": "deepseek", "model": "deepseek-v4-pro", "base_url": ""},
    "titler": {"provider": "deepseek", "model": "deepseek-v4-flash", "base_url": ""},
}

DEFAULT_RUNTIME_PROFILES = [
    {"id": "local", "backend": "local", "label": "Local host"},
    {"id": "docker-web", "backend": "container", "label": "Docker web",
     "network": "bridge", "memory": "12g", "cpus": "4", "pids_limit": 2048},
    {"id": "docker-host-target", "backend": "container", "label": "Docker host target",
     "network": "host", "memory": "12g", "cpus": "4", "pids_limit": 2048},
    {"id": "docker-offline", "backend": "container", "label": "Docker offline",
     "network": "none", "memory": "12g", "cpus": "4", "pids_limit": 2048},
    {"id": "docker-pwn-heavy", "backend": "container", "label": "Docker pwn heavy",
     "network": "bridge", "memory": "24g", "cpus": "8", "pids_limit": 4096},
]
DEFAULT_WORKER_PROFILES = [
    {"id": "claude-sub-container", "name": "claude-sub-container",
     "engine": "claude", "transport": "claude_code",
     "auth": "subscription", "credential_mode": "subscription",
     "credential_account": "claude-main", "api_key_ref": "", "base_url": "",
     "wire_api": "",
     "runtime": "docker-web", "roles": ["race", "bootstrap", "explore", "respond", "review"],
     "race": True, "max_running": 2, "max_review_running": 0, "priority": 10, "model": "",
     "enabled": True},
    {"id": "codex-sub-container", "name": "codex-sub-container",
     "engine": "codex", "transport": "codex_cli",
     "auth": "subscription", "credential_mode": "subscription",
     "credential_account": "codex-main", "api_key_ref": "", "base_url": "",
     "wire_api": "responses",
     "runtime": "docker-web", "roles": ["race", "bootstrap", "explore", "review"],
     "race": True, "max_running": 1, "max_review_running": 0, "priority": 20, "model": "",
     "enabled": True},
    {"id": "cursor-api-container", "name": "cursor-api-container",
     "engine": "cursor", "transport": "cursor_agent",
     "auth": "api_key", "credential_mode": "api_key",
     "credential_account": "cursor-main", "api_key_ref": "", "base_url": "",
     "wire_api": "",
     "runtime": "docker-web", "roles": ["race", "bootstrap", "explore", "review"],
     "race": True, "max_running": 2, "max_review_running": 0, "priority": 30, "model": "",
     "enabled": True},
]
DEFAULT_ENGINES = [p["name"] for p in DEFAULT_WORKER_PROFILES]


def resolve_worker_backend(
    *,
    request_backend: Any = None,
    config_backend: Any = None,
    env_backend: Any = None,
    default_backend: str = DEFAULT_WORKER_BACKEND,
    in_web_container: bool,
) -> str:
    """THE single backend resolver. Every caller (dispatch precheck, settings
    health endpoints, config read/write) routes through this so they can never
    disagree on the effective backend — a disagreement was a false-green axis
    (settings evaluated `local` while dispatch force-containerized).

    Precedence: explicit request > stored config > env > default. Then:
      - `container_dockerexec` is the CONTAINER transport selector; it still means
        "container" for the backend choice, so normalize it.
      - anything not in VALID_BACKENDS falls back to `local`.
      - WEB-CONTAINER OVERRIDE (always applied, NOT optional): when this process
        runs inside a container, `local` would spawn a host-native CLI inside the
        web container (no tools, wrong creds). Force `container`. The override is
        unconditional precisely so settings and dispatch are identical.
    """
    backend = request_backend or config_backend or env_backend or default_backend
    if backend == "container_dockerexec":
        backend = "container"
    if backend not in VALID_BACKENDS:
        backend = "local"
    if backend == "local" and in_web_container:
        return "container"
    return backend


def backend_for_profile(
    profile: dict[str, Any],
    *,
    runtime_profiles: list[dict[str, Any]] | None,
    worker_backend: str,
    in_web_container: bool,
) -> str:
    """Effective backend for ONE profile. A profile names a `runtime` whose own
    `backend` (e.g. `docker-web` → container, `local` → local) takes precedence
    over the global `worker_backend`; the web-container override still applies on
    top via resolve_worker_backend. This is the per-profile resolution dispatch
    uses, so the settings page must use the SAME mapping or the badge can predict
    a different backend than the run actually uses.
    """
    runtime_by_id = {
        str(r.get("id")): r for r in (runtime_profiles or []) if isinstance(r, dict)
    }
    rt = runtime_by_id.get(str(profile.get("runtime") or ""))
    rt_backend = str((rt or {}).get("backend") or "") if rt else ""
    return resolve_worker_backend(
        config_backend=rt_backend or worker_backend,
        in_web_container=in_web_container,
    )


def _profile_kind(profile: dict[str, Any]) -> str:
    mode = str(
        profile.get("credential_mode") or profile.get("auth") or "subscription"
    ).strip()
    return "api" if mode in {"api", "api_key", "oauth_token"} else "sub"


def _canonical_profile_id(profile: dict[str, Any], backend: str) -> str:
    engine = str(profile.get("engine") or "").strip()
    if not engine:
        return str(profile.get("name") or profile.get("id") or "").strip()
    kind = _profile_kind(profile)
    if backend == "local":
        return f"{engine}-api-local" if kind == "api" else f"{engine}-local"
    return f"{engine}-{kind}-container"


def _canonical_profile_aliases(profile: dict[str, Any]) -> set[str]:
    return {
        _canonical_profile_id(profile, "local"),
        _canonical_profile_id(profile, "container"),
    }


def _clean_engines(value: Any, profiles: list[dict[str, Any]] | None = None) -> list[str]:
    """Filter to known profile names, expanding legacy base-engine names."""
    return normalize_profile_roster(value, profiles or DEFAULT_WORKER_PROFILES)


def _remap_profile_ref(ref: Any, profiles: list[dict[str, Any]], backend: str) -> Any:
    if not isinstance(ref, str) or backend not in VALID_BACKENDS:
        return ref
    by_name = {str(p.get("name") or p.get("id")): p for p in profiles}
    if ref in by_name:
        return ref
    for p in profiles:
        aliases = _canonical_profile_aliases(p)
        target = _canonical_profile_id(p, backend)
        if ref in aliases and target in by_name:
            return target
    return ref


def _remap_profile_refs(value: Any, profiles: list[dict[str, Any]], backend: str) -> Any:
    if isinstance(value, list):
        return [_remap_profile_ref(v, profiles, backend) for v in value]
    return _remap_profile_ref(value, profiles, backend)


def _clean_engines_for_backend(
    value: Any,
    profiles: list[dict[str, Any]],
    backend: str,
) -> list[str]:
    return _clean_engines(_remap_profile_refs(value, profiles, backend), profiles)


def _profile_name(profile: dict[str, Any]) -> str:
    return str(profile.get("name") or profile.get("id") or "").strip()


def _ordinary_worker_roles(profile: dict[str, Any]) -> set[str]:
    roles = profile.get("roles") or []
    return {
        str(r)
        for r in roles
        if str(r) in {"race", "bootstrap", "explore", "respond"}
    }


class WorkerConfigStore:
    def __init__(self, root: str | Path = "sessions") -> None:
        self._root = Path(root)
        self.path = self._root / "_worker_config.json"
        self._data: dict[str, Any] = {}
        self._load()

    def _load(self) -> None:
        if not self.path.exists():
            return
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
            if isinstance(raw, dict):
                self._data = raw
        except (json.JSONDecodeError, OSError):
            # a corrupt config must never break startup — fall back to defaults
            self._data = {}
        self._project_identity_to_legacy()

    def _project_identity_to_legacy(self) -> None:
        """If the on-disk config is NEW-shaped (seats/credentials/environments),
        adapt it into the legacy worker_profiles/runtime_profiles `self._data`
        carries, so the entire existing get() pipeline (5 foreign keys, backend
        remap, drivers) keeps working with ZERO change. Legacy-shaped configs are
        left untouched. Never raises."""
        d = self._data
        if not (isinstance(d.get("seats"), list) and d.get("seats")):
            return
        try:
            seats = [s for s in d["seats"] if isinstance(s, dict)]
            creds = [c for c in (d.get("credentials") or []) if isinstance(c, dict)]
            envs = [e for e in (d.get("environments") or []) if isinstance(e, dict)]
            # adapt seats → legacy worker_profiles for the scheduler/drivers.
            d["worker_profiles"] = seats_to_legacy_profiles(seats, creds, envs)
            # environments → runtime_profiles (same shape, just renamed concept).
            if envs:
                d["runtime_profiles"] = [
                    {k: v for k, v in {
                        "id": e.get("id"), "backend": e.get("backend"),
                        "label": e.get("label"), "network": e.get("network"),
                        "memory": e.get("memory"), "cpus": e.get("cpus"),
                        "pids_limit": e.get("pids_limit"),
                    }.items() if v not in (None, "", 0)}
                    for e in envs
                ]
            # remap any seat-id/label foreign keys (engines[], review.engine, ...)
            # to legacy profile names so the existing remap machinery resolves them.
            alias = {str(s.get("label")): str(s.get("id")) for s in seats if s.get("label")}
            id_to_name = {str(s.get("id")): str(s.get("id")) for s in seats}

            def _to_name(ref: Any) -> Any:
                sid = resolve_seat_ref(ref, seats=seats, alias_table=alias)
                return sid if sid in id_to_name else ref

            if isinstance(d.get("engines"), list):
                d["engines"] = [_to_name(r) for r in d["engines"]]
            if isinstance(d.get("race_engines"), list):
                d["race_engines"] = [_to_name(r) for r in d["race_engines"]]
            # The dispatch lineup MUST track the seats' enabled toggles — that's
            # the only lineup control the seat UI exposes. A stale top-level
            # `engines` (e.g. left over from a legacy config, or a seat that was
            # since enabled/disabled) otherwise wins at get() (it short-circuits
            # the "else enabled seats" fallback), so enabling two more seats in
            # the UI left dispatch racing only the one stale engine. Reconcile:
            # the lineup is exactly the enabled seats, preserving the order of any
            # already named in `engines`, then appending newly-enabled ones.
            enabled_ids = [str(s.get("id")) for s in seats
                           if s.get("enabled", True) and s.get("id")]
            enabled_set = set(enabled_ids)
            prior = [r for r in (d.get("engines") or []) if r in enabled_set]
            d["engines"] = prior + [sid for sid in enabled_ids if sid not in prior]
            # race_engines is an optional SUBSET knob: keep only still-enabled
            # seats (drop stale refs), but don't force-add — empty means "all".
            if isinstance(d.get("race_engines"), list):
                d["race_engines"] = [r for r in d["race_engines"] if r in enabled_set]
            sp = d.get("stage_policy")
            if isinstance(sp, dict):
                race = sp.get("race")
                if isinstance(race, dict) and isinstance(race.get("engines"), list):
                    race["engines"] = [_to_name(r) for r in race["engines"]]
                review = (sp.get("coordinator") or {}).get("review") if isinstance(sp.get("coordinator"), dict) else None
                if isinstance(review, dict) and review.get("engine"):
                    review["engine"] = _to_name(review["engine"])
        except Exception:  # noqa: BLE001 — projection must never break startup
            pass

    def _flush(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(self._data, ensure_ascii=False, indent=2),
                       encoding="utf-8")
        tmp.replace(self.path)  # atomic on POSIX

    def _account_modes(self) -> dict[str, str]:
        """Map account_id → on-disk credential mode, so migration binds an empty
        profile to its real default account as engine_key (not host-inherit).
        Never raises — a missing/locked secrets store just yields {}."""
        try:
            from muteki.solver.credential_accounts import (
                CredentialAccountStore, account_store_root,
            )
            store = CredentialAccountStore(account_store_root(self._root))
            return {a["account_id"]: str(a.get("mode") or "") for a in store.list()}
        except Exception:  # noqa: BLE001
            return {}

    def identity_model(self) -> dict[str, Any]:
        """The NEW Credential/Seat/Environment view, authoritative when the on-disk
        config is already new-shaped. Never raises. (When legacy-shaped, callers
        should read get()['seats'/'credentials'/'environments'], which migrates.)"""
        d = self._data
        seats = [s for s in (d.get("seats") or []) if isinstance(s, dict)]
        creds = [c for c in (d.get("credentials") or []) if isinstance(c, dict)]
        envs = [e for e in (d.get("environments") or []) if isinstance(e, dict)]
        seat_alias = {str(s.get("label")): str(s.get("id")) for s in seats if s.get("label")}
        cred_alias = {str(c.get("secret_ref")): str(c.get("id"))
                      for c in creds if c.get("secret_ref")}
        return {
            "credentials": creds, "seats": seats, "environments": envs,
            "seat_alias": seat_alias, "credential_alias": cred_alias,
        }

    def get(self) -> dict[str, Any]:
        """The current default config with everything filled in (never raises)."""
        d = self._data
        runtime_profiles = self._clean_runtime_profiles(d.get("runtime_profiles"))
        worker_profiles = self._clean_worker_profiles(d.get("worker_profiles"))
        worker_backend = self._clean_backend(d.get("worker_backend"))
        engines = _clean_engines_for_backend(d.get("engines"), worker_profiles, worker_backend) or [
            p["name"] for p in worker_profiles if p.get("enabled", True)
        ]
        start_workers = self._coerce_pos_int(d.get("start_workers"), len(engines))
        max_workers = self._coerce_pos_int(d.get("max_workers"), DEFAULT_MAX_WORKERS)
        race_scout = self._coerce_bool(d.get("race_scout"), True)
        race_timeout = self._coerce_pos_int(d.get("race_timeout"), DEFAULT_RACE_TIMEOUT)
        wall_clock_budget = self._coerce_nonneg_int(
            d.get("wall_clock_budget"), DEFAULT_WALL_CLOCK_BUDGET)
        max_total_workers = self._coerce_nonneg_int(
            d.get("max_total_workers"), DEFAULT_MAX_TOTAL_WORKERS)
        cost_budget_usd = self._coerce_nonneg_float(
            d.get("cost_budget_usd"), DEFAULT_COST_BUDGET_USD)
        race_engines = _clean_engines_for_backend(
            d.get("race_engines"), worker_profiles, worker_backend)
        llm_profiles = self._clean_llm_profiles(d.get("llm_profiles"))
        raw_stage_policy = d.get("stage_policy")
        if isinstance(raw_stage_policy, dict):
            raw_stage_policy = json.loads(json.dumps(raw_stage_policy))
            race = raw_stage_policy.setdefault("race", {})
            race["engines"] = _remap_profile_refs(
                race.get("engines"), worker_profiles, worker_backend)
            review = raw_stage_policy.setdefault("coordinator", {}).setdefault("review", {})
            review["engine"] = _remap_profile_ref(
                review.get("engine") or DEFAULT_REVIEW_POLICY["engine"],
                worker_profiles,
                worker_backend,
            )
        stage_policy = self._clean_stage_policy(raw_stage_policy, {
            "race_scout": race_scout,
            "race_timeout": race_timeout,
            "race_engines": race_engines,
            "wall_clock_budget": wall_clock_budget,
            "max_total_workers": max_total_workers,
            "cost_budget_usd": cost_budget_usd,
        })
        names = {str(p.get("name") or p.get("id")) for p in worker_profiles}
        review = stage_policy.setdefault("coordinator", {}).setdefault(
            "review", dict(DEFAULT_REVIEW_POLICY))
        review_engine = _remap_profile_ref(
            review.get("engine") or DEFAULT_REVIEW_POLICY["engine"],
            worker_profiles,
            worker_backend,
        )
        if review_engine not in names:
            review_engine = next(
                (
                    str(p.get("name") or p.get("id"))
                    for p in worker_profiles
                    if "review" in (p.get("roles") or [])
                ),
                engines[0] if engines else DEFAULT_REVIEW_POLICY["engine"],
            )
        review["engine"] = review_engine
        overrides: dict[str, Any] = {}
        raw_ov = d.get("overrides")
        if isinstance(raw_ov, dict):
            for cat, ov in raw_ov.items():
                if not isinstance(ov, dict):
                    continue
                cat_engines = _clean_engines_for_backend(
                    ov.get("engines"), worker_profiles, worker_backend)
                if not cat_engines:
                    continue
                overrides[str(cat)] = {
                    "engines": cat_engines,
                    "start_workers": self._coerce_pos_int(
                        ov.get("start_workers"), len(cat_engines)),
                }
        result = {
            "engines": engines,
            "start_workers": start_workers,
            "max_workers": max_workers,
            "worker_backend": worker_backend,
            "race_scout": race_scout,
            "race_timeout": race_timeout,
            "wall_clock_budget": wall_clock_budget,
            "race_engines": race_engines,
            "max_total_workers": max_total_workers,
            "cost_budget_usd": cost_budget_usd,
            "stage_policy": stage_policy,
            "llm_profiles": llm_profiles,
            "runtime_profiles": runtime_profiles,
            "worker_profiles": worker_profiles,
            "overrides": overrides,
        }
        # ── additive: attach the new Credential/Seat/Environment view (Phase A
        # iron rule — old fields above stay; new fields are added alongside so the
        # legacy frontend keeps working while the new UI can consume these). ──
        if isinstance(self._data.get("seats"), list) and self._data.get("seats"):
            ident = self.identity_model()
        else:
            res = migrate_legacy_config(
                worker_profiles=worker_profiles, runtime_profiles=runtime_profiles,
                account_modes=self._account_modes(),
            )
            ident = {
                "credentials": [c.to_dict() for c in res.credentials],
                "seats": [s.to_dict() for s in res.seats],
                "environments": [e.to_dict() for e in res.environments],
                "seat_alias": res.seat_alias,
                "credential_alias": res.credential_alias,
            }
        result["credentials"] = ident["credentials"]
        result["seats"] = ident["seats"]
        result["environments"] = ident["environments"]
        result["seat_alias"] = ident["seat_alias"]
        result["credential_alias"] = ident["credential_alias"]
        return result

    def resolve(self, category: Optional[str]) -> dict[str, Any]:
        """The effective roster for a challenge category — the per-category
        override (if any) layered over the defaults. Returns
        {engines, start_workers, max_workers}."""
        cfg = self.get()
        ov = cfg["overrides"].get((category or "").strip())
        if ov:
            return {
                "engines": ov["engines"],
                "start_workers": ov["start_workers"],
                "max_workers": cfg["max_workers"],
                "worker_backend": cfg["worker_backend"],
                "race_scout": cfg["race_scout"],
                "race_timeout": cfg["race_timeout"],
                "wall_clock_budget": cfg["wall_clock_budget"],
                "race_engines": cfg["race_engines"],
                "max_total_workers": cfg["max_total_workers"],
                "cost_budget_usd": cfg["cost_budget_usd"],
                "stage_policy": cfg["stage_policy"],
                "llm_profiles": cfg["llm_profiles"],
                "runtime_profiles": cfg["runtime_profiles"],
                "worker_profiles": cfg["worker_profiles"],
            }
        return {
            "engines": cfg["engines"],
            "start_workers": cfg["start_workers"],
            "max_workers": cfg["max_workers"],
            "worker_backend": cfg["worker_backend"],
            "race_scout": cfg["race_scout"],
            "race_timeout": cfg["race_timeout"],
            "wall_clock_budget": cfg["wall_clock_budget"],
            "race_engines": cfg["race_engines"],
            "max_total_workers": cfg["max_total_workers"],
            "cost_budget_usd": cfg["cost_budget_usd"],
            "stage_policy": cfg["stage_policy"],
            "llm_profiles": cfg["llm_profiles"],
            "runtime_profiles": cfg["runtime_profiles"],
            "worker_profiles": cfg["worker_profiles"],
        }

    def set(
        self,
        *,
        engines: Any = None,
        start_workers: Any = None,
        max_workers: Any = None,
        worker_backend: Any = None,
        race_scout: Any = None,
        race_timeout: Any = None,
        wall_clock_budget: Any = None,
        race_engines: Any = None,
        max_total_workers: Any = None,
        cost_budget_usd: Any = None,
        stage_policy: Any = None,
        llm_profiles: Any = None,
        runtime_profiles: Any = None,
        worker_profiles: Any = None,
        overrides: Any = None,
    ) -> dict[str, Any]:
        """Update the default config. Each arg is optional; only provided fields
        change. Invalid values are rejected (raise ValueError) so a bad PUT
        doesn't silently persist garbage."""
        target_backend = (
            self._require_backend(worker_backend)
            if worker_backend is not None
            else self._clean_backend(self._data.get("worker_backend"))
        )
        if engines is not None:
            profiles_for_engine_validation = (
                self._clean_worker_profiles(worker_profiles, reject_invalid=True)
                if worker_profiles is not None
                else self._clean_worker_profiles(self._data.get("worker_profiles"))
            )
            cleaned = _clean_engines_for_backend(
                engines, profiles_for_engine_validation, target_backend)
            if not cleaned:
                raise ValueError("engines must name at least one enabled worker profile")
            self._data["engines"] = cleaned
        if start_workers is not None:
            self._data["start_workers"] = self._require_pos_int(
                start_workers, "start_workers")
        if max_workers is not None:
            self._data["max_workers"] = self._require_pos_int(
                max_workers, "max_workers")
        if worker_backend is not None:
            self._data["worker_backend"] = target_backend
        if race_scout is not None:
            self._data["race_scout"] = bool(race_scout)
        if race_timeout is not None:
            self._data["race_timeout"] = self._require_pos_int(
                race_timeout, "race_timeout")
        if wall_clock_budget is not None:
            self._data["wall_clock_budget"] = self._require_nonneg_int(
                wall_clock_budget, "wall_clock_budget")
        if race_engines is not None:
            profiles_for_engine_validation = self._clean_worker_profiles(
                worker_profiles if worker_profiles is not None else self._data.get("worker_profiles"))
            self._data["race_engines"] = _clean_engines_for_backend(
                race_engines, profiles_for_engine_validation, target_backend)
        if max_total_workers is not None:
            self._data["max_total_workers"] = self._require_nonneg_int(
                max_total_workers, "max_total_workers")
        if cost_budget_usd is not None:
            self._data["cost_budget_usd"] = self._require_nonneg_float(
                cost_budget_usd, "cost_budget_usd")
        if stage_policy is not None:
            profiles_for_stage = self._clean_worker_profiles(
                worker_profiles if worker_profiles is not None else self._data.get("worker_profiles"))
            clean_stage = (
                json.loads(json.dumps(stage_policy))
                if isinstance(stage_policy, dict)
                else stage_policy
            )
            if isinstance(clean_stage, dict):
                race = clean_stage.setdefault("race", {})
                race["engines"] = _remap_profile_refs(
                    race.get("engines"), profiles_for_stage, target_backend)
                review = clean_stage.setdefault("coordinator", {}).setdefault("review", {})
                review["engine"] = _remap_profile_ref(
                    review.get("engine") or DEFAULT_REVIEW_POLICY["engine"],
                    profiles_for_stage,
                    target_backend,
                )
            self._data["stage_policy"] = self._clean_stage_policy(clean_stage, {})
        if llm_profiles is not None:
            self._data["llm_profiles"] = self._clean_llm_profiles(
                llm_profiles, reject_invalid=True)
        if runtime_profiles is not None or worker_profiles is not None:
            next_runtime_profiles = (
                self._clean_runtime_profiles(runtime_profiles, reject_invalid=True)
                if runtime_profiles is not None
                else self._clean_runtime_profiles(self._data.get("runtime_profiles"))
            )
            next_worker_profiles = (
                self._clean_worker_profiles(worker_profiles, reject_invalid=True)
                if worker_profiles is not None
                else self._clean_worker_profiles(self._data.get("worker_profiles"))
            )
            runtime_ids = {p["id"] for p in next_runtime_profiles}
            for p in next_worker_profiles:
                if p["runtime"] not in runtime_ids:
                    raise ValueError(f"worker profile {p['id']} references unknown runtime")
            if runtime_profiles is not None:
                self._data["runtime_profiles"] = next_runtime_profiles
            if worker_profiles is not None:
                self._data["worker_profiles"] = next_worker_profiles
        if overrides is not None:
            if not isinstance(overrides, dict):
                raise ValueError("overrides must be an object")
            clean_ov: dict[str, Any] = {}
            for cat, ov in overrides.items():
                if not isinstance(ov, dict):
                    raise ValueError(f"override for {cat} must be an object")
                cat_engines = _clean_engines(
                    ov.get("engines"),
                    self._clean_worker_profiles(self._data.get("worker_profiles")),
                )
                if not cat_engines:
                    raise ValueError(f"override for {cat} must name valid worker profiles")
                entry: dict[str, Any] = {"engines": cat_engines}
                if ov.get("start_workers") is not None:
                    entry["start_workers"] = self._require_pos_int(
                        ov["start_workers"], f"{cat}.start_workers")
                clean_ov[str(cat)] = entry
            self._data["overrides"] = clean_ov
        # max_workers is a READ-ONLY derived value = sum of the eligible seats'
        # max_running. Recompute it whenever the roster (per-seat capacity) or the
        # dispatch lineup could have changed — i.e. worker_profiles or engines were
        # supplied. (The frontend no longer sends an editable max_workers; a stale
        # one in the payload is overwritten by the derived sum.) We deliberately do
        # NOT mutate any seat's max_running, so an edited value never "reverts".
        self._sync_worker_counts(
            link_profile_capacity=(
                worker_profiles is not None or engines is not None
            )
        )
        # New-schema-on-disk (user decision): whenever the legacy worker_profiles
        # change (the v2 frontend still saves in legacy shape), derive and persist
        # the Credential/Seat/Environment model alongside, so disk carries the new
        # shape as the source of truth. Reads then prefer the seats[] block.
        if worker_profiles is not None or runtime_profiles is not None:
            self._persist_identity_from_legacy()
        self._flush()
        return self.get()

    def _persist_identity_from_legacy(self) -> None:
        """Derive seats/credentials/environments from the current legacy
        worker_profiles + runtime_profiles and write them into self._data, so the
        on-disk config is the new shape. Never raises — a derivation failure just
        leaves the legacy shape (still readable)."""
        try:
            # preserve any labels the user already set on existing seats (the
            # legacy save path drops the label field, so re-deriving would reset
            # them to "<engine> worker"); keyed by the stable seat id.
            prior_labels = {
                str(s.get("id")): str(s.get("label") or "")
                for s in (self._data.get("seats") or []) if isinstance(s, dict)
            }
            cfg = self.get()  # normalized legacy view
            res = migrate_legacy_config(
                worker_profiles=cfg["worker_profiles"],
                runtime_profiles=cfg["runtime_profiles"],
                account_modes=self._account_modes(),
            )
            seats = []
            for s in res.seats:
                d = s.to_dict()
                if prior_labels.get(d["id"]):
                    d["label"] = prior_labels[d["id"]]
                seats.append(d)
            self._data["seats"] = seats
            self._data["credentials"] = [c.to_dict() for c in res.credentials]
            self._data["environments"] = [e.to_dict() for e in res.environments]
            # The seats[] block is additive; we leave the legacy engines[]/
            # review.engine foreign keys in their current (readable) form rather than
            # rewriting them to seat ids on every save. Rationale: the legacy
            # set_runtime_environment recipe path renames profiles by readable
            # canonical id, and churning foreign keys to seat ids here would collide
            # with it. resolve_seat_ref() bridges either form at the read boundaries
            # (health route, scheduler), and _project_identity_to_legacy reconciles a
            # new-shaped file on load. Stable seat ids still live in seats[].
        except Exception:  # noqa: BLE001
            pass

    def set_identity_model(
        self,
        *,
        seats: Any = None,
        credentials: Any = None,
        environments: Any = None,
    ) -> dict[str, Any]:
        """Persist the NEW Credential/Seat/Environment model to disk.

        Validates the §3.7 hard constraint (container environment forbids a
        system_inherit credential) and rejects an illegal combo with ValueError —
        the save-time gate Codex specified, so an illegal config never persists.
        After writing, re-projects to legacy worker_profiles so the in-memory
        scheduler view stays consistent. Each arg optional; only provided ones
        change. Never silently drops a bad value — it raises."""
        if seats is not None:
            if not isinstance(seats, list):
                raise ValueError("seats must be a list")
            self._data["seats"] = [s for s in seats if isinstance(s, dict)]
        if credentials is not None:
            if not isinstance(credentials, list):
                raise ValueError("credentials must be a list")
            self._data["credentials"] = [c for c in credentials if isinstance(c, dict)]
        if environments is not None:
            if not isinstance(environments, list):
                raise ValueError("environments must be a list")
            self._data["environments"] = [e for e in environments if isinstance(e, dict)]

        # §3.7 legality gate: a seat on a container environment may not use a
        # system_inherit credential (host login isn't mounted into the container).
        cred_by_id = {str(c.get("id")): c for c in self._data.get("credentials") or []}
        env_by_id = {str(e.get("id")): e for e in self._data.get("environments") or []}
        for s in self._data.get("seats") or []:
            cred = cred_by_id.get(str(s.get("credential_id"))) or {}
            env = env_by_id.get(str(s.get("environment_id"))) or {}
            kind = str(cred.get("kind") or "")
            backend = str(env.get("backend") or "")
            if kind and backend and not is_legal_combo(kind=kind, backend=backend):
                label = s.get("label") or s.get("id")
                raise ValueError(
                    f"非法组合:Agent「{label}」在容器环境下使用了「系统登录」凭据。"
                    f"容器不挂载宿主登录态,请改用引擎凭据或自定义端点。"
                )
        # keep the legacy projection in sync so get()/scheduler see the change.
        self._project_identity_to_legacy()
        self._flush()
        return self.get()

    def _sync_worker_counts(self, *, link_profile_capacity: bool) -> None:
        # Direction is roster→max (the operator owns per-seat capacity; the global
        # `max_workers` ceiling is a READ-ONLY derived value = sum of the eligible
        # seats' `max_running`). We NEVER mutate a seat's max_running here — that
        # is what ballooned a stale single-seat lineup up to max_workers and made
        # an edited value "revert" on save (Bug B). Instead max_workers tracks the
        # roster sum, up AND down, so "3 workers each running 1 → max 3" always
        # holds and editing any seat is reflected immediately.
        if link_profile_capacity:
            profiles = self._clean_worker_profiles(self._data.get("worker_profiles"))
            backend = self._clean_backend(self._data.get("worker_backend"))
            selected = _clean_engines_for_backend(
                self._data.get("engines"), profiles, backend) or [
                    _profile_name(p) for p in profiles if p.get("enabled", True)
                ]
            selected_set = set(selected)
            eligible = [
                p for p in profiles
                if _profile_name(p) in selected_set and _ordinary_worker_roles(p)
            ]
            if eligible:
                self._data["max_workers"] = sum(
                    self._coerce_pos_int(p.get("max_running"), 1) for p in eligible)

        # start_workers is still capped by the (possibly just-derived) ceiling.
        max_workers = self._coerce_pos_int(
            self._data.get("max_workers"), DEFAULT_MAX_WORKERS)
        start_workers = self._coerce_pos_int(
            self._data.get("start_workers"), len(DEFAULT_ENGINES))
        if start_workers > max_workers:
            self._data["start_workers"] = max_workers

    def set_runtime_environment(self, *, backend: str, runtime_id: str) -> dict[str, Any]:
        """Unify the run's runtime across ALL enabled worker profiles (DESIGN §5).

        Since the model is one-container-per-run, every profile that could be
        dispatched (default engines OR a per-category override's engines — i.e.
        the whole enabled set) must agree on the runtime, else the old "first
        worker's runtime wins, displayed backend lies" bug returns. So we set
        `worker_backend` AND rewrite every enabled profile's `runtime` to the
        chosen id in one atomic flush.
        """
        backend = (backend or "").strip()
        runtime_id = (runtime_id or "").strip()
        if backend not in VALID_BACKENDS:
            raise ValueError("backend must be 'local' or 'container'")
        runtime_profiles = self._clean_runtime_profiles(self._data.get("runtime_profiles"))
        rt = next((r for r in runtime_profiles if r["id"] == runtime_id), None)
        if rt is None:
            raise ValueError(f"unknown runtime id: {runtime_id}")
        if rt["backend"] != backend:
            raise ValueError(
                f"runtime {runtime_id!r} is backend {rt['backend']!r}, not {backend!r}")
        profiles = self._clean_worker_profiles(self._data.get("worker_profiles"))
        rename: dict[str, str] = {}
        taken = {str(p.get("name") or p.get("id")) for p in profiles}
        for p in profiles:
            old_id = str(p["id"])
            desired = _canonical_profile_id(p, backend)
            if old_id in _canonical_profile_aliases(p) and old_id != desired:
                if desired not in taken:
                    taken.discard(old_id)
                    taken.add(desired)
                    rename[old_id] = desired
                    p["id"] = desired
                    p["name"] = desired
            p["runtime"] = runtime_id  # whole enabled set, incl. override-only ones

        def rewrite_ref(value: Any) -> Any:
            if isinstance(value, str):
                return rename.get(value, _remap_profile_ref(value, profiles, backend))
            if isinstance(value, list):
                return [rewrite_ref(v) for v in value]
            return value

        if "engines" in self._data:
            self._data["engines"] = rewrite_ref(self._data.get("engines"))
        if "race_engines" in self._data:
            self._data["race_engines"] = rewrite_ref(self._data.get("race_engines"))
        raw_stage = self._data.get("stage_policy")
        stage = json.loads(json.dumps(raw_stage)) if isinstance(raw_stage, dict) else {}
        race = stage.setdefault("race", {})
        if race.get("engines") is not None:
            race["engines"] = rewrite_ref(race.get("engines"))
        coord = stage.setdefault("coordinator", {})
        review = coord.setdefault("review", {})
        review["engine"] = rewrite_ref(
            review.get("engine") or DEFAULT_REVIEW_POLICY["engine"])
        self._data["stage_policy"] = stage

        raw_overrides = self._data.get("overrides")
        if isinstance(raw_overrides, dict):
            overrides = json.loads(json.dumps(raw_overrides))
            for ov in overrides.values():
                if isinstance(ov, dict) and ov.get("engines") is not None:
                    ov["engines"] = rewrite_ref(ov.get("engines"))
            self._data["overrides"] = overrides

        self._data["worker_backend"] = backend
        self._data["worker_profiles"] = profiles
        self._flush()
        return self.get()

    @staticmethod
    def _coerce_pos_int(value: Any, default: int) -> int:
        try:
            n = int(value)
        except (TypeError, ValueError):
            return default
        return n if n > 0 else default

    @staticmethod
    def _coerce_nonneg_int(value: Any, default: int) -> int:
        try:
            n = int(value)
        except (TypeError, ValueError):
            return default
        return n if n >= 0 else default

    @staticmethod
    def _coerce_nonneg_float(value: Any, default: float) -> float:
        try:
            n = float(value)
        except (TypeError, ValueError):
            return default
        return n if n >= 0 else default

    @staticmethod
    def _coerce_bool(value: Any, default: bool) -> bool:
        if value is None:
            return default
        return bool(value)

    @staticmethod
    def _require_pos_int(value: Any, field: str) -> int:
        try:
            n = int(value)
        except (TypeError, ValueError) as exc:
            raise ValueError(f"{field} must be a positive integer") from exc
        if n <= 0:
            raise ValueError(f"{field} must be a positive integer")
        return n

    @staticmethod
    def _require_nonneg_int(value: Any, field: str) -> int:
        try:
            n = int(value)
        except (TypeError, ValueError) as exc:
            raise ValueError(f"{field} must be a non-negative integer") from exc
        if n < 0:
            raise ValueError(f"{field} must be a non-negative integer")
        return n

    @staticmethod
    def _require_nonneg_float(value: Any, field: str) -> float:
        try:
            n = float(value)
        except (TypeError, ValueError) as exc:
            raise ValueError(f"{field} must be a non-negative number") from exc
        if n < 0:
            raise ValueError(f"{field} must be a non-negative number")
        return n

    @staticmethod
    def _clean_llm_profiles(value: Any, *, reject_invalid: bool = False) -> dict[str, dict[str, str]]:
        if value is None:
            return {k: dict(v) for k, v in DEFAULT_LLM_PROFILES.items()}
        if not isinstance(value, dict):
            if reject_invalid:
                raise ValueError("llm_profiles must be an object")
            return {k: dict(v) for k, v in DEFAULT_LLM_PROFILES.items()}
        out = {k: dict(v) for k, v in DEFAULT_LLM_PROFILES.items()}
        for key in ("planner", "titler"):
            raw = value.get(key)
            if raw is None:
                continue
            if not isinstance(raw, dict):
                if reject_invalid:
                    raise ValueError(f"llm_profiles.{key} must be an object")
                continue
            model = str(raw.get("model") or out[key]["model"]).strip()
            provider = str(raw.get("provider") or out[key]["provider"]).strip()
            # base_url is the OpenAI-compatible endpoint override; empty = default
            # DeepSeek. The API key is NOT stored here — it stays in .env
            # (MUTEKI_DEEPSEEK_API_KEY). A non-string/garbage value normalizes to "".
            raw_base = raw.get("base_url")
            base_url = str(raw_base).strip() if isinstance(raw_base, str) else ""
            if not model:
                if reject_invalid:
                    raise ValueError(f"llm_profiles.{key}.model must be non-empty")
                model = out[key]["model"]
            out[key] = {
                "provider": provider or out[key]["provider"],
                "model": model,
                "base_url": base_url,
            }
        return out

    @staticmethod
    def _clean_stage_policy(value: Any, defaults: dict[str, Any]) -> dict[str, Any]:
        if not isinstance(value, dict):
            value = {}
        race_timeout = int(value.get("race", {}).get("timeout")
                           or defaults.get("race_timeout") or DEFAULT_RACE_TIMEOUT)
        race_enabled = bool(value.get("race", {}).get(
            "enabled", defaults.get("race_scout", True)))
        raw_race_engines = value.get("race", {}).get("engines")
        race_engines = raw_race_engines or defaults.get("race_engines") or []
        wall = int((value.get("coordinator") or {}).get(
            "wall_clock_budget", defaults.get("wall_clock_budget", 0)) or 0)
        max_workers = int(value.get("budgets", {}).get(
            "max_total_workers", defaults.get("max_total_workers", 0)) or 0)
        cost = float(value.get("budgets", {}).get(
            "cost_budget_usd", defaults.get("cost_budget_usd", 0.0)) or 0.0)
        raw_review = (value.get("coordinator") or {}).get("review")
        review = dict(DEFAULT_REVIEW_POLICY)
        if isinstance(raw_review, dict):
            review["enabled"] = bool(raw_review.get("enabled", review["enabled"]))
            review["engine"] = str(raw_review.get("engine") or review["engine"]).strip()
            for key in (
                "after_fruitless_workers", "after_duplicate_intents",
                "every_completed_workers", "candidate_spike_threshold",
                "max_concurrent", "cooldown_events", "timeout", "max_review_workers",
            ):
                if raw_review.get(key) is not None:
                    review[key] = WorkerConfigStore._coerce_nonneg_int(
                        raw_review.get(key), int(review[key]))
            for key in (
                "after_race", "on_course_correct", "on_reason_dry",
                "on_candidate_spike", "on_operator_hint", "allow_review_fallback",
            ):
                if raw_review.get(key) is not None:
                    review[key] = bool(raw_review.get(key))
        return {
            "prepare": dict(value.get("prepare") or {}),
            "race": {"enabled": race_enabled, "timeout": race_timeout,
                     "engines": list(race_engines or [])},
            "coordinator": {"wall_clock_budget": wall, "review": review},
            "budgets": {"max_total_workers": max_workers,
                        "cost_budget_usd": cost},
        }

    @staticmethod
    def _clean_backend(value: Any) -> str:
        # Single source of truth for the effective backend (precedence + alias +
        # fallback + the web-container override that coerces local→container so a
        # stale/explicit "local" never reaches the swarm). No-op on a bare host.
        return resolve_worker_backend(
            config_backend=value if isinstance(value, str) else None,
            in_web_container=is_web_container(),
        )

    @staticmethod
    def _require_backend(value: Any) -> str:
        if isinstance(value, str) and value in VALID_BACKENDS:
            if value == "local" and is_web_container():
                raise ValueError(
                    "worker_backend 'local' is not allowed when the web control "
                    "plane runs inside a container — use 'container'")
            return value
        raise ValueError("worker_backend must be local or container")

    @staticmethod
    def _clean_runtime_profiles(value: Any, *, reject_invalid: bool = False) -> list[dict[str, Any]]:
        if value is None:
            return [dict(p) for p in DEFAULT_RUNTIME_PROFILES]
        if not isinstance(value, list):
            if reject_invalid:
                raise ValueError("runtime_profiles must be a list")
            return [dict(p) for p in DEFAULT_RUNTIME_PROFILES]
        out: list[dict[str, Any]] = []
        seen: set[str] = set()
        for item in value:
            if not isinstance(item, dict):
                if reject_invalid:
                    raise ValueError("runtime profile must be an object")
                continue
            pid = str(item.get("id") or "").strip()
            backend = item.get("backend")
            if not pid or backend not in VALID_BACKENDS:
                if reject_invalid:
                    raise ValueError("runtime profile requires id and valid backend")
                continue
            out.append({
                "id": pid,
                "backend": backend,
                "label": str(item.get("label") or pid),
                "network": str(item.get("network") or ("bridge" if backend == "container" else "")),
                "memory": str(item.get("memory") or ""),
                "cpus": str(item.get("cpus") or ""),
                "pids_limit": WorkerConfigStore._coerce_nonneg_int(item.get("pids_limit"), 0),
            })
        return out or [dict(p) for p in DEFAULT_RUNTIME_PROFILES]

    @staticmethod
    def _clean_worker_profiles(value: Any, *, reject_invalid: bool = False) -> list[dict[str, Any]]:
        return normalize_worker_profiles(
            value,
            defaults=DEFAULT_WORKER_PROFILES,
            reject_invalid=reject_invalid,
        )
