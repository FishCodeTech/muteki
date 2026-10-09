"""Process-group supervision and the orphan ledger for Runtime child processes.

Every supervised engine runs under a group-leader guardian in its own
session. The guardian stays alive while descendants are being stopped and
also stops the group when its owning server dies. Stopping an engine means
stopping its group:

``terminate_tree`` sends SIGTERM to the group in repeated passes for a grace
period, then SIGKILL.  Before every signal it re-reads the group leader's
start time and refuses to signal when the pid now belongs to another
process.  The supervising owner (``require_leader=False``) still signals a
group whose leader has exited while that group id is live, because neither
Linux nor XNU reissues a pid that is still a process-group id.  Orphan
reaping does not: a dead leader does not prove who the remaining members
are.  Pid/pgid <= 1 and the server's own group are refused.

``ProcessLedger`` persists ``pid + pgid + start time + owner + command`` for
every live supervised child (JSON under ``<state>/runtime/external_agents``,
mode 0600, ``flock``-serialized).  ``reap_orphans`` runs at server startup and
only terminates entries whose owner server instance is no longer alive and
whose recorded member still has the same start time, group and command.
Missing/mismatched identities are forgotten without signalling, as in T3;
the report explicitly distinguishes dropped records from stopped groups.
"""

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import fcntl
from functools import lru_cache
import json
import logging
import os
import signal
import shutil
import sys
import threading
import time
import uuid
from dataclasses import asdict, dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Iterator, Optional, Sequence

from muteki.platform.contracts.base import utcnow

from .capabilities import ProbeCommandErrorCode, run_probe_command

_LOG = logging.getLogger(__name__)

LEDGER_VERSION = 1
#: SIGTERM passes continue this long before SIGKILL.
DEFAULT_GRACE_S = 5.0
#: Interval between SIGTERM passes.
TERM_PASS_INTERVAL_S = 1.0
#: How long to wait for the group to disappear after SIGKILL.
KILL_WAIT_S = 3.0
_POLL_S = 0.05
_IDENTITY_TIMEOUT_S = 5.0


class ProcessIdentityError(RuntimeError):
    """The start time of a live process could not be read."""

    code = "process.identity_unavailable"


class ProcessLedgerError(RuntimeError):
    """The ledger file is unreadable or has an unknown format."""

    code = "process.ledger_invalid"


# ---------------------------------------------------------------------------
# Process identity
# ---------------------------------------------------------------------------


def _linux_boot_id() -> str:
    return Path("/proc/sys/kernel/random/boot_id").read_text(encoding="ascii").strip()


@lru_cache(maxsize=1)
def _darwin_proc_info() -> tuple[Any, Any]:
    import ctypes

    # Layout and flavor from the installed macOS SDK's sys/proc_info.h.
    # ps lstart has only seconds; libproc exposes the birth time in usec.
    class ProcBsdInfo(ctypes.Structure):
        _fields_ = [
            ("prefix", ctypes.c_uint32 * 12),
            ("comm", ctypes.c_char * 16),
            ("name", ctypes.c_char * 32),
            ("suffix", ctypes.c_uint32 * 6),
            ("start_sec", ctypes.c_uint64),
            ("start_usec", ctypes.c_uint64),
        ]

    library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    query = library.proc_pidinfo
    query.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
    query.restype = ctypes.c_int
    return query, ProcBsdInfo


def _darwin_start_time(pid: int) -> Optional[str]:
    import ctypes
    import errno

    try:
        query, info_type = _darwin_proc_info()
        info = info_type()
        ctypes.set_errno(0)
        size = query(pid, 3, 0, ctypes.byref(info), ctypes.sizeof(info))
    except (OSError, AttributeError) as exc:
        raise ProcessIdentityError(f"libproc birth identity unavailable: {exc}") from exc
    if size == 0 and ctypes.get_errno() == errno.ESRCH:
        return None
    if size != ctypes.sizeof(info) or info.prefix[3] != pid:
        raise ProcessIdentityError(f"proc_pidinfo({pid}) returned {size} bytes, errno={ctypes.get_errno()}")
    return f"darwin:{info.start_sec}:{info.start_usec}"


