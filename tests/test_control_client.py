"""Host-side reverse-connect Runtime Control Plane tests (no Docker, no Go binary).

We run the real `ControlReceiver` and a fake supervisor that DIALS it (the reverse
topology), sends a Hello, then answers commands — mirroring cmd/runtime-agent. This
locks the host half: receiver handshake + token auth + run_id routing, the
_SupervisorLink op/stream multiplexing, and run_cli_streaming_rcp consuming the
stream (out lines → driver.parse_stream_steps, exit → result), plus the cancel path.
"""

from __future__ import annotations

import json
import signal
import socket
import threading
import time
from collections import OrderedDict

import pytest

from muteki.solver.cli_driver import CliResult, StreamStep
from muteki.solver import control_client as cc
from muteki.solver import control_receiver as cr


def test_control_bind_defaults_to_loopback_and_honors_env(monkeypatch):
    # P2-v3: the receiver bind address is env-driven (MUTEKI_CONTROL_BIND). Default
    # stays 127.0.0.1 (classic single-host) so this is a pure additive knob; the
    # compose layout sets 0.0.0.0 so sibling worker containers can reach it. An
    # explicit host always wins over the env default (tests pass host=...).
    # __init__ reads the module global at call time, so patch the attribute.
    monkeypatch.setattr(cr, "DEFAULT_CONTROL_BIND", "127.0.0.1")
    assert cr.ControlReceiver(port=0).host == "127.0.0.1"      # default
    monkeypatch.setattr(cr, "DEFAULT_CONTROL_BIND", "0.0.0.0")
    assert cr.ControlReceiver(port=0).host == "0.0.0.0"        # env → compose
    # explicit host beats the env default
    assert cr.ControlReceiver(host="127.0.0.1", port=0).host == "127.0.0.1"


class _FakeSupervisor:
    """A stand-in supervisor: dials the receiver, sends Hello, then services ops on
    that one connection (reverse-connect). Scriptable per-worker stream + started
    error. Records signals."""

    def __init__(self, receiver_port: int, run_id: str, token: str, *,
                 stream=None, started_error: str = ""):
        self.run_id = run_id
        self.token = token
        self.stream = stream or []          # frames to emit after 'started' (out/err/exit)
        self.started_error = started_error
        self.signals: list[dict] = []
        self._wlock = threading.Lock()
        self._s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._s.connect(("127.0.0.1", receiver_port))
        # Hello
        self._send({"hello": 1, "run_id": run_id, "token": token, "version": "fake/1"})
        ack = self._readline()
        self.ack = json.loads(ack) if ack else {}
        self._buf = b""
        self._worker_seq = 0
        if self.ack.get("ok"):
            self._t = threading.Thread(target=self._serve, daemon=True)
            self._t.start()

    def _send(self, obj: dict) -> None:
        with self._wlock:
            self._s.sendall((json.dumps(obj) + "\n").encode())

    def _readline(self) -> str:
        buf = b""
        while b"\n" not in buf:
            c = self._s.recv(4096)
            if not c:
                return ""
            buf += c
        line, _, _ = buf.partition(b"\n")
        return line.decode()

    def _serve(self) -> None:
        try:
            while True:
                while b"\n" not in self._buf:
                    c = self._s.recv(65536)
                    if not c:
                        return
                    self._buf += c
                line, _, self._buf = self._buf.partition(b"\n")
                if not line.strip():
                    continue
                req = json.loads(line.decode())
                self._handle(req)
        except OSError:
            return

    def _handle(self, req: dict) -> None:
        op = req.get("op")
        rid = req.get("req_id")
        if op == "StartWorker":
            if self.started_error:
                self._send({"t": "started", "req_id": rid, "error": self.started_error})
                return
            self._worker_seq += 1
            wid = f"w-{self._worker_seq}-test"
            self._send({"t": "started", "req_id": rid, "worker_id": wid})
            for ev in self.stream:
                ev = dict(ev)
                ev["worker_id"] = wid
                self._send(ev)
                time.sleep(0.005)
        elif op == "Signal":
            self.signals.append(req)
            self._send({"t": "resp", "req_id": rid, "ok": True})
            if str(req.get("signal") or "").upper() == "KILL":
                # Signal ACK is request delivery; a distinct exit frame is the
                # process-termination fence the real supervisor emits after Wait.
                self._send({
                    "t": "exit", "worker_id": req.get("worker_id"), "rc": 137,
                    "timed_out": False,
                })
        elif op == "Health":
            self._send({"t": "resp", "req_id": rid, "ok": True, "version": "muteki-runtime-agent/2"})
        else:
            self._send({"t": "resp", "req_id": rid, "ok": True})


