"""Runtime-environment detection shared by the swarm and the web app.

P2-v3 (unified containerization): when the WEB control plane itself runs inside a
container, worker_backend MUST be "container" — host-native agent CLIs cannot run
there, and a silent fallback to local would spawn the CLI inside the web container
(no tools, wrong credentials, broken isolation). So every place that could degrade
container→local, or that lets the operator pick "local", consults is_web_container()
and HARD-FAILS / refuses instead of falling back.

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
