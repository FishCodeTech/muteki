"""Web backend e2e (Sprint 1.1 acceptance): mock solver -> SSE -> assert the
frontend receives the full typed event stream; HITL POST lands in the run.

Runs a REAL uvicorn server on an ephemeral port in a background thread (the SSE
stream needs a real ASGI server — httpx.ASGITransport does not stream
incrementally), then drives it with httpx.
"""

import asyncio
import json
import socket
import threading
import time
import uuid
from pathlib import Path

import httpx
import pytest
import uvicorn

from apps.web.run_manager import RunManager
from apps.web.server import create_app
from muteki.core.events import EventType
from muteki.models.solve_graph import Challenge
from muteki.swarm.shared_graph import SQLiteSharedGraph


def _free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class _Server:
    def __init__(self, app) -> None:
        self.port = _free_port()
        cfg = uvicorn.Config(app, host="127.0.0.1", port=self.port, log_level="warning")
        self.server = uvicorn.Server(cfg)
        self.thread = threading.Thread(target=self.server.run, daemon=True)

    def __enter__(self) -> "_Server":
        self.thread.start()
        # wait for startup
        for _ in range(100):
            if self.server.started:
                break
            time.sleep(0.05)
        return self

    def __exit__(self, *exc) -> None:
        self.server.should_exit = True
        self.thread.join(timeout=5)

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.port}"


@pytest.fixture
def server():
    app = create_app(RunManager(sessions_root="/tmp/muteki_web_sessions"))
    with _Server(app) as s:
        yield s


async def _collect_sse(client: httpx.AsyncClient, run_id: str, seen: set,
                       stop_on: str) -> None:
    async with client.stream("GET", f"/api/runs/{run_id}/events") as resp:
        assert resp.status_code == 200
        cur = None
        async for line in resp.aiter_lines():
            if line.startswith("event:"):
                cur = line.split(":", 1)[1].strip()
                seen.add(cur)
                if cur == stop_on:
                    return


async def _collect_sse_payload(client: httpx.AsyncClient, run_id: str,
                               stop_on: str) -> dict:
    async with client.stream("GET", f"/api/runs/{run_id}/events") as resp:
        assert resp.status_code == 200
        cur = None
        async for line in resp.aiter_lines():
            if line.startswith("event:"):
                cur = line.split(":", 1)[1].strip()
            elif cur == stop_on and line.startswith("data:"):
                return json.loads(line.split(":", 1)[1].strip())["payload"]
    raise AssertionError(f"did not see {stop_on}")


async def test_mock_run_streams_full_event_set_over_sse(server) -> None:
    async with httpx.AsyncClient(base_url=server.base, timeout=30, trust_env=False) as client:
        r = await client.post("/api/runs/web-mock-1/start", json={"kind": "mock"})
        assert r.status_code == 200 and r.json()["started"] is True
        seen: set = set()
        await asyncio.wait_for(
            _collect_sse(client, "web-mock-1", seen, EventType.RUN_FINISHED.value),
            timeout=25,
        )
    assert EventType.RUN_STARTED.value in seen
    assert EventType.REASONING_DELTA.value in seen
    assert EventType.TOOL_CALL_RESULT.value in seen
    assert EventType.SOLVE_GRAPH_DELTA.value in seen
    assert EventType.COST_UPDATE.value in seen
    assert EventType.RUN_FINISHED.value in seen


async def test_mock_run_finished_carries_terminal_reason(server) -> None:
    async with httpx.AsyncClient(base_url=server.base, timeout=30, trust_env=False) as client:
        r = await client.post(
            "/api/runs/web-mock-reason/start",
            json={"kind": "mock", "expected_flags": 2},
        )
        assert r.status_code == 200 and r.json()["started"] is True
        payload = await asyncio.wait_for(
            _collect_sse_payload(
                client, "web-mock-reason", EventType.RUN_FINISHED.value),
            timeout=25,
        )
    assert payload["reason"] == "goal_met"
    assert payload["flags"] == ["flag{mock_encoding_solved}", "flag{mock_part_2}"]


