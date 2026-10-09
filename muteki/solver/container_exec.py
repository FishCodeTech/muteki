"""Container execution backend — run CLI workers INSIDE a Docker container that
ships the full Kali/CTF toolchain, so workers have a consistent tool environment
regardless of the host.

Architecture: one long-lived container per Run, or an explicitly trusted shared
container for Runs that choose the shared scope.
Inside it, an in-container supervisor (`runtime_agent`, the Runtime Control Plane)
is PID1/ENTRYPOINT and forks workers on demand. It dials an authenticated host
receiver (the `rcp` backend, default).

  - Run scope gives each Run its own container. Shared scope gives all enrolled
    Runs one container, UID, resource budget, credential projection and network.
    Those Runs are mutually trusted; workspace layout is organizational only.
    In rcp mode the supervisor is the container's main process.
  - The run's host workspace is bind-mounted at /home/kali/workspace, so worker
    products survive teardown and sibling workers share workspace artifacts via the
    same volume. A second tiny mount exposes only the reverse-connect bootstrap
    token; a third exposes the credential-account projection. The coordinator's
    event log, graph, control journal and SecretStore are never mounted.
  - A worker is started by asking the supervisor (StartWorker over the socket); the
    supervisor forks it as the kali user, applies a wall-clock cap, and
    streams its stdout/stderr back verbatim. Per-worker control (kill/pause/resume)
    is a Signal op the supervisor routes to that worker's process group — so killing
    or pausing one worker never touches a sibling, and there's no host-side PPID/
    pgid/cmdline-sentinel追溯 to sever (the reason the original docker-exec shared-
    container attempt was fragile; the supervisor owning the PIDs fixes it cleanly).
  - Run-scope teardown removes that Run's container. Shared-scope teardown stops
    the Run's registered workers; the last owner removes the pool container.

LEGACY fallback (`container_dockerexec`): the previous model shelled `docker exec`
per worker from the host into a `sleep infinity` container, with `pkill -f <tag>`
for control. Kept behind MUTEKI_WORKER_BACKEND=container_dockerexec as an emergency
escape hatch (to be removed after the rcp path settles); see `_DockerExecBackend`.

The two public entry points mirror cli_driver.run_cli / run_cli_streaming so the
solver swaps backends with one parameter. The solver's _signal_proc prefers our
proc wrapper's _container_signal (STOP/CONT/KILL).
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import secrets
import shlex
import shutil
import signal as _signal
import subprocess
import threading
import time
import uuid
from muteki.solver.worker_resource_limits import WorkerResourceLimits, resolve_worker_resource_limits
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional, Any

from muteki.solver.cli_driver import (
    CliDriver, CliResult, SecurePromptUnsupported, StreamStep,
    finalize_cli_result,
)
from muteki.solver.cli_launch_check import (
    LAUNCH_CODE_ENVIRONMENT,
    LaunchContractError,
    check_process_launch,
)
from muteki.solver.credential_accounts import CONTAINER_ACCOUNTS_ROOT

# Tool-only worker image. Real credentials are injected from Credential Accounts
# at runtime; do not bake claude/codex/cursor login state into this image.
# One generic worker image (NOT a per-recipe tag), published to Docker Hub so any
# host can `docker pull` it. Default to the moving :latest; override with
# MUTEKI_WORKER_IMAGE to pin a version (e.g. ghcr.io/fishcodetech/muteki-worker:v0.4.2).
WORKER_IMAGE = os.environ.get("MUTEKI_WORKER_IMAGE", "ghcr.io/fishcodetech/muteki-worker:latest")
CONTAINER_WORKSPACE = "/home/kali/workspace"
CONTAINER_CONTROL_DIR = "/run/muteki/control"  # bind-mounted; carries the per-run token
CONTAINER_VPN_CONFIG = "/run/muteki/vpn/client.ovpn"
CONTAINER_VPN_DIGEST = "/run/muteki-openvpn.sha256"
SHARED_RUNTIME_ID = "shared-runtime"


def _shared_runtime_id(bootstrap_root: str) -> str:
    """Keep independent installations from claiming each other's Docker pool."""
    root = str(Path(bootstrap_root).expanduser().resolve())
    return f"{SHARED_RUNTIME_ID}-{hashlib.sha256(root.encode()).hexdigest()[:16]}"


class WorkerNetworkConfigError(ValueError):
    """Requested Docker network cannot be applied without silent privilege widening.

    Raised for #171 / MNT-09.04: never rewrite ``none`` → ``bridge``. Reject
    clearly when RCP/model egress is required under ``none``, or when compose
    ``MUTEKI_WORKER_NETWORK`` conflicts with true offline networking.
    """


_VALID_WORKER_NETWORKS = frozenset({"bridge", "host", "none"})


def _normalize_requested_network(requested: str | None) -> str:
    network = str(requested or "").strip() or "bridge"
    if network not in _VALID_WORKER_NETWORKS:
        raise WorkerNetworkConfigError(
            f"worker_network must be bridge, host, or none; got {network!r}"
        )
    return network


def project_worker_network(requested: str | None = None) -> dict[str, str]:
    """Project requested vs effective Docker network without widening privileges.

    Honesty rules (#171 / MNT-09.04):
    - ``none`` means Docker ``--network none`` and is never rewritten to bridge.
    - Compose ``MUTEKI_WORKER_NETWORK`` remaps only ``bridge`` onto the shared
      network name for service discovery (web-api / lab aliases).
    - Selecting ``none`` while that override is set is rejected: a container
      cannot be truly offline and join the compose network at once.
    - ``host`` is kept as ``host`` (override does not apply).

    Returns ``{"requested", "effective", "reason"}``.
    """
    req = _normalize_requested_network(requested)
    override = (os.environ.get("MUTEKI_WORKER_NETWORK") or "").strip()
    if req == "host":
        return {"requested": "host", "effective": "host", "reason": ""}
    if req == "none":
        if override:
            raise WorkerNetworkConfigError(
                "worker_network=none conflicts with MUTEKI_WORKER_NETWORK="
                f"{override!r}: compose shared network is required for service "
                "discovery, so true Docker --network none cannot be applied. "
                "Choose bridge (joins the compose network) or unset "
                "MUTEKI_WORKER_NETWORK. Disabling WebSearch/WebFetch (offline / "
                "web_access=false) is separate from container network none and "
                "does not isolate shell egress (#171 / MNT-09.04)."
            )
        return {
            "requested": "none",
            "effective": "none",
            "reason": "docker --network none",
        }
    # Only a named bridge network may override the bridge choice.
    if override in {"host", "none"} or override.startswith("container:"):
        raise WorkerNetworkConfigError("MUTEKI_WORKER_NETWORK must name a bridge network; choose host explicitly")
    if override:
        return {
            "requested": "bridge",
            "effective": override,
            "reason": "MUTEKI_WORKER_NETWORK compose shared network",
        }
    return {"requested": "bridge", "effective": "bridge", "reason": ""}


def resolve_worker_run_network(
    requested: str | None = None,
    *,
    needs_egress: bool = True,
) -> str:
    """Return the Docker ``--network`` value, or raise WorkerNetworkConfigError.

    ``none`` is never silently upgraded to bridge. Product paths default
    ``needs_egress=True`` (RCP dial-out / remote model HTTPS) and reject ``none``
    before container create. Pass ``needs_egress=False`` only to project the raw
    Docker mode (tests / future P3-04 offline). Web-tool toggles are not a
    substitute for Docker network isolation (#171 / MNT-09.04).
    """
    projection = project_worker_network(requested)
    if needs_egress and projection["effective"] == "none":
        raise WorkerNetworkConfigError(
            "worker_network=none (Docker --network none) cannot reach the RCP "
            "control receiver or remote model/API endpoints. Refusing to create "
            "the container rather than silently upgrading to bridge. Use "
            "worker_network=bridge (or host) for egress; use offline / "
            "web_access=false to deny WebSearch/WebFetch only — that does not "
            "isolate shell egress (#171 / MNT-09.04). Full shell offline "
            "isolation remains P3-04."
        )
    return projection["effective"]


_RUN_PREFIX = "muteki-run-"
_RUN_ID_LABEL = "io.muteki.run-id-sha256"
LOG = logging.getLogger(__name__)

# Backend selection. "container" (default) → rcp supervisor. "container_dockerexec"
# → legacy host-side `docker exec` (emergency fallback). Anything else (incl unset)
# with a container handle falls back to rcp. The swarm decides local-vs-container;
# this only picks WHICH container transport.
_BACKEND = (os.environ.get("MUTEKI_WORKER_BACKEND") or "").strip().lower()
_USE_DOCKEREXEC = _BACKEND == "container_dockerexec"

# the worker binary INSIDE the container, keyed by engine. The driver resolves
# argv[0] to a HOST absolute path (e.g. /Users/.../.local/bin/claude); inside the
# container that path doesn't exist, so we replace argv[0] with the container path.
# claude/codex live in /usr/local/bin (on the default PATH); cursor-agent installs
# to ~/.local/bin, which is NOT on `docker exec`'s non-login-shell PATH — so it MUST
# be an absolute path or `exec: "cursor-agent": not found in $PATH` (the bug that
# made cursor workers instantly empty-exit in container mode). (The rcp supervisor's
# baseEnv puts ~/.local/bin on PATH too, but we keep the absolute path for parity.)
_CONTAINER_BIN = {
    "claude": "claude",
    "codex": "codex",
    "cursor": "/home/kali/.local/bin/cursor-agent",
    "pi": "pi",
    "omp": "/home/kali/.local/bin/omp",
    "opencode": "opencode",
    "kimi": "kimi",
    "grok": "/home/kali/.grok/bin/grok",
    "droid": "droid",
}

_CONTAINER_OFFLINE_BRIDGE = "/opt/muteki/offline_acp_bridge.py"
_CONTAINER_OMP_OFFLINE_CONFIG = "/opt/muteki/omp_offline_config.yml"
_CONTAINER_KIMI_OFFLINE_AGENT = "/opt/muteki/kimi_offline_agent.md"
_CONTAINER_GROK_OFFLINE_AGENT = "/opt/muteki/grok_offline_agent.md"


# P2-v3 BLOCKER-c: when the coordinator runs INSIDE the web container, a
# `docker run --mount source=<abspath>` is interpreted by the HOST daemon (the
# worker is a SIBLING container on the host's docker, reached via the mounted
# socket). An abspath computed inside the web container (e.g. /app/data/run-x)
# does not exist on the host, so the bind silently mounts an empty dir and the
# worker can't read the workspace. The compose contract (decision #2): the host
# data root is bind-mounted into the web container, and these env vars name both
# sides so we can translate a container path back to the host path for the mount:
#   MUTEKI_HOST_DATA_ROOT      — the host's real path (e.g. /opt/muteki/data)
#   MUTEKI_CONTAINER_DATA_ROOT — where it's mounted in the web container
#                                (default: same as host root → identity mirror)
# Unset (bare host) → identity, no translation.
_HOST_DATA_ROOT = (os.environ.get("MUTEKI_HOST_DATA_ROOT") or "").strip()
_CONTAINER_DATA_ROOT = (os.environ.get("MUTEKI_CONTAINER_DATA_ROOT") or _HOST_DATA_ROOT).strip()

