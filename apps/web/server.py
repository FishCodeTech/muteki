"""FastAPI backend for the web command deck (§14.1 / Sprint 1.1).

Endpoints:
  GET  /api/runs                      list known runs
  POST /api/runs/{run_id}/start       launch a run (mock driver, or swarm if a
                                       challenge spec is posted) — see drivers.py
  GET  /api/runs/{run_id}/events      SSE: the typed event stream (Last-Event-ID
                                       resume via the standard header)
  WS   /api/runs/{run_id}/terminal    sandbox terminal: TERMINAL_OUTPUT bytes
  POST /api/runs/{run_id}/control     durable operator command admission
  POST /api/runs/{run_id}/hitl        legacy adapter onto /control
  GET  /                              the single-page UI (static)

The server holds NO solving logic — it only brokers the event bus + HITL. Event
schema is the only contract (§3).
"""

from __future__ import annotations

import asyncio
import copy
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
from pydantic import ValidationError as PydanticValidationError
from sse_starlette.sse import EventSourceResponse, ServerSentEvent

from apps.web.control_adapter import ControlPayloadError
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
from apps.web.platform_update import PlatformUpdateController
from muteki.control import IdempotencyConflict, StateConflict
from muteki.core.dotenv_boot import load_env
from muteki.core.events import Event, EventType
from muteki.runtime.release_receipts import load_verified_release_receipts
from muteki.version import get_version
from muteki.solver.credential_accounts import (
    CredentialAccountStore,
    account_store_root,
)