async def test_credentials_endpoint_reads_shared_graph(tmp_path) -> None:
    mgr = RunManager(sessions_root=tmp_path)
    run = mgr.create("cred-run")
    graph_dir = mgr.workspace_dir(run.run_id) / "graph"
    graph_dir.mkdir(parents=True, exist_ok=True)
    graph = SQLiteSharedGraph.open(
        db_path=graph_dir / "shared_graph.db",
        challenge=Challenge(id="cred-run", name="cred", category="web"),
    )
    graph.add_evidence(
        actor="cli-a",
        source="curl",
        fact="admin password hunter2 successfully logs in as admin",
        verified=True,
    )
    graph.close()
    app = create_app(mgr)

    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app),
        base_url="http://test",
        timeout=10,
        trust_env=False,
    ) as client:
        resp = await client.get("/api/runs/cred-run/credentials")

    assert resp.status_code == 200
    creds = resp.json()["credentials"]
    assert len(creds) == 1
    assert creds[0]["entity"] == "admin"
    assert creds[0]["value"] == "hunter2"


async def test_hitl_post_is_accepted_and_echoed(server) -> None:
    async with httpx.AsyncClient(base_url=server.base, timeout=30, trust_env=False) as client:
        # idle run keeps the bus open so the HITL echo is observable (a fast mock
        # run could close the stream before we post)
        await client.post("/api/runs/hitl-run/start", json={"kind": "idle"})
        seen: set = set()
        # the HITL_RESPONSE should show up on the stream after we POST it
        watcher = asyncio.create_task(
            _collect_sse(client, "hitl-run", seen, EventType.HITL_RESPONSE.value)
        )
        await asyncio.sleep(0.2)
        r = await client.post(
            "/api/runs/hitl-run/hitl",
            json={"target": "solver:mock-flash", "action": "hint", "text": "try base64"},
        )
        assert r.status_code == 200 and r.json()["ok"] is True
        await asyncio.wait_for(watcher, timeout=15)
    assert EventType.HITL_RESPONSE.value in seen


async def test_non_dict_body_is_rejected_with_400(server) -> None:
    """Finding #7: a non-object JSON body (e.g. json=[]) must be a clean 400 on every
    write route — NOT an opaque 500 (/hitl, /start, folders, workers) and NOT a silent
    200 that swallows the request (PATCH /api/runs). Critically, /hitl 500'ing meant an
    operator literally could not STOP a run with a malformed client."""
    async with httpx.AsyncClient(base_url=server.base, timeout=30, trust_env=False) as client:
        await client.post("/api/runs/badbody-run/start", json={"kind": "idle"})
        for method, path in [
            ("POST", "/api/runs/badbody-run/hitl"),
            ("PATCH", "/api/runs/badbody-run"),
            ("POST", "/api/runs/badbody-run/start"),
            ("POST", "/api/folders"),
            ("PUT", "/api/settings/workers"),
            ("POST", "/api/settings/worker-model/test"),
            ("POST", "/api/runs/badbody-run/workers"),
            ("DELETE", "/api/runs/badbody-run/workers"),
        ]:
            r = await client.request(method, path, json=[])
            assert r.status_code == 400, f"{method} {path} should 400 on a list body, got {r.status_code}"


