"""Process-wide Runtime capability probe cache.

One cache serves the Adapter Registry (and through ``record.last_probe`` the
interaction matrix consumers) and the Runtime settings API, so they cannot
disagree about a Runtime instance's capabilities.

- Key: ``ProbeKey`` = adapter id + instance id + binary identity (resolved
  path, mtime, size, inode) + hash of the instance configuration + probe
  scope (``""`` for the instance itself, ``credential:<id>@<revision>`` for a
  credential-scoped model catalog probe).  A reinstalled binary or a changed
  configuration therefore never reads the previous entry.
- TTL: successful entries live ``ttl_s`` (default 300s, env
  ``MUTEKI_PROBE_CACHE_TTL_S``); failed entries ``error_ttl_s`` (default
  30s) so a fixed install is picked up quickly.  ``refresh=True`` always
  starts a new probe.
- Single-flight: concurrent callers of one key share one in-flight probe.
  A caller that is cancelled only stops waiting; the probe itself is
  cancelled when its last waiter leaves.
- Invalidation: ``invalidate(adapter_id, instance_id)`` on instance config
  change/unregister, ``invalidate_all()`` on credential change.  A probe that
  was in flight when its instance was invalidated still answers its waiters
  but is not stored.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import os
import shutil
import threading
import time
import weakref
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Awaitable, Callable, Optional

from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.protocols import ProbingAdapter

from .capabilities import CapabilityProbeReport

DEFAULT_PROBE_TTL_S = 300.0
DEFAULT_PROBE_ERROR_TTL_S = 30.0
PROBE_TTL_ENV = "MUTEKI_PROBE_CACHE_TTL_S"
PROBE_ERROR_TTL_ENV = "MUTEKI_PROBE_CACHE_ERROR_TTL_S"

PROBE_FAILED_CODE = "runtime.probe.failed"

_LOG = logging.getLogger(__name__)


def _env_seconds(name: str, default: float) -> float:
    raw = os.environ.get(name, "").strip()
    if not raw:
        return default
    try:
        value = float(raw)
    except ValueError as exc:
        raise ValueError(f"{name} must be a number of seconds, got {raw!r}") from exc
    if value < 0:
        raise ValueError(f"{name} must be >= 0, got {raw!r}")
    return value


@dataclass(frozen=True)
class BinaryIdentity:
    """Which installed executable a probe ran against."""

    path: str
    exists: bool
    mtime_ns: int = 0
    size: int = 0
    inode: int = 0

    @classmethod
    def of(cls, binary: str, *, search_path: Optional[str] = None) -> "BinaryIdentity":
        name = str(binary or "").strip()
        if not name:
            return cls(path="", exists=False)
        resolved = name if os.sep in name else shutil.which(name, path=search_path)
        if not resolved:
            return cls(path=name, exists=False)
        try:
            real = os.path.realpath(resolved)
            stat = os.stat(real)
        except OSError:
            return cls(path=resolved, exists=False)
        return cls(path=real, exists=True, mtime_ns=stat.st_mtime_ns,
                   size=stat.st_size, inode=stat.st_ino)

    def to_dict(self) -> dict[str, Any]:
        return {"path": self.path, "exists": self.exists, "mtime_ns": self.mtime_ns,
                "size": self.size, "inode": self.inode}


def config_hash(value: Any) -> str:
    """Stable hash of a JSON-like configuration (non-JSON leaves via ``str``)."""
    encoded = json.dumps(value, sort_keys=True, ensure_ascii=False, default=str)
    return hashlib.sha256(encoded.encode("utf-8")).hexdigest()


def credential_scope(credential_id: str, revision: str) -> str:
    return f"credential:{credential_id}@{revision}" if credential_id else ""


@dataclass(frozen=True)
class ProbeKey:
    adapter_id: str
    instance_id: str
    binary: BinaryIdentity
    config_hash: str
    scope: str = ""

    @property
    def instance_key(self) -> str:
        return f"{self.adapter_id}:{self.instance_id}"

    def to_dict(self) -> dict[str, Any]:
        return {"adapter_id": self.adapter_id, "instance_id": self.instance_id,
                "binary": self.binary.to_dict(), "config_hash": self.config_hash,
                "scope": self.scope}


@dataclass(frozen=True)
class ProbeError:
    """Typed record of a failed probe; ``code`` comes from the exception."""

    code: str
    exception_type: str
    message: str

    @classmethod
    def from_exception(cls, exc: BaseException) -> "ProbeError":
        code = getattr(exc, "code", None)
        return cls(
            code=str(code.value if hasattr(code, "value") else code) if code else PROBE_FAILED_CODE,
            exception_type=type(exc).__name__,
            message=str(exc),
        )

    def describe(self) -> str:
        return f"[{self.code}] {self.exception_type}: {self.message}"


class ProbeFailedError(RuntimeError):
    """A cached or fresh probe failed; ``code`` is the probe's own code."""

    def __init__(self, key: ProbeKey, error: ProbeError) -> None:
        self.key = key
        self.error = error
        self.code = error.code
        super().__init__(f"{key.instance_key} probe failed: {error.describe()}")