class _Driver:
    name = "claude"

    def parse_stream_steps(self, line):
        return [StreamStep(kind="reasoning", text=line)]

    def parse(self, out, err):
        return CliResult(text=out.strip())


@pytest.fixture
def receiver():
    """A fresh receiver on an ephemeral port (NOT the singleton, to isolate tests)."""
    rcv = cr.ControlReceiver(host="127.0.0.1", port=0)
    rcv.start()
    # discover the bound port
    port = rcv._srv.getsockname()[1]
    rcv._test_port = port
    # make the module-level helpers resolve THIS receiver
    cr.ControlReceiver._instance = rcv
    yield rcv
    try:
        rcv._srv.close()
    except OSError:
        pass
    cr.ControlReceiver._instance = None


def test_handshake_routing_and_stream(receiver):
    receiver.expect("run-1", "tok-1")
    sup = _FakeSupervisor(receiver._test_port, "run-1", "tok-1", stream=[
        {"t": "out", "line": "hello"},
        {"t": "out", "line": "world"},
        {"t": "err", "line": "warn"},
        {"t": "exit", "rc": 0, "oom": False, "timed_out": False},
    ])
    assert sup.ack.get("ok") is True
    steps = []
    res = cc.run_cli_streaming_rcp(
        _Driver(), ["claude", "-p"], run_id="run-1",
        container_cwd="/home/kali/workspace", timeout=30,
        on_step=lambda s: steps.append(s))
    assert [s.text for s in steps] == ["hello", "world"]
    assert res.text == "hello\nworld"
    assert res.runtime_status["status"] == "finished"
    assert res.runtime_status["rc"] == 0


def test_oom_from_exit_frame(receiver):
    receiver.expect("run-oom", "t")
    _FakeSupervisor(receiver._test_port, "run-oom", "t", stream=[
        {"t": "out", "line": "x"},
        {"t": "exit", "rc": 137, "oom": True, "timed_out": False},
    ])
    res = cc.run_cli_streaming_rcp(
        _Driver(), ["claude"], run_id="run-oom",
        container_cwd="/w", timeout=30, on_step=lambda s: None)
    assert res.oom_killed is True
    assert res.timed_out is False
    assert res.runtime_status["status"] == "oom"


def test_token_handshake_rejects_wrong(receiver):
    receiver.expect("run-auth", "right")
    # wrong token → receiver rejects the Hello → no link bound
    sup = _FakeSupervisor(receiver._test_port, "run-auth", "wrong")
    assert sup.ack.get("ok") is False
    # await_link must time out (no valid supervisor)
    with pytest.raises(cc.ControlError):
        cc.run_cli_streaming_rcp(_Driver(), ["claude"], run_id="run-auth",
                                 container_cwd="/w", timeout=2, on_step=lambda s: None)


def test_successful_handshake_consumes_token_and_replay_cannot_replace_live_link(receiver):
    receiver.expect("run-once", "single-use-token")
    first = _FakeSupervisor(receiver._test_port, "run-once", "single-use-token")
    assert first.ack.get("ok") is True
    original = receiver.await_link("run-once", deadline_s=1.0)
    assert "run-once" not in receiver._tokens

    # Replaying the exact capability must neither authenticate nor evict the owner.
    replay = _FakeSupervisor(receiver._test_port, "run-once", "single-use-token")
    assert replay.ack == {"ok": False, "error": "already connected"}
    assert receiver.get_link("run-once") is original
    assert original.alive
    assert original.health(timeout=1.0).get("ok") is True

    # Nor may the coordinator accidentally rotate a new token under that live link.
    with pytest.raises(cr.ControlError, match="refusing to rotate"):
        receiver.expect("run-once", "replacement-token")


def test_consumed_token_cannot_reauthenticate_after_link_drops(receiver):
    receiver.expect("run-spent", "spent-token")
    first = _FakeSupervisor(receiver._test_port, "run-spent", "spent-token")
    assert first.ack.get("ok") is True
    link = receiver.await_link("run-spent", deadline_s=1.0)
    link.close()

    replay = _FakeSupervisor(receiver._test_port, "run-spent", "spent-token")
    assert replay.ack.get("ok") is False
    assert "run-spent" not in receiver._tokens


