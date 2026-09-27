"""Prompt observability checks using synthetic content and a stubbed CLI runner."""
from __future__ import annotations

import asyncio
import json
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import patch

from muteki.core.event_bus import EventBus
from muteki.core.events import EventType
from muteki.core.session_store import SessionStore
from muteki.models.solve_graph import Challenge
from muteki.solver.cli_driver import CliResult
from muteki.solver.cli_solver import CliSolver
from muteki.solver.prompt_record import PromptArgv, redact_prompt


class FakeProc:
    def poll(self):
        return 0


class FakeDriver:
    name = "fixture"

    def build_execute(self, prompt, session, **kwargs):
        return ["fixture", "execute", prompt]

    def build_resume(self, prompt, session, **kwargs):
        return ["fixture", "resume", session, prompt]

    def secure_prompt_preflight(self):
        return True, ""

    def build_execute_stdin(self, prompt, session, **kwargs):
        return ["fixture", "stdin"]


class PromptRecordTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.bus = EventBus()
        self.events = []

        async def collect(event):
            self.events.append(event)

        self.bus.add_sink(collect)
        self.worker = CliSolver(
            None, Challenge(id="prompt-fixture", name="Prompt fixture", description="Summarize supplied text", category="misc"),
            driver=FakeDriver(), bus=self.bus, workdir=self.tmp.name,
            solver_label="cli-fixture", intent_id="task-fixture",
            identity={"model": "fixture-model"},
        )

    async def invoke(self, runner, prompt="Line one\nLine two", *, resume=False, secrets=(), env=None):
        self.worker._control_secret_values = list(secrets)
        argv, stdin = (self.worker._resume_invocation(prompt, "session-fixture") if resume
                       else self.worker._execute_invocation(prompt, "session-fixture"))
        self.assertIsInstance(argv, PromptArgv)
        with patch("muteki.solver.cli_runtime.run_cli_streaming", runner):
            await self.worker._run_streaming(argv, cwd=self.tmp.name, timeout=1, runtime_env=env or {}, stdin_text=stdin)
        return argv

    @staticmethod
    def delivered(driver, argv, **kwargs):
        kwargs["on_proc"](FakeProc())
        if kwargs.get("on_stdin_delivered"):
            kwargs["on_stdin_delivered"]()
        return CliResult(text="Fixture complete", returncode=0)

    def prompts(self):
        return [event for event in self.events if event.event_type == EventType.WORKER_PROMPT]

    async def test_exact_multiline_and_resume_are_distinct(self):
        text = "Provided notes\n" + "Unicode 内容\n" * 5000
        argv = await self.invoke(self.delivered, text)
        await self.invoke(self.delivered, "Continue the summary", resume=True)
        records = self.prompts()
        self.assertEqual(argv[-1], text)
        self.assertEqual([event.payload["status"] for event in records], ["prepared", "sent", "prepared", "sent"])
        self.assertEqual(records[0].payload["prompt"], text)
        self.assertEqual(records[0].solver_id, "cli-fixture")
        self.assertEqual(records[0].payload["intent_id"], "task-fixture")
        self.assertEqual(records[2].payload["kind"], "resume")
        self.assertNotEqual(records[0].payload["prompt_id"], records[2].payload["prompt_id"])
        self.assertNotIn("prompt", records[1].payload)

    async def test_stdin_is_not_sent_until_handoff_finishes(self):
        def runner(driver, argv, **kwargs):
            kwargs["on_proc"](FakeProc())
            kwargs["on_stdin_uncertain"]()
            return CliResult(text="", returncode=1)

        await self.invoke(runner, "Use injected fixture-value", secrets=["fixture-value"])
        records = self.prompts()
        self.assertEqual(records[0].payload["transport"], "stdin")
        self.assertEqual(records[-1].payload["status"], "unknown")
        self.assertNotIn("fixture-value", json.dumps([event.payload for event in records]))

    async def test_spawn_failure_is_not_sent(self):
        def runner(*args, **kwargs):
            raise OSError("fixture start failed")

        with self.assertRaises(OSError):
            await self.invoke(runner)
        self.assertEqual(self.prompts()[-1].payload["status"], "not_sent")

    async def test_remote_unknown_is_not_overwritten(self):
        def runner(*args, **kwargs):
            kwargs["on_start_uncertain"]()
            raise OSError("fixture acknowledgement missing")

        with self.assertRaises(OSError):
            await self.invoke(runner)
        self.assertEqual(self.prompts()[-1].payload["status"], "unknown")

    async def test_secret_transport_does_not_change_and_environment_is_masked(self):
        text = "Use fixture-secret and fixture-api-key"
        argv = await self.invoke(self.delivered, text, secrets=["fixture-secret"], env={"MODEL_API_KEY": "fixture-api-key"})
        self.assertEqual(argv, ["fixture", "stdin"])
        self.assertEqual(argv.prompt, text)
        self.assertEqual(self.prompts()[0].payload["prompt"], "Use [REDACTED] and [REDACTED]")
        self.assertTrue(self.prompts()[0].payload["redacted"])
        self.assertEqual(self.prompts()[-1].payload["status"], "sent")

    async def test_durable_event_replay(self):
        store = SessionStore(root=Path(self.tmp.name) / "events")
        self.bus.add_sink(store.append)
        await self.invoke(self.delivered)
        replayed = [event async for event in store.replay_monotonic(self.worker.run_id)]
        records = [event for event in replayed if event.event_type == EventType.WORKER_PROMPT]
        self.assertEqual(records[0].payload["prompt"], "Line one\nLine two")
        self.assertEqual(records[-1].payload["status"], "sent")

    async def test_cancellation_after_start_keeps_delivery_receipt(self):
        started = asyncio.Event()
        release = threading.Event()
        loop = asyncio.get_running_loop()

        def runner(driver, argv, **kwargs):
            kwargs["on_proc"](FakeProc())
            loop.call_soon_threadsafe(started.set)
            release.wait(1)
            return CliResult(text="", cancelled=True)

        task = asyncio.create_task(self.invoke(runner))
        await started.wait()
        task.cancel()
        release.set()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertEqual(self.prompts()[-1].payload["status"], "sent")

    def test_encoded_secret_is_masked(self):
        self.assertEqual(redact_prompt("a%2Fb a/b", ["a/b"]), ("[REDACTED] [REDACTED]", True))


if __name__ == "__main__":
    unittest.main()