async def _posix_ps_start_time(pid: int) -> Optional[str]:
    result = await run_probe_command(
        ["ps", "-o", "lstart=", "-p", str(pid)],
        timeout=_IDENTITY_TIMEOUT_S,
        env={"LC_ALL": "C", "LANG": "C", "PATH": os.environ.get("PATH") or "/bin:/usr/bin"},
    )
    text = result.stdout.strip()
    if result.error_code is ProbeCommandErrorCode.NONZERO_EXIT and not text:
        return None
    if not result.ok:
        raise ProcessIdentityError(result.describe())
    return f"ps:{text}" if text else None


async def _recorded_start_time(pid: int, recorded: str) -> Optional[str]:
    # Retain the declared identity format of pre-libproc ledgers. This is
    # format compatibility, never a fallback after a failed native lookup.
    if sys.platform == "darwin" and recorded.startswith("ps:"):
        return await _posix_ps_start_time(pid)
    return await process_start_time(pid)


async def process_start_time(pid: int) -> Optional[str]:
    """Opaque start-time token of ``pid``; ``None`` when no such process exists.

    Linux reads ``/proc/<pid>/stat`` (boot id + start ticks); macOS reads
    libproc's microsecond birth time; other POSIX systems use ``ps``. Raises
    ``ProcessIdentityError`` when the process exists but cannot be read.
    """
    if pid <= 0:
        return None
    if sys.platform.startswith("linux"):
        try:
            stat = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8", errors="replace")
        except (FileNotFoundError, ProcessLookupError):
            return None
        except OSError as exc:
            raise ProcessIdentityError(f"read /proc/{pid}/stat: {exc}") from exc
        fields = stat[stat.rindex(")") + 2:].split()
        # Field 22 of /proc/<pid>/stat; ``fields`` starts at field 3.
        return f"linux:{_linux_boot_id()}:{fields[19]}"
    if sys.platform == "darwin":
        return await asyncio.to_thread(_darwin_start_time, pid)
    return await _posix_ps_start_time(pid)


async def _group_state(pgid: int) -> str:
    """``present`` / ``absent`` / ``foreign`` (exists, owned by another user).

    A group whose remaining members are all zombies counts as ``absent``:
    nothing in it can run, and macOS answers ``killpg`` on such a group with
    EPERM, which would otherwise look like a foreign group.
    """
    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        return "absent"
    except PermissionError:
        return await _classify_unsignalable_group(pgid)
    if sys.platform.startswith("linux"):
        return await _classify_unsignalable_group(pgid)
    return "present"


async def _classify_unsignalable_group(pgid: int) -> str:
    if sys.platform.startswith("linux"):
        from .process_guardian import _linux_group_members
        try:
            members = _linux_group_members(pgid)
        except OSError as exc:
            raise ProcessIdentityError(f"read /proc for process group {pgid}: {exc}") from exc
        live = [owner for _pid, owner, state in members if not state.startswith("Z")]
        if not live:
            return "absent"
        return "foreign" if any(owner != os.getuid() for owner in live) else "present"
    result = await run_probe_command(
        ["ps", "-axo", "pgid=,uid=,stat="],
        timeout=_IDENTITY_TIMEOUT_S,
        env={"LC_ALL": "C", "LANG": "C", "PATH": os.environ.get("PATH") or "/bin:/usr/bin"},
    )
    if not result.ok:
        raise ProcessIdentityError(result.describe())
    uid = os.getuid()
    members: list[tuple[int, str]] = []
    for line in result.stdout.splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[0] == str(pgid):
            members.append((int(parts[1]), parts[2]))
    live = [(owner, stat) for owner, stat in members if not stat.startswith("Z")]
    if not live:
        return "absent"
    return "foreign" if any(owner != uid for owner, _ in live) else "present"


# ---------------------------------------------------------------------------
# Owner / record
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class ServerInstance:
    """Identity of one running Muteki server process."""

    instance_id: str
    pid: int
    start_time: str


