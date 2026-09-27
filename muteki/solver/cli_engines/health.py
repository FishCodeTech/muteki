"""CLI engine health and login probes. Moved from cli_driver.py."""
from __future__ import annotations

import contextlib as _contextlib
import json
import os
import shutil
import subprocess
import time
from pathlib import Path
from typing import Any, Optional

from muteki.solver.worker_profiles import (
    base_engine_for_profile,
    profile_uses_endpoint,
)

from muteki.solver.cli_engines.base import CliDriver
from muteki.solver.cli_engines.bins import (
    _ENV_OVERRIDE,
    _runs_ok,
    resolve_engine_bin_source,
)
from muteki.solver.cli_engines.registry import DRIVERS, driver_for
from muteki.solver.cli_engines.types import CliResult  # noqa: F401

# Deep auth-level liveness for the engine bar (FE-quota-display). `--version`
# (`available`) only proves the binary runs — it can't catch an expired headless
# auth (e.g. cursor-agent -p → "Authentication required" even though
# `cursor-agent status` shows logged-in). health_detail() shells a real one-turn
# hello, so it's expensive: cache it on its OWN throttle (>= the deck's 60s poll)
# with last-good reuse, exactly like quota. Decorative + never blocks the bar.
_HEALTH_TTL = 55.0
_health_cache: dict = {"ts": 0.0, "data": None}




@_contextlib.contextmanager
def _patched_env(values: "dict[str, str]"):
    """Temporarily overlay os.environ with `values`, restoring on exit."""
    old = {k: os.environ.get(k) for k in values}
    try:
        os.environ.update(values)
        yield
    finally:
        for k, v in old.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v


def _probe_health_with_creds(name: str, drv: "CliDriver",
                             account_root: "Optional[str]") -> "tuple[bool, str]":
    """Run a driver's health_detail() with the engine's DEFAULT-account credential
    env injected (when account_root is known) — so the global probe matches what a
    live worker sees. Critical for cursor: its headless CLI authenticates ONLY via
    CURSOR_API_KEY, so a bare probe falsely reports "Authentication required" and the
    engine bar shows a healthy engine as down. account_root=None → bare probe
    (no account store available, e.g. a TUI/test context)."""
    if account_root is None:
        return drv.health_detail()
    try:
        from muteki.solver.credential_accounts import runtime_env_for_engine
        # Local Codex subscription auth is the host's default CODEX_HOME
        # (~/.codex). A stale persisted codex-main account must not make the engine
        # bar or dispatch preflight report Codex down when the host login works.
        account_id = "" if name == "codex" else None
        env = runtime_env_for_engine(
            name, account_root=account_root, account_id=account_id, container=False).env
    except Exception:
        env = {}
    if not env:
        return drv.health_detail()
    with _patched_env(env):
        return drv.health_detail()


def engine_liveness(account_root: "Optional[str]" = None) -> dict:
    """Best-effort {engine: {healthy: bool, detail: str}} from a DEEP one-turn
    probe, throttled to one real run per _HEALTH_TTL with last-good reuse. This is
    what lets the engine bar show "cursor unavailable: Authentication required"
    instead of a green dot, even when no run is active. NEVER raises / blocks.

    `account_root` (the credential-account store) lets the probe inject each engine's
    default-account auth so cursor (CURSOR_API_KEY-only headless) isn't falsely
    reported down — mirrors the live-worker / _healthy_engines credential path."""
    now = time.time()
    cached = _health_cache.get("data")
    if cached is not None and now - _health_cache["ts"] < _HEALTH_TTL:
        return cached
    out: dict = {}
    for name, drv in DRIVERS.items():
        try:
            healthy, detail = _probe_health_with_creds(name, drv, account_root)
        except Exception as exc:  # noqa: BLE001 — bar must never break
            healthy, detail = False, str(exc)[:160]
        out[name] = {"healthy": bool(healthy), "detail": detail or ""}
    _health_cache["data"] = out
    _health_cache["ts"] = now
    return out