def test_rcp_timeout_excludes_frozen_wall_time(monkeypatch):
    paused = threading.Event()
    paused.set()
    clock = {"now": 0.0}
    signals = []

    class _Q:
        calls = 0

        def get(self, timeout):
            del timeout
            self.calls += 1
            if self.calls == 1:
                clock["now"] = 100.0  # far beyond timeout, but fully frozen
                return None
            if self.calls == 2:
                paused.clear()         # thaw with the full active budget remaining
                return None
            clock["now"] = 101.0      # one active second after thaw
            return {"t": "exit", "rc": 0, "timed_out": False}

    class _Link:
        alive = True

        def start_worker(self, spec, *, timeout, on_dispatched=None):
            del spec, timeout
            if on_dispatched is not None:
                on_dispatched()
            return "w-paused", _Q()

        def signal(self, worker_id, name, **kwargs):
            del kwargs
            signals.append((worker_id, name))
            return True

        def drop_stream(self, worker_id):
            assert worker_id == "w-paused"

    monkeypatch.setattr(cc, "_resolve_link", lambda _run_id: _Link())
    monkeypatch.setattr(cc.time, "time", lambda: clock["now"])
    monkeypatch.setattr(cc.time, "monotonic", lambda: clock["now"])

    result = cc.run_cli_streaming_rcp(
        _Driver(), ["claude"], run_id="run-paused", container_cwd="/w",
        timeout=1, on_step=lambda _s: None, paused_event=paused)

    assert result.timed_out is False
    assert signals == [], "frozen wall time must not trigger host-side KILL"


def test_rcp_proc_propagates_signal_failure_and_requires_pause_status_confirmation():
    class _Rejected:
        def signal(self, worker_id, name, **kwargs): return False
        def status(self, worker_id, **kwargs): raise AssertionError("no status on reject")

    rejected = cc._RcpProc(_Rejected(), "w-rejected")
    assert rejected._container_signal(signal.SIGSTOP) is False
    assert rejected.kill() is False

    class _StatusMismatch:
        def signal(self, worker_id, name, **kwargs): return True
        def status(self, worker_id, **kwargs):
            return {"ok": True, "state": "running", "paused": False}

    mismatch = cc._RcpProc(_StatusMismatch(), "w-mismatch")
    assert mismatch._container_signal(signal.SIGSTOP) is False

    class _Confirmed:
        paused = False
        def signal(self, worker_id, name, **kwargs):
            self.paused = name == "STOP"
            return True
        def status(self, worker_id, **kwargs):
            return {"ok": True, "state": "running", "paused": self.paused}

    confirmed = cc._RcpProc(_Confirmed(), "w-confirmed")
    assert confirmed._container_signal(signal.SIGSTOP) is True
    assert confirmed._container_signal(signal.SIGCONT) is True


def test_started_error_raises(receiver):
    receiver.expect("run-err", "t")
    _FakeSupervisor(receiver._test_port, "run-err", "t", started_error="exec: claude: not found")
    with pytest.raises(cc.ControlError):
        cc.run_cli_rcp(_Driver(), ["claude"], run_id="run-err",
                       container_cwd="/w", timeout=10)


def test_cancel_event_issues_kill(receiver):
    receiver.expect("run-cancel", "t")
    # a stream that starts but never exits → the watcher must KILL it
    sup = _FakeSupervisor(receiver._test_port, "run-cancel", "t", stream=[
        {"t": "out", "line": "begin"},
        # no exit — simulate a long-running worker
    ])
    cancel = threading.Event()
    cancel.set()
    res = cc.run_cli_streaming_rcp(
        _Driver(), ["claude"], run_id="run-cancel",
        container_cwd="/w", timeout=30, on_step=lambda s: None, cancel_event=cancel)
    time.sleep(0.05)
    assert any(s.get("signal") == "KILL" for s in sup.signals)
    assert res.cancelled is True