# The worker runs as the image's `kali` user. Do NOT hard-code its uid/gid: the
# Kali and slim Dockerfiles intentionally create the user by name, and different
# base images may assign 1000, 1001, or another value. The run workspace is created
# HOST-side by the (root) web process and bind-mounted at /home/kali/workspace, so
# it lands root-owned and the kali worker can't WRITE it. Chown the workspace tree
# to the image's actual kali uid/gid when we bring the container up so shared state
# (graph/shared_graph.db, workspace/shared, etc.) is writable.
_WORKER_USER = (os.environ.get("MUTEKI_WORKER_USER") or "kali").strip() or "kali"
_WORKER_ID_FALLBACK = (1000, 1000)
_WORKER_ID_CACHE: dict[str, tuple[int, int]] = {}
_WORKER_ID_LOCK = threading.Lock()


def _env_int(name: str) -> Optional[int]:
    raw = (os.environ.get(name) or "").strip()
    if not raw:
        return None
    try:
        value = int(raw)
    except ValueError:
        return None
    return value if value >= 0 else None


def _fallback_worker_uid_gid() -> tuple[int, int]:
    uid = _env_int("MUTEKI_WORKER_UID")
    gid = _env_int("MUTEKI_WORKER_GID")
    return (
        uid if uid is not None else _WORKER_ID_FALLBACK[0],
        gid if gid is not None else _WORKER_ID_FALLBACK[1],
    )


_WORKER_UID, _WORKER_GID = _fallback_worker_uid_gid()


def _query_worker_uid_gid(image: str) -> tuple[int, int]:
    """Read the worker uid/gid from the local worker image.

    `docker run` is used instead of image metadata because the Dockerfiles create
    `kali` by name and do not set Config.User to that uid. `image inspect` first
    prevents an accidental pull when the image is missing.
    """
    if _docker("image", "inspect", image, timeout=20).returncode != 0:
        return _fallback_worker_uid_gid()
    quoted_user = shlex.quote(_WORKER_USER)
    r = _docker(
        "run", "--rm", "--entrypoint", "sh", image,
        "-lc", f"id -u {quoted_user} && id -g {quoted_user}",
        timeout=30,
    )
    if r.returncode != 0:
        return _fallback_worker_uid_gid()
    vals: list[int] = []
    for line in (r.stdout or "").splitlines():
        try:
            vals.append(int(line.strip()))
        except ValueError:
            continue
        if len(vals) == 2:
            break
    if len(vals) != 2:
        return _fallback_worker_uid_gid()
    uid_override = _env_int("MUTEKI_WORKER_UID")
    gid_override = _env_int("MUTEKI_WORKER_GID")
    return (
        uid_override if uid_override is not None else vals[0],
        gid_override if gid_override is not None else vals[1],
    )


def _worker_uid_gid(image: str = WORKER_IMAGE) -> tuple[int, int]:
    """Actual uid/gid that host-created workspace files must be chowned to."""
    uid_override = _env_int("MUTEKI_WORKER_UID")
    gid_override = _env_int("MUTEKI_WORKER_GID")
    if uid_override is not None and gid_override is not None:
        return uid_override, gid_override
    with _WORKER_ID_LOCK:
        if image not in _WORKER_ID_CACHE:
            _WORKER_ID_CACHE[image] = _query_worker_uid_gid(image)
        uid, gid = _WORKER_ID_CACHE[image]
    return (
        uid_override if uid_override is not None else uid,
        gid_override if gid_override is not None else gid,
    )


def _chown_tree_to_worker(root: str, *, image: str = WORKER_IMAGE) -> None:
    """Best-effort recursive chown of a host dir tree to the worker uid:gid so the
    bind-mounted `kali` worker can write shared state (the blackboard DB lives here).
    No-op when we're not root (bare-host dev: the web process already owns it and
    runs the worker as itself) or the path is missing. Never raises — a failed
    chown must not break the run; the worst case is the pre-existing readonly bug."""
    try:
        if os.geteuid() != 0:  # not root → can't chown, and don't need to (same uid)
            return
    except AttributeError:  # no geteuid (non-POSIX) — nothing to do
        return
    uid, gid = _worker_uid_gid(image)
    def _chown_one(path: str) -> None:
        try:
            if os.path.islink(path):
                lchown = getattr(os, "lchown", None)
                if callable(lchown):
                    lchown(path, uid, gid)
                return
            os.chown(path, uid, gid)
        except OSError:
            pass  # one stubborn entry shouldn't abort the whole sweep

    try:
        _chown_one(root)
        for dirpath, dirnames, filenames in os.walk(root):
            for name in dirnames + filenames:
                _chown_one(os.path.join(dirpath, name))
    except OSError:
        pass


def _mount_source(path: str) -> str:
    """Translate a coordinator-visible path into the path the HOST docker daemon
    should bind-mount. Identity unless MUTEKI_HOST_DATA_ROOT is set AND `path` is
    under MUTEKI_CONTAINER_DATA_ROOT (the mirrored data volume)."""
    ap = os.path.abspath(path)
    if not _HOST_DATA_ROOT:
        return ap
    croot = os.path.abspath(_CONTAINER_DATA_ROOT)
    hroot = os.path.abspath(_HOST_DATA_ROOT)
    if croot == hroot:
        return ap  # identity mirror — container path already IS the host path
    # remap the prefix; require a real path boundary so /app/data2 isn't matched
    if ap == croot:
        return hroot
    if ap.startswith(croot + os.sep):
        return hroot + ap[len(croot):]
    # path is outside the mirrored root — pass through (best effort; logged upstream)
    return ap


def _docker(*args: str, timeout: float = 30.0) -> subprocess.CompletedProcess:
    # encoding=utf-8/errors=replace (P2-v3): docker/agent output is UTF-8; without
    # an explicit encoding text=True decodes by the host's locale (cp1252/cp936 on
    # Windows), corrupting non-ASCII output / the JSON event stream.
    return subprocess.run(["docker", *args], capture_output=True, text=True,
                          encoding="utf-8", errors="replace", timeout=timeout)


def _safe(run_id: str) -> str:
    # Docker container names accept ASCII alnum plus ``_.-``. ``str.isalnum`` also
    # accepts Unicode letters/digits, so require ASCII explicitly before preserving
    # a character; the digest below retains uniqueness after replacement.
    return "".join(
        c if (c.isascii() and (c.isalnum() or c in "-_.")) else "-"
        for c in run_id
    )


def _run_identity(run_id: str) -> str:
    """A readable, collision-resistant identifier safe for Docker/path names.

    Sanitising alone aliases values such as ``a/b`` and ``a?b``; truncating a long
    value introduces another alias class.  Keep a short readable prefix, but always
    bind it to the complete run id with a digest.
    """
    readable = _safe(run_id).strip("-_.") or "run"
    digest = _run_digest(run_id)[:16]
    return f"{readable[:72]}-{digest}"


def _run_digest(run_id: str) -> str:
    return hashlib.sha256(
        run_id.encode("utf-8", errors="surrogatepass")).hexdigest()


def _run_container_name(run_id: str) -> str:
    environment = os.environ.get("MUTEKI_ENVIRONMENT_ID", "")
    namespace = hashlib.sha256(environment.encode()).hexdigest()[:12] + "-" if environment else ""
    return f"{_RUN_PREFIX}{namespace}{_run_identity(run_id)}"


def _bootstrap_dir(
    run_id: str,
    host_workspace: str,
    bootstrap_root: Optional[str] = None,
) -> str:
    """Coordinator-private one-shot bootstrap mount, outside the worker workspace.

    The explicit state/runtime location stays under the mirrored data root
    (important when the coordinator itself runs in Docker) while remaining
    unreachable through the worker's ``/home/kali/workspace`` bind mount.
    """
    if not bootstrap_root:
        raise ValueError("RCP containers require an explicit bootstrap root")
    workspace = os.path.realpath(os.path.abspath(host_workspace))
    root = os.path.realpath(os.path.abspath(bootstrap_root))
    if os.path.commonpath((workspace, root)) == workspace:
        raise RuntimeError("RCP bootstrap root must be outside the worker mount")
    path = os.path.join(root, _run_identity(run_id))
    return path


_BOOTSTRAP_DIRS: dict[str, str] = {}
_ACCOUNT_PROJECTIONS: dict[str, str] = {}


def _shared_leases_root(bootstrap_root: str) -> Path:
    root = Path(bootstrap_root).expanduser().absolute()
    leases = root / "shared-run-leases"
    if root.is_symlink() or leases.is_symlink():
        raise RuntimeError("shared lease root cannot be a symlink")
    leases.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(leases, 0o700)
    return leases


def _shared_lease_path(logical_run_id: str, bootstrap_root: str) -> Path:
    return _shared_leases_root(bootstrap_root) / f"{_run_identity(logical_run_id)}.json"


def _other_shared_owners(logical_run_id: str, bootstrap_root: str) -> list[str]:
    leases = _shared_leases_root(bootstrap_root)
    current = _shared_lease_path(logical_run_id, bootstrap_root).name
    owners: list[str] = []
    for path in leases.iterdir():
        if path.name == current:
            continue
        if path.name.startswith(".") and path.suffix == ".tmp":
            continue
        if path.is_symlink() or not path.is_file() or path.suffix != ".json":
            raise RuntimeError("unexpected shared runtime owner record")
        try:
            item = json.loads(path.read_text(encoding="utf-8"))
            owner = str(item["run_id"])
        except (OSError, ValueError, TypeError, KeyError) as exc:
            raise RuntimeError("invalid shared runtime owner record") from exc
        if path.name != f"{_run_identity(owner)}.json":
            raise RuntimeError("shared runtime owner record identity mismatch")
        owners.append(owner)
    return owners


def _register_shared_owner(logical_run_id: str, bootstrap_root: str, policy: str) -> str:
    path = _shared_lease_path(logical_run_id, bootstrap_root)
    if path.is_symlink():
        raise RuntimeError("shared runtime owner record cannot be a symlink")
    if path.exists():
        try:
            existing = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError, TypeError) as exc:
            raise RuntimeError("invalid shared runtime owner record") from exc
        if (existing.get("run_id") != logical_run_id
                or existing.get("policy") != policy
                or not isinstance(existing.get("token"), str)
                or len(existing["token"]) < 32):
            raise RuntimeError("shared Run policy changed; start a new Run")
        token = existing["token"]
    else:
        token = secrets.token_hex(32)
        temporary = path.with_name(f".{path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp")
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            data = json.dumps({"run_id": logical_run_id, "policy": policy,
                               "token": token}).encode()
            os.write(fd, data)
            os.fsync(fd)
        finally:
            os.close(fd)
        os.replace(temporary, path)
    return token


def _cleanup_bootstrap_dir(run_id: str, *, fallback: Optional[str] = None) -> None:
    """Remove bootstrap material after the caller has proven runtime absence.

    This helper intentionally does not inspect Docker itself so teardown can make
    one authoritative proof covering the active container.  Every call
    site is behind that proof (or occurs before a brand-new container is created).
    """
    registered = _BOOTSTRAP_DIRS.pop(run_id, None)
    for path in dict.fromkeys(p for p in (registered, fallback) if p):
        try:
            shutil.rmtree(path)
        except FileNotFoundError:
            pass