def _claude_oauth() -> "Optional[tuple[str, int]]":
    """(access_token, expires_at_ms) from env / macOS Keychain / creds file.
    Returns None when no credential is found. Used by credential_accounts for
    login detection. Never raises."""
    env_tok = (os.environ.get("CLAUDE_CODE_OAUTH_TOKEN")
               or os.environ.get("ANTHROPIC_AUTH_TOKEN")
               or os.environ.get("ANTHROPIC_API_KEY"))
    if env_tok and env_tok.strip():
        return env_tok.strip(), 0
    raw: Optional[str] = None
    try:
        r = subprocess.run(
            ["security", "find-generic-password", "-s", "Claude Code-credentials", "-w"],
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=5)
        if r.returncode == 0 and r.stdout.strip():
            raw = r.stdout.strip()
    except Exception:
        pass
    if not raw:
        try:
            p = Path.home() / ".claude" / ".credentials.json"
            if p.exists():
                raw = p.read_text()
        except Exception:
            pass
    if not raw:
        return None
    try:
        d = json.loads(raw)
        o = d.get("claudeAiOauth") or d
        tok = o.get("accessToken")
        exp = int(o.get("expiresAt") or 0)
        if tok:
            return tok, exp
    except Exception:
        pass
    return None


def _cursor_session_cookie() -> "Optional[str]":
    """`WorkosCursorSessionToken=<userId>::<JWT>` from the macOS Keychain +
    cli-config, or None. Never raises. (Linux Cursor stores the token elsewhere;
    we only support the Keychain path today → None elsewhere.)"""
    tok: Optional[str] = None
    try:
        r = subprocess.run(
            ["security", "find-generic-password", "-s", "cursor-access-token", "-w"],
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=5)
        if r.returncode == 0 and r.stdout.strip():
            tok = r.stdout.strip()
    except Exception:
        pass
    if not tok:
        return None
    uid: Optional[str] = None
    try:
        cfg = Path.home() / ".cursor" / "cli-config.json"
        if cfg.exists():
            uid = str(json.loads(cfg.read_text()).get("authInfo", {}).get("userId") or "")
    except Exception:
        pass
    if not uid:
        return None
    # cookie value is "<userId>::<JWT>", url-encoded (:: → %3A%3A)
    return f"WorkosCursorSessionToken={uid}%3A%3A{tok}"


def engine_status(account_root: "Optional[str]" = None,
                  backend: str = "local",
                  profiles: "Optional[list[dict[str, Any]]]" = None) -> list[dict]:
    """Cheap per-dispatched-worker status for the deck's always-on engine bar.

    This endpoint is polled by the browser, so it must not spend model tokens. It
    only checks that the configured engine binary can start (`--version`) and
    annotates the selected worker profile/model when available. Two seats that
    share a base engine (two Pi credentials) stay two rows. Token-spending
    model probes live in `/api/engines/health`, the model-test button, and the
    dispatch-time health gate.
    """
    profile_rows = [p for p in (profiles or []) if isinstance(p, dict)]
    if profile_rows:
        # One row per dispatched worker. Two Pi seats with different
        # credentials are two engines on the deck bar, not one collapsed Pi.
        selected: list[tuple[str, dict[str, Any] | None]] = []
        for p in profile_rows:
            name = base_engine_for_profile(p)
            if name in DRIVERS:
                selected.append((name, p))
    else:
        selected = [(name, None) for name in DRIVERS]
    out: list[dict] = []
    for name, profile in selected:
        drv = driver_for(profile) if profile else DRIVERS[name]
        try:
            b = drv.bin
            ok = _runs_ok(b)
        except Exception:
            b, ok = name, False
        row = {
            "engine": name,
            "bin": b,
            "available": ok,
            # None means "not deep-probed by the always-on poll". The frontend only
            # treats explicit False as degraded; run-scoped failures and on-demand
            # checks still surface their concrete reasons.
            "healthy": None,
            "health_detail": "",
        }
        if profile:
            row.update({
                "profile_id": profile.get("id") or "",
                "profile_name": (
                    profile.get("label")
                    or profile.get("name")
                    or profile.get("id")
                    or name
                ),
                "model": str(profile.get("model") or ""),
                "backend": backend,
            })
        out.append(row)
    return out