async def test_worker_model_options_and_probe_routes(server, monkeypatch) -> None:
    from apps.web import worker_models

    seen = {}

    def fake_probe_worker_model(**kwargs):
        seen.update(kwargs)
        return {"ok": True, "detail": "模型可用", "engine": "claude", "model": kwargs["model"]}

    monkeypatch.setattr(worker_models, "probe_worker_model", fake_probe_worker_model)

    async with httpx.AsyncClient(base_url=server.base, timeout=30, trust_env=False) as client:
        opts = await client.get("/api/settings/worker-models")
        assert opts.status_code == 200
        assert opts.json()["allow_custom"] is True
        assert "claude" in opts.json()["models"]

        r = await client.post("/api/settings/worker-model/test", json={
            "profile": {"id": "claude-sub", "engine": "claude", "credential_account": "claude-main"},
            "model": "opus",
            "backend": "local",
        })
        assert r.status_code == 200
        assert r.json()["ok"] is True
        assert seen["model"] == "opus"
        assert seen["profile"]["id"] == "claude-sub"


async def test_engine_health_route_passes_enabled_worker_profiles(tmp_path, monkeypatch) -> None:
    import muteki.solver.cli_driver as cli_driver

    seen = {}

    def fake_engine_health(backend="local", account_root=None, profiles=None):
        seen["backend"] = backend
        seen["profiles"] = profiles
        return [{"engine": "claude", "healthy": True, "backend": backend}]

    monkeypatch.setattr(cli_driver, "engine_health", fake_engine_health)
    mgr = RunManager(sessions_root=str(tmp_path / "sessions"))
    mgr.worker_config.set(
        engines=["claude-main"],
        worker_backend="local",
        worker_profiles=[
            {"id": "claude-main", "name": "claude-main", "engine": "claude",
             "transport": "claude_code", "credential_account": "",
             "runtime": "local", "model": "sonnet"},
            {"id": "codex-main", "name": "codex-main", "engine": "codex",
             "transport": "codex_cli", "credential_account": "",
             "runtime": "local", "model": "gpt-5.5"},
        ],
    )
    app = create_app(mgr)
    with _Server(app) as srv:
        async with httpx.AsyncClient(base_url=srv.base, timeout=10, trust_env=False) as client:
            r = await client.get("/api/engines/health?backend=local")
    assert r.status_code == 200
    assert seen["backend"] == "local"
    assert [p["id"] for p in seen["profiles"]] == ["claude-main"]
    assert seen["profiles"][0]["model"] == "sonnet"


async def test_worker_routes_accept_empty_body(server) -> None:
    """Finding #7: the worker spawn/kill routes legitimately accept NO body ('let the
    coordinator pick the engine'), so an empty/absent body must still be a 200 — only a
    present-but-non-object body is rejected."""
    async with httpx.AsyncClient(base_url=server.base, timeout=30, trust_env=False) as client:
        await client.post("/api/runs/empty-body-run/start", json={"kind": "idle"})
        # no json= → empty body
        r = await client.post("/api/runs/empty-body-run/workers")
        assert r.status_code == 200
        r = await client.post("/api/runs/empty-body-run/workers", json={})
        assert r.status_code == 200


async def test_engines_endpoint_singleflights_slow_probe(tmp_path, monkeypatch) -> None:
    """A slow engine-status refresh must not stack duplicate CLI hello probes when
    the deck or multiple tabs hit /api/engines concurrently."""
    import muteki.solver.cli_driver as cli_driver

    calls = 0
    calls_lock = threading.Lock()

    def fake_engine_status(account_root=None, backend="local", profiles=None):
        nonlocal calls
        with calls_lock:
            calls += 1
        assert profiles is not None
        time.sleep(0.2)
        return [{"engine": "codex", "available": True, "healthy": True}]

    monkeypatch.setattr(cli_driver, "engine_status", fake_engine_status)
    app = create_app(RunManager(sessions_root=str(tmp_path / "sessions")))
    with _Server(app) as srv:
        async with httpx.AsyncClient(base_url=srv.base, timeout=10, trust_env=False) as client:
            responses = await asyncio.gather(
                client.get("/api/engines"),
                client.get("/api/engines"),
                client.get("/api/engines"),
            )
    assert [r.status_code for r in responses] == [200, 200, 200]
    assert calls == 1


