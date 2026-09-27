"""C16: interactive PTY session persistence, interrupt, ring truncate, workspace guard."""

from __future__ import annotations

import asyncio
import os
import tempfile
import time
import unittest
from pathlib import Path

from muteki.conversation.terminal_sessions import (
    TerminalSessionError,
    get_terminal_session_manager,
    reset_terminal_session_manager_for_tests,
)


def _wait_until(predicate, timeout: float = 5.0, interval: float = 0.05) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return False


class TerminalSessionsTest(unittest.TestCase):
    def setUp(self) -> None:
        reset_terminal_session_manager_for_tests()
        os.environ["MUTEKI_TERMINAL_HISTORY_BYTES"] = "4096"
        os.environ["MUTEKI_TERMINAL_IDLE_TTL_SECONDS"] = "3600"

    def tearDown(self) -> None:
        reset_terminal_session_manager_for_tests()
        os.environ.pop("MUTEKI_TERMINAL_HISTORY_BYTES", None)
        os.environ.pop("MUTEKI_TERMINAL_IDLE_TTL_SECONDS", None)

    def test_cd_persists_and_reconnect_history(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "demo_c16").mkdir()
            manager = get_terminal_session_manager()

            async def _run() -> None:
                loop = asyncio.get_running_loop()
                session = manager.create(
                    thread_id="t-c16",
                    workspace_id="w-c16",
                    root_path=str(root),
                    loop=loop,
                )
                # Give the interactive shell a moment to start.
                await asyncio.sleep(0.3)
                session.write_stdin(b"mkdir -p demo_c16 && cd demo_c16 && pwd\n")
                token = f"C16_TOKEN_{session.session_id[:8]}"
                ok = await asyncio.get_event_loop().run_in_executor(
                    None,
                    lambda: _wait_until(
                        lambda: b"demo_c16" in session.history.snapshot(),
                        timeout=6.0,
                    ),
                )
                self.assertTrue(ok, "expected pwd under demo_c16 in scrollback")
                session.write_stdin(f"echo {token}\n".encode())
                ok = await asyncio.get_event_loop().run_in_executor(
                    None,
                    lambda: _wait_until(
                        lambda: token.encode() in session.history.snapshot(),
                        timeout=6.0,
                    ),
                )
                self.assertTrue(ok, "expected echo token in scrollback")
                # Reconnect snapshot
                snap = session.history.snapshot()
                self.assertIn(b"demo_c16", snap)
                self.assertIn(token.encode(), snap)
                session.close(reason="test done")

            asyncio.run(_run())

    def test_interrupt_long_running_command(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manager = get_terminal_session_manager()

            async def _run() -> None:
                loop = asyncio.get_running_loop()
                session = manager.create(
                    thread_id="t-int",
                    workspace_id="w-int",
                    root_path=tmp,
                    loop=loop,
                )
                await asyncio.sleep(0.3)
                session.write_stdin(
                    b"python3 -c 'import time\\n"
                    b"[print(i, flush=True) or time.sleep(0.2) for i in range(80)]'\\n"
                )
                started = await asyncio.get_event_loop().run_in_executor(
                    None,
                    lambda: _wait_until(
                        lambda: b"0" in session.history.snapshot(),
                        timeout=6.0,
                    ),
                )
                self.assertTrue(started)
                session.interrupt()
                # Shell should remain running after SIGINT to the process group.
                await asyncio.sleep(0.5)
                self.assertEqual(session.status, "running")
                session.write_stdin(b"echo INTERRUPTED_OK\n")
                ok = await asyncio.get_event_loop().run_in_executor(
                    None,
                    lambda: _wait_until(
                        lambda: b"INTERRUPTED_OK" in session.history.snapshot(),
                        timeout=6.0,
                    ),
                )
                self.assertTrue(ok)
                session.close(reason="test done")

            asyncio.run(_run())

    def test_history_truncation_flag(self) -> None:
        os.environ["MUTEKI_TERMINAL_HISTORY_BYTES"] = "2048"
        reset_terminal_session_manager_for_tests()
        with tempfile.TemporaryDirectory() as tmp:
            manager = get_terminal_session_manager()

            async def _run() -> None:
                loop = asyncio.get_running_loop()
                session = manager.create(
                    thread_id="t-trunc",
                    workspace_id="w-trunc",
                    root_path=tmp,
                    loop=loop,
                )
                await asyncio.sleep(0.3)
                # Generate more than 2 KiB of output.
                session.write_stdin(b"python3 -c 'print(\"X\"*5000)'\n")
                ok = await asyncio.get_event_loop().run_in_executor(
                    None,
                    lambda: _wait_until(
                        lambda: session.history.truncated or len(session.history) >= 1500,
                        timeout=8.0,
                    ),
                )
                self.assertTrue(ok)
                self.assertTrue(session.history.truncated or len(session.history) <= 2048)
                public = session.to_public()
                if session.history.truncated:
                    self.assertTrue(public["history_truncated"])
                session.close(reason="test done")

            asyncio.run(_run())

    def test_workspace_rebind_refuses_stdin(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_a, tempfile.TemporaryDirectory() as tmp_b:
            manager = get_terminal_session_manager()

            async def _run() -> None:
                loop = asyncio.get_running_loop()
                session = manager.create(
                    thread_id="t-bind",
                    workspace_id="w-a",
                    root_path=tmp_a,
                    loop=loop,
                )
                await asyncio.sleep(0.2)
                manager.ensure_workspace(session, tmp_b, "w-b")
                self.assertTrue(session.workspace_diverged)
                with self.assertRaises(TerminalSessionError) as ctx:
                    session.write_stdin(b"echo should-fail\n")
                self.assertIn("workspace_changed", ctx.exception.code)
                session.close(reason="test done")

            asyncio.run(_run())

    def test_session_limit_per_thread(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manager = get_terminal_session_manager()

            async def _run() -> None:
                loop = asyncio.get_running_loop()
                sessions = []
                for _ in range(6):
                    sessions.append(
                        manager.create(
                            thread_id="t-limit",
                            workspace_id="w-limit",
                            root_path=tmp,
                            loop=loop,
                        )
                    )
                with self.assertRaises(TerminalSessionError) as ctx:
                    manager.create(
                        thread_id="t-limit",
                        workspace_id="w-limit",
                        root_path=tmp,
                        loop=loop,
                    )
                self.assertIn("limit", ctx.exception.code)
                for session in sessions:
                    session.close(reason="test done")

            asyncio.run(_run())


if __name__ == "__main__":
    unittest.main()