def engine_health(backend: str = "local",
                  account_root: "Optional[str]" = None,
                  profiles: "Optional[list[dict[str, Any]]]" = None) -> list[dict]:
    """A DEEP per-engine self-check (FE-healthcheck-page). `backend` selects WHAT
    is checked, because local and container exercise different things:

    - "local"     → run each driver's real healthcheck ON THE HOST (claude does a
                    1-turn dry run that exercises the host's default login + auth).
                    Answers "is the host's default CLI healthy?".
    - "container" → `docker run --rm` the worker image and verify each engine's
                    CLI launches INSIDE the container (image present + binary on
                    the container PATH). Answers "can the worker image actually
                    start each engine?". Auth-in-container is account-specific and
                    is covered by the per-account connectivity test, not here.

    When `profiles` is provided for local mode, self-check those configured worker
    profiles instead of the bare engines: that makes the button exercise the same
    credential account and selected model a real worker will use. Returns {engine,
    bin, version, healthy, detail, backend}. On-demand only."""
    if (backend or "").strip() == "container":
        return _engine_health_container()
    profile_rows = [p for p in (profiles or []) if isinstance(p, dict)]
    if profile_rows:
        from muteki.solver.credential_accounts import runtime_env_for_engine

        def _insert_model(argv: list[str], model: str) -> list[str]:
            model = (model or "").strip()
            if not model or "--model" in argv or "-m" in argv:
                return argv
            if "--" in argv:
                idx = argv.index("--")
                return [*argv[:idx], "--model", model, *argv[idx:]]
            if len(argv) <= 1:
                return [*argv, "--model", model]
            return [*argv[:-1], "--model", model, argv[-1]]

        out: list[dict] = []
        for profile in profile_rows:
            name = base_engine_for_profile(profile)
            drv = driver_for(profile)
            b, version, healthy, detail = name, "", False, ""
            try:
                b = drv.bin
                r = subprocess.run([b, "--version"], capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=20)
                raw = (r.stdout or r.stderr or "").strip()
                version = raw.splitlines()[0][:80] if raw else ""
                if r.returncode != 0:
                    detail = "binary not runnable (--version failed)"
                else:
                    account_id = str(profile.get("credential_account") or "").strip()
                    resolved_account_id = account_id if account_id else ""
                    env = runtime_env_for_engine(
                        name,
                        account_root=Path(account_root) if account_root else None,
                        account_id=resolved_account_id,
                        container=False,
                    ).env
                    old = {k: os.environ.get(k) for k in env}
                    try:
                        os.environ.update(env)
                        if profile_uses_endpoint(profile):
                            healthy, detail = drv.health_detail()
                        else:
                            argv = _insert_model(
                                drv._hello_argv(),  # noqa: SLF001 - self-check mirrors driver probe.
                                str(profile.get("model") or ""))
                            if not argv:
                                healthy, detail = False, "driver has no hello probe"
                            else:
                                rr = subprocess.run(
                                    argv, capture_output=True, text=True, encoding="utf-8", errors="replace",
                                    timeout=getattr(drv, "_HELLO_TIMEOUT", 90))
                                healthy = bool(drv._hello_ok(rr))  # noqa: SLF001
                                if not healthy:
                                    tail = (rr.stderr or rr.stdout or "").strip().splitlines()
                                    detail = (f"hello exited {rr.returncode}"
                                              + (f": {tail[-1][:120]}" if tail else ""))
                    finally:
                        for k, v in old.items():
                            if v is None:
                                os.environ.pop(k, None)
                            else:
                                os.environ[k] = v
            except FileNotFoundError:
                detail = "binary not found on PATH"
            except subprocess.TimeoutExpired:
                detail = "probe timed out"
            except Exception as e:  # noqa: BLE001
                detail = str(e)[:160]
            out.append({"engine": name, "profile_id": profile.get("id") or "",
                        "profile_name": profile.get("name") or profile.get("id") or name,
                        "model": str(profile.get("model") or ""),
                        "bin": b, "version": version, "healthy": healthy,
                        "detail": detail, "backend": "local",
                        "bin_source": resolve_engine_bin_source(name),
                        "bin_env": _ENV_OVERRIDE.get(name, "")})
        return out
    out: list[dict] = []
    for name, drv in DRIVERS.items():
        b, version, healthy, detail = name, "", False, ""
        try:
            b = drv.bin
            r = subprocess.run([b, "--version"], capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=20)
            raw = (r.stdout or r.stderr or "").strip()
            version = raw.splitlines()[0][:80] if raw else ""
            if r.returncode != 0:
                detail = "binary not runnable (--version failed)"
            else:
                # deep probe: a real one-turn hello (with one retry on a transient
                # miss). detail names the failure mode so red is actionable, not a
                # blanket "check login / quota". Inject the default-account creds so
                # cursor (CURSOR_API_KEY-only headless) isn't falsely reported down.
                healthy, detail = _probe_health_with_creds(name, drv, account_root)
        except FileNotFoundError:
            detail = "binary not found on PATH"
        except subprocess.TimeoutExpired:
            detail = "probe timed out"
        except Exception as e:  # noqa: BLE001 — surface the message to the operator
            detail = str(e)[:160]
        # bin_source tells the FE whether this path was explicitly pinned (env) or
        # auto-discovered (known-good / path) so it can warn that an unpinned local
        # default may resolve to the wrong version, and point at the env var to fix.
        out.append({"engine": name, "bin": b, "version": version,
                    "healthy": healthy, "detail": detail, "backend": "local",
                    "bin_source": resolve_engine_bin_source(name),
                    "bin_env": _ENV_OVERRIDE.get(name, "")})
    return out


