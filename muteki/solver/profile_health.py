"""Profile health — THE single source of truth for "can this worker profile run?".

Both the dispatch precheck (apps/web/drivers.py:_missing_profile_accounts, which
aborts a /start before any container is created) and the settings-page
self-check (apps/web/account_test.py + the /api/settings/profiles[/{id}]/health
endpoints) collapse to thin wrappers over `evaluate_profile_health`. Before this
module the two paths modelled health differently — the settings page asked "does
SOME account exist for this engine?" while dispatch asked "does THIS profile bind
an account, with its pinned model, that actually authenticates?" — so the page
could show green while a run died on `profile_unhealthy`. One kernel, one verdict.

Three explicit depth layers, chosen by the caller (NOT by the backend):

  binding  — local, millisecond, zero network / zero docker: is the profile
             enabled, and (when it requires one) does it bind an account whose
             credential material is actually present? Container mode ALWAYS
             requires an account. Drives the settings badge.
  plumbing — container-only: image present, the projected credential is readable
             by the container uid (#15), the CLI launches. A real `docker run`,
             but it NEVER spends quota / hits the model API.
  auth     — a real one-turn hello, using the profile's PINNED model and the
             account's resolved credential env. The ONLY layer that catches an
             expired token / 403. Runs as a HOST-LOCAL subprocess regardless of
             backend (dispatch never spins a container just to health-check), so
             the credential env is resolved with container=False ALWAYS — a
             container=True overlay would emit in-container paths (e.g.
             CODEX_HOME=/muteki_accounts/...) that don't exist on the host and
             break the local probe. "auth depth" and "container backend" are
             orthogonal: the backend only decides whether a plumbing layer ALSO
             runs.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal

from muteki.solver.credential_accounts import (
    CredentialAccountStore,
    account_store_root,
    detect_system_login,
    engine_account_id,
    runtime_env_for_engine,
)
from muteki.solver.worker_profiles import (
    base_engine_for_profile,
    profile_uses_endpoint,
)

Depth = Literal["binding", "plumbing", "auth"]
Status = Literal["ok", "blocked", "auth_failed", "disabled"]

_ORDER: dict[str, int] = {"binding": 0, "plumbing": 1, "auth": 2}

# credential modes that authenticate against a key/token (vs. a host CLI login).
_KEYED_MODES = {"api_key", "oauth_token", "api"}


@dataclass(frozen=True)
class ProfileHealth:
    """A profile's complete health verdict. NEVER constructed with a raw bool —
    callers read `status` (and the `ok` convenience for the common gate)."""

    profile_id: str
    engine: str              # base engine (claude/codex/cursor)
    backend: str             # "local" | "container"
    status: Status
    layer: str | None        # which stage failed: binding|image|mount|cli|auth|None
    blocker: str | None      # human-readable reason for a binding failure, else None
    detail: str
    model: str               # the profile's pinned model ("" if none)
    account_id: str
    # SINGLE SOURCE OF TRUTH for the UI's "bound?" question (plan §3.4): the front
    # end MUST read these instead of the literal credential_account field (which
    # caused the "未绑定 vs 已绑定" same-row contradiction).
    #   explicit  — the profile named an account that resolved.
    #   inherited — credential_account was empty; fell back to the default account
    #               (engine-main) or host login. Show "自动: <id>", not "未绑定".
    #   missing   — no usable credential at all (a real block).
    binding_kind: str = "explicit"
    effective_credential_id: str = ""

    @property
    def ok(self) -> bool:
        """True when the profile will not block a run. `disabled` counts as ok —
        dispatch skips disabled profiles, so they never block (mirrors the old
        precheck which only iterated enabled profiles)."""
        return self.status in ("ok", "disabled")


def _requires_account(profile: dict[str, Any], *, backend: str) -> bool:
    """Does this profile NEED a registered credential account to run?

    Lifted verbatim from the old drivers.py:_missing_profile_accounts inline
    check so the binding verdict is byte-for-byte the historical dispatch rule:
      - container backend ALWAYS needs an account (creds injected at runtime),
      - an explicit endpoint/base URL or API-key ref with a keyed mode needs one,
      - a keyed mode with NO usable system login needs one.
    A bare subscription on the host with a present system login does NOT.
    """
    auth = str(profile.get("credential_mode") or profile.get("auth") or "subscription")
    explicit_endpoint = bool(profile.get("base_url") or profile.get("api_key_ref"))
    system_login_ok = (
        backend != "container"
        and not explicit_endpoint
        and detect_system_login(base_engine_for_profile(profile)) == "present"
    )
    return (
        backend == "container"
        or (explicit_endpoint and auth in _KEYED_MODES)
        or (auth in _KEYED_MODES and not system_login_ok)
    )


def needs_auth_probe(profile: dict[str, Any], *, backend: str) -> bool:
    """EXPLICIT INVARIANT (do not collapse to just `_requires_account`).

    Old dispatch probed a profile for independent reasons: it requires an
    account, it uses a keyed credential mode, OR it uses a custom endpoint
    (drivers.py:79). A local api_key profile with a present host login still must
    validate the keyed account it names; a base_url endpoint profile that doesn't
    strictly require a stored account still must be probed too. The settings page
    and dispatch both gate the auth layer on this one predicate.
    """
    auth = str(profile.get("credential_mode") or profile.get("auth") or "subscription")
    return (
        _requires_account(profile, backend=backend)
        or auth in _KEYED_MODES
        or profile_uses_endpoint(profile)
    )


def evaluate_profile_health(
    profile: dict[str, Any],
    *,
    backend: str,
    sessions_root: str | Path,
    depth: Depth = "auth",
) -> ProfileHealth:
    """The single source of truth. NEVER raises — every failure is a verdict."""
    engine = base_engine_for_profile(profile)
    pid = str(profile.get("name") or profile.get("id") or engine)
    model = str(profile.get("model") or "").strip()
    account_id = str(profile.get("credential_account") or "")
    root = account_store_root(sessions_root)

    def mk(status: Status, *, layer: str | None = None,
           blocker: str | None = None, detail: str = "",
           binding_kind: str = "explicit", effective_credential_id: str = "") -> ProfileHealth:
        return ProfileHealth(
            profile_id=pid, engine=engine, backend=backend, status=status,
            layer=layer, blocker=blocker, detail=detail, model=model,
            account_id=account_id,
            binding_kind=binding_kind, effective_credential_id=effective_credential_id,
        )

    # ── layer 1: binding (always) ────────────────────────────────────────────
    # A disabled profile never reaches dispatch, so it cannot block a run. Report
    # it as `disabled` (NOT a green "ok") so the UI can render it explicitly
    # rather than implying it was checked and passed.
    if not profile.get("enabled", True):
        return mk("disabled", detail="profile disabled")

    store = CredentialAccountStore(root)
    requires = _requires_account(profile, backend=backend)
    # An empty credential_account is NOT automatically a failure: the engine's
    # credential env resolves a DEFAULT account ("{engine}-main", e.g. codex's
    # auth.json-backed codex-main) which the container projects too. So when the
    # binding is blank we fall back to that default account id and check ITS
    # presence — only blocking when even the default has no credential material.
    # This is the auth.json case: a subscription worker with no explicit binding
    # still runs if its default account is present (an expired token then surfaces
    # honestly at the auth layer as auth_failed, not as a binding block).
    effective_account_id = account_id or (
        engine_account_id(engine) if requires else ""
    )
    # binding_kind (plan §3.4): explicit when the profile named the account;
    # inherited when it was blank and we fell back to the default/host login.
    bk = "explicit" if account_id else "inherited"
    account = store.inspect(effective_account_id) if effective_account_id else None
    # `not account.present`: a registered account whose directory exists but holds
    # no credential file is reported present=False by inspect(); treat that as a
    # binding failure (the missing credential is surfaced at the cheap layer).
    if requires and (effective_account_id == "" or account is None or not account.present):
        if not effective_account_id:
            why = "未绑定账号"
        elif account is None:
            why = f"账号 {effective_account_id} 未登记"
        else:
            why = f"账号 {effective_account_id} 未登记凭据"
        return mk("blocked", layer="binding", blocker=why, detail=why,
                  binding_kind="missing", effective_credential_id=effective_account_id)

    if _ORDER[depth] < _ORDER["plumbing"]:
        return mk("ok", detail="binding ok",
                  binding_kind=bk, effective_credential_id=effective_account_id)

    # ── layer 2: plumbing (container backend only) ───────────────────────────
    # A real `docker run` that verifies the worker image, credential mount
    # readability and CLI launch — but spends NO quota. Local backend has no
    # container plumbing, so this layer is a no-op there.
    if backend == "container":
        pl = _probe_container_plumbing(engine=engine, account_id=effective_account_id, root=root)
        if not pl[0]:
            return mk("auth_failed", layer=pl[1], detail=pl[2],
                      binding_kind=bk, effective_credential_id=effective_account_id)

    if _ORDER[depth] < _ORDER["auth"]:
        return mk("ok", detail="plumbing ok",
                  binding_kind=bk, effective_credential_id=effective_account_id)

    # ── layer 3: auth (real one-turn hello, host-local, profile-pinned model) ─
    if not needs_auth_probe(profile, backend=backend):
        # e.g. a bare host subscription with a present system login: dispatch
        # never probed it, so neither do we — returning ok keeps the old
        # zero-probe fast path (no latency regression on /start).
        return mk("ok", detail="no auth probe required",
                  binding_kind=bk, effective_credential_id=effective_account_id)

    # When the WEB process itself runs inside a container (compose deploy), the
    # host-local auth probe is impossible: the engine CLI binary (claude/codex/
    # cursor) is NOT installed in the web image — it lives only in the WORKER
    # image. Shelling it here fails with "binary not found on PATH" and aborts
    # the whole run on profile_unhealthy before any worker spawns. Binding +
    # plumbing already proved the credential is present and the worker image
    # launches; the REAL auth happens when the worker container runs. (Custom
    # endpoints additionally have their own HTTP probe via account_test.) So
    # defer auth to the worker instead of false-failing on a missing local CLI.
    from muteki.core.runtime_env import is_web_container
    if is_web_container():
        return mk("ok", detail="auth deferred to worker container",
                  binding_kind=bk, effective_credential_id=effective_account_id)

    from muteki.solver.cli_driver import driver_for  # lazy: avoid import cycle

    # container=False ALWAYS — see module docstring. The probe is a host
    # subprocess; an in-container overlay would point at paths that don't exist
    # on the host.
    overlay = runtime_env_for_engine(
        engine, account_root=root, account_id=effective_account_id or None, container=False
    ).env
    env = {**os.environ, **overlay}
    try:
        # driver_for(<dict>) returns a ProfileDriver/EndpointDriver that pins the
        # profile's model into the hello argv — so a quota-exhausted DEFAULT model
        # can't false-fail a profile that runs on a different model.
        ok, detail = driver_for(profile).health_detail(env=env)
    except Exception as exc:  # noqa: BLE001
        return mk("auth_failed", layer="auth", detail=str(exc)[:160],
                  binding_kind=bk, effective_credential_id=effective_account_id)
    return mk(
        "ok" if ok else "auth_failed",
        layer=None if ok else "auth",
        detail=detail or ("ok" if ok else "unhealthy"),
        binding_kind=bk, effective_credential_id=effective_account_id,
    )


def _probe_container_plumbing(
    *, engine: str, account_id: str, root: Path
) -> tuple[bool, str | None, str]:
    """Container plumbing probe: image present + credential mount readable + CLI
    launches. Returns (ok, layer, detail). NEVER spends model quota.

    The docker mechanics are owned by apps.web.account_test._probe_container; this
    delegates to it and maps its dict result into the (ok, layer, detail) tuple
    the kernel works in, so there is still exactly one docker-run implementation.
    """
    from apps.web.account_test import _probe_container

    res = _probe_container(engine=engine, account_id=account_id, root=root)
    return bool(res.get("ok")), res.get("layer"), str(res.get("detail") or "")