def test_kill_failure_without_exit_frame_is_control_error_not_confirmed_return(
        monkeypatch):
    cancel = threading.Event()
    cancel.set()
    captured = []
    signals = []

    class _Q:
        def get(self, timeout):
            time.sleep(min(0.01, timeout))
            return None

    class _Link:
        alive = True
        def start_worker(self, spec, *, timeout, on_dispatched=None):
            del spec, timeout
            if on_dispatched is not None:
                on_dispatched()
            return "w-unconfirmed", _Q()
        def signal(self, worker_id, name, **kwargs):
            del kwargs
            signals.append((worker_id, name))
            return False
        def drop_stream(self, worker_id):
            assert worker_id == "w-unconfirmed"

    monkeypatch.setattr(cc, "_resolve_link", lambda _run_id: _Link())
    monkeypatch.setenv("MUTEKI_RCP_EXIT_CONFIRM_TIMEOUT", "0.05")

    with pytest.raises(cc.ControlError, match="exit unconfirmed"):
        cc.run_cli_streaming_rcp(
            _Driver(), ["claude"], run_id="run-unconfirmed",
            container_cwd="/w", timeout=30, on_step=lambda _s: None,
            cancel_event=cancel, on_proc=captured.append)

    assert signals and signals[0] == ("w-unconfirmed", "KILL")
    assert captured and captured[0]._exit_confirmed is False
    cc.confirm_run_absent("run-unconfirmed")
    assert captured[0]._exit_confirmed is True


def test_await_link_times_out_when_no_supervisor(receiver):
    receiver.expect("run-nobody", "t")
    # nobody dials in → await_link / wait_supervisor_ready must fail (degraded), not hang
    assert cc.wait_supervisor_ready("run-nobody", deadline_s=1.0) is False


def test_link_drop_mid_worker_raises_control_error(receiver):
    # supervisor sends an opening line then DROPS the connection with no exit frame
    # (supervisor died / container lost the link). The host must raise ControlError
    # (→ swarm marks runtime_degraded), NOT return a silent empty result.
    receiver.expect("run-drop", "t")
    sup = _FakeSupervisor(receiver._test_port, "run-drop", "t", stream=[
        {"t": "out", "line": "started-work"},
        # NO exit frame
    ])

    # close the supervisor's socket shortly after it streams the opening line, so the
    # host's queue.get returns None with link.alive False and no exit seen.
    # shutdown(SHUT_RDWR) BEFORE close: a bare close() only drops this thread's fd
    # reference, but the fake's _serve thread is blocked in recv() on the SAME socket
    # and keeps the fd (hence the connection) alive — so the receiver never sees EOF
    # and the host hangs until the q.get timeout. A real dying supervisor is a process
    # exit that closes every fd at once; shutdown() reproduces that by sending FIN
    # immediately and unblocking _serve's recv, so the drop is detected at once.
    def drop():
        time.sleep(0.1)
        try:
            sup._s.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            sup._s.close()
        except OSError:
            pass
    threading.Thread(target=drop, daemon=True).start()

    with pytest.raises(cc.ControlError):
        cc.run_cli_streaming_rcp(
            _Driver(), ["claude"], run_id="run-drop",
            container_cwd="/w", timeout=30, on_step=lambda s: None)


def test_early_frames_before_started_are_not_lost(receiver):
    # the supervisor streams a worker's opening frames immediately after "started";
    # if any arrive before the host registers the worker's queue they must be buffered
    # and flushed, not dropped (the 'world'-only regression). A burst with no gap is
    # the stress case.
    receiver.expect("run-early", "t")
    sup = _FakeSupervisor(receiver._test_port, "run-early", "t", stream=[
        {"t": "out", "line": "first"},
        {"t": "out", "line": "second"},
        {"t": "out", "line": "third"},
        {"t": "exit", "rc": 0},
    ])
    steps = []
    res = cc.run_cli_streaming_rcp(
        _Driver(), ["claude"], run_id="run-early",
        container_cwd="/w", timeout=30, on_step=lambda s: steps.append(s))
    assert [s.text for s in steps] == ["first", "second", "third"]
    assert res.runtime_status["status"] == "finished"


def test_filter_env_only_allowed_keys():
    out = cc._filter_env({
        "MUTEKI_X": "1", "ANTHROPIC_KEY": "k", "CLAUDE_CODE_OAUTH_TOKEN_FILE": "/f",
        "PATH": "/leak", "HOME": "/leak", "HOME_OK": "x",
    })
    assert out == {"MUTEKI_X": "1", "ANTHROPIC_KEY": "k", "CLAUDE_CODE_OAUTH_TOKEN_FILE": "/f"}
    assert cc._filter_env({"HOME": "/home/kali/workspace/h"}) == {"HOME": "/home/kali/workspace/h"}


