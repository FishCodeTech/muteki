"""Identity model — THE canonical Credential / Seat / Environment schema.

This replaces the old flat "worker_profile" dict (which conflated a credential,
an execution environment, and a scheduling unit into one object with redundant
fields like `auth`==`credential_mode` and `id`==`name`, and leaked implementation
details into ids like `claude-sub-container`).

Three orthogonal first-class objects (DESIGN: plan_settings_identity_refactor.md):

  Credential  — a SECRET source. Three kinds (§3.7 matrix):
      system_inherit  — inherit the host's logged-in CLI state (claude Keychain /
                        codex ~/.codex / cursor host login). NO stored secret.
                        Forbidden in container backend (host login isn't mounted).
      engine_key      — the engine's own official credential, three on-disk forms:
                        claude CLAUDE_CODE_OAUTH_TOKEN (set token) /
                        cursor CURSOR_API_KEY / codex codex-home/auth.json.
      custom_endpoint — point an engine at a third-party OpenAI/Anthropic-compatible
                        endpoint (DeepSeek/Kimi/GLM): base_url + key + target_engine.
  Seat        — an out-the-door scheduling unit (UI label: "Agent"). References a
                Credential + Environment by id, pins a `model`, owns roles/capacity.
  Environment — an execution template: backend (local|container) + container limits.

Ids are SEMI-READABLE + STABLE: `cred_<engine>_<6hex>`, `seat_<engine>_<6hex>`.
The 6hex is deterministic (sha1 of the legacy name) so a migration re-run yields
the SAME id, and the legacy name maps to it via the alias table.

Storage: these objects ARE the on-disk `_worker_config.json` shape (credentials[],
seats[], environments[]). The swarm/drivers still consume a flat legacy-profile
dict — `seat_to_legacy_profile()` (in cli_driver) adapts new→old at the boundary,
so the scheduler and drivers need no change.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass, field
from typing import Any, Literal

from muteki.solver.worker_profiles import (
    VALID_BASE_ENGINES,
    DEFAULT_ROLES,
    base_engine_for_profile,
    coerce_nonneg_int,
    coerce_pos_int,
)

CredentialKind = Literal["system_inherit", "engine_key", "custom_endpoint"]
Backend = Literal["local", "container"]

# Legacy credential-account `mode` (from CredentialAccountStore.inspect) → new kind.
# subscription_token/chatgpt_auth_home/api_key are all the engine's own official
# credential, three on-disk forms → one kind `engine_key`; the engine field keeps
# them distinct. `custom_endpoint` maps through. `empty` is a missing credential,
# not a kind.
_MODE_TO_KIND: dict[str, str] = {
    "subscription_token": "engine_key",
    "chatgpt_auth_home": "engine_key",
    "api_key": "engine_key",
    "custom_endpoint": "custom_endpoint",
}


import re as _re

# a well-formed new id: seat_<engine>_<6hex> / cred_<engine>_<6hex>. Used to make
# migration IDEMPOTENT — re-migrating an already-migrated config (the v2 frontend
# saves in legacy shape every time) must NOT re-hash a seat id into a new one, or
# the foreign keys (engines[]/review.engine) that reference the first-gen id break.
_SEAT_ID_RE = _re.compile(r"^seat_[a-z0-9]+_[0-9a-f]{6}$")
_CRED_ID_RE = _re.compile(r"^cred_[a-z0-9]+_[0-9a-f]{6}$")


def _short_hash(seed: str) -> str:
    """Deterministic 6-hex tag from a seed string (stable across migration runs)."""
    return hashlib.sha1(seed.encode("utf-8")).hexdigest()[:6]


def credential_id_for(engine: str, *, legacy_account_id: str) -> str:
    """`cred_<engine>_<6hex>` — deterministic from the legacy account id.
    Idempotent: an already-formed cred id passes through unchanged."""
    if _CRED_ID_RE.match(str(legacy_account_id or "")):
        return str(legacy_account_id)
    e = (engine or "unknown").strip().lower() or "unknown"
    return f"cred_{e}_{_short_hash('cred:' + str(legacy_account_id))}"


def seat_id_for(engine: str, *, legacy_name: str) -> str:
    """`seat_<engine>_<6hex>` — deterministic from the legacy profile name.
    Idempotent: an already-formed seat id passes through unchanged."""
    if _SEAT_ID_RE.match(str(legacy_name or "")):
        return str(legacy_name)
    e = (engine or "unknown").strip().lower() or "unknown"
    return f"seat_{e}_{_short_hash('seat:' + str(legacy_name))}"


@dataclass(frozen=True)
class Credential:
    id: str
    label: str
    engine: str                       # claude | codex | cursor
    kind: str                         # system_inherit | engine_key | custom_endpoint
    secret_ref: str = ""              # storage handle (account_id); "" for system_inherit
    target_engine: str = ""           # custom_endpoint only
    base_url: str = ""                # custom_endpoint only
    wire_api: str = ""                # custom_endpoint only (codex: responses; else "")
    updated_at: float | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "id": self.id, "label": self.label, "engine": self.engine,
            "kind": self.kind, "secret_ref": self.secret_ref,
        }
        if self.kind == "custom_endpoint":
            d["endpoint"] = {"base_url": self.base_url, "wire_api": self.wire_api}
            d["target_engine"] = self.target_engine
        if self.updated_at is not None:
            d["updated_at"] = self.updated_at
        return d


@dataclass(frozen=True)
class Environment:
    id: str
    label: str
    backend: str                      # local | container
    network: str = ""
    memory: str = ""
    cpus: str = ""
    pids_limit: int = 0

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"id": self.id, "label": self.label, "backend": self.backend}
        for k in ("network", "memory", "cpus"):
            v = getattr(self, k)
            if v:
                d[k] = v
        if self.pids_limit:
            d["pids_limit"] = self.pids_limit
        return d


@dataclass(frozen=True)
class Seat:
    id: str
    label: str
    engine: str
    credential_id: str
    environment_id: str
    model: str = ""
    roles: list[str] = field(default_factory=lambda: list(DEFAULT_ROLES))
    race: bool = True
    max_running: int = 1
    max_review_running: int = 0
    priority: int = 100
    enabled: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "label": self.label, "engine": self.engine,
            "credential_id": self.credential_id, "environment_id": self.environment_id,
            "model": self.model, "roles": list(self.roles), "race": self.race,
            "capacity": {
                "max_running": self.max_running,
                "max_review_running": self.max_review_running,
            },
            "priority": self.priority, "enabled": self.enabled,
        }


def kind_from_mode(mode: str) -> str:
    """Map a legacy credential-account `mode` to the new `kind`. Unknown → engine_key
    (a present-but-unrecognized credential is still the engine's own key, not an
    endpoint). `empty`/`unknown` callers should treat as missing, not call this."""
    return _MODE_TO_KIND.get((mode or "").strip(), "engine_key")


def is_legal_combo(*, kind: str, backend: str) -> bool:
    """§3.7 hard constraint: container backend forbids system_inherit (the host
    login is not mounted into the container, so there is nothing to inherit)."""
    if backend == "container" and kind == "system_inherit":
        return False
    return True


# ── migration: legacy worker_profiles/runtime_profiles → new identity model ──

@dataclass
class MigrationResult:
    credentials: list[Credential]
    seats: list[Seat]
    environments: list[Environment]
    # legacy ref (old profile name / hyphen canonical alias) → new seat id.
    seat_alias: dict[str, str]
    # legacy account id → new credential id.
    credential_alias: dict[str, str]


def _legacy_kind(profile: dict[str, Any]) -> str:
    """Infer the new credential kind for a legacy profile.

    A legacy profile carried `credential_mode`/`auth` (subscription|api_key|...)
    plus optional base_url. The mapping:
      - base_url present                         → custom_endpoint
      - credential_mode in keyed set             → engine_key
      - subscription with NO bound account       → system_inherit (host login)
      - subscription WITH a bound account         → engine_key (the stored token)
    """
    mode = str(profile.get("credential_mode") or profile.get("auth") or "subscription").strip()
    if str(profile.get("base_url") or "").strip():
        return "custom_endpoint"
    if mode in {"api", "api_key", "oauth_token"}:
        return "engine_key"
    # subscription: engine_key if a credential account is bound, else system_inherit.
    acct = str(profile.get("credential_account") or "").strip()
    return "engine_key" if acct else "system_inherit"


def migrate_legacy_config(
    *,
    worker_profiles: list[dict[str, Any]],
    runtime_profiles: list[dict[str, Any]],
    account_modes: dict[str, str] | None = None,
) -> MigrationResult:
    """Pure transform: legacy config → new identity model + alias tables.

    `account_modes` maps a legacy account id → its inspected `mode` (so a bound
    account's kind comes from what's actually on disk, not just the profile's
    declared credential_mode). Optional; falls back to the profile's own kind.
    NEVER raises — a malformed entry is skipped.
    """
    account_modes = account_modes or {}
    creds: dict[str, Credential] = {}
    cred_alias: dict[str, str] = {}
    seats: list[Seat] = []
    seat_alias: dict[str, str] = {}

    # 1) environments ← runtime_profiles (id reused verbatim; it's already stable).
    envs: list[Environment] = []
    for rt in runtime_profiles:
        if not isinstance(rt, dict) or not rt.get("id"):
            continue
        envs.append(Environment(
            id=str(rt["id"]), label=str(rt.get("label") or rt["id"]),
            backend="container" if str(rt.get("backend")) == "container" else "local",
            network=str(rt.get("network") or ""), memory=str(rt.get("memory") or ""),
            cpus=str(rt.get("cpus") or ""),
            pids_limit=coerce_nonneg_int(rt.get("pids_limit"), 0),
        ))

    def _ensure_credential(engine: str, profile: dict[str, Any]) -> str:
        """Synthesize/return a Credential for this profile, return its new id.

        Resolution mirrors the backend's real fallback (profile_health.py:159):
        an empty `credential_account` falls back to the default `<engine>-main`
        account, and if that account is PRESENT on disk it's a real engine_key —
        NOT system_inherit. Only when even the default has no on-disk credential
        do we treat a subscription profile as host-inherit.
        """
        legacy_acct = str(profile.get("credential_account") or "").strip()
        kind = _legacy_kind(profile)
        default_acct = f"{engine}-main"
        default_present = (
            default_acct in account_modes and account_modes[default_acct] not in ("", "empty")
        )

        if kind == "custom_endpoint":
            seed_acct = legacy_acct or f"{engine}-endpoint"
        elif legacy_acct:
            seed_acct = legacy_acct
        elif default_present:
            # empty binding but the default account exists on disk → bind to it as
            # engine_key (the codex auth.json case), matching backend fallback.
            seed_acct = default_acct
            kind = "engine_key"
        else:
            # empty binding AND no default account on disk → host login inherit.
            seed_acct = f"{engine}-host"
            kind = "system_inherit"

        # prefer the on-disk inspected mode for whichever account we resolved.
        disk_mode = account_modes.get(seed_acct) if kind != "system_inherit" else None
        if disk_mode and disk_mode != "empty":
            kind = kind_from_mode(disk_mode)

        cid = credential_id_for(engine, legacy_account_id=seed_acct)
        if legacy_acct:
            cred_alias[legacy_acct] = cid
        # also alias the default account id when we fell back to it, so a foreign
        # key that named the bare engine still resolves.
        if not legacy_acct and kind != "system_inherit":
            cred_alias.setdefault(seed_acct, cid)
        if cid not in creds:
            label = (
                f"{engine} 系统登录" if kind == "system_inherit"
                else (seed_acct if kind == "engine_key" else f"{engine} 自定义端点")
            )
            creds[cid] = Credential(
                id=cid, label=label, engine=engine, kind=kind,
                secret_ref=("" if kind == "system_inherit" else seed_acct),
                target_engine=(engine if kind == "custom_endpoint" else ""),
                base_url=str(profile.get("base_url") or "").strip(),
                wire_api=str(profile.get("wire_api") or "").strip() if kind == "custom_endpoint" else "",
            )
        return cid

    # 2) seats ← worker_profiles.
    for p in worker_profiles:
        if not isinstance(p, dict):
            continue
        engine = base_engine_for_profile(p)
        if engine not in VALID_BASE_ENGINES:
            continue
        legacy_name = str(p.get("name") or p.get("id") or "").strip()
        if not legacy_name:
            continue
        sid = seat_id_for(engine, legacy_name=legacy_name)
        cid = _ensure_credential(engine, p)
        env_id = str(p.get("runtime") or "docker-web").strip()
        roles = [str(r).strip() for r in (p.get("roles") or []) if str(r).strip()] or list(DEFAULT_ROLES)
        cap = p.get("capacity") if isinstance(p.get("capacity"), dict) else {}
        # label survives re-migration: prefer an explicit label, else the legacy
        # name — but never let the label BE the seat id (which happens on the 2nd
        # migration when name is already an id); fall back to "<engine> worker".
        label = str(p.get("label") or "").strip()
        if not label:
            label = legacy_name if not _SEAT_ID_RE.match(legacy_name) else f"{engine} worker"
        seats.append(Seat(
            id=sid, label=label, engine=engine,
            credential_id=cid, environment_id=env_id,
            model=str(p.get("model") or "").strip(),
            roles=roles,
            race=bool(p.get("race", "race" in roles)),
            max_running=coerce_pos_int(p.get("max_running", cap.get("max_running")), 1),
            max_review_running=coerce_nonneg_int(p.get("max_review_running", cap.get("max_review_running")), 0),
            priority=coerce_nonneg_int(p.get("priority"), 100),
            enabled=bool(p.get("enabled", True)),
        ))
        # alias: legacy name → new seat id (the primary foreign-key key).
        seat_alias[legacy_name] = sid

    return MigrationResult(
        credentials=list(creds.values()), seats=seats, environments=envs,
        seat_alias=seat_alias, credential_alias=cred_alias,
    )


# ── adapter: new identity model → flat legacy profile dict (plan §5.0(c)) ─────
#
# The swarm scheduler + cli drivers still consume a flat "worker_profile" dict
# (engine/model/credential_account/credential_mode/base_url/wire_api/roles/...).
# Rather than rewrite every consumer, we adapt new→old at the boundary so the
# scheduler and ProfileDriver/EndpointDriver need ZERO change.

# new credential kind → the legacy `credential_mode` the drivers branch on.
_KIND_TO_LEGACY_MODE = {
    "system_inherit": "subscription",
    "engine_key": "api_key",          # a stored official key (driver uses *_FILE env)
    "custom_endpoint": "api_key",
}


def seat_to_legacy_profile(
    seat: dict[str, Any],
    credential: dict[str, Any] | None,
    environment: dict[str, Any] | None,
) -> dict[str, Any]:
    """Flatten a Seat (+ its Credential + Environment) into the flat profile dict
    the existing scheduler/drivers expect. Pure; never raises.

    Credential mode mapping is intentionally faithful to old behavior:
      - system_inherit → "subscription" + EMPTY credential_account (the host-login
        path: runtime_env_for_engine injects nothing, the CLI inherits host login).
      - engine_key     → the stored account id in `credential_account`; mode
        "subscription" for claude/codex official tokens, "api_key" for cursor —
        but the driver only really needs base_url/account, so we keep it simple.
      - custom_endpoint → base_url + wire_api on the profile so EndpointDriver and
        runtime_env_for_engine wire the third-party endpoint.
    """
    seat = seat or {}
    credential = credential or {}
    environment = environment or {}
    engine = str(seat.get("engine") or credential.get("engine") or "").strip()
    kind = str(credential.get("kind") or "system_inherit").strip()
    cap = seat.get("capacity") if isinstance(seat.get("capacity"), dict) else {}

    endpoint = credential.get("endpoint") if isinstance(credential.get("endpoint"), dict) else {}
    base_url = str(endpoint.get("base_url") or "").strip()
    wire_api = str(endpoint.get("wire_api") or "").strip()

    # system_inherit ⇒ no stored account (inherit host login). Otherwise the
    # credential's secret_ref IS the legacy account id the driver/env injection reads.
    if kind == "system_inherit":
        credential_account = ""
    else:
        credential_account = str(credential.get("secret_ref") or "").strip()

    credential_mode = _KIND_TO_LEGACY_MODE.get(kind, "subscription")
    roles = [str(r).strip() for r in (seat.get("roles") or []) if str(r).strip()] or list(DEFAULT_ROLES)

    return {
        "id": str(seat.get("id") or ""),
        "name": str(seat.get("id") or ""),          # drivers key off id; label is UI-only
        "label": str(seat.get("label") or ""),      # carried so re-migration keeps it
        "engine": engine,
        "transport": engine,
        "credential_mode": credential_mode,
        "auth": credential_mode,
        "credential_account": credential_account,
        "api_key_ref": "",
        "base_url": base_url,
        "wire_api": wire_api or ("responses" if engine == "codex" and base_url else ""),
        "runtime": str(seat.get("environment_id") or environment.get("id") or "docker-web").strip(),
        "roles": roles,
        "race": bool(seat.get("race", "race" in roles)),
        "max_running": coerce_pos_int(seat.get("max_running", cap.get("max_running")), 1),
        "max_review_running": coerce_nonneg_int(seat.get("max_review_running", cap.get("max_review_running")), 0),
        "priority": coerce_nonneg_int(seat.get("priority"), 100),
        "model": str(seat.get("model") or "").strip(),
        "enabled": bool(seat.get("enabled", True)),
    }


def seats_to_legacy_profiles(
    seats: list[dict[str, Any]],
    credentials: list[dict[str, Any]],
    environments: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    """Adapt a whole identity model into the legacy worker_profiles list."""
    cred_by_id = {str(c.get("id")): c for c in credentials if isinstance(c, dict)}
    env_by_id = {str(e.get("id")): e for e in environments if isinstance(e, dict)}
    out: list[dict[str, Any]] = []
    for s in seats:
        if not isinstance(s, dict):
            continue
        out.append(seat_to_legacy_profile(
            s, cred_by_id.get(str(s.get("credential_id"))),
            env_by_id.get(str(s.get("environment_id"))),
        ))
    return out
