"""#218: duplicate / progress-as-started must not crush tool duration into LLM."""

from __future__ import annotations

import unittest
from datetime import datetime, timedelta, timezone
from types import SimpleNamespace

from muteki.conversation.manager import _conversation_statistics
from muteki.conversation.models import TurnRecord


def _event(event_type: str, occurred_at: datetime, **payload: object) -> SimpleNamespace:
    return SimpleNamespace(
        event_type=event_type,
        occurred_at=occurred_at,
        payload=dict(payload),
    )


def _turn(turn_id: str = "turn-1") -> TurnRecord:
    return TurnRecord(
        turn_id=turn_id,
        thread_id="thr-1",
        status="completed",
        created_at=datetime(2026, 9, 26, 7, 18, 0, tzinfo=timezone.utc),
    )


class ToolStartTimingIdempotentTests(unittest.TestCase):
    def test_progress_mapped_as_second_started_keeps_first_origin(self) -> None:
        """Replay of the sleep45 evidence shape: start, late started, complete."""
        t0 = datetime(2026, 9, 26, 7, 18, 7, 50459, tzinfo=timezone.utc)
        t_progress = datetime(2026, 9, 26, 7, 19, 9, 755463, tzinfo=timezone.utc)
        t_done = datetime(2026, 9, 26, 7, 19, 9, 757250, tzinfo=timezone.utc)
        t_end = t0 + timedelta(milliseconds=75874)
        turn_id = "turn-sleep"
        call_id = "call-sleep45"
        events = [
            _event("core.turn.started", t0, turn_id=turn_id),
            _event(
                "core.tool.started", t0, turn_id=turn_id, call_id=call_id, tool="shell",
            ),
            # Historical mis-map: TOOL_PROGRESS persisted as started.
            _event(
                "core.tool.started",
                t_progress,
                turn_id=turn_id,
                call_id=call_id,
                chunk="QA_TOOL_FINISHED\n",
            ),
            _event(
                "core.tool.completed",
                t_done,
                turn_id=turn_id,
                call_id=call_id,
                tool="shell",
            ),
            _event("core.turn.completed", t_end, turn_id=turn_id),
        ]
        stats = _conversation_statistics(events, [_turn(turn_id)], {})
        self.assertGreaterEqual(stats["tool_duration_ms"], 60_000)
        self.assertLess(stats["tool_duration_ms"], 70_000)
        # Must not collapse tool into ~2ms with LLM ≈ full turn.
        self.assertNotEqual(stats.get("tool_duration_ms"), 2)
        self.assertLess(stats["llm_duration_ms"], 30_000)
        self.assertGreater(stats["llm_duration_ms"], 5_000)

    def test_progress_event_ignored_for_timing(self) -> None:
        t0 = datetime(2026, 9, 26, 12, 0, 0, tzinfo=timezone.utc)
        t_mid = t0 + timedelta(seconds=40)
        t_done = t0 + timedelta(seconds=45)
        t_end = t0 + timedelta(seconds=50)
        turn_id = "turn-a"
        call_id = "c1"
        events = [
            _event("core.turn.started", t0, turn_id=turn_id),
            _event("core.tool.started", t0, turn_id=turn_id, call_id=call_id),
            _event(
                "core.tool.progress",
                t_mid,
                turn_id=turn_id,
                call_id=call_id,
                chunk="...",
            ),
            _event("core.tool.completed", t_done, turn_id=turn_id, call_id=call_id),
            _event("core.turn.completed", t_end, turn_id=turn_id),
        ]
        stats = _conversation_statistics(events, [_turn(turn_id)], {})
        self.assertEqual(stats["tool_duration_ms"], 45_000)
        self.assertEqual(stats["llm_duration_ms"], 5_000)

    def test_native_duration_preferred(self) -> None:
        t0 = datetime(2026, 9, 26, 12, 0, 0, tzinfo=timezone.utc)
        t_done = t0 + timedelta(seconds=10)
        turn_id = "turn-b"
        call_id = "c2"
        events = [
            _event("core.turn.started", t0, turn_id=turn_id),
            _event("core.tool.started", t0, turn_id=turn_id, call_id=call_id),
            _event(
                "core.tool.completed",
                t_done,
                turn_id=turn_id,
                call_id=call_id,
                duration_ms=45000,
            ),
            _event("core.turn.completed", t0 + timedelta(seconds=50), turn_id=turn_id),
        ]
        stats = _conversation_statistics(events, [_turn(turn_id)], {})
        self.assertEqual(stats["tool_duration_ms"], 45_000)

    def test_concurrent_tools_keyed_by_turn_and_call(self) -> None:
        t0 = datetime(2026, 9, 26, 12, 0, 0, tzinfo=timezone.utc)
        events = [
            _event("core.turn.started", t0, turn_id="t1"),
            _event("core.turn.started", t0, turn_id="t2"),
            _event("core.tool.started", t0, turn_id="t1", call_id="same"),
            _event("core.tool.started", t0, turn_id="t2", call_id="same"),
            _event(
                "core.tool.completed",
                t0 + timedelta(seconds=10),
                turn_id="t1",
                call_id="same",
            ),
            _event(
                "core.tool.completed",
                t0 + timedelta(seconds=20),
                turn_id="t2",
                call_id="same",
            ),
            _event("core.turn.completed", t0 + timedelta(seconds=30), turn_id="t1"),
            _event("core.turn.completed", t0 + timedelta(seconds=30), turn_id="t2"),
        ]
        stats = _conversation_statistics(
            events,
            [_turn("t1"), _turn("t2")],
            {},
        )
        self.assertEqual(stats["tool_duration_ms"], 30_000)

    def test_duplicate_completed_does_not_double_count(self) -> None:
        t0 = datetime(2026, 9, 26, 12, 0, 0, tzinfo=timezone.utc)
        t_done = t0 + timedelta(seconds=5)
        turn_id = "turn-c"
        call_id = "c3"
        events = [
            _event("core.turn.started", t0, turn_id=turn_id),
            _event("core.tool.started", t0, turn_id=turn_id, call_id=call_id),
            _event("core.tool.completed", t_done, turn_id=turn_id, call_id=call_id),
            _event("core.tool.completed", t_done, turn_id=turn_id, call_id=call_id),
            _event("core.turn.completed", t0 + timedelta(seconds=8), turn_id=turn_id),
        ]
        stats = _conversation_statistics(events, [_turn(turn_id)], {})
        self.assertEqual(stats["tool_duration_ms"], 5_000)

    def test_reported_duration_replay_and_late_start_count_once(self) -> None:
        t0 = datetime(2026, 9, 26, 12, 0, 0, tzinfo=timezone.utc)
        done = _event("core.tool.completed", t0 + timedelta(seconds=5), turn_id="t", call_id="c", duration_ms=4000)
        events = [done, done, _event("core.tool.started", t0, turn_id="t", call_id="c"), done]
        self.assertEqual(_conversation_statistics(events, [_turn("t")], {})["tool_duration_ms"], 4000)

    def test_out_of_order_completion_uses_earliest_start(self) -> None:
        t0 = datetime(2026, 9, 26, 12, 0, 0, tzinfo=timezone.utc)
        events = [
            _event("core.tool.completed", t0 + timedelta(seconds=5), turn_id="t", call_id="c"),
            _event("core.tool.started", t0 + timedelta(seconds=3), turn_id="t", call_id="c"),
            _event("core.tool.started", t0, turn_id="t", call_id="c"),
        ]
        self.assertEqual(_conversation_statistics(events, [_turn("t")], {})["tool_duration_ms"], 5000)

    def test_parallel_calls_use_union_for_residual_and_report_basis(self) -> None:
        t0 = datetime(2026, 9, 26, 12, 0, 0, tzinfo=timezone.utc)
        events = [
            _event("core.turn.started", t0, turn_id="t"),
            _event("core.tool.started", t0, turn_id="t", call_id="a"),
            _event("core.tool.started", t0 + timedelta(seconds=2), turn_id="t", call_id="b"),
            _event("core.tool.completed", t0 + timedelta(seconds=7), turn_id="t", call_id="a"),
            _event("core.tool.completed", t0 + timedelta(seconds=8), turn_id="t", call_id="b"),
            _event("core.turn.completed", t0 + timedelta(seconds=10), turn_id="t"),
        ]
        stats = _conversation_statistics(events, [_turn("t")], {})
        self.assertEqual(stats["tool_duration_ms"], 13000)
        self.assertEqual(stats["tool_wall_duration_ms"], 8000)
        self.assertEqual(stats["llm_duration_ms"], 2000)
        self.assertEqual(stats["tool_duration_source"], "lifecycle")
        self.assertEqual(stats["llm_duration_source"], "residual_estimate")


class ExecutorProgressMappingTests(unittest.TestCase):
    def test_tool_progress_constant_exists(self) -> None:
        from muteki.conversation import events as ev

        self.assertEqual(ev.EV_TOOL_PROGRESS, "core.tool.progress")
        self.assertNotEqual(ev.EV_TOOL_PROGRESS, ev.EV_TOOL_STARTED)


if __name__ == "__main__":
    unittest.main()
