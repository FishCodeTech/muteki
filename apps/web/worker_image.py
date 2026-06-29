"""Worker-image health for the settings page (P2-v3).

Four checks, surfaced to the UI as green / yellow / red:
  - daemon : is the docker daemon reachable (socket works)?
  - pulled : is the configured worker image present locally (docker image inspect)?
  - version: does the pulled image's OCI version label match what the app expects?
  - pull   : a one-click `docker pull` action (separate POST endpoint).

The image name is container_exec.WORKER_IMAGE (env MUTEKI_WORKER_IMAGE). The
expected version is env MUTEKI_WORKER_IMAGE_VERSION (unset → version check is
informational only: report the image's own version, status "unknown" not "red").

All docker calls go through subprocess with encoding="utf-8", errors="replace"
(P2-v3 Windows/codepage safety) and short timeouts so the settings page never
hangs on a wedged daemon.
"""

from __future__ import annotations

import json
import os
import subprocess
from typing import Any

from muteki.solver.container_exec import WORKER_IMAGE

# Expected worker-image version, set by the deployment (compose). Unset → the
# version check is informational (we report what's pulled, don't flag a mismatch).
EXPECTED_WORKER_VERSION = os.environ.get("MUTEKI_WORKER_IMAGE_VERSION", "").strip()

_VERSION_LABEL = "org.opencontainers.image.version"


def _docker(*args: str, timeout: float = 20.0) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["docker", *args],
        capture_output=True, text=True,
        encoding="utf-8", errors="replace",
        timeout=timeout,
    )


def _daemon_ok() -> tuple[bool, str]:
    """True if the docker daemon answers. `docker version --format {{.Server.Version}}`
    fails fast when the socket is missing/unreachable."""
    try:
        r = _docker("version", "--format", "{{.Server.Version}}", timeout=10.0)
    except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
        return False, f"docker CLI/daemon unreachable: {exc}"
    if r.returncode != 0:
        return False, (r.stderr or r.stdout or "docker daemon unreachable").strip()[:200]
    return True, r.stdout.strip()


def _image_version(image: str) -> "str | None":
    """The OCI version label of a locally-present image, or None if not pulled /
    no label."""
    try:
        r = _docker("image", "inspect", image,
                    "--format", "{{ index .Config.Labels \"" + _VERSION_LABEL + "\" }}",
                    timeout=15.0)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    if r.returncode != 0:
        return None
    v = (r.stdout or "").strip()
    # docker prints "<no value>" when the label is absent
    return v if v and v != "<no value>" else None


def _image_present(image: str) -> bool:
    try:
        r = _docker("image", "inspect", image, "--format", "{{.Id}}", timeout=15.0)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return False
    return r.returncode == 0


def image_status() -> dict[str, Any]:
    """Full worker-image health for the settings page."""
    image = WORKER_IMAGE
    daemon_ok, daemon_detail = _daemon_ok()
    if not daemon_ok:
        # Without a daemon nothing else is knowable.
        return {
            "image": image,
            "daemon": {"ok": False, "detail": daemon_detail},
            "pulled": {"ok": False, "detail": "docker daemon unreachable"},
            "version": {"status": "unknown", "expected": EXPECTED_WORKER_VERSION or None,
                        "actual": None, "detail": "docker daemon unreachable"},
            "overall": "red",
        }

    pulled = _image_present(image)
    actual_version = _image_version(image) if pulled else None

    if not EXPECTED_WORKER_VERSION:
        version_status = "unknown"          # informational only
        version_detail = "no expected version configured (MUTEKI_WORKER_IMAGE_VERSION)"
    elif not pulled:
        version_status = "unknown"
        version_detail = "image not pulled"
    elif actual_version == EXPECTED_WORKER_VERSION:
        version_status = "match"
        version_detail = ""
    else:
        version_status = "mismatch"
        version_detail = f"pulled {actual_version or '<no label>'} != expected {EXPECTED_WORKER_VERSION}"

    # overall: red if not pulled, yellow if pulled but version mismatch/unknown-
    # with-expected, green if pulled and (no expected version OR matches).
    if not pulled:
        overall = "red"
    elif version_status == "mismatch":
        overall = "yellow"
    else:
        overall = "green"

    return {
        "image": image,
        "daemon": {"ok": True, "detail": daemon_detail},
        "pulled": {"ok": pulled, "detail": "" if pulled else "run pull to fetch it"},
        "version": {"status": version_status, "expected": EXPECTED_WORKER_VERSION or None,
                    "actual": actual_version, "detail": version_detail},
        "overall": overall,
    }


def pull_image() -> dict[str, Any]:
    """One-click `docker pull` of the worker image. Blocking (can take minutes);
    the caller runs it off the event loop. Returns ok + the pulled version."""
    image = WORKER_IMAGE
    daemon_ok, daemon_detail = _daemon_ok()
    if not daemon_ok:
        return {"ok": False, "image": image, "detail": daemon_detail}
    try:
        r = _docker("pull", image, timeout=900.0)
    except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
        return {"ok": False, "image": image, "detail": f"pull failed: {exc}"}
    if r.returncode != 0:
        return {"ok": False, "image": image,
                "detail": (r.stderr or r.stdout or "pull failed").strip()[:300]}
    return {"ok": True, "image": image, "version": _image_version(image),
            "detail": (r.stdout or "").strip().splitlines()[-1] if r.stdout else "pulled"}
