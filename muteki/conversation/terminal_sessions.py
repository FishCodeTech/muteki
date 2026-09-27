"""Thread-scoped interactive PTY sessions for Conversation workspace terminal (C16).

v1 durability: in-process only — browser refresh reattaches while the API process
lives. API restart ends sessions (clients see exited / missing).
"""

from __future__ import annotations

import asyncio
import base64
import fcntl
import os
import signal
import struct
import termios
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

MAX_SESSIONS_PER_THREAD = 6
DEFAULT_HISTORY_BYTES = 256 * 1024
DEFAULT_IDLE_TTL_SECONDS = 45 * 60
DEFAULT_COLS = 80
DEFAULT_ROWS = 24


def _history_limit() -> int:
    raw = os.environ.get("MUTEKI_TERMINAL_HISTORY_BYTES", "").strip()
    if raw.isdigit():
        return max(1024, int(raw))
    return DEFAULT_HISTORY_BYTES


def _idle_ttl() -> float:
    raw = os.environ.get("MUTEKI_TERMINAL_IDLE_TTL_SECONDS", "").strip()
    if raw.isdigit():
        return float(max(60, int(raw)))
    return float(DEFAULT_IDLE_TTL_SECONDS)


def _shell_argv() -> list[str]:
    shell = os.environ.get("SHELL") or "/bin/bash"
    # Interactive non-login: predictable in headless/CI; cd state still persists.
    return [shell, "-i"]


class TerminalSessionError(RuntimeError):
    def __init__(self, message: str, *, code: str = "conversation.terminal.error") -> None:
        super().__init__(message)
        self.code = code


class RingBuffer:
    def __init__(self, max_bytes: int) -> None:
        self.max_bytes = max_bytes
        self._buf = bytearray()
        self.truncated = False

    def append(self, data: bytes) -> None:
        if not data:
            return
        self._buf.extend(data)
        if len(self._buf) > self.max_bytes:
            overflow = len(self._buf) - self.max_bytes
            del self._buf[:overflow]
            self.truncated = True

    def snapshot(self) -> bytes:
        return bytes(self._buf)

    def __len__(self) -> int:
        return len(self._buf)