@dataclass(frozen=True)
class ProbeEntry:
    key: ProbeKey
    probed_at: datetime
    expires_at: float
    report: Optional[CapabilityProbeReport] = None
    error: Optional[ProbeError] = None
    cached: bool = False
    exception: Optional[BaseException] = field(default=None, repr=False, compare=False)

    @property
    def ok(self) -> bool:
        return self.error is None and self.report is not None

    @property
    def capabilities(self) -> Any:
        return self.report.capabilities if self.report is not None else None

    @property
    def field_sources(self) -> dict[str, str]:
        return dict(self.report.field_sources) if self.report is not None else {}

    def fresh(self, now: Optional[float] = None) -> bool:
        return (time.monotonic() if now is None else now) < self.expires_at

    def require_report(self) -> CapabilityProbeReport:
        if self.error is not None or self.report is None:
            raise ProbeFailedError(self.key, self.error or ProbeError(
                PROBE_FAILED_CODE, "ProbeFailedError", "probe returned no report"),
            ) from self.exception
        return self.report


ProbeListener = Callable[[ProbeEntry], None]


@dataclass
class _Flight:
    task: "asyncio.Task[CapabilityProbeReport]"
    loop: asyncio.AbstractEventLoop
    generation: tuple[int, int]
    waiters: int = 0
    done: "asyncio.Future[ProbeEntry]" = field(default=None)  # type: ignore[assignment]