# in-container worker binary per engine (mirrors container_exec._CONTAINER_BIN).
_CONTAINER_ENGINE_BIN = {
    "claude": "claude",
    "codex": "codex",
    "cursor": "/home/kali/.local/bin/cursor-agent",
    "pi": "pi",
    "omp": "/home/kali/.local/bin/omp",
    "kimi": "kimi",
    "grok": "/home/kali/.grok/bin/grok",
    "opencode": "opencode",
}


def _engine_health_container() -> list[dict]:
    """Container self-check: one `docker run --rm` per engine verifying the worker
    image has a launchable CLI. No account/bench mounts — this checks the image +
    binary plumbing only (auth is the per-account test's job)."""

    out: list[dict] = []
    docker = shutil.which("docker")
    # image presence is shared across engines — probe once.
    from muteki.solver.container_exec import WORKER_IMAGE
    image_ok = False
    image_detail = ""
    if not docker:
        image_detail = "docker not found"
    else:
        try:
            r = subprocess.run([docker, "image", "inspect", WORKER_IMAGE],
                               capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=20)
            image_ok = r.returncode == 0
            if not image_ok:
                image_detail = f"image missing: {WORKER_IMAGE}"
        except subprocess.TimeoutExpired:
            image_detail = "docker image inspect timed out"
        except Exception as e:  # noqa: BLE001
            image_detail = str(e)[:120]

    for name in DRIVERS:
        bin_in = _CONTAINER_ENGINE_BIN.get(name, name)
        healthy, version, detail = False, "", ""
        if not image_ok:
            detail = image_detail
        else:
            try:
                r = subprocess.run(
                    # the image ENTRYPOINT is the runtime supervisor (a daemon); a
                    # one-shot self-check must override it with a shell via
                    # --entrypoint, else `-lc <cmd>` becomes args to the supervisor.
                    [docker, "run", "--rm", "--network", "none",
                     "--entrypoint", "bash", WORKER_IMAGE,
                     "-lc", f"{bin_in} --version 2>&1 || echo MUTEKI_CLI_FAIL"],
                    capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=60)
                raw = (r.stdout or "").strip()
                if "MUTEKI_CLI_FAIL" in raw or r.returncode != 0:
                    detail = f"{name} CLI not launchable in container"
                else:
                    healthy = True
                    version = raw.splitlines()[0][:80] if raw else ""
            except subprocess.TimeoutExpired:
                detail = "container probe timed out"
            except Exception as e:  # noqa: BLE001
                detail = str(e)[:120]
        out.append({"engine": name, "bin": bin_in, "version": version,
                    "healthy": healthy, "detail": detail, "backend": "container"})
    return out