async def test_engines_endpoint_passes_enabled_worker_profiles(tmp_path, monkeypatch) -> None:
    """The top engine bar must probe the configured worker profile/model, not a
    bare engine default. Otherwise Claude can be shown as down when the default
    model is exhausted but the selected Sonnet profile is usable."""
    import muteki.solver.cli_driver as cli_driver

    seen = {}

    def fake_engine_status(account_root=None, backend="local", profiles=None):
        seen["backend"] = backend
        seen["profiles"] = profiles
        return [{"engine": "claude", "available": True, "healthy": True}]

    monkeypatch.setattr(cli_driver, "engine_status", fake_engine_status)
    mgr = RunManager(sessions_root=str(tmp_path / "sessions"))
    mgr.worker_config.set(
        engines=["claude-main"],
        worker_backend="local",
        worker_profiles=[
            {"id": "claude-main", "name": "claude-main", "engine": "claude",
             "transport": "claude_code", "credential_account": "",
             "runtime": "local", "model": "sonnet"},
            {"id": "codex-main", "name": "codex-main", "engine": "codex",
             "transport": "codex_cli", "credential_account": "",
             "runtime": "local", "model": "gpt-5.5"},
        ],
    )
    app = create_app(mgr)
    with _Server(app) as srv:
        async with httpx.AsyncClient(base_url=srv.base, timeout=10, trust_env=False) as client:
            r = await client.get("/api/engines")

    assert r.status_code == 200
    assert seen["backend"] == "local"
    assert [p["id"] for p in seen["profiles"]] == ["claude-main"]
    assert seen["profiles"][0]["model"] == "sonnet"


async def test_credential_account_api_echoes_secret_and_persists(tmp_path) -> None:
    """The credential endpoints deliberately ECHO the stored secret back so the
    settings UI can show & edit it in place (operator request — see
    credential_accounts._read_secret_value's SECURITY POSTURE note). The route is
    password-authenticated; the plaintext travels only over that gated channel.
    Here we assert the secret round-trips through both PUT and the list GET."""
    app = create_app(RunManager(sessions_root=str(tmp_path / "sessions")))
    with _Server(app) as srv:
        async with httpx.AsyncClient(base_url=srv.base, timeout=15, trust_env=False) as client:
            r = await client.put(
                "/api/settings/credential-accounts/claude-team",
                json={"engine": "claude", "secret": "super-secret-token"},
            )
            assert r.status_code == 200
            assert r.json()["account"]["engine"] == "claude"
            # echoed for in-place editing, not masked
            assert r.json()["account"]["details"]["secret_value"] == "super-secret-token"

            listed = await client.get("/api/settings/credential-accounts")
            assert listed.status_code == 200
            accounts = listed.json()["accounts"]
            assert accounts[0]["account_id"] == "claude-team"
            assert accounts[0]["present"] is True
            assert accounts[0]["details"]["secret_value"] == "super-secret-token"

            api = await client.put(
                "/api/settings/credential-accounts/deepseek-main",
                json={
                    "engine": "api",
                    "secret": "deepseek-secret",
                    "base_url": "https://api.deepseek.example/v1",
                },
            )
            assert api.status_code == 200
            # both the api key and base_url are echoed for editing
            assert api.json()["account"]["details"]["secret_value"] == "deepseek-secret"
            assert api.json()["account"]["details"]["base_url"] is True
            assert (
                api.json()["account"]["details"]["base_url_value"]
                == "https://api.deepseek.example/v1"
            )

            bad = await client.put(
                "/api/settings/credential-accounts/bad/cut",
                json={"engine": "claude", "secret": "x"},
            )
            assert bad.status_code == 404


