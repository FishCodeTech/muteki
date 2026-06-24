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

import signal as _signal
import threading
import time
import uuid
from typing import Any, Callable, Optional

from muteki.solver.cli_driver import CliResult, StreamStep
from muteki.solver.control_receiver import ControlError, ControlReceiver, _SupervisorLink

# re-export so existing `from control_client import ControlError` keeps working.
__all__ = [
    "ControlError", "run_cli_streaming_rcp", "run_cli_rcp",
    "wait_supervisor_ready", "health", "teardown_run",
]

# Env keys forwarded to the worker (identical allow-list to the old docker-exec
# prelude): the engine credential vars + our own MUTEKI_* knobs. HOME is special-
# cased below; everything else (host PATH etc.) is supplied by the supervisor's
# baseEnv, so we don't leak the host's full environment into the container.
_ENV_PREFIXES = ("MUTEKI_", "ANTHROPIC_", "CLAUDE_", "CODEX_", "CURSOR_", "OPENAI_")
_CONTAINER_WORKSPACE = "/home/kali/workspace"


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

    def __init__(self, link: _SupervisorLink, worker_id: str):
        self._link = link
        self.worker_id = worker_id
        # a synthetic pid surrogate so callers that read `.pid` for logging don't
        # crash; real signalling never uses it (goes via worker_id on the link).
        self._pid_surrogate = (abs(hash(worker_id)) % 90000) + 10000

    @property
    def pid(self) -> int:
        return self._pid_surrogate

    def _sig(self, name: str) -> None:
        try:
            self._link.signal(self.worker_id, name)
        except ControlError:
            pass  # best-effort; a dead worker / torn-down run is fine to no-op

    def _container_signal(self, sig: int) -> None:
        """CliSolver._signal_proc maps POSIX signals here (the same hook
        _ContainerProc exposes). STOP/CONT/KILL → supervisor Signal op."""
        if sig == getattr(_signal, "SIGSTOP", 17):
            self._sig("STOP")
        elif sig == getattr(_signal, "SIGCONT", 19):
            self._sig("CONT")
        else:
            self._sig("KILL")

    send_signal = _container_signal

    def kill(self) -> None:
        self._sig("KILL")


# ── public entry points (mirror container_exec.run_cli[_streaming]_container) ──

def run_cli_streaming_rcp(
    driver, argv: list[str], *,
    run_id: str, container_cwd: str, timeout: int,
    on_step: "Callable[[StreamStep], None]",
    env: Optional[dict] = None,
    cancel_event: "Optional[threading.Event]" = None,
    on_proc: "Optional[Callable[[object], None]]" = None,
    steer_event: "Optional[threading.Event]" = None,
) -> CliResult:
    """Streaming worker run via the rcp supervisor. Mirrors
    container_exec.run_cli_streaming_container (cancel/steer/pause); control routes
    over the run's reverse control link.

    `argv` MUST already be container-side (argv[0] = in-container bin) and
    `container_cwd` already mapped — container_exec does that before calling us.
    """
    link = _resolve_link(run_id)
    spec = {
        "argv": argv,
        "cwd": container_cwd,
        "env": _filter_env(env),
        "timeout_sec": max(1, int(timeout)),
        "tag": uuid.uuid4().hex[:12],
    }
    t0 = time.time()
    worker_id, q = link.start_worker(spec, timeout=timeout)
    proc = _RcpProc(link, worker_id)
    if on_proc is not None:
        try:
            on_proc(proc)
        except Exception:
            pass

    cancelled = False
    steered = False
    timed_out = False
    oom_killed = False
    rc: Optional[int] = None
    out_lines: list[str] = []
    stderr_lines: list[str] = []

    # A watcher reacts to cancel/steer even while the stream is quiet (model thinking):
    # it KILLs the worker via the link; the stream then sees the exit frame.
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
            if time.time() - t0 > timeout + 5:
                proc.kill()
                return
            watcher_stop.wait(0.1)

    watcher = None
    if cancel_event is not None or steer_event is not None:
        watcher = threading.Thread(target=_watch, name="rcp-control-watch", daemon=True)
        watcher.start()

    try:
        while True:
            f = q.get(timeout=timeout + 30)
            if f is None:
                # queue closed (link dropped) or read timeout, with NO exit frame yet.
                if not link.alive and not (cancelled or steered):
                    # the supervisor's connection dropped mid-worker (supervisor died /
                    # container lost the control link) — NOT a normal worker finish.
                    # Surface it as a runtime failure so the swarm marks the run
                    # runtime_degraded; never let it masquerade as an empty-output
                    # worker (which would silently degrade quality, roadmap 972 / §8).
                    raise ControlError(
                        f"control link dropped mid-worker (run {run_id}, worker "
                        f"{worker_id}) — supervisor unreachable")
                # otherwise: a read timeout on a live link, or a cancel/steer kill →
                # treat as a kill so we don't hang.
                if link.alive:
                    proc.kill()
                    timed_out = True
                break
            t = f.get("t")
            if t == "out":
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
                rc = int(f.get("rc", 0))
                oom_killed = bool(f.get("oom"))
                timed_out = bool(f.get("timed_out")) or timed_out
                break
    finally:
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
                timeout: int, env: Optional[dict] = None) -> CliResult:
    """Non-streaming worker run — collects the full stream then parses once."""
    return run_cli_streaming_rcp(
        driver, argv, run_id=run_id, container_cwd=container_cwd,
        timeout=timeout, env=env, on_step=lambda _s: None)


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
