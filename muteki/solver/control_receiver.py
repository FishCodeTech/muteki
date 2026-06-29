"""Host-side control receiver for the reverse-connect Runtime Control Plane.

Topology (see docs/DESIGN_worker_image_clean_rebuild.md §8-9): the in-container
supervisor does NOT listen — it DIALS this receiver. So the host runs ONE long-lived
receiver (a TCP listener bound to MUTEKI_CONTROL_BIND, default 127.0.0.1:9100; the
compose layout sets 0.0.0.0 so sibling worker containers can reach it) that every
run's supervisor connects into. Each supervisor sends a Hello {run_id, token}; we validate
the token against what `ensure_container` registered for that run, then keep the
connection as a `_SupervisorLink` keyed by run_id.

The HOST is still the command side ("reverse-connect, forward-control"): worker
threads call `link.start_worker(...)` / `link.signal(...)` which write op frames on
the link and read back the supervisor's replies/stream — multiplexed over the single
connection by req_id (replies) and worker_id (stream frames).

This module is THREADED (plain sockets + threads), NOT asyncio, because it's driven
from the swarm's synchronous worker threads (CliSolver runs each worker in a thread).
A dedicated accept thread + one reader thread per supervisor connection feed
thread-safe queues the worker threads consume. The receiver is a process-wide
singleton started once (lazily, or from the backend lifespan).

The supervisor is a DUMB executor; this receiver is pure transport + routing. It does
NOT touch flag/fact/graph/key business logic — that stays in the swarm/gate.
"""

from __future__ import annotations

import json
import os
import socket
import threading
import time
from typing import Any, Callable, Optional

# Default host receiver port. The container reaches it via host.docker.internal:<port>.
DEFAULT_CONTROL_PORT = int(os.environ.get("MUTEKI_CONTROL_PORT", "9100"))
# What the container dials. host.docker.internal resolves to the host on Docker
# Desktop (mac/win); on Linux we add --add-host host.docker.internal:host-gateway.
CONTROL_HOST_FROM_CONTAINER = os.environ.get(
    "MUTEKI_CONTROL_HOST", "host.docker.internal")
# Address the receiver BINDS to. Default 127.0.0.1: the classic single-host
# layout where the coordinator runs on the host and worker containers reach it
# via host.docker.internal (Docker Desktop) or the bridge gateway. In the P2-v3
# compose layout the coordinator runs INSIDE the web container and workers are
# SIBLING containers on a shared compose network — a loopback bind there is
# unreachable by siblings, so compose sets MUTEKI_CONTROL_BIND=0.0.0.0. Safe to
# expose on a compose-internal network: every link is still gated by the
# per-run Hello token (see _handle_hello), and the port is not published to the
# host's public interface.
DEFAULT_CONTROL_BIND = os.environ.get("MUTEKI_CONTROL_BIND", "127.0.0.1")


class ControlError(RuntimeError):
    """A control-plane failure (no supervisor connected, link dropped, auth failed).
    The caller treats this as `runtime_degraded` — NEVER a silent local fallback."""


class _PendingReply:
    """A one-shot slot a worker thread waits on for a req_id's reply frame."""
    __slots__ = ("event", "frame")

    def __init__(self) -> None:
        self.event = threading.Event()
        self.frame: Optional[dict] = None


