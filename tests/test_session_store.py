"""Finding B: SessionStore.summary() must carry multi-flag fields through and compute
`solved` by mode, so a rehydrated multi-flag run isn't flattened to a single-flag
look-alike and a partial multi-flag run isn't falsely marked solved."""

import asyncio
import json
from pathlib import Path

import pytest

from muteki.core.events import Event, EventType
from muteki.core.session_store import ProjectionIdentityConflict, SessionStore
from muteki.core.path_ids import RunIdPathError, encode_run_id


def _write(root: Path, run_id: str, events: list[dict]) -> None:
    path = root / f"{run_id}.jsonl"
    with path.open("w", encoding="utf-8") as f:
        for i, ev in enumerate(events, 1):
            ev.setdefault("seq", i)
            ev.setdefault("ts", float(i))
            ev.setdefault("run_id", run_id)
            f.write(json.dumps(ev) + "\n")


def test_run_id_path_encoding_is_injective_and_reversible(tmp_path: Path) -> None:
    store = SessionStore(root=tmp_path)
    first = "foo..bar"
    second = "foo_bar"

    assert store._path(first) != store._path(second)
    store._path(first).write_text("", encoding="utf-8")
    store._path(second).write_text("", encoding="utf-8")
    assert set(store.list_runs()) == {first, second}


def test_overlong_unicode_run_id_fails_before_filesystem_access(tmp_path: Path) -> None:
    run_id = "题" * 256
    with pytest.raises(RunIdPathError, match="too long"):
        encode_run_id(run_id)
    store = SessionStore(root=tmp_path)
    with pytest.raises(RunIdPathError, match="too long"):
        store._path(run_id)
    assert list(tmp_path.iterdir()) == []


def test_summary_multi_flag_partial_not_solved(tmp_path: Path) -> None:
    """1/3 flags collected, run.finished solved=false → solved stays False, flags are
    preserved, and the multi-flag mode survives (expected_flags=3, multi_flag=True).
    The old summary() dropped these fields and FlagFound forced solved=True."""
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-mf", [
        {"event_type": "run.started",
         "payload": {"challenge": {"name": "triple", "category": "web",
                                   "expected_flags": 3, "multi_flag": True}}},
        {"event_type": "insight.event",
         "payload": {"kind": "FlagFound", "flag": "flag{one}"}},
        {"event_type": "run.finished",
         "payload": {"solved": False, "flags": ["flag{one}"],
                     "expected_flags": 3, "multi_flag": True}},
    ])
    s = store.summary("run-mf")
    assert s["solved"] is False, "a 1/3 multi-flag run is NOT solved"
    assert s["flags"] == ["flag{one}"]
    assert s["flag"] == "flag{one}"
    assert s["expected_flags"] == 3
    assert s["multi_flag"] is True


def test_summary_treats_preparing_as_a_persisted_started_run(tmp_path: Path) -> None:
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-preparing", [{
        "event_type": "run.preparing",
        "payload": {"challenge": {
            "name": "preflight", "category": "web",
            "expected_flags": 2, "multi_flag": True,
        }},
    }])

    summary = store.summary("run-preparing")

    assert summary["started"] is True
    assert summary["name"] == "preflight"
    assert summary["category"] == "web"
    assert summary["expected_flags"] == 2
    assert summary["multi_flag"] is True


def test_summary_multi_flag_complete_is_solved(tmp_path: Path) -> None:
    """3/3 flags collected with run.finished solved=true → solved True, all flags kept."""
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-done", [
        {"event_type": "run.started",
         "payload": {"challenge": {"expected_flags": 3, "multi_flag": True}}},
        {"event_type": "insight.event", "payload": {"kind": "FlagFound", "flag": "f1"}},
        {"event_type": "insight.event", "payload": {"kind": "FlagFound", "flag": "f2"}},
        {"event_type": "insight.event", "payload": {"kind": "FlagFound", "flag": "f3"}},
        {"event_type": "run.finished",
         "payload": {"solved": True, "flags": ["f1", "f2", "f3"],
                     "expected_flags": 3, "multi_flag": True}},
    ])
    s = store.summary("run-done")
    assert s["solved"] is True
    assert s["flags"] == ["f1", "f2", "f3"]
    assert s["expected_flags"] == 3
    assert s["multi_flag"] is True


def test_summary_single_flag_ghost_run_stays_solved(tmp_path: Path) -> None:
    """Ghost run: a FlagFound but NO run.finished (killed before emitting it). For a
    single-flag run a found flag IS a win — solved must remain True after restart,
    otherwise the rail would wrongly flip a solved run back to unsolved."""
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-ghost", [
        {"event_type": "run.started",
         "payload": {"challenge": {"name": "single", "category": "crypto"}}},
        {"event_type": "insight.event",
         "payload": {"kind": "FlagFound", "flag": "flag{got_it}"}},
        # no run.finished
    ])
    s = store.summary("run-ghost")
    assert s["solved"] is True
    assert s["flag"] == "flag{got_it}"
    assert s["flags"] == ["flag{got_it}"]
    assert s["expected_flags"] == 1
    assert s["multi_flag"] is False