def _retire_bootstrap_token(handle: "ContainerHandle") -> None:
    """Erase the host copy once the authenticated supervisor link is ready."""
    if handle.control_dir:
        try:
            os.unlink(os.path.join(handle.control_dir, "token"))
        except FileNotFoundError:
            pass
    handle.token = ""


@dataclass
class ContainerHandle:
    """Identifies a Run's selected long-lived container and mounted workspace."""
    run_id: str
    # The logical per-Run workspace supplied by the caller.  In shared mode the
    # container mounts its parent, so this must be retained separately from
    # host_workspace.  Reusing host_workspace as ensure_container() input would
    # apply that parent transform a second time after a container restart.
    run_workspace: str
    host_workspace: str
    container: str
    image: str = WORKER_IMAGE
    network: str = "bridge"
    requested_network: Optional[str] = None
    memory: Optional[str] = None
    cpus: Optional[str] = None
    pids_limit: Optional[int] = None
    resource_limits: Optional[WorkerResourceLimits] = None
    account_root: Optional[str] = None
    # IDs projected into account_root (Run or explicitly trusted pool). Empty = none.
    account_ids: tuple[str, ...] = ()
    # default: no privilege gains; elevated: explicit operator opt-in to image sudo.
    worker_privilege: str = "default"
    # account_root above is the projected directory mounted into the container;
    # this is the original host account store needed to rebuild that projection.
    account_source_root: Optional[str] = None
    shared_mount_root: Optional[str] = None
    account_projection_root: Optional[str] = None
    bootstrap_root: Optional[str] = None
    # rcp control plane (mode == "rcp"): REVERSE-CONNECT — the supervisor dials the
    # host ControlReceiver and is routed by run_id, so the host side just needs the
    # run_id (above) to find the link. control_dir carries the one-shot token into
    # PID1, then remains mounted but empty; token is cleared after authenticated
    # readiness.
    mode: str = "rcp"               # "rcp" | "dockerexec"
    control_dir: Optional[str] = None   # private host dir bind-mounted to /run/muteki/control
    token: str = ""                     # pending token only; blank after Hello succeeds
    control_run_id: str = ""
    container_scope: str = "run"
    owner_token: str = ""
    vpn_config: Optional[str] = None

    @property
    def supervisor_run_id(self) -> str:
        return self.control_run_id or self.run_id

    def to_container_cwd(self, host_cwd: str) -> str:
        """Map a host cwd under host_workspace → its path inside the container."""
        return self.to_container_path(host_cwd)

    def to_container_path(self, host_path: str) -> str:
        """Map a host path under a mounted root to its container path."""
        try:
            rel = os.path.relpath(
                os.path.realpath(os.path.abspath(host_path)),
                os.path.realpath(os.path.abspath(self.host_workspace)),
            )
        except ValueError:
            rel = ".."
        if rel == ".":
            return CONTAINER_WORKSPACE
        if rel == ".." or rel.startswith(".." + os.sep):
            if self.account_root:
                try:
                    arel = os.path.relpath(
                        os.path.realpath(os.path.abspath(host_path)),
                        os.path.realpath(os.path.abspath(self.account_root)),
                    )
                except ValueError:
                    arel = ".."
                if arel == ".":
                    return CONTAINER_ACCOUNTS_ROOT
                if arel != ".." and not arel.startswith(".." + os.sep):
                    return f"{CONTAINER_ACCOUNTS_ROOT}/{arel}"
            raise LaunchContractError(
                "host path is outside the container's declared mounts",
                code=LAUNCH_CODE_ENVIRONMENT,
                field="host_path",
                source="container_mount",
            )
        return f"{CONTAINER_WORKSPACE}/{rel}"


@dataclass
class RuntimeExecRecord:
    exec_id: str
    run_id: str
    container: str
    tag: str
    driver: str
    cwd: str
    argv0: str
    status: str = "created"
    started_at: float = field(default_factory=time.time)
    finished_at: Optional[float] = None
    rc: Optional[int] = None
    timed_out: bool = False
    oom_killed: bool = False
    cancelled: bool = False
    steered: bool = False
    error: str = ""

    def snapshot(self) -> dict[str, Any]:
        return {
            "backend": "container",
            "exec_id": self.exec_id,
            "run_id": self.run_id,
            "container": self.container,
            "tag": self.tag,
            "driver": self.driver,
            "cwd": self.cwd,
            "argv0": self.argv0,
            "status": self.status,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "rc": self.rc,
            "timed_out": self.timed_out,
            "oom_killed": self.oom_killed,
            "cancelled": self.cancelled,
            "steered": self.steered,
            "error": self.error,
        }