def test_rcp_start_worker_carries_prompt_in_stdin_field_not_argv(monkeypatch):
    secret = "rcp-stdin-secret-88776655"
    captured = {}
    lifecycle = []

    class _Q:
        def __init__(self):
            self.frames = [
                {"t": "stdin", "ok": True},
                {"t": "exit", "rc": 0},
            ]

        def get(self, timeout):
            del timeout
            return self.frames.pop(0) if self.frames else None

    class _Link:
        alive = True

        def start_worker(self, spec, *, timeout, on_dispatched=None):
            del timeout
            captured.update(spec)
            if on_dispatched is not None:
                on_dispatched()
            return "w-secret", _Q()

        def drop_stream(self, worker_id):
            assert worker_id == "w-secret"

    monkeypatch.setattr(cc, "_resolve_link", lambda run_id: _Link())
    argv = ["claude", "-p", "--no-session-persistence", "--"]
    cc.run_cli_streaming_rcp(
        _Driver(), argv, run_id="run-secret", container_cwd="/w", timeout=10,
        on_step=lambda _s: None,
        on_proc=lambda _proc: lifecycle.append("started"),
        on_stdin_delivered=lambda: lifecycle.append("stdin"),
        on_stdin_uncertain=lambda: lifecycle.append("unknown"),
        stdin_text=secret)

    assert captured["stdin"] == secret
    assert secret not in "\0".join(captured["argv"])
    assert lifecycle == ["started", "stdin"]


def test_rcp_failed_stdin_receipt_is_unknown_and_kills_known_worker(monkeypatch):
    lifecycle = []

    class _Q:
        def __init__(self):
            self.frames = [
                {"t": "stdin", "ok": False, "error": "incomplete"},
                {"t": "exit", "rc": 1},
            ]

        def get(self, timeout):
            del timeout
            return self.frames.pop(0) if self.frames else None

    class _Link:
        alive = True

        def start_worker(self, spec, *, timeout, on_dispatched=None):
            del spec, timeout
            if on_dispatched is not None:
                on_dispatched()
            return "w-partial", _Q()

        def signal(self, worker_id, name):
            lifecycle.append((worker_id, name))
            return True

        def drop_stream(self, worker_id):
            assert worker_id == "w-partial"

    monkeypatch.setattr(cc, "_resolve_link", lambda _run_id: _Link())
    cc.run_cli_streaming_rcp(
        _Driver(), ["claude", "-p", "--"], run_id="run-partial",
        container_cwd="/w", timeout=1, on_step=lambda _s: None,
        on_stdin_delivered=lambda: lifecycle.append("delivered"),
        on_stdin_uncertain=lambda: lifecycle.append("unknown"),
        stdin_text="x" * (4 * 1024 * 1024))
    assert "delivered" not in lifecycle
    assert lifecycle[0] == "unknown"
    assert ("w-partial", "KILL") in lifecycle


def test_dispatched_start_without_ack_marks_delivery_uncertain(monkeypatch):
    lifecycle = []

    class _Link:
        alive = True

        def start_worker(self, spec, *, timeout, on_dispatched=None):
            del spec, timeout
            if on_dispatched is not None:
                on_dispatched()
            raise cc.ControlError("started ACK lost")

        def teardown(self, timeout):
            del timeout
            lifecycle.append("teardown")

    monkeypatch.setattr(cc, "_resolve_link", lambda _run_id: _Link())
    with pytest.raises(cc.ControlError, match="ACK lost"):
        cc.run_cli_streaming_rcp(
            _Driver(), ["claude", "-p", "--"], run_id="run-uncertain",
            container_cwd="/w", timeout=1, on_step=lambda _s: None,
            on_start_uncertain=lambda: lifecycle.append("unknown"),
            stdin_text="one-shot-secret",
        )
    assert lifecycle == ["unknown", "teardown"]


def _unit_supervisor_link(send):
    """Build a transport-only link without binding a socket."""
    link = cr._SupervisorLink.__new__(cr._SupervisorLink)
    link.run_id = "unit"
    link.alive = True
    link._req_seq = 0
    link._req_lock = threading.Lock()
    link._pending = {}
    link._streams = {}
    link._early = {}
    link._early_count = 0
    link._stream_tombstones = OrderedDict()
    link._terminal_frames = OrderedDict()
    link._exit_callbacks = {}
    link._streams_lock = threading.Lock()
    link._send = send.__get__(link, cr._SupervisorLink)
    return link