def test_authority_recovery_flag_does_not_infer_solved(tmp_path: Path) -> None:
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-authority-recovery", [
        {"event_type": "run.started", "payload": {"challenge": {}}},
        {
            "event_type": "flag.accepted",
            "payload": {
                "schema_id": "muteki.flag-accepted-projection.v1",
                "flag": "flag{accepted_only}",
                "publication_id": "outbox:flag:" + "a" * 64,
                "evaluation_id": "a" * 64,
                "flag_digest": "b" * 64,
                "gate_receipt_digest": "c" * 64,
            },
        },
    ])

    summary = store.summary("run-authority-recovery")
    assert summary["flags"] == ["flag{accepted_only}"]
    assert summary["solved"] is False
    assert summary["finished"] is False


@pytest.mark.asyncio
async def test_append_if_absent_uses_stable_publication_identity(
    tmp_path: Path,
) -> None:
    from muteki.core.events import Event, EventType

    store = SessionStore(root=tmp_path)
    event = Event(
        event_type=EventType.FLAG_ACCEPTED,
        run_id="run-idempotent",
        payload={
            "schema_id": "muteki.flag-accepted-projection.v1",
            "flag": "flag{one}",
            "publication_id": "outbox:flag:" + "c" * 64,
        },
    )
    assert await store.append_if_absent(
        event,
        identity_field="publication_id",
        identity=event.payload["publication_id"],
    ) is True
    assert await store.append_if_absent(
        event.model_copy(update={"seq": 99, "ts": event.ts + 1}),
        identity_field="publication_id",
        identity=event.payload["publication_id"],
    ) is False
    assert len(store.load_all("run-idempotent")) == 1


@pytest.mark.asyncio
async def test_append_if_absent_rejects_divergent_projection_identity(
    tmp_path: Path,
) -> None:
    from muteki.core.events import Event, EventType

    store = SessionStore(root=tmp_path)
    publication_id = "outbox:flag:" + "d" * 64
    event = Event(
        event_type=EventType.FLAG_ACCEPTED,
        run_id="run-collision",
        payload={
            "schema_id": "muteki.flag-accepted-projection.v1",
            "publication_id": publication_id,
            "flag": "flag{one}",
        },
    )
    assert await store.append_if_absent(
        event, identity_field="publication_id", identity=publication_id
    )
    divergent = event.model_copy(
        update={"payload": {**event.payload, "flag": "flag{two}"}}
    )
    with pytest.raises(ProjectionIdentityConflict):
        await store.append_if_absent(
            divergent, identity_field="publication_id", identity=publication_id
        )
    assert len(store.load_all("run-collision")) == 1


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "mutate",
    [
        lambda event: event.model_copy(update={"event_type": EventType.BLACKBOARD_DELTA}),
        lambda event: event.model_copy(update={
            "payload": {**event.payload, "schema_id": "wrong-schema"},
        }),
        lambda event: event.model_copy(update={
            "payload": {**event.payload, "evaluation_id": "f" * 64},
        }),
    ],
)
async def test_append_if_absent_collision_compares_type_schema_and_content(
    tmp_path: Path, mutate,
) -> None:
    from muteki.core.events import Event, EventType

    store = SessionStore(root=tmp_path)
    publication_id = "outbox:flag:" + "9" * 64
    event = Event(
        event_type=EventType.FLAG_ACCEPTED,
        run_id="run-full-identity",
        payload={
            "schema_id": "muteki.flag-accepted-projection.v1",
            "publication_id": publication_id,
            "evaluation_id": "a" * 64,
            "flag": "flag{one}",
            "flag_digest": "b" * 64,
            "gate_receipt_digest": "c" * 64,
        },
    )
    assert await store.append_if_absent(
        event, identity_field="publication_id", identity=publication_id
    )
    with pytest.raises(ProjectionIdentityConflict):
        await store.append_if_absent(
            mutate(event), identity_field="publication_id", identity=publication_id
        )


