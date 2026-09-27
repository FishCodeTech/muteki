"""Unit tests for C21 agent tree upsert / projection / shell exclusion."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.conversation import events as ev
from muteki.conversation.agents import normalize_agent, upsert_agent_tree
from muteki.conversation.models import ThreadState
from muteki.conversation.projections import ConversationProjection
from muteki.conversation.store import ConversationStore
from muteki.platform.store import PlatformStore


class AgentTreeUpsertTests(unittest.TestCase):
    def test_upsert_parent_child_by_stable_id(self) -> None:
        first = upsert_agent_tree(None, {
            "revision": 1,
            "source": "fixture",
            "agents": [
                {
                    "agent_id": "parent-1",
                    "title": "Research lead",
                    "status": "running",
                    "request": "Explore auth",
                    "turn_id": "turn-a",
                },
                {
                    "agent_id": "child-1",
                    "parent_id": "parent-1",
                    "title": "Worker A",
                    "status": "pending",
                    "request": "Check login",
                    "turn_id": "turn-a",
                },
            ],
        })
        self.assertEqual(first.revision, 1)
        self.assertEqual(
            {a.agent_id: a.parent_id for a in first.agents},
            {"parent-1": None, "child-1": "parent-1"},
        )

        second = upsert_agent_tree(first, {
            "revision": 2,
            "source": "fixture",
            "agents": [
                {
                    "agent_id": "child-1",
                    "parent_id": "parent-1",
                    "title": "Worker A",
                    "status": "completed",
                    "result": "login ok",
                },
            ],
        }, patch=True)
        by_id = {a.agent_id: a for a in second.agents}
        self.assertEqual(by_id["child-1"].status, "completed")
        self.assertEqual(by_id["child-1"].result, "login ok")
        self.assertEqual(by_id["parent-1"].status, "running")

    def test_shell_tool_dict_without_agent_markers_is_not_an_agent(self) -> None:
        node = normalize_agent({
            "tool": "shell",
            "name": "bash",
            "input": {"command": "ls"},
        })
        self.assertIsNone(node)

    def test_stale_revision_ignored(self) -> None:
        current = upsert_agent_tree(None, {
            "revision": 3,
            "agents": [{"agent_id": "a1", "title": "A"}],
        })
        stale = upsert_agent_tree(current, {
            "revision": 2,
            "agents": [{"agent_id": "a9", "title": "B"}],
        })
        self.assertIs(stale, current)


class AgentProjectionTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.store = PlatformStore(Path(self._tmp.name) / "platform.db")
        self.conv = ConversationStore(self.store)
        self.projection = ConversationProjection(self.store, self.conv)
        self.thread_id = "thr-agent-test"
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
        self._apply(ev.EV_AGENT_UPDATED, {
            "revision": 1,
            "source": "fixture",
            "agents": [
                {"agent_id": "p", "title": "Parent", "status": "running",
                 "request": "do work", "turn_id": "t1"},
                {"agent_id": "c", "parent_id": "p", "title": "Child",
                 "status": "completed", "result": "done", "turn_id": "t1"},
            ],
        })
        state = self.conv.get_state(self.thread_id)
        assert state.agents is not None
        self.assertEqual(len(state.agents.agents), 2)
        self.assertFalse(state.agents.unsupported)

        # Rebuild from event stream (disconnect recovery).
        rebuilt = ConversationProjection(self.store, self.conv)
        rebuilt.rebuild_thread(self.thread_id)
        again = self.conv.get_state(self.thread_id)
        assert again.agents is not None
        self.assertEqual(
            {a.agent_id: a.parent_id for a in again.agents.agents},
            {"p": None, "c": "p"},
        )
        child = next(a for a in again.agents.agents if a.agent_id == "c")
        self.assertEqual(child.result, "done")

    def test_unsupported_clears_agents(self) -> None:
        self._apply(ev.EV_AGENT_UPDATED, {
            "revision": 1,
            "agents": [{"agent_id": "p", "title": "Parent"}],
        })
        self._apply(ev.EV_AGENT_CLEARED, {
            "unsupported": True,
            "unsupported_reason": "Pi 未上报委派事件",
            "tool_activity_summary": "正在运行命令 · 3 条工具记录",
        })
        state = self.conv.get_state(self.thread_id)
        assert state.agents is not None
        self.assertTrue(state.agents.unsupported)
        self.assertEqual(state.agents.agents, [])
        self.assertIn("工具", state.agents.tool_activity_summary or "")


class CodexAgentOwnershipTests(unittest.TestCase):
    def test_foreign_thread_notifications_do_not_project_into_root(self):
        from muteki.external_agents.codex import CodexAppServerAdapter
        from muteki.external_agents.events import EventSequencer
        adapter = CodexAppServerAdapter(schema_probe=False)
        ctx = {"thread_id": "root"}
        events, done = adapter._map_notification({
            "method": "item/agentMessage/delta",
            "params": {"threadId": "other", "turnId": "other-turn", "delta": "ordinary other thread text"},
        }, ctx, EventSequencer(), {"agent_session_id": "session"}, "root-turn")
        self.assertEqual(events, [])
        self.assertFalse(done)
        self.assertNotIn("assistant_unknown_deltas", ctx)

    def test_agent_followup_replaces_activity_and_drops_old_result(self):
        from muteki.external_agents.codex import CodexAppServerAdapter
        from muteki.external_agents.events import EventSequencer, build_event
        seq = EventSequencer()
        event = lambda kind, payload: build_event(kind, seq, agent_session_id="session", payload=payload)
        ctx = {"thread_id": "root"}
        completed = {"type": "collabAgentToolCall", "id": "first", "tool": "spawnAgent",
                     "senderThreadId": "root", "receiverThreadIds": ["child"],
                     "agentsStates": {"child": {"status": "completed", "message": "old result"}}}
        CodexAppServerAdapter._map_item(event, {"item": completed}, ctx, started=False)
        followup = {**completed, "id": "second", "tool": "followupTask", "prompt": "new ordinary task",
                    "agentsStates": {"child": {"status": "running"}}}
        mapped = CodexAppServerAdapter._map_item(event, {"item": followup}, ctx, started=True)
        node = mapped[0].payload["agents"][0]
        self.assertEqual(node["call_id"], "second")
        self.assertEqual(node["request"], "new ordinary task")
        self.assertNotIn("result", node)
        self.assertEqual(CodexAppServerAdapter._map_item(event, {"item": completed}, ctx, started=False), [])
        self.assertEqual(ctx["agent_nodes"]["child"]["status"], "running")


if __name__ == "__main__":
    unittest.main()
