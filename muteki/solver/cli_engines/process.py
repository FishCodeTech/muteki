"""Host CLI process execution. Moved from cli_driver.py."""
from __future__ import annotations

import os
import signal
import subprocess
import threading
import time
from typing import Callable, Optional


from muteki.solver.cli_engines.base import CliDriver
from muteki.solver.cli_engines.types import CliResult, StreamStep, finalize_cli_result
from muteki.solver.cli_launch_check import LaunchContractError, check_process_launch

def _local_process_table() -> "dict[int, tuple[int, int, str]]":
    """Return pid -> (ppid, pgid, start identity) for local process ownership.

    The start identity prevents a PID reused during a long task from being treated
    as a process that belongs to an older Worker.  An empty table means the
    best-effort ``ps`` sample failed; callers retain their previous observations.
    """
    try:
        out = subprocess.run(
            ["ps", "-axo", "pid=,ppid=,pgid=,lstart="],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=10,
        ).stdout
    except Exception:
        return {}
    rows: "dict[int, tuple[int, int, str]]" = {}
    for line in out.splitlines():
        parts = line.strip().split(None, 3)
        if len(parts) != 4:
            continue
        try:
            pid, ppid, pgid = int(parts[0]), int(parts[1]), int(parts[2])
        except ValueError:
            continue
        rows[pid] = (ppid, pgid, parts[3].strip())
    return rows


_LOCAL_PROCESS_OWNERS_LOCK = threading.Lock()
_LOCAL_PROCESS_OWNERS: "set[_LocalProcessOwner]" = set()
_LOCAL_PROCESS_OBSERVER: "Optional[threading.Thread]" = None


def _host_protected_pids(rows: "dict[int, tuple[int, int, str]]") -> "set[int]":
    """PIDs that must never receive a cleanup signal.

    Includes the current process and every ancestor visible in the process
    table (pytest, the terminal, the IDE).  A shared working directory is not
    evidence of Worker ownership and is never consulted here.
    """
    protected: "set[int]" = set()
    try:
        current = os.getpid()
    except Exception:
        return protected
    seen: "set[int]" = set()
    while current > 1 and current not in seen:
        seen.add(current)
        protected.add(current)
        row = rows.get(current)
        if row is None:
            try:
                current = os.getppid() if current == os.getpid() else 0
            except Exception:
                break
            continue
        current = row[0]
    try:
        protected.add(os.getppid())
    except Exception:
        pass
    return protected


def _fully_owned_process_groups(
    rows: "dict[int, tuple[int, int, str]]",
    owned_pids: "set[int]",
    *,
    protected_pids: "set[int]",
    own_pgid: int,
) -> "set[int]":
    """Return process groups whose every live member is an owned Worker PID.

    killpg delivers the signal to every member of the group.  If the group
    also contains Cursor, a terminal, pytest, or any other unowned process,
    the group is left untouched and callers must signal owned PIDs one by one.
    """
    if not rows or not owned_pids:
        return set()
    members: "dict[int, set[int]]" = {}
    for pid, (_ppid, pgid, _started) in rows.items():
        members.setdefault(pgid, set()).add(pid)
    safe: "set[int]" = set()
    seen_pgids: "set[int]" = set()
    for pid in owned_pids:
        row = rows.get(pid)
        if row is None:
            continue
        seen_pgids.add(row[1])
    for pgid in seen_pgids:
        if pgid <= 1 or pgid == own_pgid:
            continue
        group = members.get(pgid, set())
        if not group:
            continue
        if group & protected_pids:
            continue
        if group.issubset(owned_pids):
            safe.add(pgid)
    return safe


def _observe_local_process_owners() -> None:
    """Sample one process table for every running local Worker."""
    global _LOCAL_PROCESS_OBSERVER
    while True:
        with _LOCAL_PROCESS_OWNERS_LOCK:
            owners = tuple(_LOCAL_PROCESS_OWNERS)
            if not owners:
                _LOCAL_PROCESS_OBSERVER = None
                return
        rows = _local_process_table()
        if rows:
            for owner in owners:
                owner.observe(rows)
        time.sleep(0.05)