async def test_settings_redesign_endpoints(tmp_path) -> None:
    """The four new settings endpoints (DESIGN §2.3/§2.4/§5) exist and behave:
    system-login (read-only), runtime-environment write-back, account test
    (no-creds → ok:false), llm test (bogus → ok:false, no network success)."""
    app = create_app(RunManager(sessions_root=str(tmp_path / "sessions")))
    with _Server(app) as srv:
        async with httpx.AsyncClient(base_url=srv.base, timeout=20, trust_env=False) as client:
            # system-login: returns a status per engine, never errors
            sl = await client.get("/api/settings/system-login")
            assert sl.status_code == 200
            logins = sl.json()["logins"]
            assert set(logins) == {"claude", "codex", "cursor"}
            assert all(v in ("present", "absent", "unknown") for v in logins.values())

            # runtime-environment: flip to local, all profiles follow
            rt = await client.put("/api/settings/runtime-environment",
                                  json={"backend": "local", "runtime_id": "local"})
            assert rt.status_code == 200
            cfg = rt.json()["config"]
            assert cfg["worker_backend"] == "local"
            assert all(p["runtime"] == "local" for p in cfg["worker_profiles"])
            # mismatch rejected
            bad = await client.put("/api/settings/runtime-environment",
                                   json={"backend": "local", "runtime_id": "docker-web"})
            assert bad.status_code == 400

            # account test: unregistered account → ok:false, no host fallback
            at = await client.post(
                "/api/settings/credential-accounts/ghost/test",
                json={"engine": "claude", "backend": "local"})
            assert at.status_code == 200
            assert at.json()["ok"] is False

            # llm test: empty model → ok:false (no network needed)
            lt = await client.post("/api/settings/llm/test",
                                   json={"which": "planner", "model": ""})
            assert lt.status_code == 200
            assert lt.json()["ok"] is False


async def test_events_opens_for_not_yet_started_run(server) -> None:
    # A deck opens its event stream BEFORE launching a run. That must NOT 404 —
    # the endpoint creates the run handle on demand and holds the SSE open so it
    # streams the moment the run starts (no reconnect race). We confirm the
    # stream opens with 200 (then close it without waiting for events).
    async with httpx.AsyncClient(base_url=server.base, timeout=10, trust_env=False) as client:
        async with client.stream("GET", "/api/runs/not-started-yet/events") as resp:
            assert resp.status_code == 200
            assert "text/event-stream" in resp.headers.get("content-type", "")
        # The run handle now exists in memory, but /api/runs is STARTED-ONLY: a
        # merely-subscribed (never-dispatched) run is a draft stub and must NOT
        # clutter the thread rail. It appears only once it is /start-ed.
        r = await client.get("/api/runs")
        run_ids = [row["run_id"] for row in r.json()["runs"]]
        assert "not-started-yet" not in run_ids


async def test_ws_terminal_streams_sandbox_output(server) -> None:
    import websockets

    async with httpx.AsyncClient(base_url=server.base, timeout=30, trust_env=False) as client:
        await client.post("/api/runs/ws-run/start", json={"kind": "mock"})
    ws_url = server.base.replace("http://", "ws://") + "/api/runs/ws-run/terminal"
    got: list[str] = []
    async with websockets.connect(ws_url) as ws:
        # the mock emits two TERMINAL_OUTPUT lines; replay-from-0 delivers them
        try:
            for _ in range(2):
                got.append(await asyncio.wait_for(ws.recv(), timeout=10))
        except asyncio.TimeoutError:
            pass
    joined = "".join(got)
    assert "GET /secret" in joined or "auto_decode" in joined