@dataclass
class TerminalSession:
    session_id: str
    thread_id: str
    workspace_id: str
    root_path: str
    name: str = ""
    status: str = "running"
    end_reason: str = ""
    end_code: str = ""
    cols: int = DEFAULT_COLS
    rows: int = DEFAULT_ROWS
    created_at: float = field(default_factory=time.time)
    last_active_at: float = field(default_factory=time.time)
    history: RingBuffer = field(default_factory=lambda: RingBuffer(_history_limit()))
    pid: int | None = None
    master_fd: int | None = None
    _lock: threading.RLock = field(default_factory=threading.RLock, repr=False)
    _subscribers: set[asyncio.Queue[dict[str, Any]]] = field(default_factory=set, repr=False)
    _loop: asyncio.AbstractEventLoop | None = field(default=None, repr=False)
    _reader_added: bool = field(default=False, repr=False)
    _closed: bool = field(default=False, repr=False)
    workspace_diverged: bool = False

    def to_public(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "thread_id": self.thread_id,
            "workspace_id": self.workspace_id,
            "root_path": self.root_path,
            "cwd_label": self.root_path,
            "name": self.name or self.session_id[:8],
            "status": self.status,
            "end_reason": self.end_reason or None,
            "end_code": self.end_code or None,
            "cols": self.cols,
            "rows": self.rows,
            "created_at": self.created_at,
            "last_active_at": self.last_active_at,
            "history_bytes": len(self.history),
            "history_truncated": self.history.truncated,
            "workspace_diverged": self.workspace_diverged,
        }

    def touch(self) -> None:
        self.last_active_at = time.time()

    def history_b64(self) -> str:
        return base64.b64encode(self.history.snapshot()).decode("ascii")

    def mark_diverged(self) -> None:
        with self._lock:
            if self.workspace_diverged:
                return
            self.workspace_diverged = True
            self.end_code = self.end_code or "workspace_changed"
            if self.status == "running":
                self.end_reason = "工作区已变更，请新开终端"
            self._broadcast({
                "type": "status",
                "session": self.to_public(),
                "message": "工作区已变更，请新开终端",
            })

    def write_stdin(self, data: bytes) -> None:
        with self._lock:
            if self.workspace_diverged:
                raise TerminalSessionError(
                    "工作区已变更，请新开终端",
                    code="conversation.terminal.workspace_changed",
                )
            if self.status != "running" or self.master_fd is None:
                raise TerminalSessionError(
                    "终端会话已结束",
                    code="conversation.terminal.exited",
                )
            self.touch()
            try:
                os.write(self.master_fd, data)
            except OSError as exc:
                raise TerminalSessionError(str(exc)) from exc

    def resize(self, cols: int, rows: int) -> None:
        cols = max(20, min(int(cols), 500))
        rows = max(5, min(int(rows), 200))
        with self._lock:
            self.cols = cols
            self.rows = rows
            if self.master_fd is None or self.status != "running":
                return
            self.touch()
            try:
                winsize = struct.pack("HHHH", rows, cols, 0, 0)
                fcntl.ioctl(self.master_fd, termios.TIOCSWINSZ, winsize)
            except OSError:
                pass

    def interrupt(self) -> None:
        with self._lock:
            if self.workspace_diverged:
                raise TerminalSessionError(
                    "工作区已变更，请新开终端",
                    code="conversation.terminal.workspace_changed",
                )
            if self.pid is None or self.status != "running":
                return
            self.touch()
            try:
                os.killpg(self.pid, signal.SIGINT)
            except ProcessLookupError:
                pass
            except PermissionError as exc:
                raise TerminalSessionError(str(exc)) from exc

    def close(self, *, reason: str, code: str = "closed") -> None:
        with self._lock:
            if self._closed:
                return
            self._closed = True
            if self.status == "running":
                self.status = "killed" if code == "closed" else code
                self.end_reason = reason
                self.end_code = code
            self._remove_reader()
            if self.pid is not None:
                try:
                    os.killpg(self.pid, signal.SIGHUP)
                except (ProcessLookupError, PermissionError):
                    pass
                try:
                    os.killpg(self.pid, signal.SIGTERM)
                except (ProcessLookupError, PermissionError):
                    pass
            if self.master_fd is not None:
                try:
                    os.close(self.master_fd)
                except OSError:
                    pass
                self.master_fd = None
            self._broadcast({"type": "status", "session": self.to_public()})

    def subscribe(self, queue: asyncio.Queue[dict[str, Any]]) -> None:
        with self._lock:
            self._subscribers.add(queue)

    def unsubscribe(self, queue: asyncio.Queue[dict[str, Any]]) -> None:
        with self._lock:
            self._subscribers.discard(queue)

    def _broadcast(self, message: dict[str, Any]) -> None:
        for queue in list(self._subscribers):
            try:
                queue.put_nowait(message)
            except asyncio.QueueFull:
                try:
                    queue.get_nowait()
                except asyncio.QueueEmpty:
                    pass
                try:
                    queue.put_nowait(message)
                except asyncio.QueueFull:
                    pass

    def _on_master_readable(self) -> None:
        if self.master_fd is None:
            return
        try:
            chunk = os.read(self.master_fd, 8192)
        except OSError:
            chunk = b""
        if not chunk:
            self._handle_exit()
            return
        with self._lock:
            self.history.append(chunk)
            self.touch()
            truncated = self.history.truncated
            payload = {
                "type": "stdout",
                "data": base64.b64encode(chunk).decode("ascii"),
            }
            self._broadcast(payload)
            if truncated:
                self._broadcast({
                    "type": "truncated",
                    "history_truncated": True,
                    "history_bytes": len(self.history),
                })

    def _handle_exit(self) -> None:
        with self._lock:
            if self.status != "running":
                self._remove_reader()
                return
            exit_code = None
            if self.pid is not None:
                try:
                    _pid, status = os.waitpid(self.pid, os.WNOHANG)
                    if _pid:
                        exit_code = os.waitstatus_to_exitcode(status) if os.WIFEXITED(status) else -1
                except ChildProcessError:
                    pass
            self.status = "exited"
            self.end_code = "exited"
            if exit_code is not None:
                self.end_reason = f"进程已退出 (code {exit_code})"
            else:
                self.end_reason = "进程已退出"
            self._remove_reader()
            if self.master_fd is not None:
                try:
                    os.close(self.master_fd)
                except OSError:
                    pass
                self.master_fd = None
            self._broadcast({"type": "status", "session": self.to_public()})

    def _remove_reader(self) -> None:
        if self._reader_added and self._loop is not None and self.master_fd is not None:
            try:
                self._loop.remove_reader(self.master_fd)
            except Exception:
                pass
            self._reader_added = False

    def attach_loop(self, loop: asyncio.AbstractEventLoop) -> None:
        with self._lock:
            self._loop = loop
            if self.master_fd is None or self._reader_added or self.status != "running":
                return
            loop.add_reader(self.master_fd, self._on_master_readable)
            self._reader_added = True