def _register_local_process_owner(owner: "_LocalProcessOwner") -> None:
    global _LOCAL_PROCESS_OBSERVER
    with _LOCAL_PROCESS_OWNERS_LOCK:
        _LOCAL_PROCESS_OWNERS.add(owner)
        if _LOCAL_PROCESS_OBSERVER is None or not _LOCAL_PROCESS_OBSERVER.is_alive():
            _LOCAL_PROCESS_OBSERVER = threading.Thread(
                target=_observe_local_process_owners,
                name="muteki-local-process-observer",
                daemon=True,
            )
            _LOCAL_PROCESS_OBSERVER.start()


def _unregister_local_process_owner(owner: "_LocalProcessOwner") -> None:
    with _LOCAL_PROCESS_OWNERS_LOCK:
        _LOCAL_PROCESS_OWNERS.discard(owner)


class _LocalProcessOwner:
    """Persistent ownership record for one local Worker invocation.

    CLI tools may create a new session and be reparented to PID 1 while the Worker
    is still running.  A stop-time descendant scan can no longer associate those
    processes with the Worker.  This record observes descendants while the parent
    link still exists and retains their identity until they exit.

    Ownership is only the Worker root PID, processes whose parent is already
    owned in the same process-table sample, and a matching start identity
    (``lstart``) so a reused PID is not treated as the previous process.
    Working directory is not an ownership signal.
    """

    def __init__(self, proc: "subprocess.Popen") -> None:
        self.proc = proc
        self.root_pid = int(proc.pid)
        try:
            self.root_pgid = os.getpgid(self.root_pid)
        except Exception:
            self.root_pgid = -1
        self._lock = threading.Lock()
        self._tracked: "dict[int, str]" = {}
        self._closed = False
        rows = _local_process_table()
        root = rows.get(self.root_pid)
        self._tracked[self.root_pid] = root[2] if root is not None else ""
        if rows:
            self.observe(rows)
        _register_local_process_owner(self)

    def observe(self, rows: "dict[int, tuple[int, int, str]]") -> None:
        if not rows:
            return
        protected = _host_protected_pids(rows)
        with self._lock:
            if self._closed:
                return
            live: "dict[int, str]" = {}
            for pid, started in self._tracked.items():
                if pid in protected:
                    continue
                row = rows.get(pid)
                if row is None:
                    continue
                if started and row[2] != started:
                    continue
                live[pid] = row[2]

            # The initial snapshot can rarely race process startup. Bind the root
            # to its real start identity on the first successful observation.
            # poll() is None means this Popen still owns the PID, so the row
            # cannot be a reused identity for a different process.
            if self.root_pid in rows and self.root_pid not in live:
                previous = self._tracked.get(self.root_pid)
                if (
                    previous == ""
                    and self.root_pid not in protected
                    and self.proc.poll() is None
                ):
                    live[self.root_pid] = rows[self.root_pid][2]

            # Repeat until grandchildren and deeper descendants from this same
            # process-table snapshot have all been adopted.
            changed = True
            while changed:
                changed = False
                live_pids = set(live)
                for pid, (ppid, _pgid, started) in rows.items():
                    if pid in live or pid in protected or ppid not in live_pids:
                        continue
                    live[pid] = started
                    changed = True
            self._tracked = live

    def _targets(
        self, rows: "dict[int, tuple[int, int, str]]"
    ) -> "tuple[set[int], dict[int, int]]":
        self.observe(rows)
        with self._lock:
            tracked = dict(self._tracked)
        protected = _host_protected_pids(rows)
        try:
            own_pgid = os.getpgrp()
        except Exception:
            own_pgid = -1
        pids: "dict[int, int]" = {}
        for pid, started in tracked.items():
            row = rows.get(pid)
            if row is None or (started and row[2] != started):
                continue
            if pid in protected:
                continue
            pids[pid] = row[1]
        pgids = _fully_owned_process_groups(
            rows,
            set(pids),
            protected_pids=protected,
            own_pgid=own_pgid,
        )
        return pgids, pids

    def signal(self, sig: int) -> bool:
        """Signal every live process owned by this invocation.

        killpg is used only when every live member of the group is an owned
        Worker PID.  Mixed groups (Cursor, pytest, the terminal, the IDE) are
        left untouched; owned PIDs in those groups are signalled individually.
        When the process table cannot be sampled, only the Popen this owner
        started is signalled — never an unverified process group.
        """
        with self._lock:
            if self._closed:
                return True
        rows = _local_process_table()
        if not rows:
            try:
                if self.proc.poll() is None:
                    self.proc.send_signal(sig)
                return True
            except ProcessLookupError:
                return True
            except Exception:
                return False
        pgids, pids = self._targets(rows)
        if not pids:
            return True

        failed_pgids: "set[int]" = set()
        hard_failure = False
        for pgid in pgids:
            try:
                os.killpg(pgid, sig)
            except ProcessLookupError:
                failed_pgids.add(pgid)
            except Exception:
                failed_pgids.add(pgid)
                hard_failure = True
        # A PID fallback covers a group that disappeared between the sample and
        # killpg, a mixed group that must not be killed as a whole, and any
        # process with an unusable group id.
        for pid, pgid in pids.items():
            if pgid in pgids and pgid not in failed_pgids:
                continue
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                continue
            except Exception:
                hard_failure = True
        return not hard_failure

    def kill_leftovers(self) -> None:
        # Two samples close the small race where a child is created at the same
        # time the parent exits.  The observer retains an adopted child after it is
        # reparented, so the second signal still reaches it.
        self.signal(signal.SIGKILL)
        time.sleep(0.06)
        self.signal(signal.SIGKILL)

    def has_live_processes(self) -> bool:
        rows = _local_process_table()
        if rows:
            self.observe(rows)
        with self._lock:
            return bool(self._tracked)

    def close(self) -> None:
        self.kill_leftovers()
        with self._lock:
            self._closed = True
            self._tracked.clear()
        _unregister_local_process_owner(self)


