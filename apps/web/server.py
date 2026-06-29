"""FastAPI backend for the web command deck (§14.1 / Sprint 1.1).

Endpoints:
  GET  /api/runs                      list known runs
  POST /api/runs/{run_id}/start       launch a run (mock driver, or swarm if a
                                       challenge spec is posted) — see drivers.py
  GET  /api/runs/{run_id}/events      SSE: the typed event stream (Last-Event-ID
                                       resume via the standard header)
  WS   /api/runs/{run_id}/terminal    sandbox terminal: TERMINAL_OUTPUT bytes
  POST /api/runs/{run_id}/hitl        human command into the run (hint/pause/etc.)
  GET  /                              the single-page UI (static)

The server holds NO solving logic — it only brokers the event bus + HITL. Event
schema is the only contract (§3).
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import time
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Any, Optional

from fastapi import (
    FastAPI,
    File,
    HTTPException,
    Request,
    UploadFile,
    WebSocket,
    WebSocketDisconnect,
)
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse, JSONResponse
from fastapi.staticfiles import StaticFiles
from sse_starlette.sse import EventSourceResponse, ServerSentEvent

from apps.web.auth import (
    PUBLIC_API_PATHS,
    AuthConfig,
    TicketStore,
    bearer_from_header,
    check_password,
    issue_token,
    verify_token,
)
from apps.web.run_manager import Run, RunManager
from muteki.core.dotenv_boot import load_env
from muteki.core.events import Event, EventType
from muteki.solver.credential_accounts import (
    CredentialAccountStore,
    account_store_root,
)

load_env()  # local convenience: pick up repo-root .env (shell env still wins)

UI_DIR = Path(__file__).parent / "ui"

def _env_float(name: str, default: float) -> float:
    try:
        v = os.environ.get(name)
        return float(v) if v not in (None, "") else default
    except (TypeError, ValueError):
        return default


def _env_int(name: str, default: int) -> int:
    try:
        v = os.environ.get(name)
        return int(v) if v not in (None, "") else default
    except (TypeError, ValueError):
        return default


# Upload guards: a CTF handout is small (a cipher blob, a binary, a pcap). Cap
# per-file size and per-request count so a stray drag-drop can't fill the disk.
# Both are configurable for larger handouts (disk images, big pcaps):
#   MUTEKI_MAX_UPLOAD_MB    (default 25)  — per-file size cap, in MB
#   MUTEKI_MAX_UPLOAD_FILES (default 20)  — max files per request
MAX_UPLOAD_BYTES = max(1, _env_int("MUTEKI_MAX_UPLOAD_MB", 25)) * 1024 * 1024
MAX_UPLOAD_FILES = max(1, _env_int("MUTEKI_MAX_UPLOAD_FILES", 20))


async def _require_dict_body(request: "Request", *, allow_empty: bool = False) -> dict[str, Any]:
    """Parse a JSON request body and require it to be a JSON object.

    Routes used to handle this inconsistently: some did a bare `request.json()`
    (`/hitl` → opaque 500 so the operator couldn't even STOP a run), some caught
    only JSONDecodeError but then did `body.get(...)` on a parsed list (AttributeError
    → 500), and PATCH /api/runs used `if "pinned" in body` which is a valid `in` check
    on a list → silent 200 that swallowed a malformed request. This centralizes it:
    a non-object body (list, string, number, null) is always 400.

    `allow_empty`: some routes legitimately accept NO body (e.g. POST .../workers with
    no engine = "let the coordinator pick"). For those a missing/empty body parses to
    {} instead of 400 — but a present-but-non-object body is still rejected."""
    try:
        body = await request.json()
    except (json.JSONDecodeError, ValueError, UnicodeDecodeError):
        if allow_empty:
            return {}
        raise HTTPException(status_code=400, detail="request body must be a JSON object")
    if not isinstance(body, dict):
        raise HTTPException(status_code=400, detail="request body must be a JSON object")
    return body


def create_app(manager: Optional[RunManager] = None) -> FastAPI:
    mgr = manager or RunManager()

    # Retention policy (BE-auto-archive): auto-archive idle runs, then delete the
    # ones that stay idle. Defaults: archive after 3 days, delete after 7 days,
    # sweep hourly. All env-tunable; set MUTEKI_RETENTION_ENABLED=0 to disable
    # (pinned runs are NEVER auto-touched).
    @asynccontextmanager
    async def lifespan(app: FastAPI):
        # Start the reverse-connect control receiver: the in-container supervisors
        # DIAL this (host.docker.internal:<port>) — so the host must be listening
        # before any container starts. Lazy-starts on first use too, but starting it
        # here makes "control port already in use" surface at boot, not mid-run.
        try:
            from muteki.solver.control_receiver import ControlReceiver
            ControlReceiver.instance()
        except OSError as exc:  # port already bound (another backend?) — log, continue
            print(f"[control-receiver] could not bind control port: {exc}", flush=True)
        task: Optional[asyncio.Task] = None
        enabled = os.environ.get("MUTEKI_RETENTION_ENABLED", "1").lower() not in (
            "0", "false", "no", "off", "")
        if enabled:
            task = asyncio.create_task(mgr.retention_loop(
                interval_s=_env_float("MUTEKI_RETENTION_INTERVAL", 3600.0),
                archive_after_s=_env_float("MUTEKI_ARCHIVE_DAYS", 3.0) * 86400.0,
                delete_after_s=_env_float("MUTEKI_DELETE_DAYS", 7.0) * 86400.0,
            ))
        try:
            yield
        finally:
            if task is not None:
                task.cancel()
                await asyncio.gather(task, return_exceptions=True)
            # Tear down every live swarm/standby task (and its shelled CLI subprocess
            # group) so a server restart doesn't leave budget-eating zombies. This was
            # never wired up before — shutdown() existed but nothing called it.
            await mgr.shutdown()

    app = FastAPI(title="Project Muteki — Command Deck", lifespan=lifespan)
    app.state.manager = mgr

    # Auth (P3): a single-password gate in front of /api. fail_fast_check refuses
    # to start if bound to a non-loopback host with no password — see auth.py and
    # docs/_local/plan_p3_auth.md. When no password is set AND the bind is
    # loopback, auth is disabled and the deck behaves exactly as before.
    auth = AuthConfig.from_env()
    auth.fail_fast_check()
    app.state.auth = auth
    app.state.tickets = TicketStore()

    # Dev convenience: the Next dev server (:3001) can talk to this backend
    # directly. Connecting the browser's EventSource straight here (instead of
    # through Next's dev rewrite proxy) avoids the proxy BUFFERING the SSE stream
    # — the proxy holds events until the connection closes, which makes a live
    # run look frozen until it finishes. In prod the static UI is served same-
    # origin by this app, so CORS is a no-op there. Allowlist localhost only.
    app.add_middleware(
        CORSMiddleware,
        allow_origin_regex=r"http://(localhost|127\.0\.0\.1)(:\d+)?",
        allow_methods=["*"],
        allow_headers=["*"],
    )

    # Auth gate. Added AFTER CORS, so CORS wraps it (outermost) — preflight
    # OPTIONS are answered by CORS and never reach here. We still bypass OPTIONS
    # defensively (a same-origin request via the Next proxy often omits Origin,
    # so CORS does not short-circuit it). Only /api is gated; the Next server
    # (:3001) owns the UI/login page and must be secured separately when exposed
    # (reverse proxy / loopback bind) — see docs/_local/plan_p3_auth.md.
    #
    # @app.middleware("http") does NOT see websocket scope; the /terminal WS and
    # the SSE /events stream do their own ticket/token check in-handler.
    #
    # IMPORTANT (CORS): a middleware that SHORT-CIRCUITS with its own Response
    # bypasses CORSMiddleware's response path, so a cross-origin 401 would arrive
    # at the browser WITHOUT Access-Control-Allow-Origin — the browser then
    # reports a network error instead of a 401, and the frontend can't tell "needs
    # login" from "backend down". The Next dev UI (:3001) talks to this backend
    # (:8000) cross-origin, so we must mirror the CORS allow-origin header onto the
    # 401 ourselves. (CORSMiddleware only auto-adds headers when the inner app
    # actually runs; our early return never reaches it.)
    _cors_origin_re = re.compile(r"http://(localhost|127\.0\.0\.1)(:\d+)?$")

    def _unauthorized(request: Request) -> JSONResponse:
        resp = JSONResponse({"error": "unauthorized"}, status_code=401)
        origin = request.headers.get("origin")
        if origin and _cors_origin_re.match(origin):
            resp.headers["Access-Control-Allow-Origin"] = origin
            resp.headers["Vary"] = "Origin"
        return resp

    @app.middleware("http")
    async def _auth_gate(request: Request, call_next):
        cfg: AuthConfig = app.state.auth
        if not cfg.enabled:
            return await call_next(request)
        path = request.url.path
        if request.method == "OPTIONS":
            return await call_next(request)
        if not path.startswith("/api/"):
            return await call_next(request)  # static/UI (only present if built)
        if path in PUBLIC_API_PATHS:
            return await call_next(request)
        # SSE events stream authenticates via one-time ticket query param, not a
        # header (EventSource can't set headers); let the handler enforce it.
        if path.endswith("/events"):
            return await call_next(request)
        token = bearer_from_header(request.headers.get("Authorization"))
        if not verify_token(cfg, token):
            return _unauthorized(request)
        return await call_next(request)

    @app.post("/api/auth/login")
    async def auth_login(request: Request) -> Any:
        # Exchange the operator password for a signed session token. This route
        # is intentionally reachable WITHOUT a token (you have none yet). When
        # auth is disabled it still returns a (useless) token so the frontend
        # flow is uniform.
        cfg: AuthConfig = app.state.auth
        body = await _require_dict_body(request, allow_empty=True)
        if not cfg.enabled:
            return {"ok": True, "token": "", "auth_required": False}
        if not check_password(cfg, body.get("password")):
            # constant-time compare already done; uniform 401, no "wrong length"
            raise HTTPException(status_code=401, detail="invalid password")
        return {"ok": True, "token": issue_token(cfg), "auth_required": True}

    @app.get("/api/auth/me")
    async def auth_me(request: Request) -> Any:
        # Cheap "is my token still valid?" probe. Reachable past the gate only
        # with a valid token (when enabled), so a 200 means authenticated.
        # in_container (P2-v3): tells the UI the coordinator runs inside a
        # container, so the deck must force container mode and disable the
        # "local" worker-isolation toggle (local is rejected server-side anyway).
        from muteki.core.runtime_env import is_web_container
        cfg: AuthConfig = app.state.auth
        return {"authenticated": True, "auth_required": cfg.enabled,
                "in_container": is_web_container()}

    @app.post("/api/auth/ticket")
    async def auth_ticket(request: Request) -> Any:
        # Mint a one-time, short-TTL ticket for opening an SSE/WS connection
        # (which can't carry an Authorization header). Requires a valid token —
        # it sits behind the gate, so reaching here already proves auth.
        ticket = app.state.tickets.mint()
        return {"ticket": ticket}

    @app.get("/api/runs")
    async def list_runs(archived: int = 0) -> Any:
        # rich summaries (name/category/status/pinned/archived) for the thread
        # rail. ?archived=1 includes archived rows (the rail's archived view).
        return {"runs": app.state.manager.list_runs(include_archived=bool(archived))}

    @app.patch("/api/runs/{run_id}")
    async def update_run(run_id: str, request: Request) -> Any:
        # Operator rail mutations: pin / archive / rename. Body carries any of
        # {"pinned": bool, "archived": bool, "name": str}. Each is persisted to
        # the meta side-table and reflected in subsequent /api/runs summaries.
        body = await _require_dict_body(request)
        mgr = app.state.manager
        ok = True
        if "pinned" in body:
            ok = mgr.set_pinned(run_id, bool(body["pinned"]), now=time.time()) and ok
        if "archived" in body:
            ok = mgr.set_archived(run_id, bool(body["archived"])) and ok
        if "name" in body:
            ok = mgr.rename(run_id, body.get("name")) and ok
        if "folder_id" in body:
            ok = mgr.set_folder(run_id, body.get("folder_id")) and ok
        if "order" in body:
            ok = mgr.set_order(run_id, body.get("order")) and ok
        run = mgr.get(run_id)
        return {"ok": ok, "run": run.summary() if run else None}

    @app.get("/api/folders")
    async def list_folders() -> Any:
        return {"folders": app.state.manager.list_folders()}

    @app.post("/api/folders")
    async def create_folder(request: Request) -> Any:
        body = await _require_dict_body(request)
        f = app.state.manager.create_folder(body.get("name", ""))
        return {"folder": f}

    @app.patch("/api/folders/{folder_id}")
    async def update_folder(folder_id: str, request: Request) -> Any:
        body = await _require_dict_body(request)
        ok = app.state.manager.update_folder(
            folder_id, name=body.get("name"), order=body.get("order"))
        return {"ok": ok}

    @app.delete("/api/folders/{folder_id}")
    async def delete_folder(folder_id: str) -> Any:
        ok = app.state.manager.delete_folder(folder_id)
        return {"ok": ok}

    @app.delete("/api/runs/{run_id}")
    async def delete_run(run_id: str) -> Any:
        # Hard-delete: cancels the task, drops the in-memory handle, the JSONL
        # log, and the meta row. Irreversible — the UI confirms before calling.
        ok = await app.state.manager.delete(run_id)
        return {"ok": ok}

    @app.post("/api/runs/{run_id}/open")
    async def open_run_workspace(run_id: str) -> Any:
        # Reveal the run's workspace dir in the host file manager. Only meaningful
        # when the operator runs the backend locally; a no-op (ok:false) otherwise.
        ok = app.state.manager.open_workspace(run_id)
        return {"ok": ok}

    @app.get("/api/runs/{run_id}/credentials")
    async def run_credentials(run_id: str) -> Any:
        from muteki.models.solve_graph import Challenge
        from muteki.swarm.shared_graph import SQLiteSharedGraph

        mgr: RunManager = app.state.manager
        run = mgr.get(run_id)
        graph_db = mgr.workspace_dir(run_id) / "graph" / "shared_graph.db"
        if not graph_db.exists():
            return {"credentials": []}
        challenge = Challenge(
            id=run_id,
            name=(run.name if run else run_id),
            category=(run.category if run else "web") or "web",
        )
        graph = None
        try:
            graph = SQLiteSharedGraph.open(db_path=graph_db, challenge=challenge)
            return {"credentials": graph.canonical_credentials()}
        finally:
            if graph is not None:
                graph.close()

    # ── BTW side-query worker (separate one-shot process, no swarm slot) ──────
    # Independent route (not /hitl), no run.hitl queue, no InsightBus GUIDANCE,
    # no bus.emit, no CostController, no CliSolver, no scheduler/max_worker slot.
    # Each turn cold-starts one CLI worker process, passes the frontend transcript
    # for multi-turn context, streams its answer, and kills it on disconnect or
    # superseding /btw request. The process exits after the turn.
    @app.post("/api/runs/{run_id}/btw")
    async def btw(run_id: str, request: Request) -> Any:
        from apps.web.drivers import (
            _runtime_for_profile,
            _standby_profile_for,
        )
        from apps.web.worker_config import backend_for_profile, resolve_worker_backend
        from muteki.core.runtime_env import is_web_container
        from muteki.solver.btw import (
            BtwLimiter,
            BtwWorkerPaths,
            build_btw_worker_prompt,
            run_meta_dict,
            sanitize_transcript,
            stream_btw_worker_deltas,
        )
        from muteki.solver.cli_driver import driver_for
        from muteki.solver.credential_accounts import (
            account_store_root,
            runtime_env_for_engine,
        )
        from muteki.solver.worker_profiles import base_engine_for_profile

        body = await _require_dict_body(request)
        question = str(body.get("question") or "").strip()
        if not question:
            return JSONResponse({"error": "empty question"}, status_code=400)
        transcript = sanitize_transcript(body.get("transcript"))
        context_hint = str(body.get("context_hint") or "")

        mgr: RunManager = app.state.manager
        run = mgr.get(run_id)
        if run is None:
            # Unknown run → 404. Do NOT create a workspace for it.
            return JSONResponse({"error": "unknown run"}, status_code=404)

        # The worker needs a cwd, so /btw creates only a per-turn scratch dir under
        # the run workspace. It never opens the graph read-write or joins the swarm.
        safe = run_id.replace("/", "_").replace("..", "_")
        root = mgr.workspace_dir(run_id).resolve()
        graph_db = root / "graph" / "shared_graph.db"
        jsonl_path = (mgr.sessions_root / f"{safe}.jsonl").resolve()
        board_path = root / ".muteki_board.md"
        winner_path = root / "winner.json"
        arts_path = root / "arts"
        uploads_path = (mgr.sessions_root / safe / "uploads").resolve()
        challenge_name = run.name or run_id
        challenge_category = (run.category or "web") or "web"
        meta = run_meta_dict(run)
        try:
            deck_workers = getattr(run, "deck_workers", None)
            if deck_workers:
                meta["workers"] = list(deck_workers)
        except Exception:
            pass

        # Lazy-init the per-app limiter (one BtwLimiter for all runs, keyed by run_id).
        limiter: BtwLimiter = getattr(app.state, "btw_limiters", None)
        if limiter is None:
            limiter = BtwLimiter()
            app.state.btw_limiters = limiter  # type: ignore[attr-defined]

        winner: dict[str, Any] = {}
        if winner_path.exists():
            try:
                raw = json.loads(winner_path.read_text(encoding="utf-8"))
                if isinstance(raw, dict):
                    winner = raw
            except Exception:
                winner = {}
        wc = mgr.worker_config.resolve(challenge_category)
        worker_profiles = wc.get("worker_profiles") or []
        runtime_profiles = wc.get("runtime_profiles") or []

        def _pick_profile() -> tuple[dict[str, Any] | None, str]:
            requested = str(
                body.get("profile") or body.get("engine") or ""
            ).strip()
            review = ((wc.get("stage_policy") or {}).get("coordinator") or {}).get("review") or {}
            candidates = [
                requested,
                str(review.get("engine") or "").strip(),
                str(winner.get("engine") or "").strip(),
            ]
            for p in worker_profiles:
                if isinstance(p, dict) and p.get("enabled", True):
                    roles = p.get("roles") or []
                    if "respond" in roles or "review" in roles:
                        candidates.append(str(p.get("name") or p.get("id") or ""))
            candidates.extend(str(e) for e in (wc.get("engines") or []))
            candidates.extend(["claude", "codex"])
            for cand in candidates:
                if not cand:
                    continue
                profile = _standby_profile_for(cand, worker_profiles)
                if profile is not None:
                    return profile, cand
                base = base_engine_for_profile(cand)
                if base in ("claude", "codex", "cursor"):
                    return None, base
            return None, "claude"

        async def stream():
            # Register this generation as the run's active btw; cancel any prior.
            this_task = asyncio.current_task()
            if this_task is not None:
                limiter.acquire(run_id, this_task)
            profile, selected = _pick_profile()
            transport = base_engine_for_profile(profile or selected)
            worker_backend = resolve_worker_backend(
                request_backend=body.get("worker_backend"),
                config_backend=wc.get("worker_backend"),
                env_backend=os.environ.get("MUTEKI_WORKER_BACKEND"),
                in_web_container=is_web_container(),
            )
            backend = (
                backend_for_profile(
                    profile,
                    runtime_profiles=runtime_profiles,
                    worker_backend=worker_backend,
                    in_web_container=is_web_container(),
                )
                if profile else worker_backend
            )
            runtime = _runtime_for_profile(profile, runtime_profiles)
            container = None
            account_root = account_store_root(mgr.sessions_root)
            worker_root = root / "workers" / "_btw"
            worker_root.mkdir(parents=True, exist_ok=True)
            workdir = worker_root / f"{transport}-{int(time.time() * 1000)}"
            workdir.mkdir(parents=True, exist_ok=True)
            try:
                if backend == "container":
                    from muteki.solver.container_exec import (
                        _chown_tree_to_worker,
                        ensure_container,
                    )

                    container = await asyncio.to_thread(
                        ensure_container,
                        run_id,
                        str(root),
                        network=str(runtime.get("network") or "bridge"),
                        memory=str(runtime.get("memory") or "") or None,
                        cpus=str(runtime.get("cpus") or "") or None,
                        pids_limit=int(runtime.get("pids_limit") or 0) or None,
                        account_root=str(account_root),
                    )
                    await asyncio.to_thread(_chown_tree_to_worker, str(workdir))

                def _worker_path(p: Path) -> str:
                    if container is not None:
                        mapper = getattr(container, "to_container_path", None)
                        if callable(mapper):
                            return str(mapper(str(p)))
                    return str(p)

                prompt = build_btw_worker_prompt(
                    question=question,
                    paths=BtwWorkerPaths(
                        workspace=_worker_path(root),
                        jsonl=_worker_path(jsonl_path),
                        graph_db=_worker_path(graph_db),
                        board=_worker_path(board_path),
                        winner=_worker_path(winner_path),
                        arts=_worker_path(arts_path),
                        uploads=_worker_path(uploads_path),
                    ),
                    challenge_id=run_id,
                    challenge_name=challenge_name,
                    challenge_category=challenge_category,
                    run_state=str(meta.get("state") or ""),
                    context_hint=context_hint,
                    transcript=transcript,
                )
                worker_env = runtime_env_for_engine(
                    transport,
                    account_root=account_root,
                    account_id=(profile.get("credential_account") if profile else None),
                    container=container is not None,
                ).env
                worker_env["MUTEKI_BTW_WORKER"] = "1"
                worker_env["MUTEKI_BLACKBOARD_DB"] = ""
                if profile:
                    worker_env["MUTEKI_WORKER_PROFILE_ID"] = str(profile.get("id") or "")
                    worker_env["MUTEKI_CREDENTIAL_ACCOUNT_ID"] = str(
                        profile.get("credential_account") or ""
                    )
                    if profile.get("model"):
                        worker_env["MUTEKI_WORKER_MODEL"] = str(profile["model"])
                if container is not None:
                    home_host = root / "homes" / f"btw-{transport}"
                    home_host.mkdir(parents=True, exist_ok=True)
                    from muteki.solver.container_exec import _chown_tree_to_worker
                    await asyncio.to_thread(_chown_tree_to_worker, str(home_host))
                    mapper = getattr(container, "to_container_path", None)
                    worker_env["HOME"] = (
                        mapper(str(home_host)) if callable(mapper) else str(home_host)
                    )
                async for chunk in stream_btw_worker_deltas(
                    driver=driver_for(profile or transport),
                    prompt=prompt,
                    cwd=str(workdir),
                    timeout=_env_int("MUTEKI_BTW_WORKER_TIMEOUT", 240),
                    env=worker_env,
                    container=container,
                    web_access=False,
                    kb_access=False,
                ):
                    if await request.is_disconnected():
                        break
                    yield {"data": json.dumps({"delta": chunk}, ensure_ascii=False)}
            except asyncio.CancelledError:
                # limiter cancel or client disconnect — stop cleanly.
                pass
            except Exception as e:  # noqa: BLE001
                yield {"data": json.dumps({"error": str(e)[:300]}, ensure_ascii=False)}
            finally:
                this_task = asyncio.current_task()
                if this_task is not None:
                    limiter.release(run_id, this_task)
            yield {"data": json.dumps({"done": True}, ensure_ascii=False)}

        return EventSourceResponse(stream(), ping=10)

    # cheap TTL cache so a polling deck doesn't re-probe every engine's --version
    # on each request (the probes are subprocess spawns). Codex' real-turn probe can
    # legitimately take minutes during websocket→HTTPS fallback, so keep a long UI
    # TTL and singleflight refreshes: stale data is better than stacking probes.
    _engine_cache: dict[str, Any] = {"ts": 0.0, "data": None}
    _engine_cache_ttl_s = 300.0
    _engine_refresh_lock = asyncio.Lock()

    @app.get("/api/engines")
    async def engines() -> Any:
        from muteki.solver.cli_driver import engine_status

        now = time.time()
        if _engine_cache["data"] is not None and now - _engine_cache["ts"] <= _engine_cache_ttl_s:
            return {"engines": _engine_cache["data"]}
        if _engine_refresh_lock.locked() and _engine_cache["data"] is not None:
            return {"engines": _engine_cache["data"]}
        async with _engine_refresh_lock:
            now = time.time()
            if _engine_cache["data"] is not None and now - _engine_cache["ts"] <= _engine_cache_ttl_s:
                return {"engines": _engine_cache["data"]}
            # run the (blocking) probes off the event loop. Pass the account store
            # so health probes use the SAME creds the worker uses.
            acct_root = str(account_store_root(app.state.manager.sessions_root))
            try:
                cfg = app.state.manager.worker_config.get()
                backend = str(cfg.get("worker_backend") or "local")
                enabled = set(cfg.get("engines") or [])
                profiles = [
                    p for p in (cfg.get("worker_profiles") or [])
                    if (p.get("name") or p.get("id")) in enabled
                ]
            except Exception:
                backend = "local"
                profiles = []
            data = await asyncio.to_thread(engine_status, acct_root, backend, profiles)
            _engine_cache["data"] = data
            _engine_cache["ts"] = time.time()
        return {"engines": _engine_cache["data"]}

    @app.get("/api/engines/health")
    async def engines_health(request: Request) -> Any:
        # DEEP self-check. `backend` query selects local (host CLI + auth) vs
        # container (docker run --rm: image + CLI launchable inside the worker
        # image). On-demand only — the self-check page triggers it.
        from muteki.solver.cli_driver import engine_health

        backend = str(request.query_params.get("backend") or "local")
        if backend not in ("local", "container"):
            backend = "local"
        acct_root = str(account_store_root(app.state.manager.sessions_root))
        profiles = []
        if backend == "local":
            try:
                cfg = app.state.manager.worker_config.get()
                enabled = set(cfg.get("engines") or [])
                profiles = [
                    p for p in (cfg.get("worker_profiles") or [])
                    if (p.get("name") or p.get("id")) in enabled
                ]
            except Exception:
                profiles = []
        data = await asyncio.to_thread(engine_health, backend, acct_root, profiles)
        return {"engines": data}

    @app.get("/api/settings/workers")
    async def get_worker_settings() -> Any:
        # the default worker roster (engines + bootstrap count + per-category
        # overrides) the dispatch path falls back to when a request is silent.
        return {"config": app.state.manager.worker_config.get()}

    @app.put("/api/settings/workers")
    async def put_worker_settings(request: Request) -> Any:
        body = await _require_dict_body(request)
        try:
            cfg = app.state.manager.worker_config.set(
                engines=body.get("engines"),
                start_workers=body.get("start_workers"),
                max_workers=body.get("max_workers"),
                worker_backend=body.get("worker_backend"),
                race_scout=body.get("race_scout"),
                race_timeout=body.get("race_timeout"),
                wall_clock_budget=body.get("wall_clock_budget"),
                race_engines=body.get("race_engines"),
                max_total_workers=body.get("max_total_workers"),
                cost_budget_usd=body.get("cost_budget_usd"),
                stage_policy=body.get("stage_policy"),
                llm_profiles=body.get("llm_profiles"),
                runtime_profiles=body.get("runtime_profiles"),
                worker_profiles=body.get("worker_profiles"),
                overrides=body.get("overrides"),
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        return {"ok": True, "config": cfg}

    @app.put("/api/settings/identity")
    async def put_identity_model(request: Request) -> Any:
        # Save the NEW Credential/Seat/Environment model. Additive to the legacy
        # PUT /workers above (which still accepts worker_profiles/engines). The
        # store validates the container×system_inherit legality gate and rejects an
        # illegal combo with 400. GET the model back via GET /workers (config.seats
        # / config.credentials / config.environments are attached there).
        body = await _require_dict_body(request)
        try:
            cfg = app.state.manager.worker_config.set_identity_model(
                seats=body.get("seats"),
                credentials=body.get("credentials"),
                environments=body.get("environments"),
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        return {"ok": True, "config": cfg}

    @app.get("/api/settings/profiles/health")
    async def get_profiles_health() -> Any:
        # Per-profile readiness for the settings badge + account rows. Uses the
        # CHEAP binding layer (zero network / zero docker) so opening the modal
        # never fires a wall of CLI hellos — the deep auth probe is the explicit
        # "测连通" button (POST below). Backend is resolved from SERVER context
        # via backend_for_profile (the same per-profile runtime→backend mapping
        # dispatch uses), NEVER trusted from the client, so the verdict predicts
        # what a real run would use.
        from dataclasses import asdict

        from muteki.core.runtime_env import is_web_container
        from muteki.solver.profile_health import evaluate_profile_health
        from apps.web.worker_config import backend_for_profile

        cfg = app.state.manager.worker_config.get()
        profiles = [p for p in (cfg.get("worker_profiles") or []) if isinstance(p, dict)]
        runtime_profiles = cfg.get("runtime_profiles") or []
        worker_backend = str(cfg.get("worker_backend") or "")
        in_web = is_web_container()
        sessions_root = app.state.manager.sessions_root

        def _eval_all() -> list[dict]:
            out: list[dict] = []
            for p in profiles:
                backend = backend_for_profile(
                    p, runtime_profiles=runtime_profiles,
                    worker_backend=worker_backend, in_web_container=in_web,
                )
                h = evaluate_profile_health(
                    p, backend=backend, sessions_root=sessions_root, depth="binding"
                )
                out.append(asdict(h))
            return out

        return {"profiles": await asyncio.to_thread(_eval_all)}

    @app.post("/api/settings/profiles/{profile_id}/health")
    async def test_profile_health(profile_id: str) -> Any:
        # "测连通" — the DEEP probe for one profile: binding + (container) plumbing
        # + a real auth hello using the profile's PINNED model. This is the verdict
        # that matches the dispatch precheck, so a green here means the run won't
        # die on profile_unhealthy.
        from dataclasses import asdict

        from muteki.core.runtime_env import is_web_container
        from muteki.solver.profile_health import evaluate_profile_health
        from apps.web.worker_config import backend_for_profile

        cfg = app.state.manager.worker_config.get()
        profiles = [p for p in (cfg.get("worker_profiles") or []) if isinstance(p, dict)]
        match = next(
            (p for p in profiles
             if str(p.get("name") or p.get("id")) == profile_id
             or str(p.get("id")) == profile_id),
            None,
        )
        # After the identity migration a profile's id is the new seat id; the old
        # name (e.g. "claude-local") survives only in the alias table. Resolve it so
        # the "测连通" button keeps working with either an old name or a new seat id.
        if match is None:
            from muteki.solver.worker_profiles import resolve_seat_ref
            sid = resolve_seat_ref(
                profile_id, seats=cfg.get("seats") or [],
                alias_table=cfg.get("seat_alias") or {},
            )
            if sid is not None:
                match = next((p for p in profiles if str(p.get("id")) == sid), None)
        if match is None:
            raise HTTPException(status_code=404, detail=f"unknown profile: {profile_id}")
        backend = backend_for_profile(
            match, runtime_profiles=cfg.get("runtime_profiles") or [],
            worker_backend=str(cfg.get("worker_backend") or ""),
            in_web_container=is_web_container(),
        )
        h = await asyncio.to_thread(
            evaluate_profile_health,
            match, backend=backend,
            sessions_root=app.state.manager.sessions_root, depth="auth",
        )
        return asdict(h)

    @app.get("/api/settings/worker-models")
    async def get_worker_models() -> Any:
        from apps.web.worker_models import worker_model_options_payload

        return worker_model_options_payload()

    @app.post("/api/settings/worker-model/test")
    async def test_worker_model(request: Request) -> Any:
        body = await _require_dict_body(request)
        from apps.web.worker_models import probe_worker_model

        profile = body.get("profile")
        if not isinstance(profile, dict):
            raise HTTPException(status_code=400, detail="profile must be an object")
        return await asyncio.to_thread(
            probe_worker_model,
            profile=profile,
            model=str(body.get("model") or ""),
            sessions_root=app.state.manager.sessions_root,
            backend=str(body.get("backend") or "local"),
        )

    @app.get("/api/settings/worker-image")
    async def get_worker_image() -> Any:
        # P2-v3: worker-image health (daemon reachable / image pulled / version
        # match). Docker probes are blocking subprocess.run → off the event loop.
        from apps.web.worker_image import image_status
        return await asyncio.to_thread(image_status)

    @app.post("/api/settings/worker-image/pull")
    async def pull_worker_image() -> Any:
        # P2-v3: one-click `docker pull` of the worker image. Can take minutes;
        # to_thread it so the single uvicorn loop keeps serving.
        from apps.web.worker_image import pull_image
        return await asyncio.to_thread(pull_image)

    @app.get("/api/settings/credential-accounts")
    async def list_credential_accounts() -> Any:
        store = CredentialAccountStore(account_store_root(app.state.manager.sessions_root))
        return {"accounts": store.list()}

    @app.put("/api/settings/credential-accounts/{account_id}")
    async def put_credential_account(account_id: str, request: Request) -> Any:
        body = await _require_dict_body(request)
        store = CredentialAccountStore(account_store_root(app.state.manager.sessions_root))
        try:
            account = store.upsert_secret(
                account_id=account_id,
                engine=str(body.get("engine") or ""),
                secret=(body.get("secret") if body.get("secret") is not None else None),
                codex_auth_json=(
                    body.get("codex_auth_json")
                    if body.get("codex_auth_json") is not None else None
                ),
                base_url=(body.get("base_url") if body.get("base_url") is not None else None),
                target_engine=(
                    body.get("target_engine") if body.get("target_engine") is not None else None
                ),
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        return {"ok": True, "account": account}

    @app.delete("/api/settings/credential-accounts/{account_id}")
    async def delete_credential_account(account_id: str) -> Any:
        store = CredentialAccountStore(account_store_root(app.state.manager.sessions_root))
        return {"ok": store.delete(account_id)}

    @app.post("/api/settings/credential-accounts/{account_id}/import-host-codex")
    async def import_host_codex(account_id: str) -> Any:
        # One-click refresh of a codex account from the HOST's ~/.codex/auth.json.
        # `codex login` writes the host file; container workers mount the account
        # COPY, so a fresh login must be re-imported. Only valid on a bare host —
        # inside the web container ~/.codex is the container's, not the operator's.
        from muteki.core.runtime_env import is_web_container

        if is_web_container():
            raise HTTPException(
                status_code=409,
                detail="import-from-host is unavailable when the web control plane "
                       "runs in a container (~/.codex is not the operator's). Paste "
                       "or upload the auth.json instead.",
            )
        store = CredentialAccountStore(account_store_root(app.state.manager.sessions_root))
        try:
            account = await asyncio.to_thread(store.import_host_codex_auth, account_id)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        return {"ok": True, "account": account}

    @app.post("/api/settings/credential-accounts/{account_id}/test")
    async def test_credential_account(account_id: str, request: Request) -> Any:
        # Test the REGISTERED account (DESIGN §2.4 補強C-2). local → host probe with
        # the account's env; container → real `docker run --rm` plumbing test.
        # Never falls back to the host default login.
        body = await _require_dict_body(request, allow_empty=True)
        from apps.web.account_test import probe_account

        engine = str(body.get("engine") or "").strip()
        backend = str(body.get("backend") or "local").strip()
        if backend not in ("local", "container"):
            backend = "local"
        result = await asyncio.to_thread(
            probe_account,
            engine=engine,
            account_id=account_id,
            sessions_root=app.state.manager.sessions_root,
            backend=backend,
        )
        return result

    @app.get("/api/settings/system-login")
    async def get_system_login() -> Any:
        # Host-side login presence per engine (DESIGN §2.3 補強B). Drives the
        # local-mode credentials UI ("默认用系统登录"). Read-only, never raises.
        from muteki.solver.credential_accounts import detect_system_login

        logins = await asyncio.to_thread(
            lambda: {e: detect_system_login(e) for e in ("claude", "codex", "cursor")}
        )
        return {"logins": logins}

    @app.post("/api/settings/llm/test")
    async def test_llm_endpoint_route(request: Request) -> Any:
        # Test the planner/titler endpoint the operator is EDITING (DESIGN §2.4
        # 補強C-1): base_url + model from the request body, key from .env. ok by
        # API success, not content non-empty (reasoning models).
        body = await _require_dict_body(request)
        from apps.web.llm_test import test_llm_endpoint

        return await test_llm_endpoint(
            which=str(body.get("which") or "planner"),
            base_url=(body.get("base_url") if body.get("base_url") is not None else None),
            model=(body.get("model") if body.get("model") is not None else None),
        )

    @app.put("/api/settings/runtime-environment")
    async def put_runtime_environment(request: Request) -> Any:
        # Unify backend + runtime across all enabled profiles (DESIGN §5) so the
        # displayed run environment is what actually runs.
        body = await _require_dict_body(request)
        try:
            cfg = app.state.manager.worker_config.set_runtime_environment(
                backend=str(body.get("backend") or ""),
                runtime_id=str(body.get("runtime_id") or ""),
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        return {"ok": True, "config": cfg}

    @app.post("/api/runs")
    async def new_run(request: Request) -> Any:
        # Mint a fresh run id for a new conversation ("+ New solve"). The deck
        # then opens this run's SSE and POSTs /start with the dispatch prompt.
        run = app.state.manager.create_new()
        return {"run_id": run.run_id}

    @app.post("/api/runs/{run_id}/start")
    async def start_run(run_id: str, request: Request) -> Any:
        body = await _require_dict_body(request)
        from apps.web.drivers import build_driver

        driver = build_driver(body, mgr=app.state.manager)
        # seed rail metadata up front so the row appears the instant we dispatch
        # (before run.started lands) — conversational dispatch infers the rest.
        run = app.state.manager.get(run_id) or app.state.manager.create(run_id)
        ch = (body.get("challenge") or {})
        if ch.get("name"):
            run.name = ch["name"]
        if ch.get("category"):
            run.category = ch["category"]
        # Re-starting an existing run_id (e.g. a re-test redo of the same challenge):
        # the run object still carries the PRIOR run's terminal state (finished/solved/
        # flag). Reset it synchronously here so the rail doesn't show a freshly-
        # dispatched run as "已解出" until the new run.started bus event is sinked
        # (it would otherwise display the stale solved flag the whole time it runs).
        run.finished = False
        run.solved = False
        run.flag = None
        run.flags = []
        run.paused = False
        run.started = True
        await app.state.manager.start(run_id, driver)

        # ChatGPT-style auto-title: if the operator gave no explicit name, kick off
        # a background summarizer that names the conversation from the prompt and
        # emits RUN_TITLED. Fire-and-forget so it never delays swarm launch.
        if not run.name:
            prompt = body.get("prompt") or ch.get("description") or ""
            if prompt.strip():
                from apps.web.titler import generate_title

                llm_profiles = app.state.manager.worker_config.get().get("llm_profiles", {})
                titler_profile = llm_profiles.get("titler") or {}
                title_model = titler_profile.get("model")
                title_base_url = titler_profile.get("base_url") or None
                asyncio.create_task(
                    generate_title(prompt, bus=run.bus, run_id=run_id,
                                   model=title_model, base_url=title_base_url)
                )

        return {"run_id": run_id, "started": True, "kind": body.get("kind", "swarm")}

    @app.post("/api/runs/{run_id}/uploads")
    async def upload_files(
        run_id: str, files: list[UploadFile] = File(...)
    ) -> Any:
        # File-based tracks (crypto/rev/forensics/misc) ship the challenge AS
        # files. The deck POSTs them here; we save into the run's own folder
        # (sessions/{id}/uploads/) and hand back ABSOLUTE paths. The deck then
        # threads those paths into challenge.attachments at /start, and the
        # worker stages them into its cwd (CliSolver._stage_attachments). No
        # bytes flow through /start — only the saved paths.
        mgr: RunManager = app.state.manager
        # ensure a run handle exists so an upload BEFORE dispatch still works
        # (the deck promotes a draft to a real run id before uploading, but be
        # robust — mirror the get-or-create the events/start endpoints use).
        mgr.get(run_id) or mgr.create(run_id)
        if len(files) > MAX_UPLOAD_FILES:
            raise HTTPException(status_code=413, detail="too many files")

        dest_dir = mgr.uploads_dir(run_id)
        saved: list[dict[str, Any]] = []
        for uf in files:
            # SANITIZE: strip any path the client put in the name. Path(name).name
            # drops directories AND collapses "../x"/absolute paths to a basename,
            # so an upload can never escape dest_dir.
            name = Path(uf.filename or "file").name
            if not name or name in (".", ".."):
                name = "file"
            # dedupe collisions within this run's folder: foo.txt, foo-1.txt, ...
            target = dest_dir / name
            if target.exists():
                stem, suf = target.stem, target.suffix
                i = 1
                while (dest_dir / f"{stem}-{i}{suf}").exists():
                    i += 1
                target = dest_dir / f"{stem}-{i}{suf}"
            # stream to disk in chunks with a running size guard (never buffer a
            # whole file in memory; abort + clean up if it blows the cap).
            size = 0
            try:
                with target.open("wb") as out:
                    while True:
                        chunk = await uf.read(1 << 20)  # 1 MB
                        if not chunk:
                            break
                        size += len(chunk)
                        if size > MAX_UPLOAD_BYTES:
                            out.close()
                            target.unlink(missing_ok=True)
                            raise HTTPException(
                                status_code=413, detail=f"{name} too large"
                            )
                        out.write(chunk)
            finally:
                await uf.close()
            saved.append(
                {"name": target.name, "path": str(target.resolve()), "size": size}
            )
        return {"files": saved}

    @app.get("/api/runs/{run_id}/events")
    async def events(run_id: str, request: Request) -> Any:
        # Auth: EventSource can't send an Authorization header, so the SSE stream
        # authenticates via a one-time ticket (?ticket=) minted by an
        # authenticated POST /api/auth/ticket. A bearer header is also accepted
        # (non-browser clients). This MUST run before manager.create() below, so
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
        # operator stares at an empty board, then fills the form). Create the run
        # handle on demand so the SSE stays open and starts streaming the instant
        # the run starts — instead of 404ing and forcing the browser to reconnect.
        run: Run = manager.get(run_id) or manager.create(run_id)

        last_id_hdr = request.headers.get("Last-Event-ID")
        last_id = int(last_id_hdr) if last_id_hdr and last_id_hdr.isdigit() else 0
        # The in-memory ring is bounded and a rehydrated/reopened run may have a
        # fresh EventBus. Always repair from the durable JSONL first, even on
        # reconnect. SessionStore.replay_monotonic() rewrites broken historical
        # seq resets (e.g. 1808 → 1 after a backend restart) into a single SSE
        # cursor, so the browser's Last-Event-ID never skips "new" low-id events.
        fresh = last_id == 0

        async def gen():
            replayed_seq = 0
            replayed_count = 0
            last_lifecycle = ""
            async for ev in run.store.replay_monotonic(run_id, after_seq=last_id):
                replayed_seq = ev.seq
                replayed_count += 1
                if ev.event_type in (EventType.RUN_STARTED,
                                     EventType.RUN_FINISHED,
                                     EventType.RUN_REOPENED):
                    last_lifecycle = ev.event_type.value
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
            if fresh and not live and last_lifecycle in ("run.started", "run.reopened"):
                replayed_seq = max(replayed_seq, run.store.last_stream_seq(run_id)) + 1
                synth = Event(
                    event_type=EventType.RUN_FINISHED, run_id=run_id,
                    seq=replayed_seq,
                    payload={"flag": run.flag, "flags": list(run.flags),
                             "expected_flags": run.expected_flags,
                             "multi_flag": run.multi_flag,
                             "solved": run.solved})
                yield {
                    "id": str(replayed_seq),
                    "event": synth.event_type.value,
                    "data": synth.model_dump_json(),
                }
            # live tail: everything after what we just replayed (or after the
            # client's Last-Event-ID on a reconnect). A finished run's bus is
            # closed, so subscribe() returns after backlog replay. Do NOT let the
            # HTTP response EOF: browser EventSource treats EOF as an error and
            # reconnects forever, replaying finished histories in a loop. Instead,
            # keep the SSE open (ping handles liveness) and hop to a fresh bus if
            # resolve/standby reopens the run.
            manager._sync_bus_seq(run.bus, store=run.store, run_id=run_id)
            tail_from = max(last_id, replayed_seq, run.store.last_stream_seq(run_id))
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

    @app.post("/api/runs/{run_id}/resolve")
    async def resolve_run(run_id: str, request: Request) -> Any:
        """"继续做题": relaunch the full coordinator swarm on a finished run (reuses
        its workspace so verified facts carry over). Distinct from /hitl which, on a
        finished run, only cold-starts a single standby worker for a follow-up."""
        body = await _require_dict_body(request, allow_empty=True)
        ok = await app.state.manager.resolve(run_id, body)
        return {"ok": ok}

    @app.post("/api/runs/{run_id}/workers")
    async def spawn_worker(run_id: str, request: Request) -> Any:
        # operator runtime control: add a worker for a specific engine to a LIVE
        # coordinator run. Body {"engine": "cursor"|"claude"|"codex"} (optional —
        # omitted lets the coordinator pick a heterogeneity-aware engine).
        body = await _require_dict_body(request, allow_empty=True)
        ok = await app.state.manager.post_worker_cmd(
            run_id, "spawn", engine=body.get("engine"))
        return {"ok": ok}

    @app.delete("/api/runs/{run_id}/workers")
    async def kill_worker(run_id: str, request: Request) -> Any:
        # operator runtime control: stop a specific worker by its solver_id.
        body = await _require_dict_body(request, allow_empty=True)
        ok = await app.state.manager.post_worker_cmd(
            run_id, "kill", solver_id=body.get("solver_id"))
        return {"ok": ok}

    @app.post("/api/runs/{run_id}/hitl")
    async def hitl(run_id: str, request: Request) -> Any:
        body = await _require_dict_body(request)
        ok = await app.state.manager.post_hitl(
            run_id,
            body.get("target", "global"),
            body.get("action", "hint"),
            **{k: v for k, v in body.items() if k not in ("target", "action")},
        )
        return {"ok": ok}

    # static UI: the deck is the Next.js app (run `./run.sh web` → :3001, which
    # talks to this backend's /api). If a Next.js static export ever drops an
    # index.html into ui/, serve it at / too; otherwise / is unused (the bare
    # backend is API-only).
    if (UI_DIR / "index.html").exists():
        @app.get("/")
        async def index() -> Any:
            return FileResponse(UI_DIR / "index.html")

    if UI_DIR.exists():
        app.mount("/ui", StaticFiles(directory=str(UI_DIR)), name="ui")

    return app


app = create_app()