class TerminalSessionManager:
    def __init__(self) -> None:
        self._sessions: dict[str, TerminalSession] = {}
        self._by_thread: dict[str, list[str]] = {}
        self._lock = threading.RLock()

    def create(
        self,
        *,
        thread_id: str,
        workspace_id: str,
        root_path: str,
        cols: int = DEFAULT_COLS,
        rows: int = DEFAULT_ROWS,
        name: str = "",
        loop: asyncio.AbstractEventLoop | None = None,
    ) -> TerminalSession:
        root = str(Path(root_path).expanduser().resolve(strict=True))
        with self._lock:
            self._reap_idle_locked()
            existing = self._by_thread.get(thread_id, [])
            alive = [sid for sid in existing if self._sessions.get(sid) is not None]
            self._by_thread[thread_id] = alive
            if len(alive) >= MAX_SESSIONS_PER_THREAD:
                raise TerminalSessionError(
                    f"每个会话最多 {MAX_SESSIONS_PER_THREAD} 个终端标签",
                    code="conversation.terminal.limit",
                )

            master_fd, slave_fd = os.openpty()
            try:
                winsize = struct.pack(
                    "HHHH",
                    max(5, min(rows, 200)),
                    max(20, min(cols, 500)),
                    0,
                    0,
                )
                fcntl.ioctl(master_fd, termios.TIOCSWINSZ, winsize)
            except OSError:
                pass

            env = os.environ.copy()
            env["TERM"] = env.get("TERM") or "xterm-256color"
            env["MUTEKI_THREAD_ID"] = thread_id
            argv = _shell_argv()
            try:
                pid = os.fork()
            except OSError as exc:
                os.close(master_fd)
                os.close(slave_fd)
                raise TerminalSessionError(f"无法创建终端进程: {exc}") from exc

            if pid == 0:
                try:
                    os.setsid()
                    os.dup2(slave_fd, 0)
                    os.dup2(slave_fd, 1)
                    os.dup2(slave_fd, 2)
                    if slave_fd > 2:
                        os.close(slave_fd)
                    if master_fd > 2:
                        os.close(master_fd)
                    try:
                        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
                    except OSError:
                        pass
                    os.chdir(root)
                    os.execvpe(argv[0], argv, env)
                except Exception:
                    os._exit(127)

            os.close(slave_fd)
            flags = fcntl.fcntl(master_fd, fcntl.F_GETFL)
            fcntl.fcntl(master_fd, fcntl.F_SETFL, flags | os.O_NONBLOCK)

            session = TerminalSession(
                session_id=uuid.uuid4().hex,
                thread_id=thread_id,
                workspace_id=workspace_id,
                root_path=root,
                name=name.strip(),
                cols=max(20, min(int(cols), 500)),
                rows=max(5, min(int(rows), 200)),
                pid=pid,
                master_fd=master_fd,
            )
            self._sessions[session.session_id] = session
            self._by_thread.setdefault(thread_id, []).append(session.session_id)
            if loop is None:
                try:
                    loop = asyncio.get_running_loop()
                except RuntimeError:
                    loop = None
            if loop is not None:
                session.attach_loop(loop)
            return session

    def get(self, session_id: str) -> TerminalSession | None:
        with self._lock:
            self._reap_idle_locked()
            return self._sessions.get(session_id)

    def list_for_thread(self, thread_id: str) -> list[TerminalSession]:
        with self._lock:
            self._reap_idle_locked()
            ids = list(self._by_thread.get(thread_id, []))
            rows = []
            for sid in ids:
                session = self._sessions.get(sid)
                if session is not None:
                    rows.append(session)
            return rows

    def close(self, session_id: str, *, reason: str = "用户关闭终端") -> TerminalSession | None:
        with self._lock:
            session = self._sessions.get(session_id)
            if session is None:
                return None
            session.close(reason=reason, code="closed")
            return session

    def ensure_workspace(
        self,
        session: TerminalSession,
        current_root: str | None,
        current_workspace_id: str | None = None,
    ) -> None:
        if not current_root:
            session.mark_diverged()
            return
        try:
            resolved = str(Path(current_root).expanduser().resolve(strict=True))
        except OSError:
            session.mark_diverged()
            return
        if resolved != session.root_path:
            session.mark_diverged()
            return
        if current_workspace_id and current_workspace_id != session.workspace_id:
            session.mark_diverged()

    def _reap_idle_locked(self) -> None:
        ttl = _idle_ttl()
        now = time.time()
        stale = [
            sid
            for sid, session in self._sessions.items()
            if session.status == "running" and (now - session.last_active_at) > ttl
        ]
        for sid in stale:
            session = self._sessions[sid]
            session.close(reason="空闲超时，会话已关闭", code="idle_timeout")


_MANAGER: TerminalSessionManager | None = None
_MANAGER_LOCK = threading.Lock()


def get_terminal_session_manager() -> TerminalSessionManager:
    global _MANAGER
    with _MANAGER_LOCK:
        if _MANAGER is None:
            _MANAGER = TerminalSessionManager()
        return _MANAGER


def reset_terminal_session_manager_for_tests() -> None:
    """Test helper: tear down the process-global manager."""
    global _MANAGER
    with _MANAGER_LOCK:
        if _MANAGER is not None:
            for session in list(_MANAGER._sessions.values()):
                session.close(reason="test reset", code="test_reset")
            _MANAGER = None
