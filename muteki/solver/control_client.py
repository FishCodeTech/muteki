"""Host-side driver for the reverse-connect Runtime Control Plane.

The in-container supervisor dials the host's `ControlReceiver` (control_receiver.py);
this module turns a run's live `_SupervisorLink` into the same worker-execution
surface the solver expects. It mirrors container_exec.run_cli_streaming_container so
the solver swaps backends with one parameter, and `_RcpProc` duck-types the control
interface the solver's `_signal_proc` expects (`_container_signal(sig)`, `kill()`,
`pid`).

Reverse-connect, forward-control: the connection is opened by the supervisor, but the
HOST is the command side — these functions send StartWorker/Signal over the link and
consume the supervisor's multiplexed stream frames (routed by worker_id in the
receiver). The supervisor opens no port; the worker has no way to reach it.
"""

from __future__ import annotations

import os
import signal as _signal
import threading
import time
import uuid
import weakref
from typing import Any, Callable, Optional

from muteki.solver.cli_driver import CliResult, StreamStep
from muteki.solver.control_receiver import (
    ControlError, ControlReceiver, StartWorkerRejected, _SupervisorLink,
)

# re-export so existing `from control_client import ControlError` keeps working.
__all__ = [
    "ControlError", "StartWorkerRejected", "run_cli_streaming_rcp", "run_cli_rcp",
    "wait_supervisor_ready", "health", "teardown_run", "confirm_run_absent",
]

# Env keys forwarded to the worker (identical allow-list to the old docker-exec
# prelude): the engine credential vars + our own MUTEKI_* knobs. HOME is special-
# cased below; everything else (host PATH etc.) is supplied by the supervisor's
# baseEnv, so we don't leak the host's full environment into the container.
_ENV_PREFIXES = (
    "MUTEKI_", "ANTHROPIC_", "CLAUDE_", "CODEX_", "CURSOR_", "OPENAI_",
    "PI_", "KIMI_", "GROK_", "XAI_", "OPENCODE_", "DEEPSEEK_", "DSH_",
    "XDG_",
)
_CONTAINER_WORKSPACE = "/home/kali/workspace"
_RCP_PROCS_LOCK = threading.Lock()
_RCP_PROCS: dict[str, "weakref.WeakSet[_RcpProc]"] = {}


def _filter_env(env: Optional[dict]) -> dict[str, str]:
    """Same selection the old docker-exec path did: only credential/MUTEKI vars, plus
    HOME when it points inside the mounted workspace. The supervisor sets a default
    HOME=/home/kali."""
    out: dict[str, str] = {}
    if not env:
        return out
    for k, v in env.items():
        if k == "HOME":
            if str(v).startswith(f"{_CONTAINER_WORKSPACE}/"):
                out[k] = str(v)
            continue
        if k.startswith(_ENV_PREFIXES):
            out[k] = str(v)
    return out


def _resolve_link(run_id: str, *, deadline_s: float = 40.0) -> _SupervisorLink:
    """Get the run's live supervisor link, waiting for the supervisor to dial in."""
    return ControlReceiver.instance().await_link(run_id, deadline_s=deadline_s)


# ── proc wrapper the solver controls (duck-types _ContainerProc) ──────────────