class ProbeCache:
    def __init__(
        self,
        *,
        ttl_s: Optional[float] = None,
        error_ttl_s: Optional[float] = None,
    ) -> None:
        self.ttl_s = _env_seconds(PROBE_TTL_ENV, DEFAULT_PROBE_TTL_S) if ttl_s is None else float(ttl_s)
        self.error_ttl_s = (
            _env_seconds(PROBE_ERROR_TTL_ENV, DEFAULT_PROBE_ERROR_TTL_S)
            if error_ttl_s is None else float(error_ttl_s))
        self._lock = threading.RLock()
        self._entries: dict[ProbeKey, ProbeEntry] = {}
        self._flights: dict[ProbeKey, _Flight] = {}
        self._generations: dict[str, int] = {}
        self._global_generation = 0
        self._listeners: list[Any] = []

    # -- generation bookkeeping ------------------------------------------------

    def _generation(self, instance_key: str) -> tuple[int, int]:
        return self._global_generation, self._generations.get(instance_key, 0)

    # -- reads -----------------------------------------------------------------

    def peek(self, key: ProbeKey) -> Optional[ProbeEntry]:
        """Last stored entry for exactly this key, regardless of TTL."""
        with self._lock:
            return self._entries.get(key)

    def latest(self, adapter_id: str, instance_id: str, *, scope: str = "") -> Optional[ProbeEntry]:
        """Newest stored entry for an instance/scope across binary/config keys."""
        with self._lock:
            matches = [
                entry for key, entry in self._entries.items()
                if key.adapter_id == adapter_id and key.instance_id == instance_id
                and key.scope == scope
            ]
        return max(matches, key=lambda entry: entry.probed_at, default=None)

    def entries(self) -> list[ProbeEntry]:
        with self._lock:
            return list(self._entries.values())

    # -- probing ---------------------------------------------------------------

    async def get(
        self,
        key: ProbeKey,
        probe: Callable[[], Awaitable[CapabilityProbeReport]],
        *,
        refresh: bool = False,
    ) -> ProbeEntry:
        """Return a fresh entry for ``key``, probing (single-flight) if needed.

        Probe failures are returned as entries with ``error`` set; callers
        that need a report call ``entry.require_report()``.
        """
        loop = asyncio.get_running_loop()
        with self._lock:
            if not refresh:
                entry = self._entries.get(key)
                if entry is not None and entry.fresh():
                    return _as_cached(entry)
            flight = self._flights.get(key)
            if flight is not None and (
                    flight.loop is not loop
                    or flight.generation != self._generation(key.instance_key)):
                # A probe started before an invalidation ran with the old
                # credentials/config; its result is not stored and must not be
                # handed to callers that arrive after the change.
                flight = None
            if flight is None:
                flight = self._start(key, probe, loop)
            flight.waiters += 1
        try:
            entry = await asyncio.shield(flight.done)
        except asyncio.CancelledError:
            with self._lock:
                flight.waiters -= 1
                abandon = flight.waiters <= 0 and not flight.task.done()
                if abandon and self._flights.get(key) is flight:
                    # Later callers must start their own probe, not join one
                    # that is being cancelled.
                    self._flights.pop(key, None)
            if abandon:
                flight.task.cancel()
            raise
        with self._lock:
            flight.waiters -= 1
        return entry

    def _start(
        self,
        key: ProbeKey,
        probe: Callable[[], Awaitable[CapabilityProbeReport]],
        loop: asyncio.AbstractEventLoop,
    ) -> _Flight:
        generation = self._generation(key.instance_key)
        task = loop.create_task(_call(probe))
        flight = _Flight(task=task, loop=loop, generation=generation,
                         done=loop.create_future())
        self._flights[key] = flight
        task.add_done_callback(lambda finished: self._finish(key, flight, finished))
        return flight

    def _finish(self, key: ProbeKey, flight: _Flight, task: "asyncio.Task[CapabilityProbeReport]") -> None:
        now = time.monotonic()
        with self._lock:
            if self._flights.get(key) is flight:
                self._flights.pop(key, None)
        if task.cancelled():
            if not flight.done.done():
                flight.done.cancel()
            return
        exc = task.exception()
        if exc is None:
            entry = ProbeEntry(key=key, probed_at=utcnow(),
                               expires_at=now + self.ttl_s, report=task.result())
        else:
            entry = ProbeEntry(key=key, probed_at=utcnow(), expires_at=now + self.error_ttl_s,
                               error=ProbeError.from_exception(exc), exception=exc)
        with self._lock:
            store = flight.generation == self._generation(key.instance_key)
            if store:
                self._entries[key] = entry
        if not flight.done.done():
            flight.done.set_result(entry)
        if store:
            self._notify(entry)

    # -- invalidation ----------------------------------------------------------

    def invalidate(self, adapter_id: str, instance_id: Optional[str] = None) -> int:
        """Drop entries of one instance (or every instance of an adapter)."""
        with self._lock:
            keys = [key for key in self._entries
                    if key.adapter_id == adapter_id
                    and (instance_id is None or key.instance_id == instance_id)]
            for key in keys:
                self._entries.pop(key, None)
            instance_keys = {key.instance_key for key in keys} | {
                key.instance_key for key in self._flights
                if key.adapter_id == adapter_id
                and (instance_id is None or key.instance_id == instance_id)}
            if instance_id is not None:
                instance_keys.add(f"{adapter_id}:{instance_id}")
            for instance_key in instance_keys:
                self._generations[instance_key] = self._generations.get(instance_key, 0) + 1
            return len(keys)

    def invalidate_all(self) -> int:
        """Drop every entry; used when credentials or host login change."""
        with self._lock:
            count = len(self._entries)
            self._entries.clear()
            self._global_generation += 1
            return count

    # -- listeners -------------------------------------------------------------

    def subscribe(self, listener: ProbeListener) -> Callable[[], None]:
        """Call ``listener(entry)`` after each stored probe result.

        Bound methods are held weakly so a discarded service does not stay
        subscribed.  Returns an unsubscribe function.
        """
        ref: Any = (weakref.WeakMethod(listener) if hasattr(listener, "__self__")
                    else (lambda: listener))
        with self._lock:
            self._listeners.append(ref)

        def unsubscribe() -> None:
            with self._lock:
                if ref in self._listeners:
                    self._listeners.remove(ref)

        return unsubscribe

    def _notify(self, entry: ProbeEntry) -> None:
        with self._lock:
            refs = list(self._listeners)
        for ref in refs:
            listener = ref()
            if listener is None:
                with self._lock:
                    if ref in self._listeners:
                        self._listeners.remove(ref)
                continue
            try:
                listener(entry)
            except Exception:  # noqa: BLE001 — one listener must not starve the others
                _LOG.exception("probe cache listener %r failed for %s",
                               listener, entry.key.instance_key)