async def test_fresh_subscriber_replays_full_history_past_ring_overflow(tmp_path) -> None:
    """A deck opening a LONG run (more events than the in-memory ring holds) must
    still receive run.started — the first event — by replaying the durable
    SessionStore history, not just the truncated ring. Without this, a deck that
    connects mid/post-run never leaves the empty state (the real bug seen while
    backtesting a long web challenge)."""
    from muteki.core.event_bus import EventBus
    from muteki.core.events import Event

    # a run whose bus has a TINY ring so we overflow it cheaply
    manager = RunManager(sessions_root=str(tmp_path / "sessions"))
    run = manager.create("long-run")
    # swap in a small-ring bus that still persists to the same store sink
    small = EventBus(ring_size=8)
    small.add_sink(run.store.sink)
    run.bus = small

    # emit run.started (seq 1) then enough events to evict it from the ring
    await small.emit(Event(event_type=EventType.RUN_STARTED, run_id="long-run",
                           payload={"challenge": {"name": "long", "category": "web"}}))
    for i in range(40):  # >> ring_size(8) -> run.started is long gone from the ring
        await small.emit(Event(event_type=EventType.REASONING_DELTA, run_id="long-run",
                               payload={"text": f"step {i} "}))

    app = create_app(manager)
    with _Server(app) as srv:
        async with httpx.AsyncClient(base_url=srv.base, timeout=15, trust_env=False) as client:
            seen: list[str] = []
            # fresh subscribe (no Last-Event-ID): must replay from the store
            async with client.stream("GET", "/api/runs/long-run/events") as resp:
                assert resp.status_code == 200
                async for line in resp.aiter_lines():
                    if line.startswith("event:"):
                        seen.append(line.split(":", 1)[1].strip())
                    # we have the full history once we've seen the first event +
                    # several deltas; stop so the test doesn't hang on the live tail
                    if len(seen) >= 41:
                        break
    # the very first event survived ring overflow via the durable replay
    assert seen[0] == EventType.RUN_STARTED.value
    assert seen.count(EventType.REASONING_DELTA.value) == 40


@pytest.mark.asyncio
async def test_sse_reconnect_replays_monotonic_history_after_seq_reset(tmp_path) -> None:
    """A browser reconnecting with Last-Event-ID must not miss a post-restart segment.

    Regression for runs whose JSONL contained raw seq reset after continue/reopen
    (1,2,1,2). The SSE layer normalizes that to 1,2,3,4 and replays events after
    the client's cursor.
    """
    sessions = tmp_path / "sessions"
    sessions.mkdir()
    run_id = "run-reset"
    events = [
        {"event_type": "run.started", "seq": 1, "ts": 1.0, "run_id": run_id,
         "payload": {"challenge": {"name": "reset", "category": "web"}}},
        {"event_type": "reasoning.delta", "seq": 2, "ts": 2.0, "run_id": run_id,
         "payload": {"text": "before"}},
        {"event_type": "run.reopened", "seq": 1, "ts": 3.0, "run_id": run_id,
         "payload": {"reason": "resolve"}},
        {"event_type": "reasoning.delta", "seq": 2, "ts": 4.0, "run_id": run_id,
         "payload": {"text": "after"}},
    ]
    with (sessions / f"{run_id}.jsonl").open("w", encoding="utf-8") as f:
        for ev in events:
            f.write(json.dumps(ev) + "\n")

    app = create_app(RunManager(sessions_root=str(sessions)))
    with _Server(app) as srv:
        async with httpx.AsyncClient(base_url=srv.base, timeout=15, trust_env=False) as client:
            ids: list[str] = []
            seen: list[str] = []
            async with client.stream(
                "GET", f"/api/runs/{run_id}/events",
                headers={"Last-Event-ID": "2"},
            ) as resp:
                assert resp.status_code == 200
                async for line in resp.aiter_lines():
                    if line.startswith("id:"):
                        ids.append(line.split(":", 1)[1].strip())
                    elif line.startswith("event:"):
                        seen.append(line.split(":", 1)[1].strip())
                    if len(seen) >= 2:
                        break

    assert ids[:2] == ["3", "4"]
    assert seen[:2] == [EventType.RUN_REOPENED.value, EventType.REASONING_DELTA.value]