load_env()  # local convenience: pick up repo-root .env (shell env still wins)
load_verified_release_receipts(root=Path(__file__).resolve().parents[2])

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
    app.state.platform_updates = PlatformUpdateController()

    @app.get("/api/health")
    async def health() -> Any:
        return {"status": "ok", "version": get_version()}

    def llm_settings_payload(config: dict[str, Any]) -> dict[str, Any]:
        """Expose credential presence/source without returning any secret value."""
        from apps.web.llm_credentials import LlmCredentialStore
        from muteki.solver.worker_profiles import resolve_seat_ref

        payload = copy.deepcopy(config)
        store = LlmCredentialStore(app.state.manager.sessions_root)
        profiles = payload.get("llm_profiles") or {}
        for which in ("planner", "titler"):
            row = profiles.get(which)
            if isinstance(row, dict):
                row["credential_source"] = store.source(which)
        # The settings UI edits canonical Seat IDs. Scheduler policy may still be
        # stored with a legacy profile name, so translate only the API payload and
        # leave the scheduler's legacy projection unchanged.
        seats = [row for row in (payload.get("seats") or []) if isinstance(row, dict)]
        aliases = payload.get("seat_alias") if isinstance(payload.get("seat_alias"), dict) else {}
        coordinator = (payload.get("stage_policy") or {}).get("coordinator") or {}
        for key, role in (("review", "review"), ("verifier", "verifier")):
            policy = coordinator.get(key)
            if not isinstance(policy, dict):
                continue
            canonical = resolve_seat_ref(
                policy.get("engine"), seats=seats, alias_table=aliases)
            if canonical is None:
                canonical = next((
                    str(seat.get("id")) for seat in seats
                    if role in (seat.get("roles") or []) and seat.get("enabled", True)
                ), None)
            policy["engine"] = canonical or ""
        return payload

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
        if app.state.manager.is_protocol2_run(run_id):
            raise HTTPException(
                status_code=409, detail="PROTOCOL2_PURGE_UNAVAILABLE")
        ok = await app.state.manager.delete(run_id)
        return {"ok": ok}

    @app.post("/api/runs/{run_id}/archive")
    async def archive_protocol2_run(run_id: str) -> Any:
        if not app.state.manager.is_protocol2_run(run_id):
            raise HTTPException(status_code=404, detail="unknown Protocol 2 run")
        try:
            status = await app.state.manager.archive_protocol2(run_id)
        except (StateConflict, RuntimeError) as exc:
            raise HTTPException(
                status_code=409, detail=type(exc).__name__
            ) from exc
        return {"operation_id": status["operation_id"],
                "run_id": status["run_id"], "state": status["state"].upper(),
                "archive_receipt_digest": status["archive_receipt_digest"]}

    @app.get("/api/archive-operations/{operation_id}")
    async def archive_operation_status(operation_id: str) -> Any:
        adapter = app.state.manager.protocol2
        if adapter is None:
            raise HTTPException(status_code=503, detail="Protocol 2 unavailable")
        try:
            status = adapter.archive_status(operation_id)
        except KeyError as exc:
            raise HTTPException(status_code=404, detail="unknown archive operation") from exc
        return {**status, "state": status["state"].upper()}

    @app.post("/api/runs/{run_id}/purge")
    async def purge_protocol2_run(run_id: str) -> Any:
        if not app.state.manager.is_protocol2_run(run_id):
            raise HTTPException(status_code=404, detail="unknown Protocol 2 run")
        try:
            status = await app.state.manager.purge_protocol2(run_id)
        except (StateConflict, RuntimeError) as exc:
            raise HTTPException(
                status_code=409, detail=type(exc).__name__
            ) from exc
        return {"operation_id": status["operation_id"],
                "run_id": status["run_id"], "state": status["state"].upper(),
                "plan_receipt_digest": status["plan_receipt_digest"],
                "absence_receipt_digest": status["absence_receipt_digest"]}

    @app.get("/api/purge-operations/{operation_id}")
    async def purge_operation_status(operation_id: str) -> Any:
        adapter = app.state.manager.protocol2
        if adapter is None:
            raise HTTPException(status_code=503, detail="Protocol 2 unavailable")
        try:
            status = adapter.purge_status(operation_id)
        except KeyError as exc:
            raise HTTPException(status_code=404, detail="unknown purge operation") from exc
        return {**status, "state": status["state"].upper()}

    @app.post("/api/runs/{run_id}/open")
    async def open_run_workspace(run_id: str) -> Any:
        # Reveal the run's workspace dir in the host file manager. Only meaningful
        # when the operator runs the backend locally; a no-op (ok:false) otherwise.
        if app.state.manager.is_protocol2_run(run_id):
            raise HTTPException(
                status_code=409, detail="PROTOCOL2_WORKSPACE_REVEAL_UNAVAILABLE")
        ok = app.state.manager.open_workspace(run_id)
        return {"ok": ok}

    @app.get("/api/runs/{run_id}/credentials")
    async def run_credentials(run_id: str) -> Any:
        from muteki.models.solve_graph import Challenge
        from muteki.swarm.shared_graph import SQLiteSharedGraph

        mgr: RunManager = app.state.manager
        run = mgr.get(run_id)
        if run is None:
            raise HTTPException(status_code=404, detail="unknown run")
        if not mgr.is_protocol1_run(run_id, run=run):
            raise HTTPException(
                status_code=409, detail="PROTOCOL2_CREDENTIALS_UNAVAILABLE")
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
        from apps.web.drivers import _standby_profile_for, _standby_worker_env
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
        from muteki.solver.credential_accounts import account_store_root
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
        if not mgr.is_protocol1_run(run_id, run=run):
            return JSONResponse(
                {"error": "PROTOCOL2_BTW_UNAVAILABLE"}, status_code=409)

        # The worker needs a cwd, so /btw creates only a per-turn scratch dir under
        # the run workspace. It never opens the graph read-write or joins the swarm.
        safe = mgr._safe_run_id(run_id)
        root = mgr.workspace_dir(run_id).resolve()
        graph_db = root / "graph" / "shared_graph.db"
        jsonl_path = (mgr.sessions_root / f"{safe}.jsonl").resolve()
        board_path = root / ".muteki_board.md"
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

        winner = mgr.load_winner_continuation(run_id)
        wc = mgr.worker_config.resolve(challenge_category)
        worker_profiles = wc.get("worker_profiles") or []
        worker_network = str(wc.get("worker_network") or "bridge")

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
                if base in (
                    "claude", "codex", "cursor", "pi", "omp", "kimi", "grok",
                    "opencode", "dsh",
                ):
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
                    worker_backend=worker_backend,
                    in_web_container=is_web_container(),
                )
                if profile else worker_backend
            )
            container = None
            # A BTW turn on a finished run cold-starts a run container that has no
            # swarm owner to tear it down.  Remember that ownership here; live runs
            # keep sharing their existing container with the active swarm.
            owns_finished_run_container = bool(run.finished)
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
                        network=worker_network,
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
                        # The Worker-writable winner artifact is intentionally not
                        # supplied as evidence to a side-query worker.
                        winner="",
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
                worker_env = _standby_worker_env(
                    root=root,
                    label=f"btw-{transport}",
                    engine=transport,
                    profile=profile,
                    account_root=account_root,
                    container=container,
                )
                worker_env["MUTEKI_BTW_WORKER"] = "1"
                worker_env["MUTEKI_BLACKBOARD_DB"] = ""
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
                if (container is not None and owns_finished_run_container
                        and run.finished):
                    from muteki.solver.container_exec import teardown_container
                    try:
                        removed = await asyncio.to_thread(
                            teardown_container, run_id, remove=True)
                        if removed is not True:
                            import logging
                            logging.getLogger(__name__).warning(
                                "BTW container teardown could not be proven for %s",
                                run_id,
                            )
                    except Exception:
                        import logging
                        logging.getLogger(__name__).exception(
                            "BTW container teardown failed for %s", run_id)
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

    def _invalidate_engine_cache() -> None:
        # The header polls this cache slowly.  A user who changes the enabled
        # Worker roster must see the saved engine on the very next run instead of
        # the previous roster for up to five minutes.
        _engine_cache["ts"] = 0.0
        _engine_cache["data"] = None

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
        return {"config": llm_settings_payload(app.state.manager.worker_config.get())}

    @app.get("/api/settings/system-update")
    async def get_system_update() -> Any:
        return {"update": app.state.platform_updates.status()}

    @app.post("/api/settings/system-update/check")
    async def check_system_update(request: Request) -> Any:
        body = await _require_dict_body(request, allow_empty=True)
        target = str(body.get("target") or "").strip() or None
        try:
            update = await app.state.platform_updates.check(target)
        except Exception:
            update = app.state.platform_updates.status()
        return {"update": update}

    @app.post("/api/settings/system-update/install")
    async def install_system_update(request: Request) -> Any:
        body = await _require_dict_body(request, allow_empty=True)
        target = str(body.get("target") or "").strip() or None
        try:
            update = await app.state.platform_updates.start(target, force=bool(body.get("force", False)))
        except Exception as exc:
            raise HTTPException(status_code=409, detail=str(exc))
        return {"update": update}

    @app.post("/api/settings/system-update/rollback")
    async def rollback_system_update() -> Any:
        try:
            update = await app.state.platform_updates.rollback()
        except Exception as exc:
            raise HTTPException(status_code=409, detail=str(exc))
        return {"update": update}

    @app.put("/api/settings/workers")
    async def put_worker_settings(request: Request) -> Any:
        body = await _require_dict_body(request)
        raw_llm_profiles = body.get("llm_profiles")
        llm_profiles = copy.deepcopy(raw_llm_profiles)
        if isinstance(llm_profiles, dict):
            for which in ("planner", "titler"):
                row = llm_profiles.get(which)
                if isinstance(row, dict):
                    row.pop("api_key", None)
                    row.pop("clear_api_key", None)
                    row.pop("credential_source", None)
        try:
            settings = {
                "engines": body.get("engines"),
                "start_workers": body.get("start_workers"),
                "max_workers": body.get("max_workers"),
                "worker_backend": body.get("worker_backend"),
                "worker_network": body.get("worker_network"),
                "race_scout": body.get("race_scout"),
                "race_timeout": body.get("race_timeout"),
                "wall_clock_budget": body.get("wall_clock_budget"),
                "race_engines": body.get("race_engines"),
                "max_total_workers": body.get("max_total_workers"),
                "cost_budget_usd": body.get("cost_budget_usd"),
                "stage_policy": body.get("stage_policy"),
                "llm_profiles": llm_profiles,
                "worker_profiles": body.get("worker_profiles"),
                "overrides": body.get("overrides"),
            }
            if "seats" in body or "credentials" in body:
                cfg = app.state.manager.worker_config.set_configuration(
                    seats=body.get("seats"),
                    credentials=body.get("credentials"),
                    **settings,
                )
            else:
                cfg = app.state.manager.worker_config.set(**settings)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        if isinstance(raw_llm_profiles, dict):
            from apps.web.llm_credentials import LlmCredentialStore

            store = LlmCredentialStore(app.state.manager.sessions_root)
            try:
                for which in ("planner", "titler"):
                    row = raw_llm_profiles.get(which)
                    if not isinstance(row, dict):
                        continue
                    if bool(row.get("clear_api_key")):
                        store.clear(which)
                    elif isinstance(row.get("api_key"), str) and row["api_key"].strip():
                        store.save(which, row["api_key"])
            except ValueError as exc:
                raise HTTPException(status_code=400, detail=str(exc))
        _invalidate_engine_cache()
        return {"ok": True, "config": llm_settings_payload(cfg)}

    @app.put("/api/settings/identity")
    async def put_identity_model(request: Request) -> Any:
        # Save the Credential/Seat model. Additive to the legacy
        # PUT /workers above (which still accepts worker_profiles/engines). The
        # store validates the container×system_inherit legality gate and rejects an
        # illegal combo with 400. GET the model back via GET /workers.
        body = await _require_dict_body(request)
        try:
            cfg = app.state.manager.worker_config.set_identity_model(
                seats=body.get("seats"),
                credentials=body.get("credentials"),
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        _invalidate_engine_cache()
        return {"ok": True, "config": cfg}

    @app.get("/api/settings/profiles/health")
    async def get_profiles_health() -> Any:
        # Per-profile readiness for the settings badge + account rows. Uses the
        # CHEAP binding layer (zero network / zero docker) so opening the modal
        # never fires a wall of CLI hellos — the deep auth probe is the explicit
        # "测连通" button (POST below). Backend is resolved from SERVER context
        # via backend_for_profile (the same global backend mapping dispatch uses),
        # NEVER trusted from the client, so the verdict predicts
        # what a real run would use.
        from dataclasses import asdict

        from muteki.core.runtime_env import is_web_container
        from muteki.solver.profile_health import evaluate_profile_health
        from apps.web.worker_config import backend_for_profile

        cfg = app.state.manager.worker_config.get()
        profiles = [p for p in (cfg.get("worker_profiles") or []) if isinstance(p, dict)]
        worker_backend = str(cfg.get("worker_backend") or "")
        in_web = is_web_container()
        sessions_root = app.state.manager.sessions_root

        def _eval_all() -> list[dict]:
            out: list[dict] = []
            for p in profiles:
                backend = backend_for_profile(
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

        return worker_model_options_payload(app.state.manager.sessions_root)

    @app.post("/api/settings/worker-models/discover")
    async def discover_worker_models_now(request: Request) -> Any:
        from muteki.core.runtime_env import is_web_container
        from apps.web.worker_config import backend_for_profile
        from apps.web.worker_models import (
            WorkerModelDiscoveryStore,
            discover_worker_models,
            worker_model_options_payload,
        )

        cfg = app.state.manager.worker_config.get()
        profiles = [
            profile
            for profile in (cfg.get("worker_profiles") or [])
            if isinstance(profile, dict)
        ]
        body = await _require_dict_body(request, allow_empty=True)
        profile_id = str(body.get("profile_id") or "").strip()
        if profile_id:
            profiles = [
                profile for profile in profiles
                if profile_id in {
                    str(profile.get("id") or "").strip(),
                    str(profile.get("name") or "").strip(),
                }
            ]
        results: list[dict[str, Any]] = []
        for profile in profiles:
            backend = backend_for_profile(
                worker_backend=str(cfg.get("worker_backend") or ""),
                in_web_container=is_web_container(),
            )
            results.append(
                await asyncio.to_thread(
                    discover_worker_models,
                    profile=profile,
                    sessions_root=app.state.manager.sessions_root,
                    backend=backend,
                )
            )

        WorkerModelDiscoveryStore(app.state.manager.sessions_root).save_results(results)
        payload = worker_model_options_payload(app.state.manager.sessions_root)
        payload["discovery_results"] = results
        payload["discovery_ok"] = any(bool(result.get("ok")) for result in results)
        return payload

    @app.post("/api/settings/worker-model/test")
    async def test_worker_model(request: Request) -> Any:
        body = await _require_dict_body(request)
        from apps.web.worker_config import backend_for_profile
        from apps.web.worker_models import probe_worker_model
        from muteki.core.runtime_env import is_web_container

        profile = body.get("profile")
        if not isinstance(profile, dict):
            raise HTTPException(status_code=400, detail="profile must be an object")
        cfg = app.state.manager.worker_config.get()
        backend = backend_for_profile(
            worker_backend=str(cfg.get("worker_backend") or ""),
            in_web_container=is_web_container(),
        )
        return await asyncio.to_thread(
            probe_worker_model,
            profile=profile,
            model=str(body.get("model") or ""),
            reasoning_effort=str(body.get("reasoning_effort") or "default"),
            sessions_root=app.state.manager.sessions_root,
            backend=backend,
            runtime={"network": str(cfg.get("worker_network") or "bridge")},
        )

    @app.post("/api/settings/worker-model/test-batch")
    async def test_worker_models_batch(request: Request) -> Any:
        body = await _require_dict_body(request)
        from apps.web.worker_config import backend_for_profile
        from apps.web.worker_models import probe_worker_models_batch
        from muteki.core.runtime_env import is_web_container

        items = body.get("items")
        if not isinstance(items, list):
            raise HTTPException(status_code=400, detail="items must be an array")
        cfg = app.state.manager.worker_config.get()
        backend = backend_for_profile(
            worker_backend=str(cfg.get("worker_backend") or ""),
            in_web_container=is_web_container(),
        )
        return await asyncio.to_thread(
            probe_worker_models_batch,
            items=[item for item in items if isinstance(item, dict)],
            sessions_root=app.state.manager.sessions_root,
            backend=backend,
            runtime={"network": str(cfg.get("worker_network") or "bridge")},
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
        requested_engine = str(
            body.get("worker_engine") or body.get("target_engine") or body.get("engine") or ""
        ).strip().lower()
        connection = str(body.get("connection") or "").strip().lower()
        if not connection:
            legacy_engine = str(body.get("engine") or "").strip().lower()
            legacy_base_url = str(body.get("base_url") or "").strip()
            connection = (
                "custom_endpoint"
                if legacy_engine == "api" or legacy_base_url
                else "official"
            )
        if connection not in {"official", "custom_endpoint"}:
            raise HTTPException(status_code=400, detail="connection must be official or custom_endpoint")
        if requested_engine not in {
            "claude", "codex", "cursor", "pi", "omp", "kimi", "grok",
            "opencode", "dsh"
        }:
            raise HTTPException(
                status_code=400,
                detail="worker_engine must be claude, codex, cursor, pi, omp, kimi, "
                       "grok, opencode, or dsh",
            )
        base_url = str(body.get("base_url") or "").strip()
        if connection == "custom_endpoint" and not base_url:
            raise HTTPException(status_code=400, detail="自定义端点必须填写 Base URL")
        storage_engine = "api" if connection == "custom_endpoint" else requested_engine
        try:
            account = store.upsert_secret(
                account_id=account_id,
                engine=storage_engine,
                secret=(body.get("secret") if body.get("secret") is not None else None),
                codex_auth_json=(
                    body.get("codex_auth_json")
                    if body.get("codex_auth_json") is not None else None
                ),
                base_url=base_url,
                target_engine=requested_engine if connection == "custom_endpoint" else None,
                provider=(body.get("provider") if body.get("provider") is not None else None),
                clear_base_url=connection == "official",
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        return {"ok": True, "account": account}

    @app.delete("/api/settings/credential-accounts/{account_id}")
    async def delete_credential_account(account_id: str) -> Any:
        cfg = app.state.manager.worker_config.get()
        credential_ids = {
            str(item.get("id"))
            for item in (cfg.get("credentials") or [])
            if isinstance(item, dict) and str(item.get("secret_ref") or "") == account_id
        }
        used_by = [
            str(item.get("label") or item.get("id") or "Worker")
            for item in (cfg.get("seats") or [])
            if isinstance(item, dict) and str(item.get("credential_id") or "") in credential_ids
        ]
        if used_by:
            raise HTTPException(
                status_code=409,
                detail=f"账号仍被 {len(used_by)} 个 Worker 使用：{'、'.join(used_by[:4])}",
            )
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

    @app.post("/api/settings/credential-accounts/{account_id}/import-host-login")
    async def import_host_login(account_id: str, request: Request) -> Any:
        """Copy a minimal Claude, Kimi Code, or Grok host login into an account.

        Container workers receive the account projection, so they can authenticate
        without mounting the operator's complete home directory.
        """
        from muteki.core.runtime_env import is_web_container

        if is_web_container():
            raise HTTPException(
                status_code=409,
                detail="import-from-host is unavailable when the web control plane runs in a container",
            )
        body = await _require_dict_body(request)
        engine = str(body.get("engine") or "").strip().lower()
        if engine not in {"claude", "kimi", "grok"}:
            raise HTTPException(status_code=400, detail="engine must be claude, kimi, or grok")
        store = CredentialAccountStore(account_store_root(app.state.manager.sessions_root))
        try:
            account = await asyncio.to_thread(store.import_host_login, account_id, engine)
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
            lambda: {
                e: detect_system_login(e)
                for e in (
                    "claude", "codex", "cursor", "pi", "omp", "kimi", "grok",
                    "opencode", "dsh",
                )
            }
        )
        return {"logins": logins}

    @app.post("/api/settings/llm/test")
    async def test_llm_endpoint_route(request: Request) -> Any:
        # Test the planner/titler endpoint the operator is editing. A freshly
        # entered key takes precedence over the saved profile key and env fallback.
        body = await _require_dict_body(request)
        from apps.web.llm_credentials import LlmCredentialStore
        from apps.web.llm_test import test_llm_endpoint

        which = str(body.get("which") or "planner")
        entered_key = str(body.get("api_key") or "").strip()
        try:
            api_key = entered_key or LlmCredentialStore(app.state.manager.sessions_root).resolve(which)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        return await test_llm_endpoint(
            which=which,
            base_url=(body.get("base_url") if body.get("base_url") is not None else None),
            model=(body.get("model") if body.get("model") is not None else None),
            api_key=api_key,
            temperature_mode=body.get("temperature_mode"),
            temperature=body.get("temperature"),
        )

    @app.post("/api/runs")
    async def new_run(request: Request) -> Any:
        # Mint a fresh run id for a new conversation ("+ New solve"). The deck
        # then opens this run's SSE and POSTs /start with the dispatch prompt.
        run = app.state.manager.create_new()
        return {"run_id": run.run_id}

    @app.get("/api/protocol2/status")
    async def protocol2_status() -> Any:
        adapter = app.state.manager.protocol2
        if adapter is None:
            return {
                "protocol_version": 2,
                "available": False,
                "production_enabled": False,
                "reason": app.state.manager.protocol2_error or "unavailable",
            }
        return adapter.status()

    @app.get("/api/protocol2/runs/{run_id}/status")
    async def protocol2_run_status(run_id: str) -> Any:
        adapter = app.state.manager.protocol2
        if adapter is None:
            raise HTTPException(status_code=503, detail="Protocol 2 unavailable")
        try:
            return adapter.canonical_run_status(run_id)
        except KeyError as exc:
            raise HTTPException(status_code=404, detail="unknown Protocol 2 run") from exc

    @app.post("/api/runs/{run_id}/start")
    async def start_run(run_id: str, request: Request) -> Any:
        body = await _require_dict_body(request)
        from apps.web.drivers import build_driver

        driver = build_driver(body, mgr=app.state.manager)
        from muteki.core.path_ids import RunIdPathError
        try:
            run = app.state.manager.get(run_id) or app.state.manager.create(run_id)
        except RunIdPathError as exc:
            raise HTTPException(status_code=422, detail=str(exc)) from exc
        try:
            run = await app.state.manager.start(run_id, driver)
        except RunIdPathError as exc:
            raise HTTPException(status_code=422, detail=str(exc)) from exc
        except StateConflict as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc

        # Mutate rail metadata only after lifecycle admission succeeds. A duplicate
        # live start must be a pure 409 and may not erase the active generation's
        # solved/flag/name projection before the manager rejects it.
        ch = (body.get("challenge") or {})
        if ch.get("name"):
            run.name = ch["name"]
        if ch.get("category"):
            run.category = ch["category"]

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
                from apps.web.llm_credentials import LlmCredentialStore
                title_api_key = LlmCredentialStore(
                    app.state.manager.sessions_root).resolve("titler")
                title_generation = run.execution_generation
                title_bus = run.bus

                async def _generate_owned_title() -> None:
                    current = asyncio.current_task()
                    try:
                        title = await generate_title(
                        prompt, bus=None, run_id=None,
                        model=title_model, base_url=title_base_url,
                        api_key=title_api_key,
                        temperature_mode=titler_profile.get("temperature_mode"),
                        temperature=titler_profile.get("temperature"),
                        )
                        if (run.execution_generation == title_generation
                                and run.bus is title_bus and not run.finished
                                and title):
                            await title_bus.emit(Event(
                                event_type=EventType.RUN_TITLED,
                                run_id=run_id,
                                payload={
                                    "title": title,
                                    "execution_generation": title_generation,
                                },
                            ))
                    finally:
                        if run.title_task is current:
                            run.title_task = None

                run.title_task = asyncio.create_task(_generate_owned_title())

        return {"run_id": run_id, "started": True, "kind": body.get("kind", "swarm")}

    @app.post("/api/dispatch/parse")
    async def dispatch_parse_preflight(request: Request) -> Any:
        """Preflight LLM parse of a dispatch prompt, so the deck can warn BEFORE
        launch when neither the count field nor the prompt carries a collect
        quota (open-ended collect may run until the operator stops it).
        Never raises: ``parsed`` is {} when the planner LLM is unavailable or
        cannot decide — the caller falls back to its own heuristics."""
        body = await _require_dict_body(request)
        prompt = str(body.get("prompt") or "")[:4000]
        goal = str(body.get("goal") or "")
        mode = str(body.get("mode") or "ctf")
        if mode not in ("ctf", "pentest"):
            mode = "ctf"
        mgr = app.state.manager
        try:
            llm_profiles = dict(mgr.worker_config.get().get("llm_profiles") or {})
        except Exception:
            llm_profiles = {}
        planner_profile = llm_profiles.get("planner") or {}
        planner_model = str(planner_profile.get("model") or "deepseek-v4-pro")
        from apps.web.llm_credentials import LlmCredentialStore
        from muteki.core.llm import LLMClient, llm_temperature_kwargs

        llm_kwargs: dict[str, Any] = dict(
            llm_temperature_kwargs(planner_profile))
        planner_base = str(planner_profile.get("base_url") or "").strip()
        if planner_base:
            llm_kwargs["base_url"] = planner_base
        planner_key = LlmCredentialStore(mgr.sessions_root).resolve("planner")
        if planner_key:
            llm_kwargs["api_key"] = planner_key
        parsed: dict[str, Any] = {}
        try:
            from apps.web.dispatch_parse import parse_dispatch

            async with LLMClient(**llm_kwargs) as llm:
                parsed = await asyncio.wait_for(
                    parse_dispatch(
                        prompt, goal, mode, llm=llm, model=planner_model),
                    timeout=15.0,
                )
        except Exception:
            parsed = {}
        return {"parsed": parsed or {}}

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
        from muteki.core.path_ids import RunIdPathError
        try:
            mgr.get(run_id) or mgr.create(run_id)
        except RunIdPathError as exc:
            raise HTTPException(status_code=422, detail=str(exc)) from exc
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
                if ev.event_type in (EventType.RUN_PREPARING,
                                     EventType.RUN_STARTED,
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
            if (fresh and not live
                    and manager.is_protocol1_run(run_id, run=run)
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

    @app.post("/api/runs/{run_id}/control")
    async def control(run_id: str, request: Request) -> Any:
        """Persist an idempotent command; effects arrive later over SSE."""
        body = await _require_dict_body(request)
        if app.state.manager.get(run_id) is None:
            raise HTTPException(status_code=404, detail="unknown run")
        try:
            result = await app.state.manager.post_control(run_id, body)
        except (ControlPayloadError, PydanticValidationError) as exc:
            raise HTTPException(status_code=422, detail=str(exc)) from exc
        except (IdempotencyConflict, StateConflict) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc
        if result.get("status") == "rejected":
            code = str(result.get("code") or "")
            status = 409 if code == "generation_conflict" else 422
            return JSONResponse(result, status_code=status)
        return result

    @app.get("/api/runs/{run_id}/control/{command_id}")
    async def control_receipt(run_id: str, command_id: str) -> Any:
        """Reconcile a command whose SSE terminal projection was interrupted."""
        if app.state.manager.get(run_id) is None:
            raise HTTPException(status_code=404, detail="unknown run")
        if not app.state.manager.is_protocol1_run(run_id):
            raise HTTPException(
                status_code=409, detail="PROTOCOL2_CONTROL_UNAVAILABLE")
        receipt = app.state.manager.control_receipt(run_id, command_id)
        if receipt is None:
            raise HTTPException(status_code=404, detail="unknown control command")
        return receipt

    @app.post("/api/runs/{run_id}/hitl")
    async def hitl(run_id: str, request: Request) -> Any:
        body = await _require_dict_body(request)
        try:
            ok = await app.state.manager.post_hitl(
                run_id,
                body.get("target", "global"),
                body.get("action", "hint"),
                **{k: v for k, v in body.items() if k not in ("target", "action")},
            )
        except (ControlPayloadError, PydanticValidationError) as exc:
            raise HTTPException(status_code=422, detail=str(exc)) from exc
        except (IdempotencyConflict, StateConflict) as exc:
            raise HTTPException(status_code=409, detail=str(exc)) from exc
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