def _descendant_pids(root_pid: int) -> "list[int]":
    """Every descendant PID of root_pid (depth-first), via `ps -axo pid=,ppid=`.

    killpg only reaches the worker's ORIGINAL process group. A child that calls
    setsid() (a backgrounded daemon, `docker run -d`'s client, an agent helper
    that detaches) becomes its own group leader and survives killpg — it gets
    reparented to init and keeps running, holding CPU / ports / a concurrency
    slot (the "worker shows closed but its process is still alive" leak). We walk
    the live ppid table to catch those escapees too. Best-effort; [] on any error.
    """
    try:
        rows = _local_process_table()
    except Exception:
        return []
    children: "dict[int, list[int]]" = {}
    for pid, (ppid, _pgid, _started) in rows.items():
        children.setdefault(ppid, []).append(pid)
    out_pids: "list[int]" = []
    stack = list(children.get(root_pid, []))
    seen: "set[int]" = set()
    while stack:
        pid = stack.pop()
        if pid in seen or pid == root_pid:
            continue
        seen.add(pid)
        out_pids.append(pid)
        stack.extend(children.get(pid, []))
    return out_pids


def _kill_proc_tree(proc: "subprocess.Popen", *, pgid: "Optional[int]" = None) -> None:
    """Kill a worker AND its full descendant tree, then REAP it.

    The CLI agent spawns helpers (curl, sh, python, docker); killing only the
    parent can leave a child holding the stdout pipe or running detached. Three
    layers, each best-effort:
      1. os.killpg(SIGKILL) on the worker's process group (start_new_session=True
         makes the worker a group leader, so this takes down everything that
         stayed in the group at once);
      2. enumerate every descendant PID via the live ppid table and SIGKILL each
         individually — this catches children that setsid()'d out of the group
         (the orphan/leak case killpg alone misses);
      3. proc.wait() to reap the parent so it doesn't linger as a <defunct>
         zombie occupying a process-table slot.
    """
    owner = getattr(proc, "_muteki_process_owner", None)
    if isinstance(owner, _LocalProcessOwner):
        owner.signal(signal.SIGKILL)

    # 2 first: snapshot descendants BEFORE killpg, since killpg + reparent can
    # mutate the ppid table out from under us.
    descendants = _descendant_pids(proc.pid)
    try:
        target_pgid = pgid if pgid is not None else os.getpgid(proc.pid)
        os.killpg(target_pgid, signal.SIGKILL)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass
    for pid in descendants:
        try:
            os.kill(pid, signal.SIGKILL)
        except Exception:
            pass
    # reap the parent (avoid a defunct zombie). short timeout: it's been SIGKILL'd.
    try:
        proc.wait(timeout=5)
    except Exception:
        pass