def test_append_if_absent_ignores_event_invalid_identity_lookalike(
    tmp_path: Path,
) -> None:
    store = SessionStore(root=tmp_path)
    run_id = "run-invalid-identity-lookalike"
    publication_id = "outbox:flag:" + "8" * 64
    event = Event(
        event_type=EventType.FLAG_ACCEPTED,
        run_id=run_id,
        payload={
            "schema_id": "muteki.flag-accepted-projection.v1",
            "publication_id": publication_id,
            "evaluation_id": "a" * 64,
            "flag": "flag{valid_projection}",
            "flag_digest": "b" * 64,
            "gate_receipt_digest": "c" * 64,
        },
    )
    invalid = event.model_dump(mode="json")
    invalid["seq"] = "not-an-int"
    store._path(run_id).write_text(
        json.dumps(invalid) + "\n", encoding="utf-8"
    )

    async def _replay() -> list[Event]:
        return [row async for row in store.replay(run_id)]

    assert store.load_all(run_id) == []
    assert asyncio.run(_replay()) == []
    assert store.append_if_absent_sync(
        event, identity_field="publication_id", identity=publication_id
    ) is True
    loaded = store.load_all(run_id)
    assert len(loaded) == 1
    assert loaded[0]["seq"] == 0
    assert loaded[0]["payload"]["publication_id"] == publication_id



def test_append_if_absent_skips_invalid_shapes_and_separates_torn_tail(
    tmp_path: Path,
) -> None:
    from muteki.core.events import Event, EventType

    store = SessionStore(root=tmp_path)
    run_id = "run-torn"
    path = store._path(run_id)
    path.write_text('[]\n{"event_type":"run.started","payload":[]}', encoding="utf-8")
    publication_id = "outbox:flag:" + "e" * 64
    event = Event(
        event_type=EventType.FLAG_ACCEPTED,
        run_id=run_id,
        payload={
            "schema_id": "muteki.flag-accepted-projection.v1",
            "publication_id": publication_id,
            "flag": "flag{safe}",
        },
    )

    assert store.append_if_absent_sync(
        event, identity_field="publication_id", identity=publication_id
    )
    assert store.append_if_absent_sync(
        event.model_copy(update={"seq": 9}),
        identity_field="publication_id",
        identity=publication_id,
    ) is False
    parsed = []
    for raw in path.read_text(encoding="utf-8").splitlines():
        try:
            parsed.append(json.loads(raw))
        except json.JSONDecodeError:
            pass
    assert parsed[-1]["payload"]["publication_id"] == publication_id
    assert store.last_stream_seq(run_id) >= 1
    summary = store.summary(run_id)
    assert summary["flags"] == ["flag{safe}"]
    assert summary["solved"] is False


def test_summary_multi_flag_ghost_partial_not_solved(tmp_path: Path) -> None:
    """Ghost multi-flag run (no run.finished) with only 1/2 flags must NOT be solved —
    a partial multi-flag set is not a win even without a terminal event."""
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-mfg", [
        {"event_type": "run.started",
         "payload": {"challenge": {"expected_flags": 2, "multi_flag": True}}},
        {"event_type": "insight.event", "payload": {"kind": "FlagFound", "flag": "a"}},
        # no run.finished, only 1 of 2
    ])
    s = store.summary("run-mfg")
    assert s["solved"] is False
    assert s["flags"] == ["a"]
    assert s["expected_flags"] == 2
    assert s["multi_flag"] is True


def test_summary_finished_verdict_overrides_flagfound(tmp_path: Path) -> None:
    """An explicit run.finished solved=False is authoritative even if a FlagFound was
    emitted earlier (single-flag): the run was reopened / flag invalidated."""
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-inv", [
        {"event_type": "run.started", "payload": {"challenge": {}}},
        {"event_type": "insight.event", "payload": {"kind": "FlagFound", "flag": "x"}},
        {"event_type": "run.finished", "payload": {"solved": False}},
    ])
    s = store.summary("run-inv")
    assert s["solved"] is False


def test_summary_resolve_reopen_preserves_prior_flags(tmp_path: Path) -> None:
    """A resolve/continue reopen makes the run active again but must not erase the
    already recovered multi-flag results from durable history."""
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-resolve", [
        {"event_type": "run.started",
         "payload": {"challenge": {"expected_flags": 3, "multi_flag": True}}},
        {"event_type": "run.finished",
         "payload": {"solved": True, "flags": ["flag{a}", "flag{b}"],
                     "expected_flags": 3, "multi_flag": True}},
        {"event_type": "run.reopened", "payload": {"reason": "resolve"}},
    ])
    s = store.summary("run-resolve")
    assert s["finished"] is False
    assert s["solved"] is False
    assert s["flags"] == ["flag{a}", "flag{b}"]
    assert s["flag"] == "flag{a}"


