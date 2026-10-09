"""Default worker-roster configuration — which engines launch per challenge.

An OPERATOR preference (like the rail meta side-table), not part of the
event-sourced solve: a single small JSON file under the private state root, loaded on
startup and rewritten on each mutation. It answers "when a challenge is
dispatched and the request doesn't say otherwise, which engines run, and how
many bootstrap workers?" — with an optional per-category override (e.g. give pwn
only claude+codex, give web all three).

The dispatch path (apps/web/drivers.py) reads `resolve(category)` as the FALLBACK
when the request body carries no explicit engines/start_workers; an explicit body
always wins, so this never overrides an intentional per-run choice.
"""

from __future__ import annotations

import copy
import json
import math
from pathlib import Path
from typing import Any, Optional
from urllib.parse import urlsplit

from muteki.core.llm import normalize_llm_temperature
from muteki.core.runtime_env import is_web_container
from muteki.solver.credential_accounts import CredentialAccountLockTimeoutError
from muteki.solver.shared_credentials import SharedCredentialError
from muteki.solver.engine_registry import canonical_engine_id
from muteki.solver.worker_profiles import (
    VALID_BASE_ENGINES,
    base_engine_for_profile,
    normalize_profile_roster,
    normalize_worker_profiles,
    resolve_seat_ref,
)
from muteki.solver.worker_resource_limits import (
    FULL_DEFAULTS,
    resolve_worker_resource_limits,
    validate_cpus,
    validate_memory,
    validate_pids_limit,
)
from muteki.solver.identity_model import (
    migrate_legacy_config,
    migrate_identity_credential_references,
    seats_to_legacy_profiles,
)

VALID_ENGINES = VALID_BASE_ENGINES
VALID_BACKENDS = ("local", "container")
VALID_WORKER_NETWORKS = ("bridge", "host", "none")
VALID_CONTAINER_SCOPES = ("run", "shared")
DEFAULT_MAX_WORKERS = 10
DEFAULT_WORKER_BACKEND = "container"
DEFAULT_WORKER_NETWORK = "bridge"
DEFAULT_CONTAINER_SCOPE = "run"
# MNT-09.03 / #170 — full-image defaults (slim resolved via image name / env).
DEFAULT_WORKER_MEMORY = FULL_DEFAULTS.memory
DEFAULT_WORKER_CPUS = FULL_DEFAULTS.cpus
DEFAULT_WORKER_PIDS_LIMIT = FULL_DEFAULTS.pids_limit
DEFAULT_WORKER_OUTPUT_LIMIT = FULL_DEFAULTS.output_limit
DEFAULT_WORKER_DISK_LIMIT = FULL_DEFAULTS.disk_limit
DEFAULT_RACE_TIMEOUT = 300
DEFAULT_WALL_CLOCK_BUDGET = 0
DEFAULT_MAX_TOTAL_WORKERS = 0
DEFAULT_COST_BUDGET_USD = 0.0
DEFAULT_REVIEW_POLICY = {
    "enabled": True,
    "engine": "claude-sub-container",
    "reasoning_effort": "inherit",
    "after_race": False,
    "after_fruitless_workers": 3,
    "after_duplicate_intents": 2,
    "on_course_correct": False,
    "on_candidate_spike": True,
    "on_operator_hint": False,
    "on_evidence_conflict": True,
    "on_semantic_duplicate": False,
    "every_completed_workers": 0,
    "candidate_spike_threshold": 5,
    "max_concurrent": 1,
    "allow_review_fallback": False,
    "cooldown_events": 8,
    "timeout": 90,
    "max_review_workers": 12,
}
DEFAULT_VERIFIER_POLICY = {
    # Verification requires an explicitly assigned verifier seat. Keeping this
    # disabled avoids a default policy that is enabled with an empty foreign key.
    "enabled": False,
    "engine": "",
    "reasoning_effort": "inherit",
    "max_concurrent": 0,
    "allow_verifier_fallback": False,
    "timeout": 240,
    "max_verifier_workers": 24,
}
DEFAULT_LLM_PROFILES = {
    "planner": {
        "provider": "deepseek",
        "model": "deepseek-v4-pro",
        "endpoint_id": "",
        "base_url": "",
        "connection": "default",
        "temperature_mode": "omit",
    },
    "titler": {
        "provider": "deepseek",
        "model": "deepseek-v4-flash",
        "endpoint_id": "",
        "base_url": "",
        "connection": "default",
        "temperature_mode": "omit",
    },
}