async def _call(probe: Callable[[], Awaitable[CapabilityProbeReport]]) -> CapabilityProbeReport:
    report = await probe()
    if not isinstance(report, CapabilityProbeReport):
        raise TypeError(f"probe returned {type(report).__name__}, expected CapabilityProbeReport")
    return report


def _as_cached(entry: ProbeEntry) -> ProbeEntry:
    if entry.cached:
        return entry
    return ProbeEntry(key=entry.key, probed_at=entry.probed_at, expires_at=entry.expires_at,
                      report=entry.report, error=entry.error, cached=True,
                      exception=entry.exception)


_DEFAULT_CACHE: Optional[ProbeCache] = None
_DEFAULT_LOCK = threading.Lock()


def probe_cache() -> ProbeCache:
    """The process-wide cache."""
    global _DEFAULT_CACHE
    with _DEFAULT_LOCK:
        if _DEFAULT_CACHE is None:
            _DEFAULT_CACHE = ProbeCache()
        return _DEFAULT_CACHE


def invalidate_for_credential_change() -> int:
    """Credential or host-login change: every probe may depend on it."""
    return probe_cache().invalidate_all()


def adapter_binary(adapter: ProbingAdapter) -> str:
    """Executable an adapter probes (``ProbingAdapter.probe_binary``)."""
    return str(adapter.probe_binary() or "")


def driver_binary(driver: Any) -> str:
    """``CliDriver.bin`` for key building; a resolution error becomes the key."""
    try:
        return str(driver.bin or "")
    except Exception as exc:  # noqa: BLE001 — the failure becomes part of the key
        # The probe itself reports the resolution error; the key only has to
        # differ from any resolved installation.
        return f"<unresolved {type(exc).__name__}: {exc}>"


__all__ = [
    "BinaryIdentity",
    "DEFAULT_PROBE_ERROR_TTL_S",
    "DEFAULT_PROBE_TTL_S",
    "PROBE_FAILED_CODE",
    "PROBE_ERROR_TTL_ENV",
    "PROBE_TTL_ENV",
    "ProbeCache",
    "ProbeEntry",
    "ProbeError",
    "ProbeFailedError",
    "ProbeKey",
    "adapter_binary",
    "config_hash",
    "credential_scope",
    "driver_binary",
    "invalidate_for_credential_change",
    "probe_cache",
]