class _SupervisorLink:
    """One connected supervisor (one run). Owns the socket, a reader thread, and the
    multiplexing state. Thread-safe: many worker threads may drive it concurrently."""

    def __init__(self, run_id: str, conn: socket.socket, addr: Any):
        self.run_id = run_id
        self._conn = conn
        self._addr = addr
        self._wlock = threading.Lock()       # serialize writes onto the socket
        self._req_seq = 0
        self._req_lock = threading.Lock()
        self._pending: dict[int, _PendingReply] = {}   # req_id → waiter (non-stream ops)
        # stream routing: worker_id → queue of frames ("out"/"err"/"exit"); plus the
        # StartWorker "started" reply correlated by req_id.
        self._streams: dict[str, "_FrameQueue"] = {}
        # early-frame buffer: the supervisor may stream a worker's first lines BEFORE
        # the host has processed the "started" reply and registered that worker's
        # queue (it learns worker_id only from "started"). Frames arriving in that gap
        # are stashed here by worker_id and flushed when start_worker registers the
        # queue — otherwise the worker's opening output is silently dropped.
        self._early: dict[str, list[dict]] = {}
        self._streams_lock = threading.Lock()
        self.alive = True
        self._buf = b""
        self._reader = threading.Thread(target=self._read_loop, name=f"rcp-link-{run_id}", daemon=True)
        self._reader.start()

    # ── wire I/O ──────────────────────────────────────────────────────────────
    def _send(self, obj: dict) -> None:
        data = (json.dumps(obj) + "\n").encode()
        with self._wlock:
            try:
                self._conn.sendall(data)
            except OSError as e:
                self.alive = False
                raise ControlError(f"control link send failed (run {self.run_id}): {e}") from e

    def _read_loop(self) -> None:
        """Read frames forever, dispatch each to its waiter (by req_id) or stream
        queue (by worker_id). Runs until the connection closes."""
        try:
            while True:
                while b"\n" not in self._buf:
                    chunk = self._conn.recv(65536)
                    if not chunk:
                        raise ConnectionError("supervisor closed the link")
                    self._buf += chunk
                line, _, self._buf = self._buf.partition(b"\n")
                if not line.strip():
                    continue
                try:
                    f = json.loads(line.decode())
                except ValueError:
                    continue
                self._dispatch_frame(f)
        except (OSError, ConnectionError):
            pass
        finally:
            self.alive = False
            self._fail_all()

    def _dispatch_frame(self, f: dict) -> None:
        t = f.get("t")
        wid = f.get("worker_id")
        if t in ("out", "err", "exit") and wid:
            with self._streams_lock:
                q = self._streams.get(wid)
                if q is not None:
                    q.put(f)
                else:
                    # queue not registered yet (started reply still in flight) —
                    # buffer so the worker's opening frames aren't lost.
                    self._early.setdefault(wid, []).append(f)
            return
        # "started" reply (StartWorker) AND "resp" replies (Signal/Status/Health) are
        # correlated by req_id.
        rid = f.get("req_id")
        if rid is not None:
            with self._req_lock:
                waiter = self._pending.pop(int(rid), None)
            if waiter is not None:
                waiter.frame = f
                waiter.event.set()

    def _fail_all(self) -> None:
        # wake every waiter + close every stream queue so worker threads don't hang.
        with self._req_lock:
            waiters = list(self._pending.values())
            self._pending.clear()
        for w in waiters:
            w.frame = None
            w.event.set()
        with self._streams_lock:
            qs = list(self._streams.values())
            self._early.clear()
        for q in qs:
            q.close()

    # ── op API (called by worker threads) ─────────────────────────────────────
    def _next_req(self) -> int:
        with self._req_lock:
            self._req_seq += 1
            return self._req_seq

    def _request(self, op: str, *, timeout: float, **fields: Any) -> dict:
        """Send a non-stream op, block for its reply frame."""
        if not self.alive:
            raise ControlError(f"control link for run {self.run_id} is down")
        rid = self._next_req()
        waiter = _PendingReply()
        with self._req_lock:
            self._pending[rid] = waiter
        self._send({"op": op, "req_id": rid, **fields})
        if not waiter.event.wait(timeout):
            with self._req_lock:
                self._pending.pop(rid, None)
            raise ControlError(f"control op {op} timed out (run {self.run_id})")
        if waiter.frame is None:
            raise ControlError(f"control link dropped during {op} (run {self.run_id})")
        return waiter.frame

    def _stream_for(self, worker_id: str) -> "Optional[_FrameQueue]":
        with self._streams_lock:
            return self._streams.get(worker_id)

    def health(self, *, timeout: float = 5.0) -> dict:
        return self._request("Health", timeout=timeout)

    def signal(self, worker_id: str, name: str, *, timeout: float = 15.0) -> bool:
        try:
            r = self._request("Signal", worker_id=worker_id, signal=name, timeout=timeout)
            return bool(r.get("ok"))
        except ControlError:
            return False

    def status(self, worker_id: str, *, timeout: float = 10.0) -> dict:
        return self._request("Status", worker_id=worker_id, timeout=timeout)

    def teardown(self, *, timeout: float = 15.0) -> None:
        try:
            self._request("TeardownRun", timeout=timeout)
        except ControlError:
            pass

    def start_worker(self, spec: dict, *, timeout: float) -> "tuple[str, _FrameQueue]":
        """Send StartWorker, register a stream queue for the assigned worker_id, and
        return (worker_id, queue). The caller drains the queue (out/err/exit frames)."""
        if not self.alive:
            raise ControlError(f"control link for run {self.run_id} is down")
        rid = self._next_req()
        waiter = _PendingReply()
        with self._req_lock:
            self._pending[rid] = waiter
        self._send({"op": "StartWorker", "req_id": rid, "spec": spec})
        if not waiter.event.wait(min(60.0, timeout + 30)):
            with self._req_lock:
                self._pending.pop(rid, None)
            raise ControlError(f"StartWorker timed out (run {self.run_id})")
        f = waiter.frame
        if not f or f.get("t") != "started" or not f.get("worker_id"):
            err = (f or {}).get("error") or "supervisor did not start worker"
            raise ControlError(f"StartWorker failed: {err}")
        wid = f["worker_id"]
        q = _FrameQueue()
        with self._streams_lock:
            self._streams[wid] = q
            # flush any frames that arrived before this queue existed, in order.
            for early in self._early.pop(wid, []):
                q.put(early)
        return wid, q

    def drop_stream(self, worker_id: str) -> None:
        with self._streams_lock:
            self._streams.pop(worker_id, None)
            self._early.pop(worker_id, None)

    def close(self) -> None:
        self.alive = False
        try:
            self._conn.close()
        except OSError:
            pass