DEFAULT_WORKER_PROFILES = [
    {"id": "claude-sub-container", "name": "claude-sub-container",
     "engine": "claude", "transport": "claude_code",
     "auth": "subscription", "credential_mode": "subscription",
     "credential_account": "claude-main", "api_key_ref": "", "base_url": "",
     "wire_api": "",
     "roles": ["race", "bootstrap", "explore", "respond", "review"],
     "race": True, "max_running": 2, "max_review_running": 0, "priority": 10, "model": "",
     "enabled": True},
    {"id": "codex-sub-container", "name": "codex-sub-container",
     "engine": "codex", "transport": "codex_cli",
     "auth": "subscription", "credential_mode": "subscription",
     "credential_account": "codex-main", "api_key_ref": "", "base_url": "",
     "wire_api": "responses",
     "roles": ["race", "bootstrap", "explore", "review"],
     "race": True, "max_running": 1, "max_review_running": 0, "priority": 20, "model": "",
     "enabled": True},
    {"id": "cursor-api-container", "name": "cursor-api-container",
     "engine": "cursor", "transport": "cursor_agent",
     "auth": "api_key", "credential_mode": "api_key",
     "credential_account": "cursor-main", "api_key_ref": "", "base_url": "",
     "wire_api": "",
     "roles": ["race", "bootstrap", "explore", "review"],
     "race": True, "max_running": 2, "max_review_running": 0, "priority": 30, "model": "",
     "enabled": True},
    {"id": "pi-sub-container", "name": "pi-sub-container",
     "engine": "pi", "transport": "pi",
     "auth": "subscription", "credential_mode": "subscription",
     "credential_account": "pi-main", "api_key_ref": "", "base_url": "",
     "wire_api": "",
     "roles": ["race", "bootstrap", "explore", "review"],
     "race": True, "max_running": 1, "max_review_running": 0, "priority": 40, "model": "",
     "enabled": True},
    {"id": "omp-sub-container", "name": "omp-sub-container",
     "engine": "omp", "transport": "omp",
     "auth": "subscription", "credential_mode": "subscription",
     "credential_account": "omp-main", "api_key_ref": "", "base_url": "",
     "wire_api": "",
     "roles": ["race", "bootstrap", "explore", "review"],
     "race": True, "max_running": 1, "max_review_running": 0, "priority": 50, "model": "",
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
    *,
    worker_backend: str,
    in_web_container: bool,
) -> str:
    """Resolve the single backend shared by every Worker and Review Worker."""
    return resolve_worker_backend(
        config_backend=worker_backend,
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
    def __init__(self, root: str | Path = "state") -> None:
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
        identity_migrated = False
        if isinstance(self._data.get("credentials"), list) and isinstance(
            self._data.get("seats"), list
        ):
            credentials, seats, identity_migrated = (
                migrate_identity_credential_references(
                    self._data.get("credentials") or [],
                    self._data.get("seats") or [],
                )
            )
            if identity_migrated:
                self._data["credentials"] = credentials
                self._data["seats"] = seats
        if isinstance(self._data.get("seats"), list):
            # Rebuild the in-memory compatibility projection on every load so a
            # credential-center endpoint edit is visible immediately.  The
            # constructor remains side-effect free; this canonical form reaches
            # disk only on the next explicit settings save.
            self._data["credentials"] = self._credential_projections_for_seats(
                [
                    seat for seat in self._data.get("seats") or []
                    if isinstance(seat, dict)
                ]
            )
        self._project_legacy_llm_credentials()
        self._project_identity_to_legacy()
        self._data.pop("runtime_profiles", None)
        self._data.pop("environments", None)
        for seat in self._data.get("seats") or []:
            if isinstance(seat, dict):
                seat.pop("environment_id", None)
        for profile in self._data.get("worker_profiles") or []:
            if isinstance(profile, dict):
                profile.pop("runtime", None)
        # Keep constructor/import reads side-effect free. The canonical projection
        # is active in memory immediately and the next explicit settings write
        # persists it; importing apps.web.server in a test must not rewrite the
        # operator's real state/_worker_config.json.

    def _project_identity_to_legacy(self) -> None:
        """Adapt the stored Seat/Credential model into worker_profiles."""
        d = self._data
        if not (isinstance(d.get("seats"), list) and d.get("seats")):
            return
        try:
            seats = [s for s in d["seats"] if isinstance(s, dict)]
            creds = [c for c in (d.get("credentials") or []) if isinstance(c, dict)]
            legacy_profiles = [
                p for p in (d.get("worker_profiles") or [])
                if isinstance(p, dict)
            ]
            seat_ids = {
                str(s.get("id") or "") for s in seats if s.get("id")
            }
            legacy_ref_to_seat: dict[str, str] = {}
            for profile in legacy_profiles:
                engine = base_engine_for_profile(profile)
                label = str(profile.get("label") or "").strip()
                credential_id = str(
                    profile.get("credential_id") or ""
                ).strip()
                candidates = [
                    seat for seat in seats
                    if str(seat.get("engine") or "").strip() == engine
                ]
                if label:
                    labelled = [
                        seat for seat in candidates
                        if str(seat.get("label") or "").strip() == label
                    ]
                    if labelled:
                        candidates = labelled
                if credential_id and len(candidates) != 1:
                    credential_matched = [
                        seat for seat in candidates
                        if str(seat.get("credential_id") or "").strip()
                        == credential_id
                    ]
                    if credential_matched:
                        candidates = credential_matched
                if len(candidates) != 1:
                    continue
                target = str(candidates[0].get("id") or "").strip()
                refs = {
                    str(profile.get("id") or "").strip(),
                    str(profile.get("name") or "").strip(),
                    label,
                    *_canonical_profile_aliases(profile),
                }
                for ref in refs:
                    if ref:
                        legacy_ref_to_seat[ref] = target
            # adapt seats → legacy worker_profiles for the scheduler/drivers.
            d["worker_profiles"] = seats_to_legacy_profiles(seats, creds)
            # remap any seat-id/label foreign keys (engines[], review.engine, ...)
            # to legacy profile names so the existing remap machinery resolves them.
            alias = {str(s.get("label")): str(s.get("id")) for s in seats if s.get("label")}
            id_to_name = {str(s.get("id")): str(s.get("id")) for s in seats}

            def _to_name(ref: Any) -> Any:
                sid = resolve_seat_ref(ref, seats=seats, alias_table=alias)
                if sid in id_to_name:
                    return sid
                mapped = legacy_ref_to_seat.get(str(ref or "").strip(), "")
                return mapped if mapped in seat_ids else ref

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
            # Review-only seats belong to the coordinator review channel.  They
            # must never be projected into the ordinary dispatch lineup after a
            # process restart; otherwise a dedicated reviewer starts solving as
            # a normal worker even though the settings UI keeps it separate.
            enabled_ids = [
                str(s.get("id"))
                for s in seats
                if s.get("enabled", True)
                and s.get("id")
                and _ordinary_worker_roles(s)
            ]
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
                verifier = (sp.get("coordinator") or {}).get("verifier") if isinstance(sp.get("coordinator"), dict) else None
                if isinstance(verifier, dict) and verifier.get("engine"):
                    verifier["engine"] = _to_name(verifier["engine"])
        except Exception:  # noqa: BLE001 — projection must never break startup
            pass

    def _flush(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(self._data, ensure_ascii=False, indent=2),
                       encoding="utf-8")
        tmp.replace(self.path)  # atomic on POSIX

    def purge_engine(
        self, engine: str, *, removed_credential_ids: set[str] | None = None,
    ) -> dict[str, int]:
        """Remove active configuration references for a paused engine."""
        selected = str(engine or "").strip().lower()
        removed_credentials = set(removed_credential_ids or set())
        removed_refs: set[str] = set()
        counts = {"profiles": 0, "seats": 0, "credentials": 0, "references": 0}

        profiles = [
            row for row in self._data.get("worker_profiles") or []
            if isinstance(row, dict)
        ]
        kept_profiles: list[dict[str, Any]] = []
        for row in profiles:
            if canonical_engine_id(
                row.get("engine") or row.get("transport")
            ) == selected:
                counts["profiles"] += 1
                removed_refs.update({
                    str(row.get("id") or ""),
                    str(row.get("name") or ""),
                    str(row.get("label") or ""),
                })
            else:
                kept_profiles.append(row)
        self._data["worker_profiles"] = kept_profiles

        seats = [
            row for row in self._data.get("seats") or []
            if isinstance(row, dict)
        ]
        kept_seats: list[dict[str, Any]] = []
        for row in seats:
            if canonical_engine_id(row.get("engine")) == selected:
                counts["seats"] += 1
                removed_refs.update({
                    str(row.get("id") or ""),
                    str(row.get("label") or ""),
                })
                credential_id = str(row.get("credential_id") or "")
                if credential_id:
                    removed_credentials.add(credential_id)
            else:
                kept_seats.append(row)
        self._data["seats"] = kept_seats

        credentials = [
            row for row in self._data.get("credentials") or []
            if isinstance(row, dict)
        ]
        kept_credentials: list[dict[str, Any]] = []
        for row in credentials:
            credential_id = str(row.get("id") or "")
            if (
                credential_id in removed_credentials
                or canonical_engine_id(row.get("engine")) == selected
            ):
                counts["credentials"] += 1
            else:
                kept_credentials.append(row)
        self._data["credentials"] = kept_credentials

        def keep_ref(value: Any) -> bool:
            text = str(value or "").strip()
            return bool(text) and text not in removed_refs and canonical_engine_id(text) != selected

        for key in ("engines", "race_engines"):
            rows = self._data.get(key)
            if isinstance(rows, list):
                kept = [value for value in rows if keep_ref(value)]
                counts["references"] += len(rows) - len(kept)
                self._data[key] = kept

        overrides = self._data.get("overrides")
        if isinstance(overrides, dict):
            for category, row in list(overrides.items()):
                if not isinstance(row, dict):
                    continue
                refs = row.get("engines")
                if isinstance(refs, list):
                    kept = [value for value in refs if keep_ref(value)]
                    counts["references"] += len(refs) - len(kept)
                    if kept:
                        row["engines"] = kept
                    else:
                        overrides.pop(category, None)

        stage = self._data.get("stage_policy")
        if isinstance(stage, dict):
            race = stage.get("race")
            if isinstance(race, dict) and isinstance(race.get("engines"), list):
                refs = list(race["engines"])
                race["engines"] = [value for value in refs if keep_ref(value)]
                counts["references"] += len(refs) - len(race["engines"])
            coordinator = stage.get("coordinator")
            if isinstance(coordinator, dict):
                for role in ("review", "verifier"):
                    policy = coordinator.get(role)
                    if not isinstance(policy, dict):
                        continue
                    if not keep_ref(policy.get("engine")):
                        if policy.get("engine"):
                            counts["references"] += 1
                        policy["engine"] = ""
                        policy["enabled"] = False

        llm_profiles = self._data.get("llm_profiles")
        if isinstance(llm_profiles, dict):
            from apps.web.llm_credentials import (
                canonical_model_endpoint_id,
                model_endpoint_id,
            )
            from muteki.solver.credential_accounts import account_id_from_credential_id

            removed_endpoint_ids = {
                model_endpoint_id(account_id)
                for credential_id in removed_credentials
                if (account_id := account_id_from_credential_id(credential_id))
            }
            for row in llm_profiles.values():
                if not isinstance(row, dict):
                    continue
                try:
                    endpoint_id = canonical_model_endpoint_id(
                        row.get("endpoint_id") or row.get("credential_id") or "")
                except ValueError:
                    endpoint_id = ""
                if endpoint_id in removed_endpoint_ids:
                    row["endpoint_id"] = ""
                    row.pop("credential_id", None)
                    counts["references"] += 1

        self._flush()
        return counts

    def _account_modes(self) -> dict[str, str]:
        """Map account_id → on-disk credential mode, so migration binds an empty
        profile to its real default account as engine_key (not host-inherit).
        Lock contention must remain explicit instead of looking like no accounts."""
        try:
            from muteki.solver.credential_accounts import (
                CredentialAccountStore, account_store_root,
            )
            store = CredentialAccountStore(account_store_root(self._root))
            return {a["account_id"]: str(a.get("mode") or "") for a in store.list()}
        except (CredentialAccountLockTimeoutError, SharedCredentialError):
            raise
        except Exception:  # noqa: BLE001
            return {}

    def _custom_endpoint_accounts(self) -> dict[str, dict[str, str]]:
        """Return non-secret custom-endpoint account metadata keyed by account id.

        The credential account store is the UI's source of truth for base_url +
        target_engine. The scheduler/CLI drivers, however, still consume the flat
        legacy profile dict and only switch to EndpointDriver when profile.base_url
        is present. Keep that bridge here so account edits immediately affect both
        settings health checks and real dispatch without copying secrets into the
        worker config JSON.
        """
        try:
            from muteki.solver.credential_accounts import (
                CredentialAccountStore, account_store_root,
            )
            store = CredentialAccountStore(account_store_root(self._root))
            out: dict[str, dict[str, str]] = {}
            for row in store.list():
                if not isinstance(row, dict):
                    continue
                if row.get("mode") != "custom_endpoint" or not row.get("present"):
                    continue
                details = row.get("details") if isinstance(row.get("details"), dict) else {}
                base_url = str(details.get("base_url_value") or "").strip()
                if not base_url:
                    continue
                account_id = str(row.get("account_id") or "").strip()
                if not account_id:
                    continue
                out[account_id] = {
                    "base_url": base_url,
                    "target_engine": str(
                        details.get("target_engine") or row.get("engine") or ""
                    ).strip().lower(),
                }
            return out
        except (CredentialAccountLockTimeoutError, SharedCredentialError):
            raise
        except Exception:  # noqa: BLE001
            return {}

    def _credential_projections_for_seats(
        self, seats: list[dict[str, Any]],
    ) -> list[dict[str, Any]]:
        """Build the legacy scheduler projection from stable credential ids.

        The credential account store is the only editable source of identity,
        endpoint and authentication metadata.  ``credentials[]`` in the Worker
        config remains a non-secret compatibility projection for the old flat
        profile adapter; callers cannot use it to create or edit credentials.
        Custom endpoint metadata is deliberately omitted here and hydrated from
        the account store at each read/dispatch boundary.
        """
        from muteki.solver.credential_accounts import (
            CredentialAccountStore,
            account_id_from_credential_id,
            account_store_root,
            canonical_credential_id,
            engine_from_system_credential_id,
        )

        store = CredentialAccountStore(account_store_root(self._root))
        accounts = {
            str(row.get("account_id") or ""): row
            for row in store.list()
            if isinstance(row, dict) and row.get("account_id")
        }
        projected: list[dict[str, Any]] = []
        seen: set[str] = set()
        for seat in seats:
            if not isinstance(seat, dict):
                continue
            engine = base_engine_for_profile(seat)
            try:
                credential_id = canonical_credential_id(
                    seat.get("credential_id"), engine=engine)
            except ValueError:
                # Save-time identity validation reports the actionable error.
                continue
            if not credential_id or credential_id in seen:
                continue
            seen.add(credential_id)
            system_engine = engine_from_system_credential_id(credential_id)
            account_id = account_id_from_credential_id(credential_id)
            if system_engine:
                projected.append({
                    "id": credential_id,
                    "label": f"{system_engine} 系统登录",
                    "engine": system_engine,
                    "kind": "system_inherit",
                    "secret_ref": "",
                })
                continue
            account = accounts.get(account_id) or {}
            connection = str(account.get("connection") or "official")
            projected.append({
                "id": credential_id,
                "label": account_id,
                "engine": engine,
                "kind": (
                    "custom_endpoint"
                    if connection == "custom_endpoint"
                    else "engine_key"
                ),
                "secret_ref": account_id,
                **({"target_engine": engine}
                   if connection == "custom_endpoint" else {}),
            })
        return projected

    @staticmethod
    def _llm_endpoint_identity(value: Any) -> tuple[str, str, str]:
        """Normalize an OpenAI-style endpoint, treating a final /v1 as API syntax."""
        text = str(value or "").strip()
        if not text:
            return "", "", ""
        try:
            parsed = urlsplit(text)
        except ValueError:
            return "", "", ""
        path = parsed.path.rstrip("/")
        if path.casefold().endswith("/v1"):
            path = path[:-3].rstrip("/")
        return parsed.scheme.casefold(), parsed.netloc.casefold(), path

    def _project_legacy_llm_credentials(self) -> None:
        """Find a unique global account for each old inline LLM endpoint.

        This is an in-memory, auditable projection only. Importing the server does
        not write either config or secret files. A later explicit settings save
        persists the selected stable id and transfers the legacy model/provider
        metadata into the matched account.
        """
        profiles = self._data.get("llm_profiles")
        if not isinstance(profiles, dict):
            return
        try:
            from muteki.solver.credential_accounts import (
                CredentialAccountStore,
                account_credential_id,
                account_store_root,
            )

            accounts = CredentialAccountStore(
                account_store_root(self._root)).list()
        except Exception:  # noqa: BLE001
            return
        audits = self._data.setdefault("_llm_credential_migrations", {})
        if not isinstance(audits, dict):
            audits = {}
            self._data["_llm_credential_migrations"] = audits
        for which in ("planner", "titler"):
            row = profiles.get(which)
            if not isinstance(row, dict) or row.get("endpoint_id") or row.get("credential_id"):
                continue
            base_url = str(row.get("base_url") or "").strip()
            if not base_url:
                continue
            endpoint_id = self._llm_endpoint_identity(base_url)
            provider = str(row.get("provider") or "").strip()
            model = str(row.get("model") or "").strip()
            matches: list[dict[str, Any]] = []
            for account in accounts:
                if not account.get("present") or not account.get("account_id"):
                    continue
                if account.get("credential_format") != "api_key":
                    continue
                if self._llm_endpoint_identity(account.get("base_url")) != endpoint_id:
                    continue
                account_provider = str(account.get("provider") or "").strip()
                if (
                    provider and account_provider
                    and provider.casefold() != account_provider.casefold()
                ):
                    continue
                account_models = [
                    str(item).strip() for item in account.get("models") or []
                    if str(item).strip()
                ]
                if model and account_models and model not in account_models:
                    continue
                matches.append(account)
            if len(matches) != 1:
                audits[which] = {
                    "status": "ambiguous" if matches else "unmatched",
                    "matched_by": ["base_url", "provider", "models"],
                    "candidate_ids": [
                        account_credential_id(str(item["account_id"]))
                        for item in matches
                    ],
                    "legacy_base_url": base_url,
                    "legacy_provider": provider,
                    "legacy_model": model,
                }
                continue
            credential_id = account_credential_id(str(matches[0]["account_id"]))
            from apps.web.llm_credentials import model_endpoint_id

            endpoint_id = model_endpoint_id(str(matches[0]["account_id"]))
            row["endpoint_id"] = endpoint_id
            row["connection"] = "endpoint"
            audits[which] = {
                "status": "pending_explicit_save",
                "matched_by": ["base_url", "provider", "models"],
                "credential_id": credential_id,
                "endpoint_id": endpoint_id,
                "legacy_base_url": base_url,
                "legacy_provider": provider,
                "legacy_model": model,
            }

    def _persist_llm_credential_migrations(self, profiles: Any) -> None:
        """Persist pending legacy metadata only as part of an explicit save."""
        if not isinstance(profiles, dict):
            return
        audits = self._data.get("_llm_credential_migrations")
        if not isinstance(audits, dict):
            return
        grouped: dict[str, dict[str, Any]] = {}
        for which in ("planner", "titler"):
            row = profiles.get(which)
            audit = audits.get(which)
            if not isinstance(row, dict) or not isinstance(audit, dict):
                continue
            if audit.get("status") != "pending_explicit_save":
                continue
            from apps.web.llm_credentials import canonical_model_endpoint_id
            from muteki.solver.credential_accounts import account_credential_id

            endpoint_id = canonical_model_endpoint_id(
                row.get("endpoint_id") or row.get("credential_id") or "")
            if endpoint_id != str(audit.get("endpoint_id") or ""):
                continue
            account_id = endpoint_id.removeprefix("endpoint:")
            credential_id = account_credential_id(account_id)
            model = str(row.get("model") or "").strip()
            if model != str(audit.get("legacy_model") or ""):
                continue
            group = grouped.setdefault(credential_id, {
                "models": [],
                "provider": str(audit.get("legacy_provider") or ""),
                "base_url": str(audit.get("legacy_base_url") or ""),
                "profiles": [],
            })
            if model and model not in group["models"]:
                group["models"].append(model)
            group["profiles"].append(which)
        if not grouped:
            return
        from muteki.solver.credential_accounts import (
            CredentialAccountStore,
            account_id_from_credential_id,
            account_store_root,
        )

        store = CredentialAccountStore(account_store_root(self._root))
        for credential_id, metadata in grouped.items():
            account_id = account_id_from_credential_id(credential_id)
            account = store.inspect(account_id)
            if account is None or not account.present:
                raise ValueError(
                    f"兼容迁移目标凭据不可用：{credential_id}")
            details = dict(account.details or {})
            existing = next((
                item for item in store.list()
                if str(item.get("account_id") or "") == account_id
            ), {})
            models = list(existing.get("models") or [])
            for model in metadata["models"]:
                if model not in models:
                    models.append(model)
            store.upsert_secret(
                account_id=account_id,
                engine="api",
                secret=None,
                base_url=metadata["base_url"],
                target_engine=str(
                    details.get("target_engine") or account.engine or ""),
                provider=metadata["provider"],
                models=models,
            )
            for which in metadata["profiles"]:
                audits[which] = {
                    **dict(audits[which]),
                    "status": "applied",
                }

    def _hydrate_profiles_from_accounts(
        self,
        profiles: list[dict[str, Any]],
    ) -> list[dict[str, Any]]:
        """Overlay account-store custom endpoint metadata onto worker profiles.

        This fixes the "settings account has BASE_URL but Codex still calls
        OpenAI" class of bugs: Codex's provider override is driven by profile
        base_url, while the settings form stores base_url on the credential
        account. Explicit profile base_url still wins; the account store only
        fills the gap.
        """
        endpoints = self._custom_endpoint_accounts()
        if not endpoints:
            return profiles
        from muteki.solver.credential_accounts import (
            account_id_from_credential_id,
            engine_from_system_credential_id,
        )

        out: list[dict[str, Any]] = []
        for profile in profiles:
            p = dict(profile)
            engine = base_engine_for_profile(p)
            if engine not in VALID_BASE_ENGINES:
                out.append(p)
                continue
            credential_id = str(p.get("credential_id") or "").strip()
            credential_kind = str(p.get("credential_kind") or "").strip()
            # A canonical system-login selection is explicit.  An empty legacy
            # credential_account is how that selection is transported to the
            # CLI, so it must not be reinterpreted as "try <engine>-main".
            if (
                credential_kind == "system_inherit"
                or engine_from_system_credential_id(credential_id)
            ):
                out.append(p)
                continue
            explicit_account = (
                str(p.get("credential_account") or "").strip()
                or account_id_from_credential_id(credential_id)
            )
            # The default-account fallback remains only for old profiles that
            # predate canonical credential ids.  New identity records always
            # follow the credential selected in the global credential center.
            account_ids = (
                [explicit_account]
                if explicit_account
                else [f"{engine}-main"] if not credential_id else []
            )
            for account_id in account_ids:
                ep = endpoints.get(account_id)
                if not ep:
                    continue
                target = str(ep.get("target_engine") or "").strip().lower()
                # A legacy endpoint with no target marker may be used by an
                # explicitly-bound profile. Empty profile bindings only inherit
                # the engine's own default endpoint when the marker matches.
                if target and target != engine:
                    continue
                if not explicit_account and target != engine:
                    continue
                if not str(p.get("base_url") or "").strip():
                    p["base_url"] = ep["base_url"]
                p["credential_account"] = account_id
                p["credential_mode"] = "api_key"
                p["auth"] = "api_key"
                if engine == "codex" and not str(p.get("wire_api") or "").strip():
                    p["wire_api"] = "responses"
                break
            out.append(p)
        return out

    def identity_model(self) -> dict[str, Any]:
        """Return the stored Credential/Seat view. Never raises."""
        d = self._data
        seats = [s for s in (d.get("seats") or []) if isinstance(s, dict)]
        creds = [c for c in (d.get("credentials") or []) if isinstance(c, dict)]
        seat_alias = {str(s.get("label")): str(s.get("id")) for s in seats if s.get("label")}
        cred_alias = {str(c.get("secret_ref")): str(c.get("id"))
                      for c in creds if c.get("secret_ref")}
        return {
            "credentials": creds, "seats": seats,
            "seat_alias": seat_alias, "credential_alias": cred_alias,
        }

    def get(self) -> dict[str, Any]:
        """The current default config with everything filled in (never raises)."""
        d = self._data
        worker_profiles = self._hydrate_profiles_from_accounts(
            self._clean_worker_profiles(d.get("worker_profiles"))
        )
        worker_backend = self._clean_backend(d.get("worker_backend"))
        worker_network = self._clean_worker_network(d.get("worker_network"))
        worker_container_scope = self._clean_container_scope(
            d.get("worker_container_scope"))
        worker_privilege = self._clean_worker_privilege(d.get("worker_privilege"))
        resource_limits = resolve_worker_resource_limits(config=d)
        worker_memory = resource_limits.memory
        worker_cpus = resource_limits.cpus
        worker_pids_limit = resource_limits.pids_limit
        worker_output_limit = resource_limits.output_limit
        worker_disk_limit = resource_limits.disk_limit
        engines = _clean_engines_for_backend(d.get("engines"), worker_profiles, worker_backend) or [
            p["name"] for p in worker_profiles if p.get("enabled", True)
        ]
        start_workers = self._coerce_pos_int(d.get("start_workers"), len(engines))
        max_workers = self._coerce_pos_int(d.get("max_workers"), DEFAULT_MAX_WORKERS)
        race_timeout = self._coerce_pos_int(d.get("race_timeout"), DEFAULT_RACE_TIMEOUT)
        wall_clock_budget = self._coerce_nonneg_int(
            d.get("wall_clock_budget"), DEFAULT_WALL_CLOCK_BUDGET)
        max_total_workers = self._coerce_nonneg_int(
            d.get("max_total_workers"), DEFAULT_MAX_TOTAL_WORKERS)
        cost_budget_usd = self._coerce_nonneg_float(
            d.get("cost_budget_usd"), DEFAULT_COST_BUDGET_USD)
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
            verifier = raw_stage_policy.setdefault("coordinator", {}).setdefault("verifier", {})
            verifier["engine"] = _remap_profile_ref(
                verifier.get("engine") or DEFAULT_VERIFIER_POLICY["engine"],
                worker_profiles,
                worker_backend,
            )
        stage_policy = self._clean_stage_policy(raw_stage_policy, {
            "race_timeout": race_timeout,
            "wall_clock_budget": wall_clock_budget,
            "max_total_workers": max_total_workers,
            "cost_budget_usd": cost_budget_usd,
        })
        race_scout = bool(stage_policy["race"]["enabled"])
        # Fixed mode races every selected ordinary seat; auto never races.
        # Legacy flat/per-seat Race switches remain readable but cannot create
        # a third effective scheduling mode.
        race_engines = (
            [name for name in engines if any(
                _profile_name(profile) == name and _ordinary_worker_roles(profile)
                for profile in worker_profiles
            )] if race_scout else []
        )
        stage_policy["race"]["engines"] = list(race_engines)
        if race_scout:
            for profile in worker_profiles:
                if _profile_name(profile) in race_engines:
                    profile["race"] = True
                    roles = list(profile.get("roles") or [])
                    if "race" not in roles:
                        profile["roles"] = [*roles, "race"]
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
        verifier = stage_policy.setdefault("coordinator", {}).setdefault(
            "verifier", dict(DEFAULT_VERIFIER_POLICY))
        verifier_engine = _remap_profile_ref(
            verifier.get("engine") or DEFAULT_VERIFIER_POLICY["engine"],
            worker_profiles,
            worker_backend,
        )
        if verifier_engine not in names:
            verifier_engine = next(
                (
                    str(p.get("name") or p.get("id"))
                    for p in worker_profiles
                    if "verifier" in (p.get("roles") or [])
                ),
                engines[0] if engines else DEFAULT_VERIFIER_POLICY["engine"],
            )
        verifier["engine"] = verifier_engine
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
            "worker_network": worker_network,
            "worker_container_scope": worker_container_scope,
            "worker_privilege": worker_privilege,
            "worker_memory": worker_memory,
            "worker_cpus": worker_cpus,
            "worker_pids_limit": worker_pids_limit,
            "worker_output_limit": worker_output_limit,
            "worker_disk_limit": worker_disk_limit,
            "worker_vpn_enabled": self._coerce_bool(
                d.get("worker_vpn_enabled"), False),
            "race_scout": race_scout,
            "race_timeout": race_timeout,
            "wall_clock_budget": wall_clock_budget,
            "race_engines": race_engines,
            "max_total_workers": max_total_workers,
            "cost_budget_usd": cost_budget_usd,
            "stage_policy": stage_policy,
            "llm_profiles": llm_profiles,
            "worker_profiles": worker_profiles,
            "overrides": overrides,
        }
        # ── additive: attach the Credential/Seat view (Phase A
        # iron rule — old fields above stay; new fields are added alongside so the
        # legacy frontend keeps working while the new UI can consume these). ──
        if isinstance(self._data.get("seats"), list) and self._data.get("seats"):
            ident = self.identity_model()
        else:
            res = migrate_legacy_config(
                worker_profiles=worker_profiles,
                account_modes=self._account_modes(),
            )
            ident = {
                "credentials": [c.to_dict() for c in res.credentials],
                "seats": [s.to_dict() for s in res.seats],
                "seat_alias": res.seat_alias,
                "credential_alias": res.credential_alias,
            }
        result["credentials"] = ident["credentials"]
        result["seats"] = [
            {
                **seat,
                "race": True,
                "roles": list(dict.fromkeys([*(seat.get("roles") or []), "race"])),
            }
            if race_scout and str(seat.get("id") or "") in race_engines
            else seat
            for seat in ident["seats"]
        ]
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
                "worker_network": cfg["worker_network"],
                "worker_container_scope": cfg["worker_container_scope"],
                "worker_privilege": cfg.get("worker_privilege", "default"),
                **{key: cfg[key] for key in ("worker_memory", "worker_cpus", "worker_pids_limit", "worker_output_limit", "worker_disk_limit")},
                "worker_vpn_enabled": cfg["worker_vpn_enabled"],
                "race_scout": cfg["race_scout"],
                "race_timeout": cfg["race_timeout"],
                "wall_clock_budget": cfg["wall_clock_budget"],
                "race_engines": cfg["race_engines"],
                "max_total_workers": cfg["max_total_workers"],
                "cost_budget_usd": cfg["cost_budget_usd"],
                "stage_policy": cfg["stage_policy"],
                "llm_profiles": cfg["llm_profiles"],
                "worker_profiles": cfg["worker_profiles"],
            }
        return {
            "engines": cfg["engines"],
            "start_workers": cfg["start_workers"],
            "max_workers": cfg["max_workers"],
            "worker_backend": cfg["worker_backend"],
            "worker_network": cfg["worker_network"],
            "worker_container_scope": cfg["worker_container_scope"],
            "worker_privilege": cfg.get("worker_privilege", "default"),
                **{key: cfg[key] for key in ("worker_memory", "worker_cpus", "worker_pids_limit", "worker_output_limit", "worker_disk_limit")},
            "worker_vpn_enabled": cfg["worker_vpn_enabled"],
            "race_scout": cfg["race_scout"],
            "race_timeout": cfg["race_timeout"],
            "wall_clock_budget": cfg["wall_clock_budget"],
            "race_engines": cfg["race_engines"],
            "max_total_workers": cfg["max_total_workers"],
            "cost_budget_usd": cfg["cost_budget_usd"],
            "stage_policy": cfg["stage_policy"],
            "llm_profiles": cfg["llm_profiles"],
            "worker_profiles": cfg["worker_profiles"],
        }

    def set(
        self,
        *,
        engines: Any = None,
        start_workers: Any = None,
        max_workers: Any = None,
        worker_backend: Any = None,
        worker_network: Any = None,
        worker_container_scope: Any = None,
        worker_privilege: Any = None,
        worker_memory: Any = None,
        worker_cpus: Any = None,
        worker_pids_limit: Any = None,
        worker_output_limit: Any = None,
        worker_disk_limit: Any = None,
        worker_vpn_enabled: Any = None,
        race_scout: Any = None,
        race_timeout: Any = None,
        wall_clock_budget: Any = None,
        race_engines: Any = None,
        max_total_workers: Any = None,
        cost_budget_usd: Any = None,
        stage_policy: Any = None,
        llm_profiles: Any = None,
        worker_profiles: Any = None,
        overrides: Any = None,
    ) -> dict[str, Any]:
        """Update the default config. Each arg is optional; only provided fields
        change. Invalid values are rejected (raise ValueError) so a bad PUT
        doesn't silently persist garbage."""
        # Validate resource limits before changing the live in-memory config.
        # In particular, a fractional budget must never truncate to 0 (unlimited).
        for field, value in (
            ("start_workers", start_workers), ("max_workers", max_workers),
            ("race_timeout", race_timeout),
        ):
            if value is not None:
                self._require_pos_int(value, field)
        for field, value in (
            ("wall_clock_budget", wall_clock_budget),
            ("max_total_workers", max_total_workers),
        ):
            if value is not None:
                self._require_nonneg_int(value, field)
        if cost_budget_usd is not None:
            self._require_nonneg_float(cost_budget_usd, "cost_budget_usd")
        if stage_policy is not None:
            self._validate_stage_limits(stage_policy)
        if worker_profiles is not None:
            self._clean_worker_profiles(worker_profiles, reject_invalid=True)
        # Reject bad resource limits before any live mutation (#170).
        if worker_memory is not None:
            validate_memory(worker_memory)
        if worker_cpus is not None:
            validate_cpus(worker_cpus)
        if worker_pids_limit is not None:
            validate_pids_limit(worker_pids_limit)
        if worker_output_limit is not None:
            validate_memory(worker_output_limit, field="worker_output_limit")
        if worker_disk_limit is not None:
            validate_memory(worker_disk_limit, field="worker_disk_limit")
        if isinstance(overrides, dict):
            for cat, override in overrides.items():
                if isinstance(override, dict) and override.get("start_workers") is not None:
                    self._require_pos_int(override["start_workers"], f"{cat}.start_workers")
        previous_data = copy.deepcopy(self._data)
        previous_seats = copy.deepcopy(previous_data.get("seats") or [])
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
        if worker_network is not None:
            self._data["worker_network"] = self._require_worker_network(worker_network)
        if worker_container_scope is not None:
            self._data["worker_container_scope"] = self._require_container_scope(
                worker_container_scope)
        if worker_privilege is not None:
            self._data["worker_privilege"] = self._require_worker_privilege(
                worker_privilege)
        if worker_memory is not None:
            self._data["worker_memory"] = validate_memory(worker_memory)
        if worker_cpus is not None:
            self._data["worker_cpus"] = validate_cpus(worker_cpus)
        if worker_pids_limit is not None:
            self._data["worker_pids_limit"] = validate_pids_limit(worker_pids_limit)
        if worker_output_limit is not None:
            self._data["worker_output_limit"] = validate_memory(
                worker_output_limit, field="worker_output_limit")
        if worker_disk_limit is not None:
            self._data["worker_disk_limit"] = validate_memory(
                worker_disk_limit, field="worker_disk_limit")
        if worker_vpn_enabled is not None:
            self._data["worker_vpn_enabled"] = bool(worker_vpn_enabled)
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
                verifier = clean_stage.setdefault("coordinator", {}).setdefault("verifier", {})
                verifier["engine"] = _remap_profile_ref(
                    verifier.get("engine") or DEFAULT_VERIFIER_POLICY["engine"],
                    profiles_for_stage,
                    target_backend,
                )
            self._data["stage_policy"] = self._clean_stage_policy(clean_stage, {})
        if llm_profiles is not None:
            self._data["llm_profiles"] = self._clean_llm_profiles(
                llm_profiles, reject_invalid=True)
        if worker_profiles is not None:
            self._data["worker_profiles"] = self._clean_worker_profiles(
                worker_profiles, reject_invalid=True)
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
        # Swarm prefers stage_policy.race.engines over the flat race_engines
        # knob. A partial PUT that updated only one field used to leave a stale
        # subset in stage_policy and silently drop race workers. Only sync when
        # one of those two fields was in this PUT, so an unrelated save cannot
        # copy a stale subset back onto the flat roster.
        if race_engines is not None or stage_policy is not None:
            self._sync_race_engine_roster(prefer_flat=race_engines is not None)
        # New-schema-on-disk (user decision): whenever the legacy worker_profiles
        # change (the v2 frontend still saves in legacy shape), derive and persist
        # the Credential/Seat model alongside, so disk carries the new
        # shape as the source of truth. Reads then prefer the seats[] block.
        if worker_profiles is not None:
            self._persist_identity_from_legacy()
        try:
            seeds = self._validate_identity_backend(
                target_backend,
                compatible_seats=previous_seats,
            )
            self._persist_worker_model_seeds(seeds)
            if llm_profiles is not None:
                self._persist_llm_credential_migrations(llm_profiles)
                self._data["llm_profiles"] = self._clean_llm_profiles(
                    llm_profiles, reject_invalid=True)
        except ValueError:
            # Validation runs after the legacy/new projections have been built;
            # restore the prior identity projection if the final binding is bad.
            self._data = previous_data
            raise
        effective = self.get()
        self._data["stage_policy"] = effective["stage_policy"]
        self._data["race_scout"] = effective["race_scout"]
        self._data["race_engines"] = effective["race_engines"]
        if effective["race_scout"]:
            race_set = set(effective["race_engines"])
            for key in ("worker_profiles", "seats"):
                for profile in self._data.get(key) or []:
                    if not isinstance(profile, dict):
                        continue
                    if str(profile.get("name") or profile.get("id") or "") not in race_set:
                        continue
                    profile["race"] = True
                    roles = list(profile.get("roles") or [])
                    if "race" not in roles:
                        profile["roles"] = [*roles, "race"]
        self._flush()
        return self.get()

    def _sync_race_engine_roster(self, *, prefer_flat: bool) -> None:
        """Keep the flat race_engines knob and stage_policy.race.engines aligned."""
        stage = self._data.setdefault("stage_policy", {})
        if not isinstance(stage, dict):
            stage = {}
            self._data["stage_policy"] = stage
        race = stage.setdefault("race", {})
        if not isinstance(race, dict):
            race = {}
            stage["race"] = race
        flat = self._data.get("race_engines")
        staged = race.get("engines")
        if prefer_flat and isinstance(flat, list):
            race["engines"] = list(flat)
            return
        if isinstance(staged, list):
            self._data["race_engines"] = list(staged)

    def _persist_identity_from_legacy(self) -> None:
        """Derive seats/credentials from the current worker profiles and write
        them into self._data, so the
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
            # The seats[] block is additive; we leave the legacy engines[]/
            # review.engine foreign keys in their current (readable) form rather than
            # rewriting them to seat ids on every save. Rationale: the legacy
            # resolve_seat_ref() bridges either form at the read boundaries
            # (health route, scheduler), and _project_identity_to_legacy reconciles a
            # new-shaped file on load. Stable seat ids still live in seats[].
        except Exception:  # noqa: BLE001
            pass

    def set_identity_model(
        self,
        *,
        seats: Any = None,
        credentials: Any = None,
        worker_backend: Any = None,
        worker_network: Any = None,
    ) -> dict[str, Any]:
        """Persist the Credential/Seat model to disk.

        Validates the hard constraint that container execution forbids a
        system_inherit credential and rejects an illegal combo with ValueError —
        the save-time gate Codex specified, so an illegal config never persists.
        After writing, re-projects to legacy worker_profiles so the in-memory
        scheduler view stays consistent. Each arg optional; only provided ones
        change. Never silently drops a bad value — it raises."""
        next_seats = self._data.get("seats") or []
        next_credentials = self._data.get("credentials") or []
        if seats is not None:
            next_seats = self._clean_seats(seats)
        if credentials is not None:
            if not isinstance(credentials, list):
                raise ValueError("credentials must be a list")
            next_credentials = [c for c in credentials if isinstance(c, dict)]
        next_credentials, next_seats, _ = migrate_identity_credential_references(
            next_credentials, next_seats)
        next_credentials = self._credential_projections_for_seats(next_seats)
        backend = (
            self._require_backend(worker_backend)
            if worker_backend is not None
            else self._clean_backend(self._data.get("worker_backend"))
        )
        next_network = (
            self._require_worker_network(worker_network)
            if worker_network is not None
            else self._data.get("worker_network")
        )
        seeds = self._validate_identity_backend(
            backend,
            seats=next_seats,
            credentials=next_credentials,
            compatible_seats=self._data.get("seats") or [],
        )
        self._persist_worker_model_seeds(seeds)
        self._data["seats"] = next_seats
        self._data["credentials"] = next_credentials
        if worker_backend is not None:
            self._data["worker_backend"] = backend
        if worker_network is not None:
            self._data["worker_network"] = next_network
        # keep the legacy projection in sync so get()/scheduler see the change.
        self._project_identity_to_legacy()
        self._sync_worker_counts(link_profile_capacity=True)
        self._flush()
        return self.get()

    def detach_credential_references(
        self, credential_id: str,
    ) -> dict[str, Any]:
        """Atomically detach one global credential from Worker-owned settings.

        Credential deletion is a cleanup operation over existing configuration.
        It must not re-admit every unrelated active Seat through today's stricter
        save-time model-test gate: historical rows may remain runnable while a
        different credential is being removed.  This method therefore validates
        only the target reference, disables matching workers, restores matching
        planner/titler profiles to their default connection, and writes the whole
        Worker config once.
        """
        from muteki.solver.credential_accounts import (
            account_credential_id,
            canonical_credential_id,
            system_credential_id,
        )

        stable_id = canonical_credential_id(credential_id)
        if not stable_id:
            raise ValueError("credential_id is required")

        previous_data = copy.deepcopy(self._data)
        detached_worker_ids: set[str] = set()
        detached_llm_profiles: list[str] = []

        def stable_ref(row: dict[str, Any]) -> str:
            engine = base_engine_for_profile(row)
            raw = str(row.get("credential_id") or "").strip()
            if not raw and row.get("credential_account"):
                try:
                    raw = account_credential_id(
                        str(row.get("credential_account") or ""))
                except ValueError:
                    return ""
            if (
                not raw
                and str(row.get("credential_kind") or "") == "system_inherit"
            ):
                try:
                    raw = system_credential_id(engine)
                except ValueError:
                    return ""
            try:
                return canonical_credential_id(raw, engine=engine)
            except ValueError:
                return ""

        try:
            raw_seats = self._data.get("seats")
            if isinstance(raw_seats, list):
                seats = copy.deepcopy(raw_seats)
                seats_changed = False
                for seat in seats:
                    if not isinstance(seat, dict) or stable_ref(seat) != stable_id:
                        continue
                    worker_id = str(seat.get("id") or "")
                    if worker_id:
                        detached_worker_ids.add(worker_id)
                    seat["enabled"] = False
                    seat["credential_id"] = ""
                    seats_changed = True
                if seats_changed:
                    self._data["seats"] = seats
                    self._data["credentials"] = (
                        self._credential_projections_for_seats(seats)
                    )
                    self._project_identity_to_legacy()

            # A rolling-migration config may still contain profiles that do not
            # have a Seat row.  Detach those with the same narrow comparison.
            raw_profiles = self._data.get("worker_profiles")
            if isinstance(raw_profiles, list):
                profiles = copy.deepcopy(raw_profiles)
                profiles_changed = False
                for profile in profiles:
                    if (
                        not isinstance(profile, dict)
                        or stable_ref(profile) != stable_id
                    ):
                        continue
                    worker_id = str(
                        profile.get("id") or profile.get("name") or "")
                    if worker_id:
                        detached_worker_ids.add(worker_id)
                    profile["enabled"] = False
                    profile["credential_id"] = ""
                    profile.pop("credential_account", None)
                    profile.pop("credential_kind", None)
                    profiles_changed = True
                if profiles_changed:
                    self._data["worker_profiles"] = profiles

            raw_llm_profiles = self._data.get("llm_profiles")
            if isinstance(raw_llm_profiles, dict):
                from apps.web.llm_credentials import (
                    canonical_model_endpoint_id,
                    model_endpoint_id,
                )
                from muteki.solver.credential_accounts import account_id_from_credential_id

                account_id = account_id_from_credential_id(stable_id)
                target_endpoint_id = model_endpoint_id(account_id) if account_id else ""
                llm_profiles = copy.deepcopy(raw_llm_profiles)
                for which in ("planner", "titler"):
                    profile = llm_profiles.get(which)
                    if not isinstance(profile, dict):
                        continue
                    try:
                        profile_endpoint_id = canonical_model_endpoint_id(
                            profile.get("endpoint_id")
                            or profile.get("credential_id")
                            or "")
                    except ValueError:
                        profile_endpoint_id = ""
                    if profile_endpoint_id != target_endpoint_id:
                        continue
                    profile["endpoint_id"] = ""
                    profile.pop("credential_id", None)
                    profile["connection"] = "default"
                    profile["base_url"] = ""
                    profile.pop("credential_migration", None)
                    detached_llm_profiles.append(which)
                if detached_llm_profiles:
                    self._data["llm_profiles"] = llm_profiles
                    audits = self._data.get("_llm_credential_migrations")
                    if isinstance(audits, dict):
                        for which in detached_llm_profiles:
                            audits.pop(which, None)

            if detached_worker_ids:
                self._sync_worker_counts(link_profile_capacity=True)
            changed = bool(detached_worker_ids or detached_llm_profiles)
            if changed:
                self._flush()
            return {
                "changed": changed,
                "workers": len(detached_worker_ids),
                "llm_profiles": list(detached_llm_profiles),
            }
        except Exception:
            self._data = previous_data
            raise

    def _validate_identity_backend(
        self,
        backend: str,
        *,
        seats: Any = None,
        credentials: Any = None,
        compatible_seats: Any = None,
    ) -> dict[str, dict[str, Any]]:
        """Validate enabled seats against one final target backend."""
        target_seats = (
            self._data.get("seats") or []
            if seats is None
            else seats
        )
        from apps.web.worker_models import worker_model_options_payload
        from muteki.solver.credential_accounts import (
            CredentialAccountStore,
            account_id_from_credential_id,
            account_store_root,
            canonical_credential_id,
            detect_system_login,
            engine_from_system_credential_id,
        )

        store = CredentialAccountStore(account_store_root(self._root))
        public_accounts = {
            str(item.get("account_id") or ""): item
            for item in store.list()
            if isinstance(item, dict)
        }
        model_catalog = (
            worker_model_options_payload(self._root).get("models") or {})
        system_status: dict[str, str] = {}
        compatibility = {
            str(item.get("id") or ""): item
            for item in (compatible_seats or [])
            if isinstance(item, dict)
        }
        seeds: dict[str, dict[str, Any]] = {}

        def catalog_models(engine: str, account_id: str = "") -> list[str]:
            out: list[str] = []
            account = public_accounts.get(account_id) or {}
            for item in [
                *(account.get("models") or []),
                *(model_catalog.get(engine) or []),
            ]:
                value = str(
                    item.get("id") if isinstance(item, dict) else item
                ).strip()
                if value and value not in out:
                    out.append(value)
            return out

        for s in target_seats:
            # Disabled seats are retained and never dispatched.
            # They may keep their host-login binding while an active container
            # roster uses an injectable credential.
            if not bool(s.get("enabled", True)):
                continue
            label = str(s.get("label") or s.get("id") or "未命名 Agent")
            engine = base_engine_for_profile(s)
            raw_credential_id = str(s.get("credential_id") or "").strip()
            try:
                credential_id = canonical_credential_id(
                    raw_credential_id, engine=engine)
            except ValueError as exc:
                raise ValueError(
                    f"Agent「{label}」的 credential_id 无效或引擎不匹配") from exc
            if not credential_id:
                raise ValueError(f"Agent「{label}」必须选择 credential_id")

            system_engine = engine_from_system_credential_id(credential_id)
            account_id = account_id_from_credential_id(credential_id)
            if system_engine:
                if backend != "local":
                    raise ValueError(
                        f"Agent「{label}」在容器环境下不能使用系统登录凭据")
                if system_engine not in system_status:
                    system_status[system_engine] = detect_system_login(system_engine)
                if system_status[system_engine] != "present":
                    raise ValueError(
                        f"Agent「{label}」的系统登录凭据 {credential_id} 当前不可用")
                models = catalog_models(engine)
            elif account_id:
                account = store.inspect(account_id)
                if account is None or not account.present:
                    raise ValueError(
                        f"Agent「{label}」引用的凭据账号不存在或未就绪：{credential_id}")
                details = dict(account.details or {})
                account_engine = str(
                    details.get("target_engine") or account.engine or ""
                ).strip().lower()
                if account_engine != engine:
                    raise ValueError(
                        f"Agent「{label}」使用 {engine}，凭据 {credential_id} "
                        f"绑定的是 {account_engine or '未知引擎'}")
                models = catalog_models(engine, account_id)
            else:
                raise ValueError(f"Agent「{label}」的 credential_id 无效")

            model = str(s.get("model") or "").strip()
            if not model:
                # Empty means the Runtime/CLI default model and remains legal.
                continue
            previous_seat = compatibility.get(str(s.get("id") or ""))
            binding_unchanged = False
            if previous_seat and bool(previous_seat.get("enabled", True)):
                previous_engine = base_engine_for_profile(previous_seat)
                try:
                    previous_credential_id = canonical_credential_id(
                        previous_seat.get("credential_id") or "",
                        engine=previous_engine,
                    )
                except ValueError:
                    previous_credential_id = ""
                binding_unchanged = (
                    previous_engine == engine
                    and previous_credential_id == credential_id
                    and str(previous_seat.get("model") or "").strip() == model
                )
            # A settings save may include the whole roster even when the operator
            # only changed Planner/Titler, budgets, or another unrelated field.
            # Preserve an unchanged, already-persisted model binding without
            # demanding a new test. New/enabled/changed bindings still pass the
            # real-test and catalog gates below.
            if binding_unchanged:
                continue
            last_test = store.last_test(credential_id, backend=backend, model=model)
            tested_model = str((last_test or {}).get("model") or "").strip()
            if last_test and last_test.get("ok") and tested_model and tested_model not in models:
                models.append(tested_model)
            if not last_test or not last_test.get("ok") or tested_model != model:
                raise ValueError(
                    f"Agent「{label}」的模型 {model!r} 尚未使用凭据 "
                    f"{credential_id} 完成真实测试")
            if model not in models:
                raise ValueError(
                    f"Agent「{label}」的模型 {model!r} 不属于凭据 "
                    f"{credential_id} 的 models")
        return seeds

    def _persist_worker_model_seeds(
        self, seeds: dict[str, dict[str, Any]]
    ) -> None:
        """Seed only unchanged legacy seat models during an explicit save."""
        if not seeds:
            return
        from muteki.solver.credential_accounts import (
            CredentialAccountStore,
            account_store_root,
        )

        store = CredentialAccountStore(account_store_root(self._root))
        audits = self._data.setdefault("_worker_credential_model_migrations", {})
        for account_id, seed in seeds.items():
            account = store.inspect(account_id)
            if account is None or not account.present:
                raise ValueError(f"凭据账号不可用：account:{account_id}")
            details = dict(account.details or {})
            public = next((
                item for item in store.list()
                if str(item.get("account_id") or "") == account_id
            ), {})
            models = list(public.get("models") or [])
            for model in seed.get("models") or []:
                if model not in models:
                    models.append(model)
            metadata_only_engine = (
                account.engine
                if account.mode in {"chatgpt_auth_home", "login_home"}
                or account.engine == "cursor"
                else "api" if details.get("api_key_file")
                else account.engine
            )
            store.upsert_secret(
                account_id=account_id,
                engine=metadata_only_engine,
                secret=None,
                base_url=str(details.get("base_url_value") or ""),
                target_engine=str(
                    details.get("target_engine") or account.engine or ""),
                provider=str(details.get("provider") or ""),
                models=models,
            )
            audits[account_id] = {
                "status": "applied",
                "models": list(seed.get("models") or []),
                "seat_ids": list(seed.get("seats") or []),
            }

    def set_configuration(
        self,
        *,
        seats: Any,
        credentials: Any = None,
        **settings: Any,
    ) -> dict[str, Any]:
        """Validate and persist identity, runtime and policy as one final state.

        ``set()`` performs the only file replacement. Any validation failure
        restores the in-memory snapshot, so callers never observe a half-saved
        backend/identity combination.
        """
        next_seats = self._clean_seats(seats)
        if credentials is not None and not isinstance(credentials, list):
            raise ValueError("credentials must be a list")
        previous = copy.deepcopy(self._data)
        try:
            # Accept the old field only to translate legacy ``cred_*`` seat ids.
            # Persisted rows are always rebuilt from the global credential store.
            next_credentials = [
                copy.deepcopy(c) for c in (credentials or [])
                if isinstance(c, dict)
            ]
            next_credentials, next_seats, _ = migrate_identity_credential_references(
                next_credentials, next_seats)
            next_credentials = self._credential_projections_for_seats(next_seats)
            self._data["seats"] = next_seats
            self._data["credentials"] = next_credentials
            self._project_identity_to_legacy()
            target_backend = self._require_backend(
                settings.get("worker_backend")
                if settings.get("worker_backend") is not None
                else self._clean_backend(self._data.get("worker_backend"))
            )
            seeds = self._validate_identity_backend(
                target_backend,
                compatible_seats=previous.get("seats") or [],
            )
            self._persist_worker_model_seeds(seeds)
            return self.set(**settings)
        except Exception:
            self._data = previous
            raise

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
        except (TypeError, ValueError, OverflowError) as exc:
            raise ValueError(f"{field} must be a positive integer") from exc
        if isinstance(value, bool) or n <= 0 or (not isinstance(value, str) and value != n):
            raise ValueError(f"{field} must be a positive integer")
        return n

    @staticmethod
    def _require_nonneg_int(value: Any, field: str) -> int:
        try:
            n = int(value)
        except (TypeError, ValueError, OverflowError) as exc:
            raise ValueError(f"{field} must be a non-negative integer") from exc
        if isinstance(value, bool) or n < 0 or (not isinstance(value, str) and value != n):
            raise ValueError(f"{field} must be a non-negative integer")
        return n

    @staticmethod
    def _require_nonneg_float(value: Any, field: str) -> float:
        try:
            n = float(value)
        except (TypeError, ValueError, OverflowError) as exc:
            raise ValueError(f"{field} must be a non-negative number") from exc
        if isinstance(value, bool) or not math.isfinite(n) or n < 0:
            raise ValueError(f"{field} must be a non-negative number")
        return n

    @classmethod
    def _clean_worker_capacity(cls, value: Any, field: str) -> dict[str, Any]:
        if not isinstance(value, dict):
            raise ValueError(f"{field} must be an object")
        result = copy.deepcopy(value)
        for key in ("max_running", "max_review_running", "max_verifier_running"):
            if key in result:
                require = cls._require_pos_int if key == "max_running" else cls._require_nonneg_int
                result[key] = require(result[key], f"{field}.{key}")
        return result

    @classmethod
    def _clean_seats(cls, value: Any) -> list[dict[str, Any]]:
        if not isinstance(value, list):
            raise ValueError("seats must be a list")
        result = []
        for index, row in enumerate(value):
            field = f"seats[{index}]"
            seat = cls._clean_worker_capacity(row, field)
            if "capacity" in seat:
                seat["capacity"] = cls._clean_worker_capacity(seat["capacity"], f"{field}.capacity")
            result.append(seat)
        return result

    @classmethod
    def _validate_stage_limits(cls, value: Any) -> None:
        if not isinstance(value, dict):
            raise ValueError("stage_policy must be an object")
        limits = (
            (("race",), ("timeout",), (), ()),
            (("coordinator",), (), ("wall_clock_budget",), ()),
            (("budgets",), (), ("max_total_workers",), ("cost_budget_usd",)),
            (("coordinator", "review"), (), ("max_concurrent", "max_review_workers"), ()),
            (("coordinator", "verifier"), (), ("max_concurrent", "max_verifier_workers"), ()),
        )
        for path, positive, nonnegative, amounts in limits:
            section = value
            for part in path:
                section = section.get(part, {})
                if not isinstance(section, dict):
                    raise ValueError(f"stage_policy.{'.'.join(path)} must be an object")
            for keys, require in (
                (positive, cls._require_pos_int),
                (nonnegative, cls._require_nonneg_int),
                (amounts, cls._require_nonneg_float),
            ):
                for key in keys:
                    if key in section:
                        require(section[key], f"stage_policy.{'.'.join(path)}.{key}")

    def _clean_llm_profiles(
        self, value: Any, *, reject_invalid: bool = False
    ) -> dict[str, dict[str, Any]]:
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
            from apps.web.llm_credentials import canonical_model_endpoint_id

            try:
                endpoint_id = canonical_model_endpoint_id(
                    raw.get("endpoint_id") or raw.get("credential_id") or "")
            except ValueError as exc:
                if reject_invalid:
                    raise ValueError(
                        f"llm_profiles.{key}.endpoint_id is invalid") from exc
                endpoint_id = ""
            # base_url is old-read compatibility only. Current writes select an
            # account:<id>; endpoint and key are resolved from that account.
            raw_base = raw.get("base_url")
            base_url = str(raw_base).strip() if isinstance(raw_base, str) else ""
            if reject_invalid and base_url:
                raise ValueError(
                    f"llm_profiles.{key}.base_url is read-only; select endpoint_id")
            connection = str(
                raw.get("connection")
                or ("endpoint" if endpoint_id else "custom_endpoint" if base_url else "default")
            ).strip().lower()
            if connection == "credential":
                connection = "endpoint"
            if connection not in {"default", "custom_endpoint", "endpoint"}:
                if reject_invalid:
                    raise ValueError(
                        f"llm_profiles.{key}.connection must be endpoint")
                connection = "custom_endpoint" if base_url else "default"
            if endpoint_id:
                connection = "endpoint"
                base_url = ""
            elif connection == "custom_endpoint" and not base_url:
                if reject_invalid:
                    raise ValueError(
                        f"llm_profiles.{key}.endpoint_id is required")
                connection = "default"
            if connection == "default":
                base_url = ""
            if not model:
                if reject_invalid:
                    raise ValueError(f"llm_profiles.{key}.model must be non-empty")
                model = out[key]["model"]
            temperature_mode, temperature = normalize_llm_temperature(
                raw.get("temperature_mode"),
                raw.get("temperature"),
                reject_invalid=reject_invalid,
                field=f"llm_profiles.{key}.temperature",
            )
            out[key] = {
                "provider": provider or out[key]["provider"],
                "model": model,
                "endpoint_id": endpoint_id,
                "base_url": base_url,
                "connection": connection,
                "temperature_mode": temperature_mode,
                "temperature": temperature,
            }
            migration = (
                self._data.get("_llm_credential_migrations") or {}
            ).get(key)
            if isinstance(migration, dict):
                out[key]["credential_migration"] = copy.deepcopy(migration)
        return out

    @staticmethod
    def _clean_stage_policy(value: Any, defaults: dict[str, Any]) -> dict[str, Any]:
        if not isinstance(value, dict):
            value = {}
        raw_coordinator = value.get("coordinator")
        coordinator = raw_coordinator if isinstance(raw_coordinator, dict) else {}
        dispatch_mode = str(
            coordinator.get("dispatch_mode") or "fixed"
        ).strip().lower()
        if dispatch_mode not in {"fixed", "auto"}:
            dispatch_mode = "fixed"
        race_timeout = int(value.get("race", {}).get("timeout")
                           or defaults.get("race_timeout") or DEFAULT_RACE_TIMEOUT)
        race_enabled = dispatch_mode == "fixed"
        raw_race_engines = value.get("race", {}).get("engines")
        race_engines = raw_race_engines or defaults.get("race_engines") or []
        wall = int(coordinator.get(
            "wall_clock_budget", defaults.get("wall_clock_budget", 0)) or 0)
        max_workers = int(value.get("budgets", {}).get(
            "max_total_workers", defaults.get("max_total_workers", 0)) or 0)
        cost = float(value.get("budgets", {}).get(
            "cost_budget_usd", defaults.get("cost_budget_usd", 0.0)) or 0.0)
        raw_review = coordinator.get("review")
        review = dict(DEFAULT_REVIEW_POLICY)
        if isinstance(raw_review, dict):
            review["enabled"] = bool(raw_review.get("enabled", review["enabled"]))
            review["engine"] = str(raw_review.get("engine") or review["engine"]).strip()
            review_effort = str(
                raw_review.get("reasoning_effort") or "inherit").strip().lower()
            if review_effort in {
                "inherit", "default", "none", "minimal", "low", "medium",
                "high", "xhigh", "max",
            }:
                review["reasoning_effort"] = review_effort
            for key in (
                "after_fruitless_workers", "after_duplicate_intents",
                "every_completed_workers", "candidate_spike_threshold",
                "max_concurrent", "cooldown_events", "timeout", "max_review_workers",
            ):
                if raw_review.get(key) is not None:
                    review[key] = WorkerConfigStore._coerce_nonneg_int(
                        raw_review.get(key), int(review[key]))
            review["timeout"] = max(30, min(90, int(review["timeout"])))
            for key in (
                "after_race", "on_course_correct",
                "on_candidate_spike", "on_operator_hint", "on_evidence_conflict",
                "on_semantic_duplicate", "allow_review_fallback",
            ):
                if raw_review.get(key) is not None:
                    review[key] = bool(raw_review.get(key))
        # Review is an evidence-driven audit lane. Legacy persisted settings may
        # still request post-race, periodic, course-change, or hint-triggered
        # reviews; normalize those expensive triggers out of the effective policy.
        review.update({
            "after_race": False,
            "on_course_correct": False,
            "on_operator_hint": False,
            "on_semantic_duplicate": False,
            "every_completed_workers": 0,
            "max_concurrent": 1,
        })
        raw_verifier = coordinator.get("verifier")
        verifier = dict(DEFAULT_VERIFIER_POLICY)
        if isinstance(raw_verifier, dict):
            verifier["enabled"] = bool(raw_verifier.get("enabled", verifier["enabled"]))
            verifier["engine"] = str(raw_verifier.get("engine") or verifier["engine"]).strip()
            verifier_effort = str(
                raw_verifier.get("reasoning_effort") or "inherit").strip().lower()
            if verifier_effort in {
                "inherit", "default", "none", "minimal", "low", "medium",
                "high", "xhigh", "max",
            }:
                verifier["reasoning_effort"] = verifier_effort
            for key in ("max_concurrent", "timeout", "max_verifier_workers"):
                if raw_verifier.get(key) is not None:
                    verifier[key] = WorkerConfigStore._coerce_nonneg_int(
                        raw_verifier.get(key), int(verifier[key]))
            if raw_verifier.get("allow_verifier_fallback") is not None:
                verifier["allow_verifier_fallback"] = bool(
                    raw_verifier.get("allow_verifier_fallback"))
        return {
            "prepare": dict(value.get("prepare") or {}),
            "race": {"enabled": race_enabled, "timeout": race_timeout,
                     "engines": list(race_engines or [])},
            "coordinator": {"dispatch_mode": dispatch_mode,
                            "wall_clock_budget": wall, "review": review,
                            "verifier": verifier},
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
    def _clean_worker_network(value: Any) -> str:
        if isinstance(value, str) and value in VALID_WORKER_NETWORKS:
            return value
        return DEFAULT_WORKER_NETWORK

    @staticmethod
    def _require_worker_network(value: Any) -> str:
        if isinstance(value, str) and value in VALID_WORKER_NETWORKS:
            # Surface compose / none conflicts at save time (#171 / MNT-09.04).
            from muteki.solver.container_exec import (
                WorkerNetworkConfigError,
                project_worker_network,
            )
            try:
                project_worker_network(value)
            except WorkerNetworkConfigError as exc:
                raise ValueError(str(exc)) from exc
            return value
        raise ValueError("worker_network must be bridge, host, or none")

    @staticmethod
    def _clean_container_scope(value: Any) -> str:
        return value if isinstance(value, str) and value in {"run", "shared"} else "run"

    @staticmethod
    def _require_container_scope(value: Any) -> str:
        if not isinstance(value, str) or value not in {"run", "shared"}:
            raise ValueError("worker_container_scope must be run or shared")
        return value

    @staticmethod
    def _clean_worker_privilege(value: Any) -> str:
        if isinstance(value, str) and value in {"default", "elevated"}:
            return value
        return "default"

    @staticmethod
    def _require_worker_privilege(value: Any) -> str:
        if isinstance(value, str) and value in {"default", "elevated"}:
            return value
        raise ValueError("worker_privilege must be default or elevated")

    @staticmethod
    def _clean_worker_profiles(value: Any, *, reject_invalid: bool = False) -> list[dict[str, Any]]:
        if reject_invalid and isinstance(value, list):
            value = [
                WorkerConfigStore._clean_worker_capacity(row, f"worker_profiles[{index}]")
                for index, row in enumerate(value)
            ]
        return normalize_worker_profiles(
            value,
            defaults=DEFAULT_WORKER_PROFILES,
            reject_invalid=reject_invalid,
        )