def test_summary_false_positive_reopen_drops_only_target_flag(tmp_path: Path) -> None:
    """A per-flag false-positive reopen removes only the targeted bad flag when the
    rail is rebuilt from JSONL after a server restart."""
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-fp", [
        {"event_type": "run.started",
         "payload": {"challenge": {"expected_flags": 3, "multi_flag": True}}},
        {"event_type": "run.finished",
         "payload": {"solved": True, "flags": ["flag{a}", "flag{b}", "flag{c}"],
                     "expected_flags": 3, "multi_flag": True}},
        {"event_type": "run.reopened", "payload": {"flag": "flag{b}"}},
    ])
    s = store.summary("run-fp")
    assert s["finished"] is False
    assert s["solved"] is False
    assert s["flags"] == ["flag{a}", "flag{c}"]
    assert s["flag"] == "flag{a}"


def test_summary_ignores_invalidated_flag_replayed_from_blackboard(tmp_path: Path) -> None:
    """After mark_false the coordinator can replay an old flag_found from the shared
    graph. Rehydrated rail summaries must remember the invalidation and ignore that
    stale flag, including any stale terminal event that carries it."""
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-replay-fp", [
        {"event_type": "run.started",
         "payload": {"challenge": {"name": "vm", "category": "reverse"}}},
        {"event_type": "run.finished",
         "payload": {"solved": True, "flag": "flag{bad}"}},
        {"event_type": "blackboard.delta",
         "payload": {"kind": "flag_invalidated", "flag": "flag{bad}"}},
        {"event_type": "run.reopened", "payload": {"flag": "flag{bad}"}},
        {"event_type": "blackboard.delta",
         "payload": {"kind": "flag_found", "flag": "flag{bad}"}},
        {"event_type": "run.finished",
         "payload": {"solved": True, "flag": "flag{bad}"}},
    ])
    s = store.summary("run-replay-fp")
    assert s["flags"] == []
    assert s["flag"] is None
    assert s["solved"] is False


def test_summary_empty_run_defaults(tmp_path: Path) -> None:
    """A run with no events yet returns safe defaults including the new fields."""
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-empty", [])
    s = store.summary("run-empty")
    assert s["flags"] == []
    assert s["expected_flags"] == 1
    assert s["multi_flag"] is False
    assert s["solved"] is False


def test_replay_and_load_all_continue_after_malformed_and_invalid_rows(
    tmp_path: Path,
) -> None:
    store = SessionStore(root=tmp_path)
    run_id = "run-invalid-rows"
    valid_start = Event(
        event_type=EventType.RUN_STARTED,
        seq=1,
        ts=1.0,
        run_id=run_id,
        payload={"challenge": {"name": "valid"}},
    )
    valid_later = Event(
        event_type=EventType.REASONING_DELTA,
        seq=2,
        ts=2.0,
        run_id=run_id,
        payload={"text": "later"},
    )
    rows = [
        valid_start.model_dump_json(),
        "{malformed",
        json.dumps(["valid-json", "not-an-object"]),
        json.dumps({
            "event_type": "not.a.real.event",
            "seq": 999,
            "ts": 3.0,
            "run_id": run_id,
            "payload": {},
        }),
        valid_later.model_dump_json(),
    ]
    store._path(run_id).write_text("\n".join(rows) + "\n", encoding="utf-8")

    async def _replay() -> list[Event]:
        return [event async for event in store.replay(run_id)]

    replayed = asyncio.run(_replay())
    loaded = store.load_all(run_id)
    assert [event.event_type for event in replayed] == [
        EventType.RUN_STARTED, EventType.REASONING_DELTA,
    ]
    assert [row["event_type"] for row in loaded] == [
        "run.started", "reasoning.delta",
    ]


def test_replay_monotonic_repairs_seq_reset(tmp_path: Path) -> None:
    """Durable SSE replay must repair historical raw seq resets.

    A backend restart/continue path once produced JSONL like 1,2,3,1,2. The
    browser uses Last-Event-ID as a stream cursor, so replay must expose that
    as 1,2,3,4,5 without rewriting the file.
    """
    store = SessionStore(root=tmp_path)
    _write(tmp_path, "run-reset", [
        {"event_type": "run.started", "seq": 1, "payload": {}},
        {"event_type": "reasoning.delta", "seq": 2, "payload": {"text": "before"}},
        {"event_type": "run.finished", "seq": 3, "payload": {}},
        {"event_type": "run.reopened", "seq": 1, "payload": {"reason": "resolve"}},
        {"event_type": "reasoning.delta", "seq": 2, "payload": {"text": "after"}},
    ])

    async def _collect():
        return [ev async for ev in store.replay_monotonic("run-reset", after_seq=3)]

    replayed = asyncio.run(_collect())
    assert store.last_stream_seq("run-reset") == 5
    assert [ev.seq for ev in replayed] == [4, 5]
    assert [ev.event_type.value for ev in replayed] == ["run.reopened", "reasoning.delta"]