class RuntimeExecRegistry:
    """Host-side bookkeeping of worker execs (both backends), surfaced to the deck."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._records: dict[str, RuntimeExecRecord] = {}

    def create(
        self, *, handle: ContainerHandle, tag: str, driver: str,
        cwd: str, argv: list[str],
    ) -> RuntimeExecRecord:
        rec = RuntimeExecRecord(
            exec_id=uuid.uuid4().hex[:12],
            run_id=handle.run_id,
            container=handle.container,
            tag=tag,
            driver=driver,
            cwd=cwd,
            argv0=argv[0] if argv else "",
        )
        with self._lock:
            self._records[rec.exec_id] = rec
        return rec

    def mark(self, rec: RuntimeExecRecord, **fields: Any) -> None:
        with self._lock:
            for k, v in fields.items():
                if hasattr(rec, k):
                    setattr(rec, k, v)
            self._records[rec.exec_id] = rec

    def finish(self, rec: RuntimeExecRecord, **fields: Any) -> dict[str, Any]:
        fields.setdefault("finished_at", time.time())
        fields.setdefault("status", "finished")
        self.mark(rec, **fields)
        return rec.snapshot()

    def snapshot(self, exec_id: str) -> Optional[dict[str, Any]]:
        with self._lock:
            rec = self._records.get(exec_id)
            return rec.snapshot() if rec else None

    def by_run(self, run_id: str) -> list[dict[str, Any]]:
        with self._lock:
            return [r.snapshot() for r in self._records.values() if r.run_id == run_id]


_RUNTIME_REGISTRY = RuntimeExecRegistry()


def runtime_execs_for_run(run_id: str) -> list[dict[str, Any]]:
    return _RUNTIME_REGISTRY.by_run(run_id)


# ── run-level setup / teardown ───────────────────────────────────────────────

# Serialise ensure_container across the many worker threads that spawn at once.
# Without this, two first-time workers race: A `docker run`s the container (state
# briefly "created"), B sees a non-running/non-None state and `docker rm -f`s A's
# container mid-creation → every subsequent worker dies "No such container" (the bug
# that made all worker turns 0.02s rc=1 empty exits). The lock + a created-state
# aware path (start, don't blindly remove) fixes it.
_ENSURE_LOCK = threading.RLock()
_ACCOUNT_PROJECTION_LOCK = threading.Lock()



def ensure_container(run_id: str, host_workspace: str, *,
                     image: str = WORKER_IMAGE, network: str = "bridge",
                     memory: Optional[str] = None,
                     cpus: Optional[str] = None,
                     pids_limit: Optional[int] = None,
                     output_limit: Optional[str] = None,
                     disk_limit: Optional[str] = None,
                     account_root: Optional[str] = None,
                     account_ids: Optional[list[str] | tuple[str, ...] | set[str]] = None,
                     container_scope: str = "run",
                     shared_mount_root: Optional[str] = None,
                     account_projection_root: Optional[str] = None,
                     bootstrap_root: Optional[str] = None,
                     vpn_config: Optional[str] = None,
                     worker_privilege: str = "default") -> ContainerHandle:
    """Bring up a Run container or an explicitly trusted shared container.

    ``account_ids`` selects which credential accounts are
    projected; default empty means project none. ``worker_privilege`` is
    ``default`` (deny privilege gains) or ``elevated`` (keep image sudo).
    """
    limits = resolve_worker_resource_limits(image=image, memory=memory, cpus=cpus,
                                            pids_limit=pids_limit, output_limit=output_limit, disk_limit=disk_limit)
    memory, cpus, pids_limit = limits.memory, limits.cpus, limits.pids_limit
    requested_network = _normalize_requested_network(network)
    logical_run_id = run_id
    run_workspace = str(Path(host_workspace).expanduser().resolve())
    account_source_root = (
        str(Path(account_root).expanduser().resolve()) if account_root else None
    )
    if container_scope not in {"run", "shared"}:
        raise ValueError("container_scope must be run or shared")
    if container_scope == "shared":
        if not shared_mount_root or not bootstrap_root:
            raise ValueError("shared scope requires dedicated mount and bootstrap roots")
        shared_root = Path(shared_mount_root).expanduser().resolve()
        if shared_root.is_symlink() or Path(shared_mount_root).is_symlink():
            raise RuntimeError("shared mount root cannot be a symlink")
        if Path(run_workspace).parent != shared_root:
            raise ValueError("shared Run workspace must be a direct slot of its dedicated pool root")
        private_root = Path(bootstrap_root).expanduser().resolve()
        if private_root == shared_root or private_root.is_relative_to(shared_root):
            raise ValueError("shared runtime bootstrap root must be outside its worker mount")
        if account_projection_root:
            projection = Path(account_projection_root).expanduser().resolve()
            if projection == shared_root or projection.is_relative_to(shared_root):
                raise ValueError("shared account projection root must be outside its worker mount")
        host_workspace = str(shared_root)
        run_id = _shared_runtime_id(bootstrap_root)
    else:
        host_workspace = run_workspace
    privilege = str(worker_privilege or "default").strip().lower()
    if privilege not in {"default", "elevated"}:
        raise ValueError("worker_privilege must be default or elevated")
    selected_account_ids = tuple(sorted({
        str(a).strip() for a in (account_ids or ()) if str(a).strip()
    }))
    os.makedirs(host_workspace, exist_ok=True)
    # Make the Worker-only workspace writable by the kali uid. Coordinator logs,
    # secrets and SharedGraph live outside this mount and are never chowned here.
    _chown_tree_to_worker(run_workspace, image=image)
    if container_scope == "shared" and getattr(os, "geteuid", lambda: -1)() == 0:
        uid, gid = _worker_uid_gid(image)
        os.chown(host_workspace, uid, gid)
    mount_account_root = None
    r = _docker("image", "inspect", image, timeout=20)
    if r.returncode != 0:
        raise RuntimeError(
            f"worker image {image!r} not found — pull the image built by GitHub Actions")
    name = _run_container_name(run_id)

    mode = "dockerexec" if _USE_DOCKEREXEC else "rcp"
    if container_scope == "shared" and mode != "rcp":
        raise RuntimeError("shared scope requires the scoped RCP supervisor")
    # RCP supervisor dials out to the host receiver; remote model HTTPS also needs
    # egress. ``none`` is never rewritten to bridge (#171 / MNT-09.04) — resolve
    # rejects clearly when egress is required. Web-tool deny (offline) is separate
    # from Docker network isolation; full shell offline remains P3-04.
    network = resolve_worker_run_network(network, needs_egress=True)
    host_net = str(network).strip() == "host"
    owner_token = ""

    def _handle(**kwargs: Any) -> ContainerHandle:
        return ContainerHandle(
            run_id=logical_run_id,
            run_workspace=run_workspace,
            control_run_id=run_id,
            container_scope=container_scope,
            owner_token=owner_token,
            vpn_config=vpn_config,
            account_source_root=account_source_root,
            account_ids=selected_account_ids,
            worker_privilege=privilege,
            resource_limits=limits,
            requested_network=requested_network,
            shared_mount_root=(
                str(Path(shared_mount_root).expanduser().resolve())
                if shared_mount_root else None
            ),
            account_projection_root=(
                str(Path(account_projection_root).expanduser().resolve())
                if account_projection_root else None
            ),
            bootstrap_root=(
                str(Path(bootstrap_root).expanduser().resolve())
                if bootstrap_root else None
            ),
            **kwargs,
        )

    def _activate(handle: ContainerHandle) -> None:
        if handle.container_scope != "shared":
            return
        from muteki.solver.control_client import register_owner
        if not register_owner(handle.supervisor_run_id, handle.run_id,
                              handle.owner_token,
                              handle.to_container_path(handle.run_workspace)):
            raise RuntimeError("shared runtime rejected Run registration")

    with _ENSURE_LOCK:
        receiver = None
        if mode == "rcp":
            from muteki.solver.control_receiver import ControlReceiver
            receiver = ControlReceiver.instance()
        state = _container_state(name)
        vpn_digest = None
        if vpn_config:
            vpn_digest = hashlib.sha256(Path(vpn_config).read_bytes()).hexdigest()
        policy = hashlib.sha256(json.dumps({
            "image": image, "network": network, "privilege": privilege,
            "accounts": selected_account_ids, "workspace": host_workspace,
            "account_source_root": account_source_root,
            "projection_root": account_projection_root,
            "memory": memory, "cpus": cpus, "pids_limit": pids_limit,
            "vpn_config": vpn_config, "vpn_digest": vpn_digest,
        }, sort_keys=True).encode()).hexdigest()
        if container_scope == "shared":
            other_owners = _other_shared_owners(logical_run_id, bootstrap_root)
            for owner in other_owners:
                other_lease = _shared_lease_path(owner, bootstrap_root)
                try:
                    owner_policy = json.loads(other_lease.read_text(encoding="utf-8"))["policy"]
                except (OSError, ValueError, TypeError, KeyError) as exc:
                    raise RuntimeError("invalid shared runtime owner policy") from exc
                if owner_policy != policy:
                    raise RuntimeError("shared runtime policy differs from another Run owner")
        reusable = (state in {"running", "created", "paused", "restarting"}
                    and (mode == "dockerexec" or (state == "running" and receiver.has_link(run_id))))
        if state is not None and not reusable:
            stale_bootstrap = _BOOTSTRAP_DIRS.get(run_id) or _container_bootstrap_source(name)
            _docker("rm", "-f", name, timeout=20)
            if not _container_absence_proven(name):
                raise RuntimeError("stale container absence could not be proven before reprojecting credentials")
            if container_scope == "shared":
                from muteki.solver.control_client import confirm_run_absent
                for owner in (logical_run_id, *other_owners):
                    confirm_run_absent(owner)
            if receiver is not None:
                receiver.forget(run_id)
            _cleanup_bootstrap_dir(run_id, fallback=stale_bootstrap)
            state = None
        if reusable:
            existing = _docker("inspect", "--format",
                               '{{index .Config.Labels "io.muteki.worker-policy"}}', name)
            if existing.returncode != 0 or existing.stdout.strip() != policy:
                raise RuntimeError("Worker container policy changed; stop the existing Run container before restarting")
        if container_scope == "shared":
            owner_token = _register_shared_owner(logical_run_id, bootstrap_root, policy)
        mount_account_root = None
        if account_root:
            if not account_projection_root:
                raise ValueError("container credentials require an account projection root")
            os.makedirs(account_root, exist_ok=True)
            # Mount a container-READABLE projection of the account store, NOT the raw
            # 0600 host store (#15: the container 'kali' user's uid differs from the
            # host owner so it can't read 0600 files; #14: codex needs CODEX_HOME/
            # auth.json WRITABLE to refresh its token). project_account_root copies the
            # store under the (gitignored, ephemeral) run workspace with container-
            # readable perms + a writable codex-home, leaving the host store untouched
            # and read-only. The projection — not the raw store — is what's mounted.
            from muteki.solver.credential_accounts import project_account_root
            projection_base = Path(account_projection_root).expanduser().resolve()
            mounted_root = Path(host_workspace).resolve()
            try:
                projection_base.relative_to(mounted_root)
            except ValueError:
                pass
            else:
                raise ValueError("account projection root cannot be worker-visible")
            projection_base.mkdir(mode=0o700, parents=True, exist_ok=True)
            projection = str(projection_base / _run_identity(run_id))
            with _ACCOUNT_PROJECTION_LOCK:
                if Path(projection).is_symlink():
                    raise RuntimeError("account projection cannot be a symlink")
                # Preserve directory inodes and refreshed CLI state on repeated setup.
                project_account_root(
                    account_root, projection, account_ids=selected_account_ids,
                )
            if not Path(projection).is_dir() or Path(projection).is_symlink():
                raise RuntimeError("account projection is not a real directory")
            mount_account_root = projection
            _ACCOUNT_PROJECTIONS[run_id] = projection
        if state == "running":
            if mode == "dockerexec":
                handle = _handle(
                    host_workspace=host_workspace, container=name,
                    image=image, network=network, memory=memory, cpus=cpus,
                    pids_limit=pids_limit, account_root=mount_account_root,
                    mode=mode,
                )
                _ensure_openvpn(handle)
                return handle
            # Never rotate a bootstrap token underneath a live reverse-control
            # owner.  The token was consumed at Hello and the file was unlinked;
            # the authenticated socket is now the authority.
            assert receiver is not None
            if receiver.has_link(run_id):
                handle = _handle(
                    host_workspace=host_workspace, container=name,
                    image=image, network=network, memory=memory, cpus=cpus,
                    pids_limit=pids_limit, account_root=mount_account_root,
                    mode=mode, control_dir=_BOOTSTRAP_DIRS.get(run_id), token="",
                )
                _await_supervisor(handle)
                _activate(handle)
                _ensure_openvpn(handle)
                return handle
            # A running container without a live receiver link cannot authenticate
            # again: its one-shot token has already been consumed (or its bootstrap
            # state is unknowable after a coordinator restart).  Recreate it instead
            # of manufacturing a replacement credential for an orphan runtime.
            stale_bootstrap = (_BOOTSTRAP_DIRS.get(run_id)
                               or _container_bootstrap_source(name))
            _docker("rm", "-f", name, timeout=20)
            if not _container_absence_proven(name):
                raise RuntimeError(
                    f"orphan runtime {name} has no live control link and its "
                    "absence could not be proven")
            receiver.forget(run_id)
            _cleanup_bootstrap_dir(run_id, fallback=stale_bootstrap)
            state = None
        if state in ("created", "restarting", "paused"):
            if mode == "dockerexec":
                # Docker-exec transport has no authenticated link to preserve.
                _docker("start", name, timeout=20)
                if _container_state(name) == "running":
                    handle = _handle(
                        host_workspace=host_workspace, container=name,
                        image=image, network=network, memory=memory, cpus=cpus,
                        pids_limit=pids_limit, account_root=mount_account_root,
                        mode=mode,
                    )
                    _ensure_openvpn(handle)
                    return handle
            # In RCP mode a non-running pre-existing container has no usable live
            # link.  Remove and bootstrap a fresh supervisor instead of rotating a
            # token beneath an ambiguous owner.
            stale_bootstrap = (_BOOTSTRAP_DIRS.get(run_id)
                               or _container_bootstrap_source(name))
            _docker("rm", "-f", name, timeout=20)
            if not _container_absence_proven(name):
                raise RuntimeError(
                    f"stale runtime {name} could not be proven absent before recreate")
            if receiver is not None:
                receiver.forget(run_id)
            _cleanup_bootstrap_dir(run_id, fallback=stale_bootstrap)
            state = None
        if state is not None:
            # genuinely dead (exited/dead) leftover — remove and recreate clean.
            stale_bootstrap = (_BOOTSTRAP_DIRS.get(run_id)
                               or _container_bootstrap_source(name))
            _docker("rm", "-f", name, timeout=20)
            if not _container_absence_proven(name):
                raise RuntimeError(
                    f"stale runtime {name} could not be proven absent before recreate")
            if receiver is not None:
                receiver.forget(run_id)
            _cleanup_bootstrap_dir(run_id, fallback=stale_bootstrap)

        control_dir: Optional[str] = None
        token = ""
        if mode == "rcp":
            # A private sibling mount ferries a single-use token to PID1.  It is
            # deliberately outside host_workspace, because every worker can read
            # that workspace.  PID1 unlinks the file immediately after reading it;
            # the receiver atomically consumes the expected value on the first
            # successful Hello.
            assert receiver is not None
            if not _container_absence_proven(name):
                raise RuntimeError(
                    f"cannot bootstrap {name}: container absence is not proven")
            receiver.forget(run_id)
            control_dir = _bootstrap_dir(run_id, host_workspace, bootstrap_root)
            _cleanup_bootstrap_dir(run_id, fallback=control_dir)
            bootstrap_parent = os.path.dirname(control_dir)
            os.makedirs(bootstrap_parent, mode=0o700, exist_ok=True)
            os.chmod(bootstrap_parent, 0o700)
            os.mkdir(control_dir, mode=0o700)
            os.chmod(control_dir, 0o700)
            try:
                _BOOTSTRAP_DIRS[run_id] = control_dir
                token = secrets.token_hex(32)
                token_path = os.path.join(control_dir, "token")
                fd = os.open(token_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                try:
                    payload = token.encode("ascii")
                    if os.write(fd, payload) != len(payload):
                        raise OSError("short write creating RCP bootstrap token")
                finally:
                    os.close(fd)
                receiver.expect(run_id, token)
            except Exception:
                # No container exists yet (proved above), so bootstrap preparation
                # failure can be erased without creating an unknown-runtime window.
                receiver.forget(run_id)
                _cleanup_bootstrap_dir(run_id, fallback=control_dir)
                raise

        handle = _handle(host_workspace=host_workspace, container=name,
                         image=image, network=network, memory=memory, cpus=cpus,
                         pids_limit=pids_limit, account_root=mount_account_root,
                         mode=mode, control_dir=control_dir, token=token)
        # --mount (key=value) NOT -v: the workspace path has run_id ("nyu:KEY") whose
        # colon makes `-v host:ctr:rw` mis-parse → silent bind-mount failure. `--init`
        # reaps zombie trees. In rcp mode the ENTRYPOINT (supervisor) is the keepalive
        # (no `sleep infinity`); docker-exec mode uses sleep as PID 1.
        run_cmd = [
            "run", "-d", "--init", "--name", name,
            "--label", f"{_RUN_ID_LABEL}={_run_digest(run_id)}",
            "--label", f"io.muteki.worker-policy={policy}",
            "--network", network,
            "--tmpfs", "/tmp:rw,exec,size=2g",
            "--mount",
            f"type=bind,source={_mount_source(host_workspace)},target={CONTAINER_WORKSPACE}",
        ]
        if os.environ.get("MUTEKI_ENVIRONMENT_ID"):
            run_cmd += ["--label", f"io.muteki.environment-id={os.environ['MUTEKI_ENVIRONMENT_ID']}"]
        if mode == "rcp" and control_dir:
            run_cmd += [
                "--mount",
                f"type=bind,source={_mount_source(control_dir)},target={CONTAINER_CONTROL_DIR}",
            ]
            # The supervisor dials OUT to host.docker.internal. On Docker Desktop that
            # DNS resolves to the host automatically; on a Linux host it does NOT, so
            # add it explicitly mapped to the gateway. (Harmless on Docker Desktop —
            # the explicit entry just shadows the built-in with the same target.)
            if not host_net:
                run_cmd += ["--add-host", "host.docker.internal:host-gateway"]
        if memory:
            run_cmd += ["--memory", str(memory)]
        if cpus:
            run_cmd += ["--cpus", str(cpus)]
        if pids_limit and int(pids_limit) > 0:
            run_cmd += ["--pids-limit", str(int(pids_limit))]
        if account_root:
            # Static projection is read-only even for elevated workers. Only the
            # explicitly mounted refresh-state directories below are writable.
            run_cmd += [
                "--mount",
                f"type=bind,source={_mount_source(handle.account_root)},target={CONTAINER_ACCOUNTS_ROOT},readonly",
            ]
        if mount_account_root:
            from muteki.solver.credential_accounts import _WRITABLE_STATE_DIRS
            for account_id in selected_account_ids:
                for state_dir in _WRITABLE_STATE_DIRS:
                    source = str(Path(mount_account_root) / account_id / state_dir)
                    target = f"{CONTAINER_ACCOUNTS_ROOT}/{account_id}/{state_dir}"
                    run_cmd += ["--mount", f"type=bind,source={_mount_source(source)},target={target}"]
        if privilege == "default":
            run_cmd += ["--security-opt", "no-new-privileges=true"]
        if vpn_config:
            resolved_vpn = os.path.realpath(os.path.abspath(vpn_config))
            if not os.path.isfile(resolved_vpn):
                raise RuntimeError("OpenVPN 配置文件不存在")
            run_cmd += [
                "--cap-add", "NET_ADMIN",
                "--device", "/dev/net/tun:/dev/net/tun",
                "--mount",
                f"type=bind,source={_mount_source(os.path.dirname(resolved_vpn))},target=/run/muteki/vpn,readonly",
            ]
        if mode == "dockerexec":
            # The worker image has the RCP supervisor as its ENTRYPOINT. Override
            # it for the docker-exec transport so the keepalive is really
            # sleep(1), rather than passing "sleep infinity" to the supervisor.
            run = _docker(
                *run_cmd, "--entrypoint", "sleep", image, "infinity", timeout=60
            )
        else:
            # ENTRYPOINT (supervisor) runs; append the reverse-connect args so it DIALS
            # the host receiver and identifies as this run. These append to the exec-
            # form ENTRYPOINT; the supervisor ignores the baked --sock/--workspace and
            # uses --connect/--run-id (token comes from the bind-mounted control file).
            from muteki.solver.control_receiver import (
                CONTROL_HOST_FROM_CONTAINER, ControlReceiver)
            supervisor_args = [
                "--connect", f"{CONTROL_HOST_FROM_CONTAINER}:{ControlReceiver.instance().port}",
                "--run-id", run_id,
            ]
            if container_scope == "shared":
                supervisor_args.append("--shared-pool")
            run = _docker(*run_cmd, image, *supervisor_args, timeout=60)
        if run.returncode != 0:
            # lost a create race (name conflict) → reuse whoever won if it's up.
            st = _container_state(name)
            if st == "running" or (mode == "rcp" and st == "created"):
                if mode == "rcp":
                    _await_supervisor(handle)
                    _activate(handle)
                    _retire_bootstrap_token(handle)
                _ensure_openvpn(handle)
                return handle
            if mode == "rcp" and _container_absence_proven(name):
                assert receiver is not None
                receiver.forget(run_id)
                _cleanup_bootstrap_dir(run_id)
            raise RuntimeError(f"failed to start run container {name}: {run.stderr.strip()[:300]}")
        if mode == "dockerexec" and _container_state(name) != "running":
            _docker("rm", "-f", name, timeout=20)
            raise RuntimeError(f"docker-exec runtime {name} exited during startup")
        if mode == "rcp":
            _await_supervisor(handle)
            _activate(handle)
            _retire_bootstrap_token(handle)
        _ensure_openvpn(handle)
        return handle


def _ensure_openvpn(handle: ContainerHandle) -> None:
    """Start the optional VPN, reloading a changed mounted config before a new Run."""
    if not handle.vpn_config:
        return
    mounted = _docker("exec", handle.container, "test", "-r", CONTAINER_VPN_CONFIG,
                      timeout=10)
    if mounted.returncode != 0:
        raise RuntimeError("共享容器已存在但未挂载 OpenVPN 配置，请先停止现有任务")
    digest = _docker(
        "exec", handle.container, "sha256sum", CONTAINER_VPN_CONFIG, timeout=10,
    )
    current_digest = (digest.stdout or "").split(maxsplit=1)[0]
    if digest.returncode != 0 or not current_digest:
        raise RuntimeError("无法读取 OpenVPN 配置摘要")
    active = _docker(
        "exec", handle.container, "sh", "-lc",
        f"pgrep -x openvpn >/dev/null && ip link show tun0 >/dev/null 2>&1 "
        f"&& test \"$(cat {CONTAINER_VPN_DIGEST} 2>/dev/null)\" = \"{current_digest}\"",
        timeout=10,
    )
    if active.returncode == 0:
        return
    stopped = _docker(
        "exec", "--user", "root", handle.container, "sh", "-lc",
        "pkill -x openvpn >/dev/null 2>&1 || true; "
        "i=0; while pgrep -x openvpn >/dev/null && [ $i -lt 50 ]; do "
        "i=$((i+1)); sleep 0.1; done; ! pgrep -x openvpn >/dev/null",
        timeout=10,
    )
    if stopped.returncode != 0:
        raise RuntimeError("旧 OpenVPN 进程未能停止")

    def _log_tail() -> str:
        detail = _docker(
            "exec", handle.container, "tail", "-n", "30",
            "/run/muteki-openvpn.log", timeout=10,
        )
        return (detail.stdout or detail.stderr or "")[-800:]

    # TSec permits only one client per profile.  Immediately reconnecting after a
    # previous client exits can briefly return AUTH_FAILED while the server still
    # owns that old session.  Retry only that explicit transient response; invalid
    # credentials and every other startup failure must remain immediate failures.
    retry_deadline = time.monotonic() + 90
    while True:
        started = _docker(
            "exec", "--user", "root", handle.container,
            "openvpn", "--config", CONTAINER_VPN_CONFIG,
            "--daemon", "muteki-openvpn", "--writepid", "/run/muteki-openvpn.pid",
            "--log", "/run/muteki-openvpn.log", timeout=20,
        )
        if started.returncode != 0:
            raise RuntimeError(
                f"OpenVPN 启动失败: {(started.stderr or started.stdout or '')[:240]}"
            )
        attempt_deadline = time.monotonic() + 30
        while time.monotonic() < attempt_deadline:
            ready = _docker(
                "exec", handle.container, "ip", "link", "show", "tun0", timeout=5,
            )
            if ready.returncode == 0:
                recorded = _docker(
                    "exec", "--user", "root", handle.container, "sh", "-lc",
                    f"printf '%s\\n' '{current_digest}' > {CONTAINER_VPN_DIGEST}",
                    timeout=10,
                )
                if recorded.returncode != 0:
                    raise RuntimeError("无法记录 OpenVPN 配置摘要")
                return
            alive = _docker(
                "exec", handle.container, "pgrep", "-x", "openvpn", timeout=5,
            )
            if alive.returncode != 0:
                detail = _log_tail()
                if ("Exceeded number of clients connecting to server" in detail
                        and time.monotonic() < retry_deadline):
                    time.sleep(5)
                    break
                if "AUTH_FAILED" in detail:
                    raise RuntimeError(f"OpenVPN 认证失败: {detail}")
                raise RuntimeError(f"OpenVPN 在建立 tun0 前退出: {detail}")
            time.sleep(0.5)
        else:
            detail = _log_tail()
            if time.monotonic() < retry_deadline:
                _docker(
                    "exec", "--user", "root", handle.container,
                    "pkill", "-x", "openvpn", timeout=10,
                )
                time.sleep(2)
                continue
            raise RuntimeError(f"OpenVPN 未建立 tun0: {detail}")

        if time.monotonic() >= retry_deadline:
            raise RuntimeError(
                "OpenVPN 服务端拒绝连接：同一配置的客户端连接数已满，"
                f"自动重试后仍未释放。{_log_tail()}"
            )


def _await_supervisor(handle: ContainerHandle) -> None:
    """Block until the run's supervisor has DIALED the host receiver and answers
    Health. The container being up isn't enough — the supervisor must have connected
    back AND passed the token handshake. Raises if it never dials in (container
    started but control plane never connected → a runtime failure, not a silent local
    fallback)."""
    if handle.mode != "rcp":
        return
    from muteki.solver.control_client import wait_supervisor_ready
    if not wait_supervisor_ready(handle.supervisor_run_id, deadline_s=40.0):
        raise RuntimeError(
            f"runtime supervisor for run {handle.supervisor_run_id} never dialed back "
            f"(container {handle.container} up but control plane unreachable)")
    if handle.container_scope == "shared":
        from muteki.solver.control_client import health
        if not health(handle.supervisor_run_id).get("scoped_teardown"):
            raise RuntimeError("shared scope requires an image with scoped Run teardown support")


def _container_state(name: str) -> Optional[str]:
    r = _docker("inspect", "-f", "{{.State.Status}}", name, timeout=15)
    if r.returncode != 0:
        return None
    s = (r.stdout or "").strip()
    return s or None


def _oom_kill_count(container: str) -> Optional[int]:
    """Read the container cgroup's cumulative `oom_kill` counter (cgroup v2
    memory.events; v1 memory.oom_control fallback). Returns None if it can't be
    read. We snapshot this before a worker exec and re-read after: a NONZERO delta
    means the kernel OOM-killer SIGKILL'd a process in this container during the
    run — the discriminator that tells a real wall-clock timeout (137 after the
    full budget) apart from an OOM victim (137 early, empty transcript). (Used by
    the legacy dockerexec backend; the rcp supervisor computes this itself.)"""
    r = _docker("exec", container, "sh", "-c",
                "cat /sys/fs/cgroup/memory.events 2>/dev/null || "
                "cat /sys/fs/cgroup/memory/memory.oom_control 2>/dev/null",
                timeout=15)
    if r.returncode != 0:
        return None
    for line in (r.stdout or "").splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[0] == "oom_kill":
            try:
                return int(parts[1])
            except ValueError:
                return None
    return None


def _container_absence_proven(name: str) -> bool:
    """True only when Docker itself confirms that a container does not exist."""
    result = _docker("inspect", name, timeout=15)
    if result.returncode == 0:
        return False
    detail = f"{result.stdout}\n{result.stderr}".lower()
    return "no such object" in detail or "no such container" in detail


def _container_mount_source(name: str, destination: str) -> Optional[str]:
    """Recover one exact bind-mount source across coordinator restarts."""
    template = (
        '{{range .Mounts}}{{if eq .Destination "' + destination
        + '"}}{{.Source}}{{end}}{{end}}'
    )
    result = _docker("inspect", "-f", template, name, timeout=15)
    if result.returncode != 0:
        return None
    source = (result.stdout or "").strip()
    return source or None


def _container_bootstrap_source(name: str) -> Optional[str]:
    return _container_mount_source(name, CONTAINER_CONTROL_DIR)


def _cleanup_account_projection(path: Optional[str], owner_id: str) -> None:
    if not path:
        return
    candidate = Path(path)
    # Only process-registered projections reach this path. The caller may have
    # configured a different private projection base, so its basename is not a
    # safe or reliable cleanup criterion. The generated Run identity still is.
    if candidate.is_symlink() or candidate.name != _run_identity(owner_id):
        LOG.warning("refusing unexpected account projection cleanup: %s", path)
        return
    shutil.rmtree(candidate, ignore_errors=True)


def teardown_container(run_id: str, *, remove: bool = True,
                       container_scope: str = "run",
                       bootstrap_root: Optional[str] = None) -> bool:
    """Serialize whole-run teardown against bootstrap/recreation."""
    with _ENSURE_LOCK:
        if container_scope == "shared":
            return _teardown_shared_owner_locked(
                run_id, remove=remove, bootstrap_root=bootstrap_root)
        return _teardown_container_locked(run_id, remove=remove)


def _teardown_shared_owner_locked(logical_run_id: str, *, remove: bool,
                                  bootstrap_root: Optional[str]) -> bool:
    root = bootstrap_root
    if not root:
        return False
    lease = _shared_lease_path(logical_run_id, root)
    if lease.is_symlink():
        return False
    if lease.is_file():
        try:
            item = json.loads(lease.read_text(encoding="utf-8"))
        except (OSError, ValueError, TypeError):
            return False
        if item.get("run_id") != logical_run_id:
            return False
        owner_token = str(item.get("token") or "")
    else:
        owner_token = ""
    others = _other_shared_owners(logical_run_id, root)
    runtime_id = _shared_runtime_id(root)
    name = _run_container_name(runtime_id)
    state = _container_state(name)
    from muteki.solver.control_receiver import ControlReceiver
    live_link = (ControlReceiver.instance().has_link(runtime_id)
                 if state == "running" else False)
    if others and state == "running" and live_link:
        if not remove:
            return False
        from muteki.solver.control_client import health, teardown_owner
        try:
            if not health(runtime_id, timeout=2).get("scoped_teardown"):
                return False
            if not teardown_owner(runtime_id, logical_run_id, owner_token):
                return False
        except Exception:
            return False
    else:
        if state is not None and not remove:
            return False
        bootstrap = (_BOOTSTRAP_DIRS.get(runtime_id)
                     or _container_bootstrap_source(name))
        if remove and state is not None:
            _docker("rm", "-f", name, timeout=20)
        if not _container_absence_proven(name):
            return False
        from muteki.solver.control_client import confirm_run_absent
        confirm_run_absent(logical_run_id)
        for owner in others:
            confirm_run_absent(owner)
        confirm_run_absent(runtime_id)
        try:
            ControlReceiver.instance().forget(runtime_id)
        except Exception:
            pass
        _cleanup_bootstrap_dir(runtime_id, fallback=bootstrap)
        projection = _ACCOUNT_PROJECTIONS.pop(runtime_id, None)
        _cleanup_account_projection(projection, runtime_id)
    lease.unlink(missing_ok=True)
    return True


def _teardown_container_locked(run_id: str, *, remove: bool = True) -> bool:
    """Tear down the run's container and its process namespace."""
    name = _run_container_name(run_id)
    bootstrap_dir = _BOOTSTRAP_DIRS.get(run_id) or _container_bootstrap_source(name)
    # Delete only a projection registered by this process. After a coordinator
    # restart the mount source is untrusted container metadata; leaving an orphan
    # for later reuse/GC is safer than recursively deleting an attacker-chosen path.
    account_projection = _ACCOUNT_PROJECTIONS.get(run_id)
    if remove:
        _docker("rm", "-f", name, timeout=20)
    proven = _container_absence_proven(name)
    if proven:
        # Only discard the reverse-control owner after Docker proves every matching
        # runtime absent. A failed rm must keep the link/token retryable.
        try:
            from muteki.solver.control_client import confirm_run_absent
            confirm_run_absent(run_id)
        except Exception:
            # Preserve the process-local owner until the hard-exit fence can be
            # published; a later teardown retry sees the same Docker absence proof.
            return False
        try:
            from muteki.solver.control_receiver import ControlReceiver
            ControlReceiver.instance().forget(run_id)
        except Exception:
            pass
        # The one-shot mount directory is deliberately retained while Docker cannot
        # prove absence: an unknown/live container may still have it mounted.  Once
        # absence is authoritative, erase the empty mount and any unconsumed token.
        _cleanup_bootstrap_dir(run_id, fallback=bootstrap_dir)
        _cleanup_account_projection(account_projection, run_id)
        _ACCOUNT_PROJECTIONS.pop(run_id, None)
    return proven


