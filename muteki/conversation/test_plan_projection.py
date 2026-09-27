"""Unit tests for C20 structured plan upsert / reconnect / ACP mapping."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.conversation import events as ev
from muteki.conversation.models import ThreadState
from muteki.conversation.plan import upsert_plan
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.external_agents.acp import normalize_session_update
from muteki.external_agents.codex import CodexAppServerAdapter, _codex_plan_payload
from muteki.external_agents.events import EventSequencer
from muteki.platform.contracts.external_agents import AgentEventType
from muteki.platform.store import PlatformStore


class PlanUpsertTests(unittest.TestCase):
    def test_upsert_by_stable_task_id(self) -> None:
        first = upsert_plan(None, {
            "revision": 1,
            "phase": "proposed",
            "tasks": [
                {"task_id": "t1", "title": "One", "status": "pending"},
                {"task_id": "t2", "title": "Two", "status": "pending"},
            ],
            "source": "fixture",
        })
        assert first is not None
        self.assertEqual(first.revision, 1)
        self.assertEqual([task.task_id for task in first.tasks], ["t1", "t2"])

        second = upsert_plan(first, {
            "revision": 2,
            "phase": "executing",
            "tasks": [
                {"task_id": "t1", "title": "One", "status": "in_progress"},
                {"task_id": "t3", "title": "Three", "status": "pending"},
            ],
            "source": "fixture",
        }, patch=True)
        assert second is not None
        self.assertEqual(second.revision, 2)
        self.assertEqual(
            {task.task_id: task.status for task in second.tasks},
            {"t1": "in_progress", "t2": "pending", "t3": "pending"},
        )

    def test_stale_revision_ignored(self) -> None:
        current = upsert_plan(None, {
            "revision": 3,
            "tasks": [{"task_id": "t1", "title": "A"}],
        })
        stale = upsert_plan(current, {
            "revision": 2,
            "tasks": [{"task_id": "t9", "title": "B"}],
        })
        self.assertIs(stale, current)

    def test_explicit_null_clears_pending_decision(self) -> None:
        proposed = upsert_plan(None, {
            "revision": 1,
            "phase": "awaiting_decision",
            "tasks": [{"task_id": "t1", "title": "One"}],
            "awaiting": {"kind": "plan_accept", "summary": "Awaiting"},
            "pending_amendment": {"text": "Change One", "status": "unconfirmed"},
        })
        assert proposed is not None
        confirmed = upsert_plan(proposed, {
            "revision": 2,
            "phase": "executing",
            "tasks": [{"task_id": "t1", "title": "One", "status": "in_progress"}],
            "awaiting": None,
            "pending_amendment": None,
        })
        assert confirmed is not None
        self.assertIsNone(confirmed.awaiting)
        self.assertIsNone(confirmed.pending_amendment)


class PlanProjectionTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.store = PlatformStore(Path(self._tmp.name) / "platform.db")
        self.conv = ConversationStore(self.store)
        self.projection = ConversationProjection(self.store, self.conv)
        self.thread_id = "thr-plan-test"
        self.conv.save_state(ThreadState(thread_id=self.thread_id))

    def tearDown(self) -> None:
        self.store.close()
        self._tmp.cleanup()

    def _apply(self, event_type: str, payload: dict) -> None:
        stored = self.store.append_events(
            ev.thread_event(self.thread_id, event_type, payload),
        )
        for event in stored:
            self.projection.apply(event)

    def test_projection_upsert_and_replay(self) -> None:
        self._apply(ev.EV_PLAN_UPDATED, {
            "revision": 1,
            "phase": "proposed",
            "source": "fixture",
            "tasks": [
                {"task_id": "t1", "title": "One", "status": "pending"},
                {"task_id": "t2", "title": "Two", "status": "pending"},
            ],
        })
        state = self.conv.get_state(self.thread_id)
        assert state.plan is not None
        self.assertEqual(len(state.plan.tasks), 2)

        self._apply(ev.EV_PLAN_UPDATED, {
            "revision": 2,
            "phase": "executing",
            "patch": True,
            "source": "fixture",
            "tasks": [
                {"task_id": "t1", "title": "One", "status": "in_progress"},
                {"task_id": "t3", "title": "Three", "status": "pending"},
            ],
        })
        state = self.conv.get_state(self.thread_id)
        assert state.plan is not None
        self.assertEqual(len(state.plan.tasks), 3)
        self.assertEqual({task.task_id for task in state.plan.tasks}, {"t1", "t2", "t3"})

        events = self.store.read_events(
            ev.AGGREGATE_THREAD, self.thread_id, limit=10_000,
        )
        fresh = ConversationStore(self.store)
        fresh.save_state(ThreadState(thread_id=self.thread_id))
        rebuilt = ConversationProjection(self.store, fresh)
        for event in events:
            rebuilt.apply(event)
        again = fresh.get_state(self.thread_id)
        assert again.plan is not None
        self.assertEqual(len(again.plan.tasks), 3)
        self.assertEqual(again.plan.revision, 2)

    def test_markdown_todos_do_not_create_plan(self) -> None:
        self._apply(ev.EV_MESSAGE_COMPLETED, {
            "turn_id": "turn-1",
            "role": "assistant",
            "text": "- [x] done\n- [ ] todo",
        })
        state = self.conv.get_state(self.thread_id)
        self.assertIsNone(state.plan)

    def test_unsupported_clear(self) -> None:
        self._apply(ev.EV_PLAN_CLEARED, {
            "unsupported": True,
            "unsupported_reason": "Pi 无计划事件",
        })
        state = self.conv.get_state(self.thread_id)
        assert state.plan is not None
        self.assertEqual(state.plan.phase, "unsupported")
        self.assertEqual(state.plan.tasks, [])


class AdapterPlanMappingTests(unittest.TestCase):
    def test_acp_plan_is_not_message_delta(self) -> None:
        mapped = normalize_session_update({
            "sessionUpdate": "plan",
            "entries": [
                {"id": "a", "content": "Research", "status": "pending"},
                {"id": "b", "title": "Implement", "status": "in_progress"},
            ],
        })
        self.assertEqual(len(mapped), 1)
        etype, native, payload = mapped[0]
        self.assertEqual(etype, AgentEventType.PLAN_UPDATED)
        self.assertEqual(native, "acp.plan")
        self.assertNotIn("text", payload)
        self.assertEqual(payload["phase"], "executing")
        self.assertEqual(
            [row["task_id"] for row in payload["tasks"]],
            ["a", "b"],
        )

    def test_codex_plan_payload_and_notification(self) -> None:
        payload = _codex_plan_payload({
            "turnId": "turn-9",
            "plan": {
                "title": "Ship C20",
                "steps": [
                    {"id": "t1", "title": "Contracts", "status": "pending"},
                    {"id": "t2", "title": "UI", "status": "pending"},
                ],
            },
        }, patch=False)
        self.assertEqual(payload["phase"], "proposed")
        self.assertEqual(len(payload["tasks"]), 2)
        delta = _codex_plan_payload({
            "item": {"id": "t1", "title": "Contracts", "status": "in_progress"},
        }, patch=True)
        self.assertTrue(delta["patch"])
        self.assertEqual(delta["phase"], "executing")

        adapter = object.__new__(CodexAppServerAdapter)
        events, done = CodexAppServerAdapter._map_notification(
            adapter,
            {
                "method": "turn/plan/updated",
                "params": {
                    "turnId": "turn-9",
                    "plan": {"steps": [{"id": "t1", "title": "One"}]},
                },
            },
            {},
            EventSequencer(),
            {"agent_session_id": "sess-1"},
            "turn-9",
        )
        self.assertFalse(done)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0].event_type, AgentEventType.PLAN_UPDATED)

        ignored, _ = CodexAppServerAdapter._map_notification(
            adapter,
            {"method": "thread/name/updated", "params": {"name": "x"}},
            {},
            EventSequencer(),
            {"agent_session_id": "sess-1"},
            "turn-9",
        )
        self.assertEqual(ignored, [])


if __name__ == "__main__":
    unittest.main()