def run_cli(driver: CliDriver, argv: list[str], *, cwd: str, timeout: int,
            env: Optional[dict] = None, container: "Optional[object]" = None,
            stdin_text: "Optional[str]" = None) -> CliResult:
    """Run a CLI driver's argv as a subprocess and parse the result. `env`, if
    given, OVERLAYS os.environ (so the worker inherits PATH etc. plus our vars).

    `container`: if a ContainerHandle is given, the worker runs INSIDE that
    isolated Docker container (can't read the host bench tree) instead of bare on
    the host. None → host subprocess (default, unchanged)."""
    # engine-default env (pi/omp offline toggles) sits UNDER any credential overlay.
    # getattr: duck-typed driver doubles predate the hook.
    _env_extra = getattr(driver, "env_extra", None)
    extra = _env_extra() if callable(_env_extra) else {}
    if extra:
        env = {**extra, **(env or {})}
    if container is not None:
        from muteki.solver.container_exec import run_cli_container
        return run_cli_container(driver, argv, handle=container, cwd=cwd,
                                 timeout=timeout, env=env, stdin_text=stdin_text)
    t0 = time.time()
    # Some CLIs (including OpenCode run) prefer PWD over process.cwd(). An
    # inherited shell PWD must not route a child into the launcher's workspace.
    run_env = {**os.environ, **(env or {}), "PWD": os.path.abspath(cwd)}
    check_process_launch(
        argv, cwd=cwd, env=run_env, stdin_text=stdin_text, source="host-run")
    try:
        input_kwargs = ({"input": stdin_text} if stdin_text is not None
                        else {"stdin": subprocess.DEVNULL})
        proc = subprocess.run(
            argv, cwd=cwd, capture_output=True, text=True, encoding="utf-8",
            errors="replace", timeout=timeout, env=run_env, **input_kwargs)
    except ValueError as exc:
        if "null byte" in str(exc).casefold():
            raise LaunchContractError(
                f"host-run: process rejected a NUL byte: {exc}",
                code="process_input_illegal",
                field="argv",
                source="host-run",
            ) from exc
        raise
    except subprocess.TimeoutExpired as e:
        out = e.stdout if isinstance(e.stdout, str) else ""
        err = e.stderr if isinstance(e.stderr, str) else ""
        res = driver.parse(out or "", err or "")
        res.timed_out = True
        res.elapsed_s = time.time() - t0
        return res
    res = driver.parse(proc.stdout or "", proc.stderr or "")
    finalize_cli_result(
        res,
        driver_name=driver.name,
        stdout=proc.stdout or "",
        stderr=proc.stderr or "",
        returncode=proc.returncode,
    )
    res.elapsed_s = time.time() - t0
    return res