class _RcpProc:
    """Represents ONE worker the supervisor is running. The solver's `_signal_proc`
    routes STOP/CONT/KILL here via `_container_signal`, which sends a Signal op on the
    run's control link."""

    def __init__(self, link: _SupervisorLink, worker_id: str, *, run_id: str = ""):
        self._link = link
        self.worker_id = worker_id
        self.run_id = run_id or str(getattr(link, "run_id", "") or "")
        # a synthetic pid surrogate so callers that read `.pid` for logging don't
        # crash; real signalling never uses it (goes via worker_id on the link).
        self._pid_surrogate = (abs(hash(worker_id)) % 90000) + 10000
        # Transport return is not exit proof. Only the supervisor's terminal
        # ``exit`` frame may flip this fence.
        self._exit_confirmed = False
        if self.run_id:
            with _RCP_PROCS_LOCK:
                _RCP_PROCS.setdefault(self.run_id, weakref.WeakSet()).add(self)
        register_exit = getattr(self._link, "register_exit_callback", None)
        if callable(register_exit):
            register_exit(self.worker_id, self._confirm_exit)

    @property
    def pid(self) -> int:
        return self._pid_surrogate

    def _sig(self, name: str) -> bool:
        try:
            if not self._link.signal(self.worker_id, name):
                return False
            if name in ("STOP", "CONT"):
                status = self._link.status(self.worker_id)
                if not bool(status.get("ok")) or status.get("state") != "running":
                    return False
                return bool(status.get("paused")) is (name == "STOP")
            return True
        except ControlError:
            return False

    def _container_signal(self, sig: int) -> bool:
        """CliSolver._signal_proc maps POSIX signals here (the same hook
        _ContainerProc exposes). STOP/CONT/KILL → supervisor Signal op."""
        if sig == getattr(_signal, "SIGSTOP", 17):
            return self._sig("STOP")
        elif sig == getattr(_signal, "SIGCONT", 19):
            return self._sig("CONT")
        return self._sig("KILL")

    send_signal = _container_signal

    def kill(self) -> bool:
        return self._sig("KILL")

    def _confirm_exit(self) -> None:
        self._exit_confirmed = True
        unregister_exit = getattr(self._link, "unregister_exit_callback", None)
        if callable(unregister_exit):
            try:
                unregister_exit(self.worker_id)
            except Exception:
                pass


def confirm_run_absent(run_id: str) -> None:
    """Confirm every retained worker after Docker proves its PID namespace absent.

    This recovers a lost terminal frame without weakening the fence: neither
    transport return nor Signal ACK calls this; only authoritative run-container
    absence does.
    """
    with _RCP_PROCS_LOCK:
        procs = list(_RCP_PROCS.pop(str(run_id), weakref.WeakSet()))
    for proc in procs:
        proc._confirm_exit()


# ── public entry points (mirror container_exec.run_cli[_streaming]_container) ──

