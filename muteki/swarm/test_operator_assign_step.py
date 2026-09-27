"""Operator assign-step (下达) keeps the verbatim intent open."""
from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.models.solve_graph import Challenge
from muteki.swarm.coordinator_control_command import ControlCommandContext
from muteki.swarm.coordinator_control_guidance import apply_control_context
from muteki.swarm.shared_graph import SQLiteSharedGraph


class _Insight:
    async def guidance(self, *args, **kwargs):
        return None

    async def dead_end(self, *args, **kwargs):
        return None


class _Harness:
    def __init__(self, graph: SQLiteSharedGraph) -> None:
        self.shared_graph = graph
        self._live_solvers: dict = {}
        self._standing_guidance: list[str] = []
        self._next_worker_guidance: list[str] = []
        self._target_epoch = 1
        self._target_redirect = ""
        self.review_policy = {"on_operator_hint": False}
        self._operator_event = None
        self._pending_help: list = []
        self.insight = _Insight()

    def _control_target_solvers(self, target: str) -> list:
        return []

    def _worker_runtime_exit_confirmed(self, worker) -> bool:
        return False

    async def _emit_coord_bb(self, *args, **kwargs):
        return None

    def _queue_review_request(self, **kwargs):
        return None

    def _ack_control(self, *args, **kwargs):
        return None


class OperatorAssignStepTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        challenge = Challenge(id="c-assign", name="t", category="web")
        self.graph = SQLiteSharedGraph(
            Path(self.tmp.name) / "graph.db", challenge)
        self.harness = _Harness(self.graph)

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def _intent_rows(self) -> list[tuple]:
        return self.graph._conn.execute(
            "SELECT intent_id, goal, status FROM intents "
            "WHERE challenge_id=?",
            (self.graph.challenge.id,),
        ).fetchall()

    async def _assign(self, action: str, text: str) -> None:
        ctx = ControlCommandContext(
            cmd={"action": action, "text": text, "target": "global"},
            payload={},
            text=text,
            delivery_text=text,
            action=action,
            original_action=action,
            target="global",
            request_id="",
            command_id="",
            scope_kind="global",
            scope_value="",
            scope_is_global=True,
            continuation_intent_id="",
        )
        await apply_control_context(self.harness, ctx)

    async def test_verbatim_request_text_stays_open(self) -> None:
        text = "inspect FCGI_BEGIN_REQUEST and REQUEST_METHOD"
        await self._assign("directive", text)
        rows = self._intent_rows()
        self.assertEqual(len(rows), 1)
        intent_id, goal, status = rows[0]
        self.assertTrue(str(intent_id).startswith("I-D-"))
        self.assertEqual(goal, text)
        self.assertEqual(status, "open")

    async def test_hint_stays_visible_without_creating_a_step(self) -> None:
        text = "the box password is hunter2"
        await self._assign("hint", text)
        self.assertEqual(self._intent_rows(), [])
        self.assertIn(text, self.harness._standing_guidance)
        yaml_text = self.graph.to_ctf_graph_yaml()
        self.assertIn("operatorNotes:", yaml_text)
        self.assertIn("hunter2", yaml_text)

    async def test_focus_and_redirect_aliases_are_the_same(self) -> None:
        await self._assign("focus", "try FCGI_BEGIN_REQUEST on the login")
        await self._assign("redirect", "continue at https://example.test/app")
        rows = self._intent_rows()
        self.assertEqual(len(rows), 2)
        self.assertTrue(all(status == "open" for _iid, _goal, status in rows))
        self.assertEqual(
            self.harness._target_redirect, "https://example.test/app")


if __name__ == "__main__":
    unittest.main()