@dataclass(frozen=True)
class ProcessOwner:
    server: ServerInstance
    adapter_id: str
    session_id: str = ""


@dataclass(frozen=True)
class ProcessRecord:
    pid: int
    pgid: int
    start_time: str
    owner: ProcessOwner
    command: tuple[str, ...]
    label: str = ""
    recorded_at: str = field(default_factory=lambda: utcnow().isoformat())
    command_fingerprint: str = ""
    containment: str = "process_group"

    def to_json(self) -> dict[str, Any]:
        data = asdict(self)
        data["command"] = list(self.command)
        return data

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> "ProcessRecord":
        owner = data["owner"]
        server = owner["server"]
        return cls(
            pid=int(data["pid"]),
            pgid=int(data["pgid"]),
            start_time=str(data["start_time"]),
            owner=ProcessOwner(
                server=ServerInstance(
                    instance_id=str(server["instance_id"]),
                    pid=int(server["pid"]),
                    start_time=str(server["start_time"]),
                ),
                adapter_id=str(owner["adapter_id"]),
                session_id=str(owner.get("session_id") or ""),
            ),
            command=tuple(str(item) for item in data["command"]),
            command_fingerprint=str(data.get("command_fingerprint") or ""),
            containment=str(data.get("containment") or "process_group"),
            label=str(data.get("label") or ""),
            recorded_at=str(data.get("recorded_at") or ""),
        )


_SERVER_LOCK = threading.Lock()
_SERVER: Optional[ServerInstance] = None


async def current_server_instance() -> ServerInstance:
    """This process's server identity (instance id is fixed per process)."""
    global _SERVER
    with _SERVER_LOCK:
        if _SERVER is not None and _SERVER.pid == os.getpid():
            return _SERVER
    start = await process_start_time(os.getpid())
    if start is None:
        raise ProcessIdentityError("own process start time is unavailable")
    with _SERVER_LOCK:
        if _SERVER is None or _SERVER.pid != os.getpid():
            _SERVER = ServerInstance(
                instance_id=f"muteki-{uuid.uuid4().hex}", pid=os.getpid(), start_time=start)
        return _SERVER


async def server_instance_alive(server: ServerInstance) -> bool:
    current = _SERVER
    if current is not None and current.instance_id == server.instance_id:
        return current.pid == os.getpid()
    return await _recorded_start_time(server.pid, server.start_time) == server.start_time


# ---------------------------------------------------------------------------
# Ledger
# ---------------------------------------------------------------------------


def default_ledger_path(state_root: Optional[str | Path] = None) -> Path:
    root = Path(state_root) if state_root is not None else Path(
        os.environ.get("MUTEKI_STATE_ROOT") or "state")
    return root / "runtime" / "external_agents" / "process_ledger.json"