# ── argv translation (shared by both backends) ───────────────────────────────

def _containerize_argv(driver_name: str, argv: list[str]) -> list[str]:
    if not argv:
        return argv
    bin_in_container = _CONTAINER_BIN.get(driver_name)
    out = list(argv)
    if len(out) >= 2 and os.path.basename(out[1]) == "offline_acp_bridge.py":
        out[0] = "python3"
        out[1] = _CONTAINER_OFFLINE_BRIDGE
        for index, arg in enumerate(out):
            if arg == "--agent-bin" and index + 1 < len(out):
                out[index + 1] = bin_in_container or os.path.basename(out[index + 1])
            elif arg.startswith("--agent-arg=") and os.path.basename(
                    arg.removeprefix("--agent-arg=")) == "omp_offline_config.yml":
                out[index] = f"--agent-arg={_CONTAINER_OMP_OFFLINE_CONFIG}"
        return out
    if driver_name == "opencode" and len(out) >= 3 and out[0] == "env":
        out[2] = bin_in_container or "opencode"
    elif driver_name == "grok" and len(out) >= 4 and out[0] == "env":
        out[3] = bin_in_container or "grok"
    else:
        out[0] = bin_in_container or os.path.basename(out[0])
    if driver_name == "kimi":
        for index, arg in enumerate(out[:-1]):
            if arg == "--agent-file":
                out[index + 1] = _CONTAINER_KIMI_OFFLINE_AGENT
    if driver_name == "grok":
        for index, arg in enumerate(out[:-1]):
            if arg == "--agent":
                out[index + 1] = _CONTAINER_GROK_OFFLINE_AGENT
    return out