def test_dropped_stream_tombstone_discards_large_late_output_but_keeps_exit_fence():
    def no_send(self, obj):
        del self, obj

    link = _unit_supervisor_link(no_send)
    worker_id = "w-late"
    link._streams[worker_id] = cr._FrameQueue()
    proc = cc._RcpProc(link, worker_id, run_id="unit")
    link.drop_stream(worker_id)

    for index in range(20_000):
        link._dispatch_frame({
            "t": "out" if index % 2 == 0 else "err",
            "worker_id": worker_id,
            "line": "x" * 32,
        })

    assert worker_id not in link._early
    assert link._early_count == 0
    assert len(link._stream_tombstones) <= cr._STREAM_TOMBSTONES_MAX
    assert proc._exit_confirmed is False

    link._dispatch_frame({"t": "exit", "worker_id": worker_id, "rc": 137})
    assert proc._exit_confirmed is True
    assert worker_id not in link._early

    for index in range(cr._STREAM_TOMBSTONES_MAX + 100):
        wid = f"w-dropped-{index}"
        link._streams[wid] = cr._FrameQueue()
        link.drop_stream(wid)
    assert len(link._stream_tombstones) == cr._STREAM_TOMBSTONES_MAX


def test_pre_registration_early_frame_buffers_are_bounded_per_worker_and_link():
    def no_send(self, obj):
        del self, obj

    link = _unit_supervisor_link(no_send)
    for index in range(cr._EARLY_FRAMES_PER_WORKER * 3):
        link._dispatch_frame({
            "t": "out", "worker_id": "w-burst", "line": str(index),
        })
    assert len(link._early["w-burst"]) == cr._EARLY_FRAMES_PER_WORKER

    for index in range(cr._EARLY_FRAMES_TOTAL * 3):
        link._dispatch_frame({
            "t": "out", "worker_id": f"w-many-{index}", "line": "x",
        })
    assert link._early_count == cr._EARLY_FRAMES_TOTAL
    assert all(
        len(frames) <= cr._EARLY_FRAMES_PER_WORKER
        for frames in link._early.values()
    )
    assert len(link._early) <= cr._EARLY_FRAMES_TOTAL


def test_start_worker_fences_dispatch_before_sendall_failure():
    lifecycle = []

    def fail_send(self, obj):
        del self, obj
        raise cr.ControlError("partial send")

    link = _unit_supervisor_link(fail_send)
    with pytest.raises(cr.ControlError, match="partial send"):
        link.start_worker(
            {"argv": ["claude"]}, timeout=1,
            on_dispatched=lambda: lifecycle.append("dispatched"))
    assert lifecycle == ["dispatched"]
    assert link._pending == {}


def test_started_frame_with_worker_id_and_spawn_error_is_failure():
    def spawn_error(self, obj):
        waiter = self._pending[obj["req_id"]]
        waiter.frame = {
            "t": "started", "req_id": obj["req_id"],
            "worker_id": "w-failed", "error": "exec: not found",
        }
        waiter.event.set()

    link = _unit_supervisor_link(spawn_error)
    with pytest.raises(cr.StartWorkerRejected, match="exec: not found"):
        link.start_worker({"argv": ["missing"]}, timeout=1)
    assert link._streams == {}


def test_definitive_spawn_rejection_does_not_mark_remote_delivery_uncertain(monkeypatch):
    lifecycle = []

    class _Link:
        def start_worker(self, spec, *, timeout, on_dispatched=None):
            del spec, timeout
            if on_dispatched is not None:
                on_dispatched()
            raise cr.StartWorkerRejected("exec failed")

        def teardown(self, timeout):
            del timeout
            lifecycle.append("teardown")

    monkeypatch.setattr(cc, "_resolve_link", lambda _run_id: _Link())
    with pytest.raises(cr.StartWorkerRejected, match="exec failed"):
        cc.run_cli_streaming_rcp(
            _Driver(), ["claude", "-p", "--"], run_id="run-rejected",
            container_cwd="/w", timeout=1, on_step=lambda _s: None,
            on_start_uncertain=lambda: lifecycle.append("unknown"),
            on_stdin_uncertain=lambda: lifecycle.append("stdin-unknown"),
            stdin_text="one-shot-secret")
    assert lifecycle == []