def run_cli_streaming_rcp(
    driver, argv: list[str], *,
    run_id: str, container_cwd: str, timeout: int,
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
    """Streaming worker run via the rcp supervisor. Mirrors
    container_exec.run_cli_streaming_container (cancel/steer/pause); control routes
    over the run's reverse control link.

    `argv` MUST already be container-side (argv[0] = in-container bin) and
    `container_cwd` already mapped — container_exec does that before calling us.
    For stdin prompts, `started` proves only that the child exists. A separate
    supervisor `stdin` frame proves the complete pipe write+close and is the delivery
    boundary. A definitive started(error) is pre-delivery rejection; failed/missing
    stdin receipt and send/timeout/link loss after dispatch are delivery-unknown.
    ``paused_event`` mirrors the coordinator-confirmed STOP state; both host-side
    timeout backstops discount those intervals, matching the supervisor's
    authoritative pause-aware active-time budget.
    """
    link = _resolve_link(run_id)
    spec = {
        "argv": argv,
        "cwd": container_cwd,
        "env": _filter_env(env),
        "timeout_sec": max(1, int(timeout)),
        "tag": uuid.uuid4().hex[:12],
    }
    if stdin_text is not None:
        # Transport-only field: never duplicate the plaintext into argv/tag/runtime
        # diagnostics.  The authenticated per-run control link hands it directly to
        # the supervisor, which connects it to the child's stdin and retains no copy.
        spec["stdin"] = stdin_text
    t0 = time.time()
    active_t0 = time.monotonic()
    pause_lock = threading.Lock()
    pause_state = {
        "accum": 0.0,
        "since": (active_t0 if paused_event is not None
                  and paused_event.is_set() else None),
    }

    def active_elapsed() -> float:
        """Monotonic runtime excluding operator-confirmed STOP intervals."""
        now = time.monotonic()
        with pause_lock:
            if paused_event is not None and paused_event.is_set():
                if pause_state["since"] is None:
                    pause_state["since"] = now
                paused = pause_state["accum"] + (now - pause_state["since"])
            else:
                if pause_state["since"] is not None:
                    pause_state["accum"] += now - pause_state["since"]
                    pause_state["since"] = None
                paused = pause_state["accum"]
        return (now - active_t0) - paused
    dispatched = False
    stdin_pending = stdin_text is not None
    stdin_notice_lock = threading.Lock()

    def _notify_stdin(delivered: bool) -> None:
        nonlocal stdin_pending
        with stdin_notice_lock:
            if not stdin_pending:
                return
            stdin_pending = False
        callback = on_stdin_delivered if delivered else on_stdin_uncertain
        if callable(callback):
            try:
                callback()
            except Exception:
                pass

    def _dispatched() -> None:
        nonlocal dispatched
        dispatched = True

    try:
        worker_id, q = link.start_worker(
            spec, timeout=timeout, on_dispatched=_dispatched)
    except StartWorkerRejected:
        # A well-formed started(error=...) frame is a proof from the supervisor that
        # cmd.Start failed and no child received stdin.  Keep the reservation pending
        # so ordinary pre-delivery cleanup can release/reassign it; do not quarantine
        # the run as an uncertain remote owner.
        raise
    except ControlError:
        if dispatched:
            _notify_stdin(False)
            if on_start_uncertain is not None:
                try:
                    on_start_uncertain()
                except Exception:
                    pass
            # No worker_id exists for a targeted signal. Ask the supervisor to kill
            # the whole run's worker set; if the link itself died, outer container
            # ownership remains and the run-level teardown is still required.
            try:
                link.teardown(timeout=5.0)
            except Exception:
                pass
        raise
    proc = _RcpProc(link, worker_id, run_id=run_id)
    proc_registered = True
    if on_proc is not None:
        try:
            on_proc(proc)
        except Exception:
            proc_registered = False
    registration_failed = not proc_registered
    if stdin_text is not None and registration_failed:
        _notify_stdin(False)

    cancelled = False
    steered = False
    timed_out = False
    oom_killed = False
    rc: Optional[int] = None
    out_lines: list[str] = []
    stderr_lines: list[str] = []

    termination_lock = threading.Lock()
    termination = {
        "requested_at": None,
        "reason": "",
        "signal_ok": False,
    }

    def _request_termination(reason: str) -> bool:
        ok = proc.kill()
        now = time.monotonic()
        with termination_lock:
            if termination["requested_at"] is None:
                termination["requested_at"] = now
                termination["reason"] = reason
            termination["signal_ok"] = bool(termination["signal_ok"] or ok)
        return ok

    def _termination_snapshot() -> tuple[Optional[float], str, bool]:
        with termination_lock:
            return (
                termination["requested_at"],
                str(termination["reason"]),
                bool(termination["signal_ok"]),
            )

    try:
        exit_grace_s = max(0.05, float(os.environ.get(
            "MUTEKI_RCP_EXIT_CONFIRM_TIMEOUT", "5")))
    except (TypeError, ValueError):
        exit_grace_s = 5.0

    if registration_failed:
        _request_termination("proc_registration_failed")

    # A watcher reacts to cancel/steer even while the stream is quiet (model thinking):
    # it KILLs the worker via the link; the stream then sees the exit frame.
    watcher_stop = threading.Event()

    def _watch() -> None:
        nonlocal cancelled, steered, timed_out
        while not watcher_stop.is_set():
            if cancel_event is not None and cancel_event.is_set():
                cancelled = True
                _request_termination("cancel")
                return
            if steer_event is not None and steer_event.is_set():
                steered = True
                _request_termination("steer")
                return
            if active_elapsed() > timeout + 5:
                timed_out = True
                _request_termination("host_timeout_backstop")
                return
            watcher_stop.wait(0.1)

    watcher = None
    if cancel_event is not None or steer_event is not None:
        watcher = threading.Thread(target=_watch, name="rcp-control-watch", daemon=True)
        watcher.start()

    try:
        while True:
            # Poll the multiplexed stream so a host-side cancel/steer can finish
            # promptly even when a damaged supervisor acknowledges KILL but never
            # emits the worker's terminal exit frame.
            f = q.get(timeout=min(0.25, max(0.05, float(timeout))))
            if f is None:
                # queue closed (link dropped) or read timeout, with NO exit frame yet.
                if not link.alive:
                    # the supervisor's connection dropped mid-worker (supervisor died /
                    # container lost the control link) — NOT a normal worker finish.
                    # Surface it as a runtime failure so the swarm marks the run
                    # runtime_degraded; never let it masquerade as an empty-output
                    # worker (which would silently degrade quality, roadmap 972 / §8).
                    raise ControlError(
                        f"control link dropped mid-worker (run {run_id}, worker "
                        f"{worker_id}) — supervisor unreachable")
                requested_at, reason, signal_ok = _termination_snapshot()
                if requested_at is not None:
                    if time.monotonic() - requested_at <= exit_grace_s:
                        continue
                    raise ControlError(
                        f"worker exit unconfirmed after {reason or 'termination'} "
                        f"(run {run_id}, worker {worker_id}, "
                        f"signal_ack={signal_ok})")
                # A quiet live worker is normal while the model is thinking. Only
                # ask it to terminate after the supervisor grace window; even then,
                # wait for an exit frame rather than laundering Signal ACK as exit.
                if active_elapsed() <= timeout + 5:
                    continue
                timed_out = True
                _request_termination("host_timeout_backstop")
                continue
            t = f.get("t")
            if t == "stdin":
                delivered = bool(f.get("ok"))
                _notify_stdin(delivered)
                if not delivered:
                    # Partial/failed pipe delivery is terminal for this worker. The
                    # typed context callback strands the reservation as unknown; kill
                    # the known child and wait for its normal exit frame.
                    _request_termination("stdin_delivery_failed")
            elif t == "out":
                line = f.get("line", "")
                out_lines.append(line + "\n")
                try:
                    steps = driver.parse_stream_steps(line)  # ALL blocks (#18)
                except Exception:
                    steps = []
                for step in steps:
                    try:
                        on_step(step)
                    except Exception:
                        pass
            elif t == "err":
                stderr_lines.append(f.get("line", "") + "\n")
            elif t == "exit":
                proc._confirm_exit()
                rc = int(f.get("rc", 0))
                oom_killed = bool(f.get("oom"))
                timed_out = bool(f.get("timed_out")) or timed_out
                break
    finally:
        # Exit/link-loss/cancel without a terminal stdin receipt is never safe to
        # replay: bytes may have crossed before the evidence frame was lost.
        _notify_stdin(False)
        watcher_stop.set()
        if watcher is not None:
            watcher.join(timeout=1)
        link.drop_stream(worker_id)

    elapsed = time.time() - t0
    if oom_killed:
        timed_out = False  # an OOM is never also a timeout

    res = driver.parse("".join(out_lines), "".join(stderr_lines))
    res.timed_out = timed_out
    res.oom_killed = oom_killed
    res.cancelled = cancelled
    res.steered = steered
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
    res.runtime_status = {
        "backend": "container_rcp",
        "worker_id": worker_id,
        "status": status,
        "rc": rc,
        "timed_out": timed_out,
        "oom_killed": oom_killed,
        "cancelled": cancelled,
        "steered": steered,
        "elapsed_s": elapsed,
    }
    return res


def run_cli_rcp(driver, argv: list[str], *, run_id: str, container_cwd: str,
                timeout: int, env: Optional[dict] = None,
                stdin_text: Optional[str] = None) -> CliResult:
    """Non-streaming worker run — collects the full stream then parses once."""
    return run_cli_streaming_rcp(
        driver, argv, run_id=run_id, container_cwd=container_cwd,
        timeout=timeout, env=env, on_step=lambda _s: None,
        stdin_text=stdin_text)


# ── lifecycle helpers used by container_exec ──────────────────────────────────

def wait_supervisor_ready(run_id: str, *, deadline_s: float = 40.0) -> bool:
    """Block until the run's supervisor has dialed in AND answers Health. Returns
    False on timeout (caller surfaces runtime_degraded — never a local fallback)."""
    try:
        link = ControlReceiver.instance().await_link(run_id, deadline_s=deadline_s)
        r = link.health(timeout=5.0)
        return bool(r.get("ok"))
    except ControlError:
        return False


def health(run_id: str, *, timeout: float = 10.0) -> dict:
    link = ControlReceiver.instance().await_link(run_id, deadline_s=timeout)
    return link.health(timeout=timeout)


def teardown_run(run_id: str) -> None:
    """Ask the run's supervisor to KILL all workers, then forget the link (the
    container itself is removed by container_exec via `docker rm -f`)."""
    link = ControlReceiver.instance().get_link(run_id)
    if link is not None:
        link.teardown()
    ControlReceiver.instance().forget(run_id)