# ── public entry points (mirror cli_driver.run_cli / run_cli_streaming) ───────

def _ensure_alive(handle: ContainerHandle) -> None:
    with _ENSURE_LOCK:
        _ensure_alive_locked(handle)


def _ensure_alive_locked(handle: ContainerHandle) -> None:
    """Guarantee the run container is up right before a worker starts. If a teardown
    / crash / race removed it, lazily recreate it (same name + mounts) so this worker
    doesn't die "No such container". Cheap when it's already running (one inspect).
    Re-syncs the handle's rcp token (+ receiver registration) if it had to recreate."""
    if handle.container_scope == "shared":
        root = handle.bootstrap_root
        if not root:
            raise RuntimeError("shared Run has no active runtime ownership record")
        lease = _shared_lease_path(handle.run_id, root)
        if lease.is_symlink() or not lease.is_file():
            raise RuntimeError("shared Run has no active runtime ownership record")
        try:
            active_token = json.loads(lease.read_text(encoding="utf-8"))["token"]
        except (OSError, ValueError, TypeError, KeyError) as exc:
            raise RuntimeError("shared Run ownership record is invalid") from exc
        if active_token != handle.owner_token:
            raise RuntimeError("shared Run handle belongs to an older ownership generation")
    if _container_state(handle.container) == "running":
        if handle.mode != "rcp":
            return
        from muteki.solver.control_receiver import ControlReceiver
        if ControlReceiver.instance().has_link(handle.supervisor_run_id):
            if handle.container_scope == "shared":
                from muteki.solver.control_client import register_owner
                if not register_owner(
                    handle.supervisor_run_id, handle.run_id, handle.owner_token,
                    handle.to_container_path(handle.run_workspace),
                ):
                    raise RuntimeError("shared runtime rejected Run re-registration")
            return
        # A running PID namespace with no authenticated reverse link is an orphan,
        # not a healthy keepalive.  Fall through to ensure_container, which proves
        # removal and issues a fresh one-shot bootstrap for a new supervisor.
    fresh = ensure_container(handle.run_id, handle.run_workspace,
                             image=handle.image, network=(handle.requested_network or handle.network),
                             memory=handle.memory, cpus=handle.cpus,
                             pids_limit=handle.pids_limit,
                             output_limit=(handle.resource_limits.output_limit if handle.resource_limits else None),
                             disk_limit=(handle.resource_limits.disk_limit if handle.resource_limits else None),
                             account_root=handle.account_source_root,
                             account_ids=getattr(handle, "account_ids", ()) or (),
                             container_scope=handle.container_scope,
                             shared_mount_root=handle.shared_mount_root,
                             account_projection_root=handle.account_projection_root,
                             bootstrap_root=handle.bootstrap_root,
                             vpn_config=handle.vpn_config,
                             worker_privilege=getattr(handle, "worker_privilege", "default") or "default")
    # A recreate can change every mounted/control identity.  Adopt the complete
    # fresh mapping so path translation and later recreations use the same roots
    # that Docker actually mounted.
    handle.run_workspace = fresh.run_workspace
    handle.host_workspace = fresh.host_workspace
    handle.container = fresh.container
    handle.image = fresh.image
    handle.network = fresh.network
    handle.requested_network = fresh.requested_network
    handle.memory = fresh.memory
    handle.cpus = fresh.cpus
    handle.pids_limit = fresh.pids_limit
    handle.resource_limits = fresh.resource_limits
    handle.account_root = fresh.account_root
    handle.account_source_root = fresh.account_source_root
    handle.account_ids = getattr(fresh, "account_ids", ())
    handle.worker_privilege = getattr(fresh, "worker_privilege", "default")
    handle.shared_mount_root = fresh.shared_mount_root
    handle.account_projection_root = fresh.account_projection_root
    handle.bootstrap_root = fresh.bootstrap_root
    handle.mode = fresh.mode
    handle.control_dir = fresh.control_dir
    handle.token = fresh.token
    handle.control_run_id = fresh.control_run_id
    handle.container_scope = fresh.container_scope
    handle.owner_token = fresh.owner_token
    handle.vpn_config = fresh.vpn_config