class ProcessLedger:
    """JSON ledger of live supervised children, shared by server processes."""

    def __init__(self, path: str | Path) -> None:
        self.path = Path(path)
        self._lock = threading.RLock()

    @contextlib.contextmanager
    def _locked(self) -> Iterator[None]:
        with self._lock:
            self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            lock_path = self.path.with_suffix(self.path.suffix + ".lock")
            fd = os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o600)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX)
                yield
            finally:
                fcntl.flock(fd, fcntl.LOCK_UN)
                os.close(fd)

    def _read(self) -> list[ProcessRecord]:
        try:
            raw = self.path.read_text(encoding="utf-8")
        except FileNotFoundError:
            return []
        try:
            data = json.loads(raw)
            if not isinstance(data, dict) or data.get("version") != LEDGER_VERSION:
                raise ValueError(f"unsupported ledger version {data.get('version') if isinstance(data, dict) else type(data).__name__}")
            return [ProcessRecord.from_json(item) for item in data.get("entries") or []]
        except (ValueError, KeyError, TypeError) as exc:
            raise ProcessLedgerError(f"{self.path}: {type(exc).__name__}: {exc}") from exc

    def _write(self, records: Sequence[ProcessRecord]) -> None:
        payload = json.dumps(
            {"version": LEDGER_VERSION, "entries": [record.to_json() for record in records]},
            ensure_ascii=False, indent=2)
        tmp = self.path.with_name(f".{self.path.name}.{os.getpid()}.{uuid.uuid4().hex}.tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp, self.path)

    def entries(self) -> list[ProcessRecord]:
        with self._locked():
            return self._read()

    def add(self, record: ProcessRecord) -> None:
        with self._locked():
            records = [item for item in self._read() if item.pid != record.pid]
            records.append(record)
            self._write(records)

    def remove(self, record: ProcessRecord) -> bool:
        with self._locked():
            records = self._read()
            kept = [item for item in records
                    if not (item.pid == record.pid and item.start_time == record.start_time)]
            if len(kept) == len(records):
                return False
            self._write(kept)
            return True


_LEDGER_LOCK = threading.Lock()
_DEFAULT_LEDGER: Optional[ProcessLedger] = None


def configure_process_ledger(path: Optional[str | Path]) -> Optional[ProcessLedger]:
    """Set the ledger used by ``spawn_supervised`` when none is passed.

    The server configures it at startup; without it children are still
    group-supervised but not recorded for cross-restart reaping.
    """
    global _DEFAULT_LEDGER
    with _LEDGER_LOCK:
        _DEFAULT_LEDGER = ProcessLedger(path) if path is not None else None
        return _DEFAULT_LEDGER


def default_process_ledger() -> Optional[ProcessLedger]:
    with _LEDGER_LOCK:
        return _DEFAULT_LEDGER


# ---------------------------------------------------------------------------
# Termination
# ---------------------------------------------------------------------------


class TerminateOutcome(str, Enum):
    TERMINATED = "terminated"
    KILLED = "killed"
    ALREADY_EXITED = "already_exited"
    REFUSED_INVALID_PID = "refused_invalid_pid"
    REFUSED_OWN_GROUP = "refused_own_group"
    REFUSED_IDENTITY_MISMATCH = "refused_identity_mismatch"
    REFUSED_LEADER_GONE = "refused_leader_gone"
    REFUSED_FOREIGN_GROUP = "refused_foreign_group"
    SURVIVED_SIGKILL = "survived_sigkill"


_STOPPED = {TerminateOutcome.TERMINATED, TerminateOutcome.KILLED, TerminateOutcome.ALREADY_EXITED}


@dataclass(frozen=True)
class TerminateResult:
    outcome: TerminateOutcome
    pid: int
    pgid: int
    signals: tuple[str, ...] = ()
    detail: str = ""

    @property
    def code(self) -> str:
        return f"process.terminate.{self.outcome.value}"

    @property
    def stopped(self) -> bool:
        """The group no longer exists (whether or not we signalled it)."""
        return self.outcome in _STOPPED

    @property
    def stale(self) -> bool:
        """Drop this ledger entry instead of signalling it again.

        T3's persistent server ledger forgets missing/mismatched recorded
        members without signalling. ``stopped`` remains false when unknown
        survivors may still be alive; dropping identity is not termination.
        """
        return self.stopped or self.outcome in {
            TerminateOutcome.REFUSED_IDENTITY_MISMATCH,
            TerminateOutcome.REFUSED_LEADER_GONE,
        }


async def _leader_state(record: ProcessRecord) -> str:
    """``match`` / ``mismatch`` / ``leader_gone`` / ``gone`` / ``foreign``."""
    group = await _group_state(record.pgid)
    if group == "absent":
        return "gone"
    if group == "foreign":
        return "foreign"
    current = await _recorded_start_time(record.pid, record.start_time)
    if current is not None:
        if not record.start_time or current != record.start_time:
            return "mismatch"
        try:
            if os.getpgid(record.pid) != record.pgid:
                return "mismatch"
        except ProcessLookupError:
            return "leader_gone"
        if record.command_fingerprint and await _command_fingerprint(record.pid) != record.command_fingerprint:
            return "mismatch"
        return "match"
    return "leader_gone"


async def _wait_group_gone(record: ProcessRecord, timeout: float) -> bool:
    # asyncio's child watcher reaps our own exited leader, so a zombie does
    # not keep the group alive; orphans are reaped by init/launchd.
    deadline = time.monotonic() + timeout
    while True:
        if await _group_state(record.pgid) == "absent":
            return True
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return False
        await asyncio.sleep(min(_POLL_S, remaining))


async def terminate_tree(
    record: ProcessRecord,
    *,
    grace_s: float = DEFAULT_GRACE_S,
    require_leader: bool = True,
) -> TerminateResult:
    """Stop ``record``'s process group: SIGTERM passes, grace, then SIGKILL.

    ``require_leader=True`` (orphan reaping) refuses unless the leader pid is
    alive with the recorded start time, which is the only check that the
    group is still that process and not a reused pid.  The supervising owner
    passes ``require_leader=False`` so members that stayed in the recorded
    group are still stopped after the leader exits.  Descendants that left
    the group are never signalled: once the leader pid is gone they cannot
    be proven to belong to this record.
    """
    pid, pgid = record.pid, record.pgid
    sent: list[str] = []

    def result(outcome: TerminateOutcome, detail: str = "") -> TerminateResult:
        return TerminateResult(outcome=outcome, pid=pid, pgid=pgid, signals=tuple(sent), detail=detail)

    if pid <= 1 or pgid <= 1:
        return result(TerminateOutcome.REFUSED_INVALID_PID, f"pid={pid} pgid={pgid}")
    if pgid == os.getpgrp() or pid == os.getpid():
        return result(TerminateOutcome.REFUSED_OWN_GROUP, "record names the server's own process group")

    async def checked_signal(sig: signal.Signals) -> Optional[TerminateResult]:
        state = await _leader_state(record)
        if state == "mismatch":
            return result(TerminateOutcome.REFUSED_IDENTITY_MISMATCH,
                          f"pid {pid} no longer has start time {record.start_time!r}")
        if state == "foreign":
            return result(TerminateOutcome.REFUSED_FOREIGN_GROUP,
                          f"process group {pgid} belongs to another user")
        if state == "gone":
            return result(TerminateOutcome.TERMINATED if sent else TerminateOutcome.ALREADY_EXITED)
        if state == "leader_gone" and require_leader and not sent:
            return result(TerminateOutcome.REFUSED_LEADER_GONE,
                          f"leader {pid} exited; group {pgid} survivors cannot be matched to the record")
        try:
            os.killpg(pgid, sig)
        except ProcessLookupError:
            return result(TerminateOutcome.TERMINATED if sent else TerminateOutcome.ALREADY_EXITED)
        except PermissionError as exc:
            if await _group_state(pgid) == "absent":
                return result(TerminateOutcome.TERMINATED if sent else TerminateOutcome.ALREADY_EXITED)
            return result(TerminateOutcome.REFUSED_FOREIGN_GROUP,
                          f"{sig.name} to process group {pgid} not permitted: {exc}")
        sent.append(sig.name)
        return None

    deadline = time.monotonic() + max(0.0, grace_s)
    while True:
        refused = await checked_signal(signal.SIGTERM)
        if refused is not None:
            return refused
        pass_wait = min(TERM_PASS_INTERVAL_S, max(0.0, deadline - time.monotonic()))
        if await _wait_group_gone(record, pass_wait):
            return result(TerminateOutcome.TERMINATED)
        if time.monotonic() >= deadline:
            break
    refused = await checked_signal(signal.SIGKILL)
    if refused is not None:
        return refused
    if await _wait_group_gone(record, KILL_WAIT_S):
        return result(TerminateOutcome.KILLED)
    return result(TerminateOutcome.SURVIVED_SIGKILL,
                  f"process group {pgid} still exists {KILL_WAIT_S:g}s after SIGKILL")


# ---------------------------------------------------------------------------
# Spawn
# ---------------------------------------------------------------------------

_USE_DEFAULT_LEDGER: Any = object()


async def _command_fingerprint(pid: int) -> Optional[str]:
    """Compare complete command identity without persisting its contents."""
    if sys.platform.startswith("linux"):
        try:
            command = Path(f"/proc/{pid}/cmdline").read_bytes()
        except (FileNotFoundError, ProcessLookupError):
            return None
        except OSError as exc:
            raise ProcessIdentityError(f"read process command identity {pid}: {exc}") from exc
    else:
        result = await run_probe_command(
            ["/bin/ps", "-ww", "-o", "args=", "-p", str(pid)],
            timeout=_IDENTITY_TIMEOUT_S,
            env={"LC_ALL": "C", "LANG": "C", "PATH": "/bin:/usr/bin"})
        if result.error_code is ProbeCommandErrorCode.NONZERO_EXIT:
            return None
        if not result.ok:
            raise ProcessIdentityError(f"process command identity lookup failed ({result.error_code})")
        command = result.stdout.strip().encode()
    return hashlib.sha256(command).hexdigest() if command else None


async def _recorded_group_member(pgid: int) -> tuple[int, str, str]:
    """At spawn, a live member can witness a group whose wrapper already left."""
    candidates = [pgid]
    if await process_start_time(pgid) is None:
        if sys.platform.startswith("linux"):
            from .process_guardian import _linux_group_members
            candidates = [pid for pid, uid, state in _linux_group_members(pgid)
                          if uid == os.getuid() and not state.startswith("Z")]
        else:
            result = await run_probe_command(
                ["/bin/ps", "-axo", "pid=,pgid=,uid=,stat="],
                timeout=_IDENTITY_TIMEOUT_S,
                env={"LC_ALL": "C", "LANG": "C", "PATH": "/bin:/usr/bin"})
            if not result.ok:
                raise ProcessIdentityError(f"process group member lookup failed ({result.error_code})")
            candidates = []
            for line in result.stdout.splitlines():
                row = line.split()
                if len(row) == 4 and row[1] == str(pgid) and row[2] == str(os.getuid()) and not row[3].startswith("Z"):
                    candidates.append(int(row[0]))
    for pid in candidates:
        start = await process_start_time(pid)
        fingerprint = await _command_fingerprint(pid) if start else None
        try:
            matched = os.getpgid(pid) == pgid
        except ProcessLookupError:
            matched = False
        if start and fingerprint and matched and await process_start_time(pid) == start:
            return pid, start, fingerprint
    return pgid, "", ""


@dataclass
class SupervisedProcess:
    process: "asyncio.subprocess.Process"
    record: ProcessRecord
    ledger: Optional[ProcessLedger]
    termination: Optional[TerminateResult] = None

    @property
    def pid(self) -> int:
        return self.process.pid

    async def terminate(self, *, grace_s: float = DEFAULT_GRACE_S) -> TerminateResult:
        """Stop the whole group and drop the ledger entry once it is gone."""
        result = await terminate_tree(
            self.record, grace_s=grace_s, require_leader=False)
        if self.process.returncode is None and result.stopped:
            await self.process.wait()
        if result.stale and self.ledger is not None:
            self.ledger.remove(self.record)
        self.termination = result
        return result


async def _stop_unregistered_process(proc: Any) -> None:
    """Fresh unreaped child ownership, including a cancelled spawn handshake."""
    with contextlib.suppress(ProcessLookupError):
        os.killpg(proc.pid, signal.SIGTERM)
    try:
        await asyncio.wait_for(asyncio.shield(proc.wait()), timeout=DEFAULT_GRACE_S + 1)
    except asyncio.TimeoutError:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(proc.pid, signal.SIGKILL)
        await proc.wait()


async def spawn_supervised(
    argv: Sequence[str],
    *,
    adapter_id: str,
    session_id: str = "",
    label: str = "",
    ledger: Any = _USE_DEFAULT_LEDGER,
    **subprocess_kwargs: Any,
) -> SupervisedProcess:
    """``asyncio.create_subprocess_exec`` in a new session, recorded in the ledger.

    ``subprocess_kwargs`` are passed through (stdin/stdout/stderr/cwd/env);
    process-group options are owned by this function.
    """
    for reserved in ("start_new_session", "process_group", "preexec_fn"):
        if reserved in subprocess_kwargs:
            raise ValueError(f"spawn_supervised owns {reserved!r}")
    target = default_process_ledger() if ledger is _USE_DEFAULT_LEDGER else ledger
    command = tuple(str(item) for item in argv)
    server = await current_server_instance()
    if not command:
        raise ValueError("spawn_supervised requires a command")
    # Preserve create_subprocess_exec's early missing/permission errors.
    command_env = subprocess_kwargs.get("env") or os.environ
    executable = command[0]
    if os.sep in executable:
        path = Path(subprocess_kwargs.get("cwd") or os.getcwd()) / executable
        if not path.exists():
            raise FileNotFoundError(executable)
        if not os.access(path, os.X_OK):
            raise PermissionError(executable)
    elif shutil.which(executable, path=command_env.get("PATH")) is None:
        raise FileNotFoundError(executable)
    ready_read, ready_write = os.pipe()
    inherited_fds = tuple(subprocess_kwargs.pop("pass_fds", ()))
    proc = None
    try:
        spawning = asyncio.create_task(asyncio.create_subprocess_exec(
            sys.executable, str(Path(__file__).with_name("process_guardian.py")),
            str(server.pid), f"--ready-fd={ready_write}",
            *(["--track-descendants"] if sys.platform.startswith("linux") and adapter_id == "grok.acp" else []), *command,
            start_new_session=True, pass_fds=(*inherited_fds, ready_write), **subprocess_kwargs))
        try:
            proc = await asyncio.shield(spawning)
        except asyncio.CancelledError:
            # The subprocess transport may already have forked the guardian.
            # Obtain its ownership handle before propagating cancellation;
            # killing only that transport could leave its engine running.
            proc = await spawning
            raise
        os.close(ready_write)
        ready_write = -1
        ready = await asyncio.wait_for(asyncio.to_thread(os.read, ready_read, 1), timeout=_IDENTITY_TIMEOUT_S)
        if ready not in {b"R", b"C", b"D"}:
            raise ProcessIdentityError("process.guardian.start_failed: guardian did not publish its identity")
        member_pid, start, command_fingerprint = await _recorded_group_member(proc.pid)
    except BaseException:
        # Unreaped child: its pid and group cannot have been reused.
        if proc is not None:
            await _stop_unregistered_process(proc)
        raise
    finally:
        os.close(ready_read)
        if ready_write >= 0:
            os.close(ready_write)
    record = ProcessRecord(
        pid=member_pid,
        pgid=proc.pid,
        start_time=start or "",
        owner=ProcessOwner(server=server, adapter_id=adapter_id, session_id=session_id),
        command=command,
        command_fingerprint=command_fingerprint,
        containment={b"R": "process_group", b"C": "linux_cgroup", b"D": "observed_descendants"}[ready],
        label=label,
    )
    if ready == b"D":
        _LOG.warning("process-ledger-reduced-guarantee: %s uses observed pid/birth descendants; no writable delegated cgroup", adapter_id)
    # A leader that already exited has no identity to match after a restart.
    if target is not None and start:
        try:
            target.add(record)
        except BaseException:
            await _stop_unregistered_process(proc)
            raise
    return SupervisedProcess(process=proc, record=record, ledger=target if start else None)


# ---------------------------------------------------------------------------
# Orphan reaping
# ---------------------------------------------------------------------------


@dataclass
class ReapReport:
    ledger_path: str
    reaped: list[dict[str, Any]] = field(default_factory=list)
    dropped: list[dict[str, Any]] = field(default_factory=list)
    kept_live_owner: list[dict[str, Any]] = field(default_factory=list)
    failed: list[dict[str, Any]] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


def _entry_view(record: ProcessRecord, result: Optional[TerminateResult] = None) -> dict[str, Any]:
    view = {
        "pid": record.pid,
        "pgid": record.pgid,
        "adapter_id": record.owner.adapter_id,
        "session_id": record.owner.session_id,
        "owner_instance": record.owner.server.instance_id,
        "label": record.label,
    }
    if result is not None:
        view.update({"code": result.code, "signals": list(result.signals), "detail": result.detail})
    return view


async def reap_orphans(
    ledger: Optional[ProcessLedger] = None,
    *,
    grace_s: float = DEFAULT_GRACE_S,
) -> ReapReport:
    """Stop children left by Muteki server instances that are no longer alive.

    An entry is terminated only when its owner instance is dead and its
    leader pid is alive with the recorded start time.  Entries whose process
    is gone or whose pid was reused are dropped without signalling.
    Missing recorded members are dropped without signalling, as in T3's
    server ledger; a dropped entry is never reported as terminated.
    Supervised engines now keep a guardian alive while stopping descendants.
    """
    target = ledger if ledger is not None else default_process_ledger()
    if target is None:
        raise ProcessLedgerError("no process ledger configured")
    report = ReapReport(ledger_path=str(target.path))
    for record in target.entries():
        if await server_instance_alive(record.owner.server):
            report.kept_live_owner.append(_entry_view(record))
            continue
        result = await terminate_tree(record, grace_s=grace_s, require_leader=True)
        if result.outcome in {TerminateOutcome.TERMINATED, TerminateOutcome.KILLED}:
            target.remove(record)
            report.reaped.append(_entry_view(record, result))
        elif result.stale:
            target.remove(record)
            report.dropped.append(_entry_view(record, result))
            if not result.stopped:
                _LOG.warning("dropping stale orphan identity %s without signalling: %s %s",
                             record.pid, result.code, result.detail)
        else:
            report.failed.append(_entry_view(record, result))
            _LOG.warning("orphan process %s not reaped: %s %s", record.pid, result.code, result.detail)
    return report


async def terminate_owned(
    ledger: Optional[ProcessLedger] = None,
    *,
    grace_s: float = DEFAULT_GRACE_S,
) -> ReapReport:
    """Stop every ledger entry owned by this server instance (shutdown path).

    Runtime sessions that are idle at shutdown still hold a child process
    tree; adapters that were not closed individually would leave it behind.
    Entries are removed once their group is confirmed gone.  The terminated
    entries are returned in ``reaped``; ``failed`` lists groups that could
    not be stopped (identity refused or survived SIGKILL).
    Only processes recorded by ``spawn_supervised`` are signalled; processes
    never recorded, including descendants that left that group, are left
    running because a dead leader cannot prove they belong to this server.
    """
    target = ledger if ledger is not None else default_process_ledger()
    if target is None:
        raise ProcessLedgerError("no process ledger configured")
    server = await current_server_instance()
    report = ReapReport(ledger_path=str(target.path))
    owned = [record for record in target.entries()
             if record.owner.server.instance_id == server.instance_id]
    results = await asyncio.gather(*(
        terminate_tree(record, grace_s=grace_s, require_leader=False)
        for record in owned))
    for record, result in zip(owned, results):
        if result.stale:
            target.remove(record)
            (report.reaped if result.stopped else report.dropped).append(
                _entry_view(record, result))
        else:
            report.failed.append(_entry_view(record, result))
            _LOG.warning("owned process %s not stopped at shutdown: %s %s",
                         record.pid, result.code, result.detail)
    return report


__all__ = [
    "DEFAULT_GRACE_S",
    "LEDGER_VERSION",
    "ProcessIdentityError",
    "ProcessLedger",
    "ProcessLedgerError",
    "ProcessOwner",
    "ProcessRecord",
    "ReapReport",
    "ServerInstance",
    "SupervisedProcess",
    "TerminateOutcome",
    "TerminateResult",
    "configure_process_ledger",
    "current_server_instance",
    "default_ledger_path",
    "default_process_ledger",
    "process_start_time",
    "reap_orphans",
    "server_instance_alive",
    "spawn_supervised",
    "terminate_owned",
    "terminate_tree",
]