@pytest.mark.asyncio
async def test_upload_lands_in_run_dir_and_returns_abs_path(tmp_path) -> None:
    """A file POSTed to /uploads lands in sessions/{id}/uploads/ and the endpoint
    returns its ABSOLUTE path — exactly what challenge.attachments needs so the
    worker can stage it into its cwd."""
    sessions = tmp_path / "sessions"
    app = create_app(RunManager(sessions_root=str(sessions)))
    with _Server(app) as srv:
        async with httpx.AsyncClient(base_url=srv.base, timeout=15, trust_env=False) as client:
            files = {"files": ("cipher.txt", b"deadbeef", "text/plain")}
            r = await client.post("/api/runs/run-0001/uploads", files=files)
            assert r.status_code == 200
            saved = r.json()["files"]
            assert len(saved) == 1
            assert saved[0]["name"] == "cipher.txt"
            assert saved[0]["size"] == 8
            p = Path(saved[0]["path"])
            assert p.is_absolute() and p.exists()
            assert p.read_bytes() == b"deadbeef"
            # lands under sessions/<run_id>/uploads/, beside (not colliding with)
            # the run's {id}.jsonl log
            assert p.parent == (sessions / "run-0001" / "uploads")


@pytest.mark.asyncio
async def test_upload_sanitizes_path_traversal_filename(tmp_path) -> None:
    """A hostile filename (path traversal / absolute) is reduced to its basename
    and can never escape the run's uploads dir."""
    sessions = tmp_path / "sessions"
    app = create_app(RunManager(sessions_root=str(sessions)))
    uploads = sessions / "run-0002" / "uploads"
    with _Server(app) as srv:
        async with httpx.AsyncClient(base_url=srv.base, timeout=15, trust_env=False) as client:
            files = {"files": ("../../etc/evil", b"x", "application/octet-stream")}
            r = await client.post("/api/runs/run-0002/uploads", files=files)
            assert r.status_code == 200
            p = Path(r.json()["files"][0]["path"])
            assert p.parent == uploads          # stayed inside the run's folder
            assert p.name == "evil"             # only the basename survived
            assert not (sessions.parent / "etc" / "evil").exists()


# ── ghost-running guard: a run whose durable history ENDS on run.started with no
# live task must get a synthetic RUN_FINISHED on stream open, so the deck settles
# to "finished" (not stuck on running → only Stop shown). This is the run-4305 fix.
@pytest.fixture
def server_mgr():
    mgr = RunManager(sessions_root="/tmp/muteki_web_ghost_sessions")
    app = create_app(mgr)
    with _Server(app) as s:
        yield s, mgr


async def test_events_injects_run_finished_for_ghost_run(server_mgr) -> None:
    from muteki.core.events import Event
    s, mgr = server_mgr
    rid = "ghost-run-1"
    run = mgr.create(rid)
    # seed durable history that ENDS on run.started (the ghost shape) — no finish.
    await run.bus.emit(Event(event_type=EventType.RUN_STARTED, run_id=rid,
                             payload={"challenge": {"name": "x"}}))
    await run.bus.emit(Event(event_type=EventType.REASONING_DELTA, run_id=rid,
                             payload={"text": "working...\n"}))
    run.started = True
    run.finished = False
    run.task = None  # dead task → ghost
    # opening a FRESH event stream must replay history THEN inject RUN_FINISHED.
    async with httpx.AsyncClient(base_url=s.base, timeout=30, trust_env=False) as client:
        seen: set = set()
        await asyncio.wait_for(
            _collect_sse(client, rid, seen, EventType.RUN_FINISHED.value), timeout=15)
    assert EventType.RUN_STARTED.value in seen
    assert EventType.RUN_FINISHED.value in seen  # the synthetic terminator