class _FrameQueue:
    """A simple closeable blocking queue of stream frames for one worker."""

    def __init__(self) -> None:
        self._cv = threading.Condition()
        self._items: list[dict] = []
        self._closed = False

    def put(self, f: dict) -> None:
        with self._cv:
            self._items.append(f)
            self._cv.notify()

    def close(self) -> None:
        with self._cv:
            self._closed = True
            self._cv.notify_all()

    def get(self, timeout: float) -> Optional[dict]:
        """Return the next frame, or None if closed+drained or on timeout."""
        deadline = time.time() + timeout
        with self._cv:
            while not self._items and not self._closed:
                remaining = deadline - time.time()
                if remaining <= 0:
                    return None
                self._cv.wait(remaining)
            if self._items:
                return self._items.pop(0)
            return None  # closed and drained


class ControlReceiver:
    """Process-wide singleton: the host's listening endpoint every supervisor dials.

    Maintains run_id → _SupervisorLink. `ensure_container` registers a run's expected
    token via `expect(run_id, token)` BEFORE starting the container; when the
    supervisor dials in with a matching Hello, the link is bound and `await_link`
    returns it to the waiting worker thread.
    """

    _instance: "Optional[ControlReceiver]" = None
    _instance_lock = threading.Lock()

    def __init__(self, host: Optional[str] = None, port: int = DEFAULT_CONTROL_PORT):
        # host=None → env-driven default (MUTEKI_CONTROL_BIND, normally 127.0.0.1;
        # 0.0.0.0 in the compose layout). An explicit host always wins (tests).
        self.host = host if host is not None else DEFAULT_CONTROL_BIND
        self.port = port
        self._tokens: dict[str, str] = {}             # run_id → expected token
        self._links: dict[str, _SupervisorLink] = {}  # run_id → live link
        self._lock = threading.Lock()
        self._link_event = threading.Condition(self._lock)
        self._srv: Optional[socket.socket] = None
        self._started = False

    @classmethod
    def instance(cls) -> "ControlReceiver":
        with cls._instance_lock:
            if cls._instance is None:
                cls._instance = ControlReceiver()
                cls._instance.start()
            return cls._instance

    def start(self) -> None:
        if self._started:
            return
        srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind((self.host, self.port))
        srv.listen(64)
        self._srv = srv
        self._started = True
        threading.Thread(target=self._accept_loop, name="rcp-receiver-accept", daemon=True).start()

    def _accept_loop(self) -> None:
        assert self._srv is not None
        while True:
            try:
                conn, addr = self._srv.accept()
            except OSError:
                return
            threading.Thread(target=self._handshake, args=(conn, addr),
                             name="rcp-handshake", daemon=True).start()

    def _handshake(self, conn: socket.socket, addr: Any) -> None:
        """Read the supervisor's Hello, validate the token, bind the link."""
        conn.settimeout(30.0)
        try:
            buf = b""
            while b"\n" not in buf:
                chunk = conn.recv(4096)
                if not chunk:
                    conn.close()
                    return
                buf += chunk
            line, _, _ = buf.partition(b"\n")
            hello = json.loads(line.decode())
        except (OSError, ValueError):
            conn.close()
            return
        run_id = hello.get("run_id") or ""
        token = hello.get("token") or ""
        with self._lock:
            expected = self._tokens.get(run_id)
        ok = expected is not None and token == expected
        try:
            conn.sendall((json.dumps({"ok": ok, **({} if ok else {"error": "unauthorized"})}) + "\n").encode())
        except OSError:
            conn.close()
            return
        if not ok:
            conn.close()
            return
        conn.settimeout(None)
        link = _SupervisorLink(run_id, conn, addr)
        with self._lock:
            old = self._links.get(run_id)
            self._links[run_id] = link
            self._link_event.notify_all()
        if old is not None:
            old.close()

    # ── API for ensure_container / worker threads ─────────────────────────────
    def expect(self, run_id: str, token: str) -> None:
        """Register the token a run's supervisor must present. Call BEFORE the
        container starts (the supervisor may dial in immediately)."""
        with self._lock:
            self._tokens[run_id] = token

    def await_link(self, run_id: str, *, deadline_s: float = 40.0) -> _SupervisorLink:
        """Block until the run's supervisor has dialed in (and is alive). Raises
        ControlError on timeout — surfaced as runtime_degraded, never a local fallback."""
        t0 = time.time()
        with self._lock:
            while True:
                link = self._links.get(run_id)
                if link is not None and link.alive:
                    return link
                remaining = deadline_s - (time.time() - t0)
                if remaining <= 0:
                    raise ControlError(
                        f"no supervisor connected for run {run_id} within {deadline_s:.0f}s "
                        f"(container up but control plane never dialed back)")
                self._link_event.wait(remaining)

    def get_link(self, run_id: str) -> Optional[_SupervisorLink]:
        with self._lock:
            link = self._links.get(run_id)
            return link if (link and link.alive) else None

    def has_link(self, run_id: str) -> bool:
        return self.get_link(run_id) is not None

    def forget(self, run_id: str) -> None:
        """Drop a run's link + token (on teardown)."""
        with self._lock:
            link = self._links.pop(run_id, None)
            self._tokens.pop(run_id, None)
        if link is not None:
            link.close()
