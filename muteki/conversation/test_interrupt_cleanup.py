"""Operator stop must release owned tools before acknowledging cleanup (#295)."""
from __future__ import annotations

import asyncio
import os
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock

from muteki.conversation.executor import ExternalAgentSessionExecutor
from muteki.conversation.models import ThreadState, ThreadRuntimeSelection, TurnRecord
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.conversation import events as ev
from muteki.external_agents.rpc import StdioJsonlPeer
from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.external_agents import AgentSessionRef
from muteki.platform.contracts.objects import AgentSession, Thread
from muteki.platform.contracts.receipts import CommandReceipt, ReceiptState
from muteki.platform.store import PlatformStore


class InterruptCleanupTest(unittest.IsolatedAsyncioTestCase):
    def executor(self, adapter):
        record = AgentSession(agent_session_id="owned", adapter_id="fixture", thread_id="thread", resume_handle="native-handle")
        executor = ExternalAgentSessionExecutor.__new__(ExternalAgentSessionExecutor)
        ref = AgentSessionRef(agent_session_id="owned", adapter_id="fixture", resume_handle="native-handle")
        executor._ref_for = lambda _: (adapter, ref)
        executor._session_record = lambda _: record
        executor._registry = SimpleNamespace(get=lambda *_: adapter)
        executor._tasks = {}
        executor._stop_fences = {}
        executor._live = {"owned", "other-session"}
        executor._capability_cache = {}
        executor._capability_failures = {}
        events = []
        executor._emit = lambda *args: events.append(args)
        return executor, record, events

    async def test_confirmed_stop_closes_only_its_session_and_preserves_resume(self):
        adapter = SimpleNamespace(
            interrupt=AsyncMock(return_value=CommandReceipt(command_id="stop", state=ReceiptState.COMPLETED)),
            close=AsyncMock(),
        )
        executor, record, events = self.executor(adapter)
        task = asyncio.create_task(asyncio.sleep(0.01))
        executor._tasks["thread"] = task
        await executor.interrupt("thread")
        self.assertTrue(task.done())
        adapter.close.assert_awaited_once()
        self.assertEqual(adapter.close.call_args.args[0].agent_session_id, "owned")
        self.assertEqual(record.resume_handle, "native-handle")
        self.assertEqual(executor._live, {"other-session"})
        self.assertEqual(events[-1][2]["reason"], "operator_stopped")

    async def test_rejected_stop_does_not_close_or_claim_cleanup(self):
        adapter = SimpleNamespace(
            interrupt=AsyncMock(return_value=CommandReceipt(command_id="stop", state=ReceiptState.FAILED)),
            close=AsyncMock(),
        )
        executor, _, events = self.executor(adapter)
        receipt = await executor.interrupt("thread")
        self.assertIs(receipt.state, ReceiptState.FAILED)
        adapter.close.assert_not_awaited()
        self.assertFalse(events)
        self.assertIn("owned", executor._live)

    async def test_teardown_failure_is_reported_without_false_closed_event(self):
        adapter = SimpleNamespace(
            interrupt=AsyncMock(return_value=CommandReceipt(command_id="stop", state=ReceiptState.COMPLETED)),
            close=AsyncMock(side_effect=RuntimeError("teardown failed")),
        )
        executor, _, events = self.executor(adapter)
        with self.assertRaisesRegex(RuntimeError, "teardown failed"):
            await executor.interrupt("thread")
        self.assertFalse(events)
        self.assertIn("owned", executor._live)
        self.assertFalse(executor._stop_fences)

    async def test_continuation_cannot_start_while_old_session_is_closing(self):
        entered = asyncio.Event()
        release = asyncio.Event()

        async def close(_):
            entered.set()
            await release.wait()

        adapter = SimpleNamespace(
            interrupt=AsyncMock(return_value=CommandReceipt(command_id="stop", state=ReceiptState.COMPLETED)),
            close=close,
        )
        executor, _, _ = self.executor(adapter)
        executor._locks = {}
        executor._conv = SimpleNamespace(get_turn=lambda _: None)
        ran = asyncio.Event()

        async def run(_):
            ran.set()

        executor._run_turn = run
        stopping = asyncio.create_task(executor.interrupt("thread"))
        await entered.wait()
        resumed = asyncio.create_task(executor._run_guarded(SimpleNamespace(thread_id="thread", turn_id="next")))
        await asyncio.sleep(0)
        self.assertFalse(ran.is_set())
        release.set()
        await stopping
        await resumed
        self.assertTrue(ran.is_set())
        self.assertFalse(executor._stop_fences)

    async def test_actual_owned_child_is_gone_but_independent_process_survives(self):
        # The child deliberately inherits stdout, as a real native tool can.
        # It sleeps only; there is no model, network, or workspace mutation.
        source = '''
import json, subprocess, sys
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({"id": request["id"], "result": {"child": child.pid}}), flush=True)
'''
        peer = StdioJsonlPeer([sys.executable, "-u", "-c", source])
        independent = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        child = None
        try:
            await peer.start()
            child = (await peer.request("fixture"))["result"]["child"]
            adapter = SimpleNamespace(
                interrupt=AsyncMock(return_value=CommandReceipt(command_id="stop", state=ReceiptState.COMPLETED)),
                close=lambda _: peer.close(),
            )
            executor, _, _ = self.executor(adapter)
            await asyncio.wait_for(executor.interrupt("thread"), timeout=8)
            for _ in range(50):
                state = subprocess.run(["ps", "-p", str(child), "-o", "stat="], capture_output=True, text=True).stdout.strip()
                if not state or state.startswith("Z"):
                    break
                await asyncio.sleep(0.02)
            self.assertTrue(not state or state.startswith("Z"), state)
            self.assertIsNone(independent.poll())
        finally:
            if child:
                try:
                    os.kill(child, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            await peer.close()
            independent.terminate()
            independent.wait(timeout=5)

    async def test_closed_projection_preserves_native_resume_identity_across_rebuild(self):
        with tempfile.TemporaryDirectory() as directory:
            store = PlatformStore(db_path=Path(directory) / "platform.db")
            try:
                conv = ConversationStore(store)
                projection = ConversationProjection(store, conv, sessions_root=directory)
                selection = ThreadRuntimeSelection(thread_id="thread", adapter_id="fixture", credential_id="selected-account")
                record = AgentSession(agent_session_id="owned", adapter_id="fixture", thread_id="thread", resume_handle="original-native-thread", closed_at=utcnow())
                store.save(record)
                conv.save_state(ThreadState(thread_id="thread", agent_session_id="owned", session_runtime_key=selection.session_key))
                for event in store.append_events(ev.thread_event("thread", ev.EV_SESSION_CLOSED, {
                    "agent_session_id": "owned", "reason": "operator_stopped", "resume_available": True,
                })):
                    projection.apply(event)
                restored = conv.get_state("thread")
                self.assertEqual(restored.agent_session_id, "owned")
                self.assertEqual(restored.session_runtime_key, selection.session_key)
                executor = ExternalAgentSessionExecutor.__new__(ExternalAgentSessionExecutor)
                executor._conv = conv
                executor._store = store
                executor._manager = SimpleNamespace(runtime_selection=lambda _: selection)
                executor._live = set()
                adapter = object()
                executor._adapter_for = lambda *_: adapter
                executor._start = AsyncMock(return_value=(adapter, "resumed"))
                result = await executor.ensure_session(Thread(thread_id="thread"), TurnRecord(thread_id="thread", kind="resume"))
                self.assertEqual(result, (adapter, "resumed"))
                self.assertEqual(executor._start.call_args.kwargs["resume_handle"], "original-native-thread")
                self.assertEqual(executor._start.call_args.kwargs["agent_session_id"], "owned")
                # Ordinary session closure still clears the active/resume pointer.
                for event in store.append_events(ev.thread_event("thread", ev.EV_SESSION_CLOSED, {
                    "agent_session_id": "owned", "reason": "runtime_switch",
                })):
                    projection.apply(event)
                self.assertIsNone(conv.get_state("thread").agent_session_id)
            finally:
                store.close()


if __name__ == "__main__":
    unittest.main()