async def test_finished_event_stream_stays_open_after_replay(server_mgr) -> None:
    from muteki.core.events import Event
    s, mgr = server_mgr
    rid = f"finished-run-{uuid.uuid4().hex}"
    run = mgr.create(rid)
    await run.bus.emit(Event(event_type=EventType.RUN_STARTED, run_id=rid,
                             payload={"challenge": {"name": "x"}}))
    await run.bus.emit(Event(event_type=EventType.RUN_FINISHED, run_id=rid,
                             payload={"solved": False}))
    run.started = True
    run.finished = True
    await run.bus.close()

    async with httpx.AsyncClient(base_url=s.base, timeout=30, trust_env=False) as client:
        async with client.stream("GET", f"/api/runs/{rid}/events") as resp:
            assert resp.status_code == 200
            lines = resp.aiter_lines()
            seen_finished = False
            in_finished = False
            async for line in lines:
                if line.startswith("event:") and EventType.RUN_FINISHED.value in line:
                    in_finished = True
                elif in_finished and line == "":
                    seen_finished = True
                    break
            assert seen_finished

            async def next_nonempty_line_or_closed() -> str:
                try:
                    while True:
                        line = await lines.__anext__()
                        if line:
                            return line
                except StopAsyncIteration:
                    return "__closed__"

            # A closed response makes EventSource reconnect forever. The stream
            # should instead stay idle/pending after replay (ping frames arrive
            # later, outside this short window).
            with pytest.raises(asyncio.TimeoutError):
                await asyncio.wait_for(next_nonempty_line_or_closed(), timeout=0.25)


def test_rehydrate_force_settles_started_unfinished_run(tmp_path) -> None:
    # ghost run: a run whose on-disk summary says started=True but finished=False
    # (killed mid-run before RUN_FINISHED). On restart, _rehydrate has no live task,
    # so it MUST settle it to finished — else the rail spins forever.
    from muteki.core.events import Event
    sessions = tmp_path / "sessions"
    mgr1 = RunManager(sessions_root=str(sessions))
    run = mgr1.create("ghost-2")

    async def seed() -> None:
        await run.bus.emit(Event(event_type=EventType.RUN_STARTED, run_id="ghost-2",
                                 payload={"challenge": {"name": "x"}}))
        run.started = True
        run.finished = False
        await run.bus.close()
    asyncio.run(seed())

    # a fresh manager rehydrates from disk → the started-but-unfinished run settles.
    mgr2 = RunManager(sessions_root=str(sessions))
    r2 = mgr2.runs.get("ghost-2")
    assert r2 is not None
    assert r2.started is True
    assert r2.finished is True   # force-settled (was a ghost otherwise)


def test_start_finally_emits_run_finished_on_cancel(tmp_path) -> None:
    # if a driver is CANCELLED mid-run (server restart, manual cancel) before it
    # emits its own RUN_FINISHED, the _go finally must synthesize one so the deck
    # gets a terminal event (no infinite spinner).
    mgr = RunManager(sessions_root=str(tmp_path / "sessions"))
    seen: list = []

    async def go() -> None:
        async def driver(run) -> None:
            run.bus.add_sink(lambda ev: seen.append(ev.event_type))
            await asyncio.sleep(10)   # long-running; we cancel it before it finishes

        run = await mgr.start("cancel-run", driver)
        await asyncio.sleep(0.05)
        run.task.cancel()
        try:
            await run.task
        except asyncio.CancelledError:
            pass
        assert run.finished is True
        assert EventType.RUN_FINISHED in seen  # synthesized in the finally

    asyncio.run(go())


def test_start_finally_includes_runtime_failure_detail(tmp_path) -> None:
    mgr = RunManager(sessions_root=str(tmp_path / "sessions"))
    seen: list[dict] = []

    async def go() -> None:
        async def sink(ev) -> None:
            if ev.event_type is EventType.RUN_FINISHED:
                seen.append(ev.payload)

        async def driver(run) -> None:
            run.bus.add_sink(sink)
            raise RuntimeError("profile_unhealthy missing credential account(s): cursor-api-local:cursor-main")

        run = await mgr.start("failed-run", driver)
        await run.task
        assert run.finished is True

    asyncio.run(go())
    assert seen
    assert seen[-1]["reason"] == "runtime_failure"
    assert "cursor-api-local:cursor-main" in seen[-1]["detail"]
