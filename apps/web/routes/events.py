"""events routes moved from server.py."""
from __future__ import annotations

import asyncio
import json
from typing import Any

from fastapi import (
    FastAPI,
    HTTPException,
    Request,
    WebSocket,
    WebSocketDisconnect,
)
from sse_starlette.sse import EventSourceResponse, ServerSentEvent

from apps.web.auth import (
    AuthConfig,
    bearer_from_header,
    verify_token,
)
from apps.web.run_manager import RunManager
from muteki.core.events import Event, EventType


_REPLAY_BATCH_SIZE = 500
_REPLAY_RESET_EVENT = "replay.reset"
_REPLAY_BATCH_EVENT = "replay.batch"
_REPLAY_COMPLETE_EVENT = "replay.complete"


def _replay_batch_frame(events: list[Event]) -> dict[str, str]:
    return {
        "id": str(events[-1].seq),
        "event": _REPLAY_BATCH_EVENT,
        "data": json.dumps(
            {
                "events": [event.model_dump(mode="json") for event in events],
                "last_seq": events[-1].seq,
            },
            ensure_ascii=False,
            separators=(",", ":"),
        ),
    }


def register(app: FastAPI) -> None:
    h = app.state.route_helpers
    _dispatch_run_command = h._dispatch_run_command
    _run_receipt_status = h._run_receipt_status

    @app.get("/api/runs/{run_id}/events")
    async def events(
        run_id: str, request: Request, replay: str = "", after: int = 0,
    ) -> Any:
        # Auth: EventSource can't send an Authorization header, so the SSE stream
        # authenticates via a one-time ticket (?ticket=) minted by an
        # authenticated POST /api/auth/ticket. A bearer header is also accepted
        # (non-browser clients). This MUST run before the compatibility create, so
        # an unauthenticated open can't spawn empty run handles.
        cfg: AuthConfig = app.state.auth
        if cfg.enabled:
            tok = bearer_from_header(request.headers.get("Authorization"))
            authed = verify_token(cfg, tok) or app.state.tickets.redeem(
                request.query_params.get("ticket"))
            if not authed:
                raise HTTPException(status_code=401, detail="unauthorized")
        manager: RunManager = app.state.manager
        # A deck commonly opens its event stream BEFORE the run is launched (the
        # operator stares at an empty board, then fills the form). The compatibility
        # create still enters the shared Command API before this read-only stream
        # opens, so there is no route-level Manager state mutation.
        run = manager.get(run_id)
        if run is None:
            ensured = await _dispatch_run_command(
                "run.create", run_id, {"legacy": True, "run_id": run_id})
            if ensured.error is not None:
                raise HTTPException(
                    status_code=_run_receipt_status(ensured),
                    detail=ensured.error.message)
            run = manager.get(run_id)
        if run is None:
            raise HTTPException(status_code=500, detail="run handle unavailable")

        last_id_hdr = request.headers.get("Last-Event-ID")
        header_id = int(last_id_hdr) if last_id_hdr and last_id_hdr.isdigit() else 0
        requested_after = header_id if header_id > 0 else max(0, after)
        reset_replay = requested_after > run.bus.current_seq
        last_id = 0 if reset_replay else requested_after
        # The in-memory ring serves recent resume cursors without scanning JSONL.
        # Older cursors fall back to durable replay. SessionStore.replay_monotonic()
        # rewrites broken historical seq resets (e.g. 1808 → 1 after a backend
        # restart) into a single SSE cursor, so Last-Event-ID never skips low ids.
        fresh = last_id == 0
        batched_replay = replay == "batch"

        async def gen():
            replayed_seq = 0
            replayed_count = 0
            replay_batch: list[Event] = []
            last_lifecycle = ""
            if batched_replay and reset_replay:
                yield {
                    "event": _REPLAY_RESET_EVENT,
                    "data": json.dumps(
                        {"watermark_seq": run.bus.current_seq},
                        separators=(",", ":"),
                    ),
                }
            replay_bus = run.bus
            replay_window = (
                await replay_bus.replay_window(last_id) if last_id > 0 else None
            )
            if replay_window is not None:
                buffered_events, replayed_seq = replay_window

                async def buffered_source():
                    for event in buffered_events:
                        yield event

                source = buffered_source()
            else:
                source = run.store.replay_monotonic(
                    run_id, after_seq=last_id)
            async for ev in source:
                replayed_seq = max(replayed_seq, ev.seq)
                replayed_count += 1
                if ev.event_type in (EventType.RUN_PREPARING,
                                     EventType.RUN_STARTED,
                                     EventType.RUN_FINISHED,
                                     EventType.RUN_REOPENED):
                    last_lifecycle = ev.event_type.value
                if batched_replay:
                    replay_batch.append(ev)
                    if len(replay_batch) >= _REPLAY_BATCH_SIZE:
                        yield _replay_batch_frame(replay_batch)
                        replay_batch = []
                        if await request.is_disconnected():
                            return
                else:
                    yield {
                        "id": str(ev.seq),
                        "event": ev.event_type.value,
                        "data": ev.model_dump_json(),
                    }
                    if await request.is_disconnected():
                        return
                # A large historical run can replay thousands of JSONL events.
                # Yield to uvicorn periodically so sidebar polls and live-run
                # control requests do not look "backend frozen" during replay.
                if replayed_count % 100 == 0:
                    await asyncio.sleep(0)
            # Ghost-running guard: only needed for a fresh full replay. On reconnect
            # with no durable events after Last-Event-ID, we do not know the last
            # lifecycle from the skipped prefix and should simply wait on the bus.
            task = getattr(run, "task", None)
            live = task is not None and not task.done()
            if (fresh and not live
                    and last_lifecycle in (
                        "run.preparing", "run.started", "run.reopened")):
                replayed_seq = max(replayed_seq, run.store.last_stream_seq(run_id)) + 1
                synth = Event(
                    event_type=EventType.RUN_FINISHED, run_id=run_id,
                    seq=replayed_seq,
                    payload={"flag": run.flag, "flags": list(run.flags),
                             "expected_flags": run.expected_flags,
                             "multi_flag": run.multi_flag,
                             "solved": run.solved})
                if batched_replay:
                    replay_batch.append(synth)
                else:
                    yield {
                        "id": str(replayed_seq),
                        "event": synth.event_type.value,
                        "data": synth.model_dump_json(),
                    }
            if batched_replay:
                if replay_batch:
                    yield _replay_batch_frame(replay_batch)
                    replay_batch = []
                yield {
                    "event": _REPLAY_COMPLETE_EVENT,
                    "data": json.dumps(
                        {"last_seq": max(last_id, replayed_seq)},
                        separators=(",", ":"),
                    ),
                }
            # live tail: everything after what we just replayed (or after the
            # client's Last-Event-ID on a reconnect). A finished run's bus is
            # closed, so subscribe() returns after backlog replay. Do NOT let the
            # HTTP response EOF: browser EventSource treats EOF as an error and
            # reconnects forever, replaying finished histories in a loop. Instead,
            # keep the SSE open (ping handles liveness) and hop to a fresh bus if
            # resolve/standby reopens the run.
            manager._bump_bus_seq(run.bus, max(last_id, replayed_seq))
            # Do not advance the cursor from a second store lookup here.  An event
            # can commit after replay reached EOF but before this line; adopting its
            # sequence without yielding it would skip that event permanently.  Live
            # in-process writes are present in the EventBus ring and subscribe()
            # delivers everything after the last sequence actually replayed.
            tail_from = max(last_id, replayed_seq)
            while True:
                bus = run.bus
                async for ev in bus.subscribe(last_event_id=tail_from):
                    tail_from = ev.seq
                    yield {
                        "id": str(ev.seq),
                        "event": ev.event_type.value,
                        "data": ev.model_dump_json(),
                    }
                    if await request.is_disconnected():
                        return
                while run.bus is bus:
                    if await request.is_disconnected():
                        return
                    # STOP/COMPLETE confirms runtime exit only after the generation
                    # task has emitted RUN_FINISHED and closed its bus.  The final
                    # control receipt is therefore a valid late publication on that
                    # closed bus.  Re-enter subscribe() when its sequence advances so
                    # the existing SSE connection receives the durable receipt.
                    if bus.current_seq > tail_from:
                        break
                    await asyncio.sleep(1)

        return EventSourceResponse(
            gen(),
            ping=10,
            ping_message_factory=lambda: ServerSentEvent(comment="muteki-ping"),
        )

    @app.websocket("/api/runs/{run_id}/terminal")
    async def terminal(ws: WebSocket, run_id: str) -> None:
        # Auth check BEFORE accept(): a WebSocket can't carry an Authorization
        # header from the browser, so it presents a one-time ticket (?ticket=)
        # or a bearer token (?token=, non-browser). Reject the handshake outright
        # (close 4401) on failure so we never expose an authenticated socket.
        cfg: AuthConfig = app.state.auth
        if cfg.enabled:
            authed = app.state.tickets.redeem(ws.query_params.get("ticket")) or \
                verify_token(cfg, ws.query_params.get("token"))
            if not authed:
                await ws.close(code=4401)
                return
        await ws.accept()
        manager: RunManager = app.state.manager
        run = manager.get(run_id)
        if run is None:
            await ws.close(code=4004)
            return
        try:
            # replay from 0 so a terminal opened mid/just-after a run still shows
            # the buffered output, then streams live
            async for ev in run.bus.subscribe(last_event_id=0):
                if ev.event_type is EventType.TERMINAL_OUTPUT:
                    await ws.send_text(ev.payload.get("text", ""))
        except WebSocketDisconnect:
            return
        except asyncio.CancelledError:
            return
