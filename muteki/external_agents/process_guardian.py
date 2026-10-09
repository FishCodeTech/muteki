"""Keep an owned process-group identity alive until its engine tree is stopped.

Executed as a standalone script: it must not import adapters or their SDKs.
The engine inherits the original stdio, cwd and environment. The guardian
closes its copies, so it cannot hold a protocol pipe open after engine exit.
"""

from __future__ import annotations

import contextlib
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def _linux_group_members(pgid: int) -> list[tuple[int, int, str]]:
    """Read pid, uid and state without requiring procps in a slim image."""
    members: list[tuple[int, int, str]] = []
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            stat = (path / "stat").read_text(encoding="utf-8")
            # ``comm`` may contain spaces and parentheses. The fields after
            # its final ')' start with state, ppid and process-group id.
            fields = stat[stat.rindex(")") + 2:].split()
            if int(fields[2]) == pgid:
                members.append((int(path.name), path.stat().st_uid, fields[0]))
        except (FileNotFoundError, ProcessLookupError):
            continue
    return members


def _live_members(pgid: int) -> bool:
    if sys.platform.startswith("linux"):
        return any(pid != os.getpid() and not state.startswith("Z")
                   for pid, _uid, state in _linux_group_members(pgid))
    with subprocess.Popen(
        ["ps", "-axo", "pid=,pgid=,stat="], stdout=subprocess.PIPE,
        stdin=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        text=True, env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"},
    ) as probe:
        try:
            output, _ = probe.communicate(timeout=2)
        except BaseException:
            probe.kill()
            probe.wait(timeout=2)
            raise
        if probe.returncode != 0:
            raise subprocess.CalledProcessError(probe.returncode, "ps")
        probe_pid = probe.pid
    for line in output.splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[1] == str(pgid):
            if int(parts[0]) not in (os.getpid(), probe_pid) and not parts[2].startswith("Z"):
                return True
    return False


def main() -> int:
    owner_pid = int(sys.argv[1])
    command = sys.argv[2:]
    ready_fd = None
    if command and command[0].startswith("--ready-fd="):
        ready_fd = int(command.pop(0).split("=", 1)[1])
    descendants = bool(command and command[0] == "--track-descendants")
    if descendants:
        command.pop(0)
    # A container's service may legitimately be pid 1. The *engine group*
    # is always created separately and is never pid/group 1.
    if not command or owner_pid <= 0:
        raise ValueError("process.guardian.invalid_arguments")
    if os.getpgrp() != os.getpid():
        os.setsid()
    pgid = os.getpid()
    stopping = False

    def stop(_signum: int, _frame: object) -> None:
        nonlocal stopping
        stopping = True

    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, stop)
    # A server that died before this script started must not launch an engine.
    if os.getppid() != owner_pid:
        return 125
    lease = tree = None
    if descendants and sys.platform.startswith("linux"):
        if __package__:
            from .process_tree import CgroupLease, DescendantLedger
        else:
            from process_tree import CgroupLease, DescendantLedger
        lease = CgroupLease.create()
        tree = DescendantLedger(pgid)
    # Publish ownership only after the guardian's own exec has finished.
    # A parent observing immediately after posix_spawn may still see the
    # launch stub's command line rather than this stable identity.
    if ready_fd is not None:
        os.write(ready_fd, b"C" if lease else b"D" if tree else b"R")
        os.close(ready_fd)
    child = os.fork()
    if child == 0:
        for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP, signal.SIGPIPE):
            signal.signal(sig, signal.SIG_DFL)
        try:
            if lease is not None:
                lease.join()  # Before exec: descendants inherit containment.
            os.execvpe(command[0], command, os.environ)
        except OSError as exc:
            os.write(2, f"process.exec_failed: {type(exc).__name__}: {exc}\n".encode())
            os._exit(127)
    for fd in (0, 1, 2):
        with contextlib.suppress(OSError):
            os.close(fd)
    status: int | None = None
    while not stopping and os.getppid() == owner_pid:
        if tree is not None:
            try:
                tree.observe()
            except OSError:
                # Losing observation must still enter owned-tree cleanup.
                stopping = True
                break
        waited, value = os.waitpid(child, os.WNOHANG)
        if waited:
            status = value
            break
        time.sleep(0.05)
    # Keep the group leader alive throughout cleanup: its birth identity is
    # still verifiable by a restarting server, even after the engine exited.
    deadline = time.monotonic() + 5
    try:
        while (_live_members(pgid) or (tree is not None and tree.observe())
               or (lease is not None and lease.populated())):
            if tree is not None:
                tree.signal(signal.SIGKILL if time.monotonic() >= deadline - 1 else signal.SIGTERM)
            if lease is not None and time.monotonic() >= deadline - 1:
                lease.kill()
            if time.monotonic() >= deadline:
                os.killpg(pgid, signal.SIGKILL)
            os.killpg(pgid, signal.SIGTERM)
            if status is None:
                waited, value = os.waitpid(child, os.WNOHANG)
                if waited:
                    status = value
            time.sleep(0.05)
        if lease is not None:
            lease.remove()
    except (OSError, subprocess.SubprocessError):
        # This group was created by this still-live guardian. Failure to
        # inspect it cannot transfer ownership or justify leaving it running.
        try:
            if lease is not None:
                lease.kill()
            if tree is not None:
                tree.signal(signal.SIGKILL)
        finally:
            os.killpg(pgid, signal.SIGKILL)
    if status is None:
        _, status = os.waitpid(child, 0)
    if os.WIFSIGNALED(status):
        sig = os.WTERMSIG(status)
        if sig not in (signal.SIGKILL, signal.SIGSTOP):
            signal.signal(sig, signal.SIG_DFL)
        os.kill(os.getpid(), sig)
        return 128 + sig
    return os.WEXITSTATUS(status)


if __name__ == "__main__":
    raise SystemExit(main())