def run_cli_container(driver: CliDriver, argv: list[str], *, handle: ContainerHandle,
                      cwd: str, timeout: int, env: Optional[dict] = None,
                      stdin_text: Optional[str] = None) -> CliResult:
    """Non-streaming worker run inside the run container. Dispatches to rcp (default)
    or the legacy docker-exec backend based on handle.mode."""
    if handle.mode != "rcp" and stdin_text is not None:
        raise SecurePromptUnsupported(
            "legacy docker-exec cannot prove an exact stdin prompt reached the "
            "inner worker; use the RCP container backend")
    _ensure_alive(handle)
    cont_cwd = handle.to_container_cwd(cwd)
    if handle.mode == "rcp":
        from muteki.solver.control_client import run_cli_rcp
        cont_argv = _containerize_argv(driver.name, argv)
        check_process_launch(
            cont_argv, cwd=cont_cwd, env=env, stdin_text=stdin_text,
            source="container-run")
        tag = uuid.uuid4().hex[:12]
        rec = _RUNTIME_REGISTRY.create(handle=handle, tag=tag, driver=driver.name,
                                       cwd=cont_cwd, argv=cont_argv)
        _RUNTIME_REGISTRY.mark(rec, status="running")
        rcp_kwargs = {"stdin_text": stdin_text} if stdin_text is not None else {}
        res = run_cli_rcp(driver, cont_argv, run_id=handle.supervisor_run_id,
                          container_cwd=cont_cwd, timeout=timeout, env=env,
                          resource_limits=handle.resource_limits,
                          owner_run_id=handle.run_id if handle.container_scope == "shared" else "",
                          owner_token=handle.owner_token if handle.container_scope == "shared" else "",
                          owner_workspace=handle.to_container_path(handle.run_workspace)
                          if handle.container_scope == "shared" else "", **rcp_kwargs)
        rs = res.runtime_status or {}
        observed_rc = rs.get("rc")
        if res.returncode is None and observed_rc is not None:
            res.returncode = int(observed_rc)
        status = rs.get("status") or ("oom" if res.oom_killed else "output_limit" if res.output_limit else "disk_limit" if res.disk_limit else "timeout" if res.timed_out else "finished")
        res.runtime_status = {**rs, **_RUNTIME_REGISTRY.finish(
            rec, status=status, rc=observed_rc,
            timed_out=res.timed_out, oom_killed=res.oom_killed,
            error=(res.raw_stderr or "").strip()[:300])}
        return res
    return _DockerExecBackend.run(driver, argv, handle=handle, cwd=cwd,
                                  timeout=timeout, env=env, stdin_text=stdin_text)


def run_cli_streaming_container(
    driver: CliDriver, argv: list[str], *, handle: ContainerHandle,
    cwd: str, timeout: int,
    on_step: "Callable[[StreamStep], None]",
    env: Optional[dict] = None,
    cancel_event: "Optional[threading.Event]" = None,
    on_proc: "Optional[Callable[[object], None]]" = None,
    on_start_uncertain: "Optional[Callable[[], None]]" = None,
    on_stdin_delivered: "Optional[Callable[[], None]]" = None,
    on_stdin_uncertain: "Optional[Callable[[], None]]" = None,
    steer_event: "Optional[threading.Event]" = None,
    paused_event: "Optional[threading.Event]" = None,
    stdin_text: Optional[str] = None,
) -> CliResult:
    """Streaming worker run inside the run container — mirrors
    cli_driver.run_cli_streaming (cancel/steer/pause). Dispatches to rcp (default,
    control over the UDS) or the legacy docker-exec backend based on handle.mode.

    `paused_event` is accepted for signature parity with the host runner; in container
    mode the timeout is enforced supervisor-side (its kill-timer is pause-aware — it
    stops the clock on STOP and resumes on CONT), so the host does not run a
    wall-clock kill loop here and the event is forwarded for any backend that wants it."""
    if handle.mode != "rcp" and stdin_text is not None:
        # docker -i accepting bytes only proves its client-side pipe was filled; a
        # missing container/bad cwd/inner exec failure can still prevent the worker
        # from ever receiving them.  Without an inner ACK there is no honest exact
        # delivery receipt, so the emergency legacy backend fails closed.
        raise SecurePromptUnsupported(
            "legacy docker-exec cannot prove an exact stdin prompt reached the "
            "inner worker; use the RCP container backend")
    _ensure_alive(handle)
    cont_cwd = handle.to_container_cwd(cwd)
    if handle.mode == "rcp":
        from muteki.solver.control_client import run_cli_streaming_rcp
        cont_argv = _containerize_argv(driver.name, argv)
        check_process_launch(
            cont_argv, cwd=cont_cwd, env=env, stdin_text=stdin_text,
            source="container-stream")
        tag = uuid.uuid4().hex[:12]
        rec = _RUNTIME_REGISTRY.create(handle=handle, tag=tag, driver=driver.name,
                                       cwd=cont_cwd, argv=cont_argv)
        _RUNTIME_REGISTRY.mark(rec, status="running")

        # adapt on_proc so the registry status tracks pause/kill the way the
        # docker-exec path did (the rcp proc has no runtime_record of its own).
        def _on_proc(proc: object) -> None:
            if on_proc is not None:
                on_proc(proc)

        rcp_kwargs = {"stdin_text": stdin_text} if stdin_text is not None else {}
        if on_stdin_delivered is not None:
            rcp_kwargs["on_stdin_delivered"] = on_stdin_delivered
        if on_stdin_uncertain is not None:
            rcp_kwargs["on_stdin_uncertain"] = on_stdin_uncertain
        res = run_cli_streaming_rcp(
            driver, cont_argv, run_id=handle.supervisor_run_id,
            container_cwd=cont_cwd, timeout=timeout, on_step=on_step, env=env,
            owner_run_id=handle.run_id if handle.container_scope == "shared" else "",
            owner_token=handle.owner_token if handle.container_scope == "shared" else "",
            owner_workspace=handle.to_container_path(handle.run_workspace)
            if handle.container_scope == "shared" else "",
            cancel_event=cancel_event, on_proc=_on_proc,
            on_start_uncertain=on_start_uncertain, steer_event=steer_event,
            paused_event=paused_event, resource_limits=handle.resource_limits,
            **rcp_kwargs)
        rs = res.runtime_status or {}
        if res.returncode is None and rs.get("rc") is not None:
            res.returncode = int(rs["rc"])
        res.runtime_status = {**rs, **_RUNTIME_REGISTRY.finish(
            rec, status=rs.get("status", "finished"), rc=rs.get("rc"),
            timed_out=res.timed_out, oom_killed=res.oom_killed,
            cancelled=res.cancelled, steered=res.steered,
            error=(res.raw_stderr or "").strip()[:300])}
        return res
    return _DockerExecBackend.run_streaming(
        driver, argv, handle=handle, cwd=cwd, timeout=timeout, on_step=on_step,
        env=env, cancel_event=cancel_event, on_proc=on_proc, steer_event=steer_event,
        on_stdin_delivered=on_stdin_delivered,
        on_stdin_uncertain=on_stdin_uncertain,
        stdin_text=stdin_text)


# ── LEGACY docker-exec backend (emergency fallback only) ──────────────────────
# Kept behind MUTEKI_WORKER_BACKEND=container_dockerexec. The host shells `docker
# exec` per worker into a `sleep infinity` container; per-worker control is
# `pkill -f muteki_wtag_<tag>` inside the container. To be removed once rcp settles.

class _ContainerProc:
    """Wraps ONE worker's `docker exec`. Control maps to `pkill -<SIG> -f
    MUTEKI_WTAG=<tag>` INSIDE the container, so kill/pause/resume hit only this
    worker's process tree — a sibling in the same container is untouched."""

    def __init__(self, container: str, tag: str, client_proc: subprocess.Popen,
                 runtime_record: Optional[RuntimeExecRecord] = None):
        self.container = container
        self.tag = tag
        self._client_proc = client_proc
        self._runtime_record = runtime_record

    @property
    def pid(self):
        return self._client_proc.pid

    def _pkill(self, sig: str) -> None:
        _docker("exec", self.container, "pkill", sig, "-f", f"muteki_wtag_{self.tag}",
                timeout=15)

    def _container_signal(self, sig: int) -> None:
        if sig == getattr(_signal, "SIGSTOP", 17):
            if self._runtime_record is not None:
                _RUNTIME_REGISTRY.mark(self._runtime_record, status="paused")
            self._pkill("-STOP")
        elif sig == getattr(_signal, "SIGCONT", 19):
            if self._runtime_record is not None:
                _RUNTIME_REGISTRY.mark(self._runtime_record, status="running")
            self._pkill("-CONT")
        else:
            if self._runtime_record is not None:
                _RUNTIME_REGISTRY.mark(self._runtime_record, status="killing", cancelled=True)
            self._pkill("-KILL")

    send_signal = _container_signal

    def kill(self) -> None:
        if self._runtime_record is not None:
            _RUNTIME_REGISTRY.mark(self._runtime_record, status="killing", cancelled=True)
        self._pkill("-KILL")
        try:
            self._client_proc.kill()
        except Exception:
            pass