def run_cli_streaming(
    driver: CliDriver, argv: list[str], *, cwd: str, timeout: int,
    on_step: "Callable[[StreamStep], None]", env: Optional[dict] = None,
    inherit_env: bool = True,
    cancel_event: "Optional[threading.Event]" = None,
    on_proc: "Optional[Callable[[subprocess.Popen], None]]" = None,
    on_start_uncertain: "Optional[Callable[[], None]]" = None,
    on_stdin_delivered: "Optional[Callable[[], None]]" = None,
    on_stdin_uncertain: "Optional[Callable[[], None]]" = None,
    steer_event: "Optional[threading.Event]" = None,
    paused_event: "Optional[threading.Event]" = None,
    container: "Optional[object]" = None,
    stdin_text: "Optional[str]" = None,
    popen_wrapper: "Optional[Callable[[Callable[[], subprocess.Popen]], subprocess.Popen]]" = None,
    on_raw_streams: "Optional[Callable[[str, str], None]]" = None,
) -> CliResult:
    """Like run_cli, but reads stdout LINE BY LINE and fires on_step(StreamStep)
    for each parsed line as it arrives — so a caller can surface live progress.
    The full stdout is still accumulated and run through driver.parse() for the
    final CliResult (flag/cost/session), identical to the non-streaming path.
    `env`, if given, normally overlays ``os.environ``.  A host authority may set
    ``inherit_env=False`` when it has sealed a complete launch environment rather
    than an override set.

    Runtime control (dispatcher control over a stateless worker subprocess):
      - `cancel_event`: when set, the subprocess is KILLED immediately (not just
        the asyncio task — that left the CLI agent running, see bug #2). A watcher
        thread kills it the instant the event fires, even if the model is mid-think
        and stdout is quiet (the per-line loop alone could wait minutes).
      - `on_proc`: invoked once with the live Popen so the caller can SIGSTOP /
        SIGCONT it for HITL pause/resume. The worker keeps the same PID, so a paused
        agent is genuinely frozen, not killed.
      - `stdin_text`: optional in-memory prompt transported through a private pipe,
        never argv. `on_stdin_delivered` fires only after the whole payload is
        accepted by that pipe; write/close failure or an unjoined writer fires
        `on_stdin_uncertain` instead. At most one of those callbacks is emitted.
      - `paused_event`: set by the caller while the worker is SIGSTOP-frozen (HITL
        pause). The timeout is computed against wall-clock MINUS time spent paused, so
        a long operator pause can't trip the turn timeout and mislabel a deliberately
        frozen worker as `timed_out` (M7).
      - `steer_event`: like cancel, but means END THIS PASS without marking the worker
        dead — an operator hint/redirect/focus cuts the current pass so the swarm can
        respawn a worker that picks up the queued guidance. The subprocess is killed
        and res.steered=True; there is NO resume loop (single-shot), so the caller does
        not reconnect — steered only keeps the session id from being downgraded.
        cancel_event takes PRECEDENCE: a stop during a steer must still die.

    `container`: if a ContainerHandle is given, the worker runs INSIDE that
    isolated Docker container; all control (cancel/steer/pause) routes in via
    `docker exec kill`. None → host subprocess (default, unchanged).
    """
    # engine-default env (pi/omp offline toggles) sits UNDER any credential overlay.
    # getattr: duck-typed driver doubles predate the hook.
    _env_extra = getattr(driver, "env_extra", None)
    extra = _env_extra() if callable(_env_extra) else {}
    if extra:
        env = {**extra, **(env or {})}
    if container is not None:
        from muteki.solver.container_exec import run_cli_streaming_container
        delivery_kwargs = {}
        if on_stdin_delivered is not None:
            delivery_kwargs["on_stdin_delivered"] = on_stdin_delivered
        if on_stdin_uncertain is not None:
            delivery_kwargs["on_stdin_uncertain"] = on_stdin_uncertain
        return run_cli_streaming_container(
            driver, argv, handle=container, cwd=cwd, timeout=timeout,
            on_step=on_step, env=env, cancel_event=cancel_event,
            on_proc=on_proc, on_start_uncertain=on_start_uncertain,
            steer_event=steer_event, paused_event=paused_event,
            stdin_text=stdin_text, **delivery_kwargs)
    import subprocess as _sp

    if type(inherit_env) is not bool:
        raise TypeError("inherit_env must be an exact boolean")

    t0 = time.time()
    # M7: pause-aware timeout. `paused_accum` is the total wall-clock the worker spent
    # SIGSTOP-frozen by the operator; `pause_since` marks the start of the current
    # freeze (None when running). active_elapsed() subtracts paused time so a paused
    # worker can't be killed as `timed_out`. _pause_lock guards the two counters since
    # the watcher thread and the read loop both call active_elapsed().
    _pause_lock = threading.Lock()
    _pause_state = {"accum": 0.0, "since": None}  # mutated under _pause_lock

    def active_elapsed() -> float:
        """Wall-clock since t0 MINUS time spent paused. Folds the in-progress freeze
        in live so a worker paused RIGHT NOW doesn't keep accruing toward timeout."""
        now = time.time()
        if paused_event is not None and paused_event.is_set():
            with _pause_lock:
                if _pause_state["since"] is None:
                    _pause_state["since"] = now          # freeze just began
                paused = _pause_state["accum"] + (now - _pause_state["since"])
        else:
            with _pause_lock:
                if _pause_state["since"] is not None:    # freeze just ended → bank it
                    _pause_state["accum"] += now - _pause_state["since"]
                    _pause_state["since"] = None
                paused = _pause_state["accum"]
        return (now - t0) - paused

    run_env = {**os.environ, **(env or {})} if inherit_env else dict(env or {})
    # Bind directory metadata after overlays, including sealed launch envs.
    run_env["PWD"] = os.path.abspath(cwd)
    check_process_launch(
        argv, cwd=cwd, env=run_env, stdin_text=stdin_text, source="host-stream")
    # start_new_session=True puts the worker (and every descendant — the CLI agent
    # spawns curl/python/sh helpers) in its OWN process group. Killing just the
    # parent leaves a `sleep`/`curl` child holding the stdout pipe open, so the read
    # loop blocks until timeout (the deeper form of bug #2). We kill the whole GROUP.
    def _spawn_local() -> "subprocess.Popen":
        try:
            child = _sp.Popen(
                argv,
                cwd=cwd,
                stdout=_sp.PIPE,
                stderr=_sp.PIPE,
                stdin=(_sp.PIPE if stdin_text is not None else _sp.DEVNULL),
                text=True,
                encoding="utf-8",
                errors="replace",
                bufsize=1,
                env=run_env,
                start_new_session=True,
            )
        except ValueError as exc:
            if "null byte" in str(exc).casefold():
                raise LaunchContractError(
                    f"host-stream: process rejected a NUL byte: {exc}",
                    code="process_input_illegal",
                    field="argv",
                    source="host-stream",
                ) from exc
            raise
        # Register inside the spawn callback. The C6 wrapper may perform work after
        # Popen returns and before this function's caller regains control; observing
        # here keeps that interval inside the ownership boundary.
        setattr(child, "_muteki_process_owner", _LocalProcessOwner(child))
        return child

    # The C6 host adapter supplies this one narrowly-scoped wrapper so it can hold
    # its interlock across the actual Popen instruction and its immediate canonical
    # start receipt.  Ordinary worker execution keeps the historical direct path.
    proc = popen_wrapper(_spawn_local) if popen_wrapper is not None else _spawn_local()
    process_owner = getattr(proc, "_muteki_process_owner", None)
    try:
        proc_pgid: "Optional[int]" = os.getpgid(proc.pid)
    except Exception:
        proc_pgid = None
    proc_registered = True
    if on_proc is not None:
        try:
            on_proc(proc)
        except Exception:
            proc_registered = False

    # Feed the prompt only after Popen/on_proc established the disclosure fence.
    # A thread avoids deadlocking on a prompt larger than the pipe buffer while the
    # main thread concurrently drains worker output.  The text is never part of argv.
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
                if proc.stdin is not None:
                    proc.stdin.write(stdin_text)
                    proc.stdin.close()
                    delivered = True
            except (BrokenPipeError, OSError, ValueError):
                pass
            _notify_stdin(
                on_stdin_delivered if delivered else on_stdin_uncertain)

        stdin_thread = threading.Thread(
            target=_feed_stdin, name="cli-secret-stdin", daemon=True)
        stdin_thread.start()
    elif proc.stdin is not None:
        # A pre-start cancellation or failed context journal commit kills the child
        # before disclosure.  Close the pipe without writing the secret.
        try:
            proc.stdin.close()
        except (OSError, ValueError):
            pass
        _notify_stdin(on_stdin_uncertain)

    cancelled = False
    steered = False
    timed_out = False
    # Watcher thread: kill the subprocess the moment cancel OR steer fires, AND
    # enforce the wall-clock timeout. Without it, a control signal during a long
    # model "think" (no stdout) wouldn't be observed until the next line — which may
    # never come — and, more critically, a worker that emits ZERO stdout would block
    # the `for line in proc.stdout` read loop FOREVER (the in-loop timeout check at
    # the bottom never runs because the iterator never yields). The watcher is the
    # ONLY thing that can break a silent hang, so it ALWAYS runs — its startup is
    # deliberately NOT gated on cancel/steer being present (it used to be, which left
    # a bare `run_cli_streaming(..., timeout=N)` call with no timeout enforcement at
    # all). Killing the proc tree closes stdout, which unblocks the read loop.
    watcher_stop = threading.Event()

    def _watch() -> None:
        nonlocal cancelled, steered, timed_out
        while not watcher_stop.is_set():
            # cancel takes precedence over steer: a stop during a steer must die.
            if cancel_event is not None and cancel_event.is_set():
                cancelled = True
                _kill_proc_tree(proc, pgid=proc_pgid)
                return
            if steer_event is not None and steer_event.is_set():
                steered = True
                _kill_proc_tree(proc, pgid=proc_pgid)
                return
            if timeout > 0 and active_elapsed() > timeout:
                # Enforce the timeout HERE: the main read loop may be blocked on a
                # silent process and can't self-time-out. Kill the tree (unblocks the
                # read loop) and mark timed_out so the result reflects it. Uses
                # pause-aware elapsed so a frozen worker isn't killed for being paused.
                # timeout<=0：对话模式不设整轮上限。
                timed_out = True
                _kill_proc_tree(proc, pgid=proc_pgid)
                return
            # A CLI can exit while a detached tool command keeps an inherited pipe
            # open. End those owned commands immediately so the stdout reader and
            # runtime-exit fence can complete.
            if proc.poll() is not None:
                if isinstance(process_owner, _LocalProcessOwner):
                    process_owner.kill_leftovers()
                return
            watcher_stop.wait(0.1)

    watcher = threading.Thread(target=_watch, name="cli-control-watch", daemon=True)
    watcher.start()

    out_lines: list[str] = []
    err_lines: list[str] = []
    parser_errors: list[str] = []
    stream_error = ""

    def _drain_stderr() -> None:
        try:
            assert proc.stderr is not None
            for err_line in proc.stderr:
                err_lines.append(err_line)
        except Exception:
            pass

    stderr_thread = threading.Thread(
        target=_drain_stderr, name="cli-stderr-drain", daemon=True)
    stderr_thread.start()
    try:
        assert proc.stdout is not None
        for line in proc.stdout:
            out_lines.append(line)
            if cancel_event is not None and cancel_event.is_set():
                cancelled = True
                _kill_proc_tree(proc, pgid=proc_pgid)
                break
            if steer_event is not None and steer_event.is_set():
                steered = True
                _kill_proc_tree(proc, pgid=proc_pgid)
                break
            if timeout > 0 and active_elapsed() > timeout:
                _kill_proc_tree(proc, pgid=proc_pgid)
                timed_out = True
                break
            try:
                steps = driver.parse_stream_steps(line)  # #18: ALL blocks, not just first
            except Exception as exc:  # noqa: BLE001
                steps = []
                parser_errors.append(
                    f"stream parser failed: {type(exc).__name__}: {exc}")
            for step in steps:
                try:
                    on_step(step)
                except Exception:
                    pass  # a deck-emit failure must never kill the worker
        if timeout > 0:
            proc.wait(timeout=max(1, timeout - int(active_elapsed())))
        else:
            proc.wait()
    except _sp.TimeoutExpired:
        _kill_proc_tree(proc, pgid=proc_pgid)
        timed_out = True
    except Exception as exc:  # noqa: BLE001
        _kill_proc_tree(proc, pgid=proc_pgid)
        stream_error = f"stdout stream failed: {type(exc).__name__}: {exc}"
    finally:
        watcher_stop.set()
        if watcher is not None:
            watcher.join(timeout=1)
        # Some CLIs spawn sidecars that inherit stderr and outlive the parent. A
        # blocking proc.stderr.read() here keeps the worker task alive forever even
        # though the CLI parent is gone, so drain stderr in a thread and tear down
        # any leftover process-group holders if EOF does not arrive promptly.
        stderr_thread.join(timeout=1)
        if stderr_thread.is_alive():
            _kill_proc_tree(proc, pgid=proc_pgid)
            # 不在读取线程仍持有 TextIOWrapper 锁时调用 close()：close 会等待
            # 该锁，继承 stderr 的孤立 sidecar 可因此把 Worker 阻塞到自身退出。
            # 读取线程是 daemon；主进程退出后保留已读 stderr 并立即返回。
        if stdin_thread is not None:
            stdin_thread.join(timeout=1)
            if stdin_thread.is_alive():
                _notify_stdin(on_stdin_uncertain)
        if isinstance(process_owner, _LocalProcessOwner):
            process_owner.close()
    stdout = "".join(out_lines)
    stderr = "".join(err_lines)
    # The callback is deliberately after both pipes reached their bounded terminal
    # and before driver parsing can discard or normalize any content.  C6 uses this
    # narrow host-only seam to seal the exact text observed by this audited Popen
    # reader.  Callback failure propagates: an execution whose evidence could not be
    # sealed is UNKNOWN, never a successful unaccounted observation.
    if on_raw_streams is not None:
        on_raw_streams(stdout, stderr)
    res = driver.parse(stdout, stderr or "")
    res.timed_out = timed_out
    res.cancelled = cancelled
    res.steered = steered
    finalize_cli_result(
        res,
        driver_name=driver.name,
        stdout=stdout,
        stderr=stderr or "",
        returncode=proc.returncode,
    )
    if parser_errors or stream_error:
        result_error = stream_error or parser_errors[0]
        res.error = result_error
    res.elapsed_s = time.time() - t0
    return res
