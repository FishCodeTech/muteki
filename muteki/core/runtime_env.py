"""Runtime-environment detection shared by the swarm and the web app.

The default sibling deployment keeps Worker CLIs in separate containers.
MUTEKI_CONTAINER_MODE=single explicitly selects a self-contained image that has
the controller, Worker CLIs and tools together; Workers use local subprocesses.
Container detection remains factual in both deployments. This mode is a deployment
choice, never a fallback when Docker is unavailable.

Detection order (most authoritative first):
  1. MUTEKI_IN_CONTAINER — explicit, set by docker-compose. Wins outright (incl. an
     explicit "0"/"false" to force-disable, e.g. for a test that runs under Docker
     but wants host semantics).
  2. /.dockerenv — present in virtually every Docker container.
  3. /proc/1/cgroup mentions docker/containerd/kubepods — Linux cgroup fallback.

This is about the CO-ORDINATOR's own environment (the web/api process), NOT the
worker containers it launches as siblings.
"""

from __future__ import annotations

import os

_TRUE = {"1", "true", "yes", "on"}
_FALSE = {"0", "false", "no", "off", ""}


def worker_container_mode() -> str:
    """Return the explicitly selected container deployment (default: sibling)."""
    mode = os.environ.get("MUTEKI_CONTAINER_MODE", "sibling").strip().lower()
    if mode not in {"sibling", "single"}:
        raise ValueError("MUTEKI_CONTAINER_MODE must be sibling or single")
    return mode


def is_single_container() -> bool:
    """Whether this controller explicitly shares its container with Workers."""
    return worker_container_mode() == "single" and is_web_container()


def is_web_container() -> bool:
    """True if THIS process (the coordinator / web-api) is running inside a container.

    Cheap and side-effect-free; safe to call on hot paths. Re-reads the environment
    each call so a test can monkeypatch MUTEKI_IN_CONTAINER without reimporting.
    """
    explicit = os.environ.get("MUTEKI_IN_CONTAINER")
    if explicit is not None:
        v = explicit.strip().lower()
        if v in _TRUE:
            return True
        if v in _FALSE:
            return False
        # any other non-empty value is treated as truthy (defensive)
        return True
    # No explicit flag → sniff the filesystem.
    try:
        if os.path.exists("/.dockerenv"):
            return True
    except OSError:
        pass
    try:
        with open("/proc/1/cgroup", "r", encoding="utf-8", errors="replace") as fh:
            blob = fh.read()
        if any(tok in blob for tok in ("docker", "containerd", "kubepods")):
            return True
    except OSError:
        pass
    return False


def native_user_environment(env: dict[str, str] | None = None) -> dict[str, str]:
    """Normalize native launch state without changing explicit account overlays."""
    from pathlib import Path

    source = dict(os.environ if env is None else env)
    if source.get("MUTEKI_HOST_DISCOVERY", os.environ.get("MUTEKI_HOST_DISCOVERY")) == "0":
        return source
    if os.name != "posix":
        return source
    import pwd
    account = pwd.getpwuid(os.getuid())
    home = Path(account.pw_dir).resolve()
    # A supplied HOME denotes a complete inherited environment. An overlay
    # without HOME may intentionally select a registered account's native root.
    inherited = source if env is None or "HOME" in source else os.environ
    homes = [Path(inherited[key]).resolve() for key in ("HOME", "USERPROFILE") if inherited.get(key)]
    private_roots = [value for value in homes if value != home]
    if inherited.get("MUTEKI_CHAT_PRIVATE_ROOT"):
        private_roots.append(Path(inherited["MUTEKI_CHAT_PRIVATE_ROOT"]).resolve())
    trusted = not private_roots

    def managed_path(value: str) -> bool:
        path = Path(value).resolve()
        return (any(path == root or path.is_relative_to(root) for root in private_roots)
                or any(part in {"_platform", ".muteki-opencode", "muteki_accounts"} for part in path.parts))

    overlay = env is not None and "HOME" not in source
    source.update(HOME=str(home), USERPROFILE=str(home), USER=account.pw_name, LOGNAME=account.pw_name)
    for key, relative in {"XDG_CONFIG_HOME": ".config", "XDG_CACHE_HOME": ".cache", "XDG_DATA_HOME": ".local/share"}.items():
        value = source.get(key) if overlay and key in source else inherited.get(key)
        source[key] = value if value and ((overlay and key in source) or (trusted and not managed_path(value))) else str(home / relative)
    for key in ("CODEX_HOME", "CLAUDE_CONFIG_DIR", "CURSOR_CONFIG_DIR", "GROK_HOME", "KIMI_CODE_HOME", "PI_CODING_AGENT_DIR"):
        if overlay and key in source:
            continue
        value = inherited.get(key)
        # Empty overlays also shadow poisoned keys when callers merge os.environ.
        if not trusted or (value and managed_path(value)):
            source[key] = ""
        elif value:
            source[key] = value
    path_value = source.get("PATH") if overlay and "PATH" in source else inherited.get("PATH")
    if path_value:
        source["PATH"] = os.pathsep.join(value for value in path_value.split(os.pathsep) if value and not managed_path(value))
    source["MUTEKI_CHAT_PRIVATE_ROOT"] = ""
    return source
