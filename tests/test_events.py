"""Round-trip + payload-shape coverage for every event type (Sprint 0.1 acceptance)."""

import json

import pytest

from muteki.core.events import (
    Event,
    EventType,
    context_state_payload,
    cost_payload,
    hitl_response_payload,
    insight_payload,
    solve_graph_delta_payload,
    tool_result_payload,
    worker_status_payload,
)


@pytest.mark.parametrize("etype", list(EventType))
def test_every_event_type_roundtrips(etype: EventType) -> None:
    ev = Event(event_type=etype, seq=7, run_id="run-1", payload={"k": "v"})
    raw = ev.model_dump_json()
    back = Event.model_validate_json(raw)
    assert back == ev
    assert back.event_type is etype
    assert back.seq == 7
    assert back.ts > 0  # auto-filled


def test_ts_autofill_and_override() -> None:
    a = Event(event_type=EventType.RUN_STARTED, run_id="r")
    assert a.ts > 0
    b = Event(event_type=EventType.RUN_STARTED, run_id="r", ts=123.0)
    assert b.ts == 123.0


def test_sse_frame_carries_seq_and_type() -> None:
    ev = Event(event_type=EventType.TEXT_MESSAGE_DELTA, seq=42, run_id="r")
    frame = ev.to_sse()
    assert frame.startswith("id: 42\n")
    assert "event: text.delta\n" in frame
    assert frame.endswith("\n\n")
    # data line is valid JSON
    data_line = [l for l in frame.splitlines() if l.startswith("data: ")][0]
    parsed = json.loads(data_line[len("data: ") :])
    assert parsed["seq"] == 42


def test_context_state_payload_shape() -> None:
    p = context_state_payload(
        zones=[{"label": "system", "tokens": 1200}],
        total=1200,
        limit=200000,
        compacted=[{"label": "history", "tokens": 5000}],
    )
    assert p["total"] == 1200
    assert p["limit"] == 200000
    assert p["compacted"][0]["tokens"] == 5000


def test_tool_result_payload_shape() -> None:
    p = tool_result_payload("web.http", {"status": 200}, artifact_id="a1", truncated=True)
    assert p["tool"] == "web.http"
    assert p["artifact_id"] == "a1"
    assert p["truncated"] is True


def test_insight_payload_kinds() -> None:
    fact = insight_payload("FactDiscovered", fact="offset=40", by="GPT-5")
    assert fact["kind"] == "FactDiscovered"
    assert fact["fact"] == "offset=40"
    flag = insight_payload("FlagFound", flag="flag{x}")
    assert flag["kind"] == "FlagFound"


def test_cost_payload_rounds_usd() -> None:
    p = cost_payload("challenge", usd=0.123456789, tokens=31000, challenge_id="c1")
    assert p["usd"] == 0.123457
    assert p["tokens"] == 31000
    assert p["challenge_id"] == "c1"


def test_worker_status_payload_shape() -> None:
    p = worker_status_payload(False, status="offline", reason="timeout", engine="claude")
    assert p == {
        "online": False,
        "status": "offline",
        "reason": "timeout",
        "engine": "claude",
        "session": "",
    }


def test_worker_status_payload_carries_session() -> None:
    p = worker_status_payload(True, status="online", reason="started",
                              engine="codex", session="thr-123")
    assert p["session"] == "thr-123"


def test_worker_status_payload_can_carry_backend_role() -> None:
    p = worker_status_payload(True, status="online", reason="started",
                              engine="claude", worker_role="review")
    assert p["worker_role"] == "review"


def test_worker_status_payload_can_carry_runtime_status() -> None:
    p = worker_status_payload(
        False, status="offline", reason="oom", engine="codex",
        runtime={"backend": "container", "status": "oom", "oom_killed": True})
    assert p["runtime"]["backend"] == "container"
    assert p["runtime"]["oom_killed"] is True


def test_hitl_and_solvegraph_payloads() -> None:
    h = hitl_response_payload("solver:opus-max", "hint", text="try ret2csu")
    assert h["target"] == "solver:opus-max"
    assert h["text"] == "try ret2csu"
    sg = solve_graph_delta_payload("hypothesis_status", id="H3", status="refuted")
    assert sg["kind"] == "hypothesis_status"
    assert sg["status"] == "refuted"