class _DockerExecBackend:
    """The previous host-side `docker exec` worker transport, isolated here as an
    emergency fallback. All the hard-won docker-exec bug fixes live in this class."""

    @staticmethod
    def _exec_argv(handle: ContainerHandle, argv: list[str], *, container_cwd: str,
                   env: Optional[dict], driver_name: str, tag: str, timeout: int,
                   has_stdin: bool = False) -> list[str]:
        """Build `docker exec -w <cwd> -e ... <container> sh -c 'exec timeout -s KILL <N> <argv> </dev/null'`.

        - NO `setsid`: the worker MUST stay the docker-exec FOREGROUND process. setsid
          detaches it → docker exec returns in ~2s with an EOF'd stdout while the worker
          keeps running orphaned (every worker turn looked like a 2-10s empty exit).
        - container-side `timeout -s KILL <N>s` is the authoritative wall-clock cap.
        - `< /dev/null`: docker exec without -i leaves the CLIs waiting on stdin
          (codex hangs); /dev/null = instant EOF.
        - MUTEKI_WTAG env + the `muteki_wtag_<tag>` sentinel `$0` let `pkill -f <tag>`
          target ONLY this worker's tree for per-worker kill/pause.
        """
        argv = _containerize_argv(driver_name, argv)
        cmd = ["docker", "exec"]
        if has_stdin:
            # Keep docker's stdin attached so the host can pipe the prompt.  The
            # plaintext is passed via subprocess input, never interpolated here.
            cmd.append("-i")
        cmd += ["-w", container_cwd, "-e", f"MUTEKI_WTAG={tag}"]
        if env:
            for k, v in env.items():
                if k == "HOME":
                    if str(v).startswith(f"{CONTAINER_WORKSPACE}/"):
                        cmd += ["-e", f"{k}={v}"]
                    continue
                if k.startswith((
                    "MUTEKI_", "ANTHROPIC_", "CLAUDE_", "CODEX_", "CURSOR_", "OPENAI_",
                    "PI_", "KIMI_", "GROK_", "XAI_", "OPENCODE_", "DEEPSEEK_", "XDG_"
                )):
                    cmd += ["-e", f"{k}={v}"]
            if env.get("MUTEKI_CHALLENGE_MODE") == "pentest":
                from muteki.solver.browser_coord import managed_browser_worker_path
                cmd += ["-e", f"PATH={managed_browser_worker_path(env)}"]
        cmd.append(handle.container)
        prelude = [
            'if [ -r "$CLAUDE_CODE_OAUTH_TOKEN_FILE" ]; then '
            'export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$CLAUDE_CODE_OAUTH_TOKEN_FILE")"; fi',
            'if [ -r "$ANTHROPIC_AUTH_TOKEN_FILE" ]; then '
            'export ANTHROPIC_AUTH_TOKEN="$(cat "$ANTHROPIC_AUTH_TOKEN_FILE")"; fi',
            'if [ -r "$CURSOR_API_KEY_FILE" ]; then '
            'export CURSOR_API_KEY="$(cat "$CURSOR_API_KEY_FILE")"; fi',
            'if [ -r "$ANTHROPIC_API_KEY_FILE" ]; then '
            'export ANTHROPIC_API_KEY="$(cat "$ANTHROPIC_API_KEY_FILE")"; fi',
            'if [ -r "$OPENAI_API_KEY_FILE" ]; then '
            'export OPENAI_API_KEY="$(cat "$OPENAI_API_KEY_FILE")"; fi',
            'if [ -r "$OPENCODE_API_KEY_FILE" ]; then '
            'export OPENCODE_API_KEY="$(cat "$OPENCODE_API_KEY_FILE")"; fi',
            'if [ -r "$DEEPSEEK_API_KEY_FILE" ]; then '
            'export DEEPSEEK_API_KEY="$(cat "$DEEPSEEK_API_KEY_FILE")"; fi',
            'if [ -r "$KIMI_MODEL_API_KEY_FILE" ]; then '
            'export KIMI_MODEL_API_KEY="$(cat "$KIMI_MODEL_API_KEY_FILE")"; fi',
            'if [ -r "$XAI_API_KEY_FILE" ]; then '
            'export XAI_API_KEY="$(cat "$XAI_API_KEY_FILE")"; fi',
        ]
        stdin_redirect = "" if has_stdin else " < /dev/null"
        inner = (
            "; ".join(prelude)
            + f"; exec timeout -s KILL {max(1, int(timeout))}s {shlex.join(argv)}"
            + stdin_redirect
        )
        cmd += ["sh", "-c", inner, f"muteki_wtag_{tag}"]
        return cmd

    @staticmethod
    def run(driver: CliDriver, argv: list[str], *, handle: ContainerHandle,
            cwd: str, timeout: int, env: Optional[dict] = None,
            stdin_text: Optional[str] = None) -> CliResult:
        tag = uuid.uuid4().hex[:12]
        cont_cwd = handle.to_container_cwd(cwd)
        full = _DockerExecBackend._exec_argv(handle, argv, container_cwd=cont_cwd, env=env,
                                             driver_name=driver.name, tag=tag, timeout=timeout,
                                             has_stdin=stdin_text is not None)
        rec = _RUNTIME_REGISTRY.create(
            handle=handle, tag=tag, driver=driver.name,
            cwd=cont_cwd, argv=_containerize_argv(driver.name, argv))
        _RUNTIME_REGISTRY.mark(rec, status="running")
        t0 = time.time()
        oom_before = _oom_kill_count(handle.container)
        try:
            input_kwargs = ({"input": stdin_text} if stdin_text is not None
                            else {"stdin": subprocess.DEVNULL})
            proc = subprocess.run(
                full, capture_output=True, text=True, encoding="utf-8",
                errors="replace", timeout=timeout + 15, **input_kwargs)
        except subprocess.TimeoutExpired as e:
            _docker("exec", handle.container, "pkill", "-KILL", "-f", f"muteki_wtag_{tag}",
                    timeout=15)
            out = e.stdout if isinstance(e.stdout, str) else ""
            err = e.stderr if isinstance(e.stderr, str) else ""
            res = driver.parse(out or "", err or "")
            res.timed_out = True
            res.elapsed_s = time.time() - t0
            res.runtime_status = _RUNTIME_REGISTRY.finish(
                rec, status="timeout", timed_out=True, error="host timeout")
            return res
        res = driver.parse(proc.stdout or "", proc.stderr or "")
        finalize_cli_result(
            res, driver_name=driver.name,
            stdout=proc.stdout or "", stderr=proc.stderr or "",
            returncode=proc.returncode)
        if proc.returncode == 137:
            oom_after = _oom_kill_count(handle.container)
            if (oom_before is not None and oom_after is not None
                    and oom_after > oom_before):
                res.oom_killed = True
            else:
                res.timed_out = True
        res.elapsed_s = time.time() - t0
        status = "oom" if res.oom_killed else "timeout" if res.timed_out else "finished"
        res.runtime_status = _RUNTIME_REGISTRY.finish(
            rec, status=status, rc=proc.returncode,
            timed_out=res.timed_out, oom_killed=res.oom_killed,
            error=(proc.stderr or "").strip()[:300])
        return res

    @staticmethod
    def run_streaming(
        driver: CliDriver, argv: list[str], *, handle: ContainerHandle,
        cwd: str, timeout: int,
        on_step: "Callable[[StreamStep], None]",
        env: Optional[dict] = None,
        cancel_event: "Optional[threading.Event]" = None,
        on_proc: "Optional[Callable[[object], None]]" = None,
        steer_event: "Optional[threading.Event]" = None,
        on_stdin_delivered: "Optional[Callable[[], None]]" = None,
        on_stdin_uncertain: "Optional[Callable[[], None]]" = None,
        stdin_text: Optional[str] = None,
    ) -> CliResult:
        tag = uuid.uuid4().hex[:12]
        cont_cwd = handle.to_container_cwd(cwd)
        full = _DockerExecBackend._exec_argv(handle, argv, container_cwd=cont_cwd, env=env,
                                             driver_name=driver.name, tag=tag, timeout=timeout,
                                             has_stdin=stdin_text is not None)
        rec = _RUNTIME_REGISTRY.create(
            handle=handle, tag=tag, driver=driver.name,
            cwd=cont_cwd, argv=_containerize_argv(driver.name, argv))
        _RUNTIME_REGISTRY.mark(rec, status="running")

        t0 = time.time()
        oom_before = _oom_kill_count(handle.container)
        client = subprocess.Popen(full, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                  stdin=(subprocess.PIPE if stdin_text is not None
                                         else subprocess.DEVNULL),
                                  text=True, encoding="utf-8", errors="replace", bufsize=1)
        proc = _ContainerProc(handle.container, tag, client, runtime_record=rec)
        proc_registered = True
        if on_proc is not None:
            try:
                on_proc(proc)
            except Exception:
                proc_registered = False

        stdin_thread: "Optional[threading.Thread]" = None
        stdin_notice_lock = threading.Lock()
        stdin_notice_sent = False

        def _notify_stdin(callback: "Optional[Callable[[], None]]") -> None:
            nonlocal stdin_notice_sent
            with stdin_notice_lock:
                if stdin_notice_sent:
                    return
                stdin_notice_sent = True
            if callable(callback):
                try:
                    callback()
                except Exception:
                    pass

        if stdin_text is not None and proc_registered and not (
            cancel_event is not None and cancel_event.is_set()
        ):
            def _feed_stdin() -> None:
                delivered = False
                try:
                    if client.stdin is not None:
                        client.stdin.write(stdin_text)
                        client.stdin.close()
                        delivered = True
                except (BrokenPipeError, OSError, ValueError):
                    pass
                _notify_stdin(
                    on_stdin_delivered if delivered else on_stdin_uncertain)

            stdin_thread = threading.Thread(
                target=_feed_stdin, name="container-secret-stdin", daemon=True)
            stdin_thread.start()
        elif client.stdin is not None:
            try:
                client.stdin.close()
            except (OSError, ValueError):
                pass
            _notify_stdin(on_stdin_uncertain)

        cancelled = False
        steered = False
        watcher_stop = threading.Event()

        def _watch() -> None:
            nonlocal cancelled, steered
            while not watcher_stop.is_set():
                if cancel_event is not None and cancel_event.is_set():
                    cancelled = True
                    proc.kill()
                    return
                if steer_event is not None and steer_event.is_set():
                    steered = True
                    proc.kill()
                    return
                if time.time() - t0 > timeout:
                    return
                watcher_stop.wait(0.1)

        watcher = None
        if cancel_event is not None or steer_event is not None:
            watcher = threading.Thread(target=_watch, name="container-control-watch", daemon=True)
            watcher.start()

        out_lines: list[str] = []
        timed_out = False
        try:
            assert client.stdout is not None
            for line in client.stdout:
                out_lines.append(line)
                if cancel_event is not None and cancel_event.is_set():
                    cancelled = True
                    proc.kill()
                    break
                if steer_event is not None and steer_event.is_set():
                    steered = True
                    proc.kill()
                    break
                if time.time() - t0 > timeout:
                    proc.kill()
                    timed_out = True
                    break
                try:
                    steps = driver.parse_stream_steps(line)  # #18: ALL blocks, not just first
                except Exception:
                    steps = []
                for step in steps:
                    try:
                        on_step(step)
                    except Exception:
                        pass
            client.wait(timeout=max(1, timeout + 15 - int(time.time() - t0)))
        except subprocess.TimeoutExpired:
            proc.kill()
            timed_out = True
        except Exception:
            proc.kill()
        finally:
            watcher_stop.set()
            if watcher is not None:
                watcher.join(timeout=1)
            if stdin_thread is not None:
                stdin_thread.join(timeout=1)
                if stdin_thread.is_alive():
                    _notify_stdin(on_stdin_uncertain)
        stderr = ""
        try:
            stderr = client.stderr.read() if client.stderr else ""
        except Exception:
            pass
        elapsed = time.time() - t0
        rc = client.poll()
        oom_killed = False
        if oom_before is not None:
            oom_after = _oom_kill_count(handle.container)
            if oom_after is not None and oom_after > oom_before:
                oom_killed = True
        if oom_killed:
            timed_out = False
        elif rc == 137:
            timed_out = True
        if os.environ.get("MUTEKI_CONTAINER_DEBUG") and (not out_lines or stderr.strip()):
            try:
                _dbg = f"/tmp/muteki_container_diag/{tag}.log"
                os.makedirs("/tmp/muteki_container_diag", exist_ok=True)
                with open(_dbg, "w") as _f:
                    # Prompts, argv, stdout and stderr may contain an exact
                    # materialised operator secret. This low-level layer does not
                    # own the runtime credentials, so debug diagnostics are metadata
                    # only—never attempt heuristic partial logging.
                    _f.write(
                        f"argv0={os.path.basename(str(full[0])) if full else ''}\n"
                        f"argc={len(full)}\n"
                        f"rc={client.poll()}\n"
                        f"elapsed={time.time()-t0:.2f}s\n"
                        f"stdout_lines={len(out_lines)}\n"
                        f"stderr_present={bool(stderr.strip())}\n")
            except Exception:
                pass
        stdout = "".join(out_lines)
        res = driver.parse(stdout, stderr or "")
        res.timed_out = timed_out
        res.oom_killed = oom_killed
        res.cancelled = cancelled
        res.steered = steered
        finalize_cli_result(
            res, driver_name=driver.name, stdout=stdout,
            stderr=stderr or "", returncode=rc)
        res.elapsed_s = elapsed
        if oom_killed:
            status = "oom"
        elif timed_out:
            status = "timeout"
        elif cancelled:
            status = "cancelled"
        elif steered:
            status = "steered"
        else:
            status = "finished"
        res.runtime_status = _RUNTIME_REGISTRY.finish(
            rec, status=status, rc=rc, timed_out=timed_out,
            oom_killed=oom_killed, cancelled=cancelled, steered=steered,
            error=(stderr or "").strip()[:300])
        return res
