"""runs routes moved from server.py."""
from __future__ import annotations

import asyncio
import json
import os
import shutil
import sqlite3
import time
from pathlib import Path
from typing import Any

from fastapi import (
    FastAPI,
    File,
    HTTPException,
    Request,
    UploadFile,
)
from fastapi.responses import JSONResponse
from sse_starlette.sse import EventSourceResponse

from apps.web.run_manager import RunManager
from apps.web.routes.common import (
    MAX_UPLOAD_BYTES,
    MAX_UPLOAD_FILES,
    _env_int,
    _require_dict_body,
)
from muteki.solver.engine_registry import (
    SUPPORTED_ENGINE_IDS,
)
from muteki.solver.credential_accounts import (
    account_store_root,
)

def register(app: FastAPI) -> None:
    h = app.state.route_helpers
    _reject_temporarily_disabled_engine = h._reject_temporarily_disabled_engine
    _dispatch_run_command = h._dispatch_run_command
    _run_receipt_status = h._run_receipt_status

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
        body["now"] = time.time()
        receipt = await _dispatch_run_command("run.rail.update", run_id, body)
        if receipt.error is not None:
            return JSONResponse(
                {"detail": receipt.error.message,
                 "error": receipt.error.model_dump(mode="json")},
                status_code=_run_receipt_status(receipt))
        return receipt.output

    @app.get("/api/folders")
    async def list_folders() -> Any:
        return {"folders": app.state.manager.list_folders()}

    @app.post("/api/folders")
    async def create_folder(request: Request) -> Any:
        body = await _require_dict_body(request)
        receipt = await _dispatch_run_command(
            "run.folder.create", "folders", {"name": body.get("name", "")})
        if receipt.error is not None:
            return JSONResponse(
                {"detail": receipt.error.message},
                status_code=_run_receipt_status(receipt))
        return {"folder": receipt.output.get("folder")}

    @app.patch("/api/folders/{folder_id}")
    async def update_folder(folder_id: str, request: Request) -> Any:
        body = await _require_dict_body(request)
        receipt = await _dispatch_run_command(
            "run.folder.update", folder_id, body)
        return receipt.output if receipt.error is None else {"ok": False}

    @app.delete("/api/folders/{folder_id}")
    async def delete_folder(folder_id: str) -> Any:
        receipt = await _dispatch_run_command(
            "run.folder.delete", folder_id, {})
        return receipt.output if receipt.error is None else {"ok": False}

    @app.delete("/api/runs/{run_id}")
    async def delete_run(run_id: str) -> Any:
        # Hard-delete: cancels the task, drops the in-memory handle, the JSONL
        # log, and the meta row. Irreversible — the UI confirms before calling.
        receipt = await _dispatch_run_command("run.rail.delete", run_id, {})
        if receipt.error is not None:
            raise HTTPException(
                status_code=_run_receipt_status(receipt),
                detail=receipt.error.code)
        return receipt.output

    @app.post("/api/runs/{run_id}/open")
    async def open_run_workspace(run_id: str) -> Any:
        # Reveal the run's workspace dir in the host file manager. Only meaningful
        # when the operator runs the backend locally; a no-op (ok:false) otherwise.
        receipt = await _dispatch_run_command("run.open", run_id, {})
        if receipt.error is not None:
            raise HTTPException(
                status_code=_run_receipt_status(receipt),
                detail=receipt.error.code)
        return receipt.output

    @app.get("/api/runs/{run_id}/credentials")
    async def run_credentials(run_id: str) -> Any:
        from muteki.models.solve_graph import Challenge
        from muteki.swarm.shared_graph import SQLiteSharedGraph

        mgr: RunManager = app.state.manager
        run = mgr.get(run_id)
        if run is None:
            raise HTTPException(status_code=404, detail="unknown run")
        graph_db = mgr.graph_dir(run_id) / "shared_graph.db"
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
        from muteki.solver.worker_profiles import base_engine_for_profile

        body = await _require_dict_body(request)
        _reject_temporarily_disabled_engine(body)
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
        safe = mgr._safe_run_id(run_id)
        root = mgr.workspace_dir(run_id).resolve()
        graph_db = mgr.graph_dir(run_id) / "shared_graph.db"
        jsonl_path = (mgr.event_root / f"{safe}.jsonl").resolve()
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
                if base in SUPPORTED_ENGINE_IDS:
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
            btw_container_id = f"{run_id}:btw"
            account_root = account_store_root(mgr.state_root)
            worker_root = root / "workers" / "_btw"
            worker_root.mkdir(parents=True, exist_ok=True)
            workdir = worker_root / f"{transport}-{int(time.time() * 1000)}"
            workdir.mkdir(parents=True, exist_ok=True)
            visible_jsonl = jsonl_path
            visible_graph = graph_db
            try:
                if backend == "container":
                    from muteki.solver.container_exec import (
                        _chown_tree_to_worker,
                        ensure_container,
                    )

                    # Event and graph authority remain outside the worker-mounted
                    # sessions tree. Give the read-only side worker point-in-time
                    # copies inside its own scratch directory instead of mounting
                    # coordinator state or silently mapping an inaccessible path.
                    context_dir = workdir / "context"
                    context_dir.mkdir(parents=True, exist_ok=True)
                    visible_jsonl = context_dir / "events.jsonl"
                    if jsonl_path.is_file():
                        shutil.copyfile(jsonl_path, visible_jsonl)
                    visible_graph = context_dir / "shared_graph.db"
                    if graph_db.is_file():
                        source_db = sqlite3.connect(
                            f"{graph_db.resolve().as_uri()}?mode=ro", uri=True
                        )
                        target_db = sqlite3.connect(visible_graph)
                        try:
                            source_db.backup(target_db)
                        finally:
                            target_db.close()
                            source_db.close()

                    from muteki.solver.credential_accounts import engine_account_id
                    from muteki.solver.worker_resource_limits import resolve_worker_resource_limits
                    limits = resolve_worker_resource_limits(config=wc)
                    container = await asyncio.to_thread(
                        ensure_container,
                        btw_container_id,
                        str(root.parent),
                        network=worker_network,
                        memory=limits.memory, cpus=limits.cpus, pids_limit=limits.pids_limit,
                        output_limit=limits.output_limit, disk_limit=limits.disk_limit,
                        account_root=str(account_root),
                        account_ids=[str((profile or {}).get("credential_account") or engine_account_id(transport))],
                        worker_privilege=str(wc.get("worker_privilege") or "default"),
                        account_projection_root=str(mgr.account_projection_root),
                        bootstrap_root=str(mgr.container_bootstrap_root),
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
                        jsonl=_worker_path(visible_jsonl),
                        graph_db=_worker_path(visible_graph),
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
                from muteki.core.usage import cli_usage
                usage_generation = run.execution_generation
                def record_btw_usage(result):
                    mgr.usage.record(cli_usage(result), identity=result.usage_id,
                                     run_id=run_id, generation=usage_generation,
                                     workspace_kind="single-security-task", actor_kind="auxiliary",
                                     role="btw", model=(profile or {}).get("model"), engine=transport)
                async for chunk in stream_btw_worker_deltas(
                    driver=driver_for(profile or transport),
                    usage_callback=record_btw_usage,
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
                if container is not None:
                    from muteki.solver.container_exec import teardown_container
                    try:
                        removed = await asyncio.to_thread(
                            teardown_container, btw_container_id, remove=True)
                        if removed is not True:
                            import logging
                            logging.getLogger(__name__).warning(
                                "BTW container teardown could not be proven for %s",
                                btw_container_id,
                            )
                    except Exception:
                        import logging
                        logging.getLogger(__name__).exception(
                            "BTW container teardown failed for %s", btw_container_id)
                this_task = asyncio.current_task()
                if this_task is not None:
                    limiter.release(run_id, this_task)
            yield {"data": json.dumps({"done": True}, ensure_ascii=False)}

        return EventSourceResponse(stream(), ping=10)

    @app.post("/api/runs")
    async def new_run(request: Request) -> Any:
        # Mint a fresh run id for a new conversation ("+ New solve"). The deck
        # then opens this run's SSE and POSTs /start with the dispatch prompt.
        receipt = await _dispatch_run_command(
            "run.create", "", {"legacy": True})
        if receipt.error is not None or not receipt.run_id:
            return JSONResponse(
                {"error": receipt.error.model_dump(mode="json")
                 if receipt.error else {"code": "run.create.failed"}},
                status_code=_run_receipt_status(receipt),
            )
        return {"run_id": receipt.run_id,
                "receipt": receipt.model_dump(mode="json")}

    @app.post("/api/runs/{run_id}/start")
    async def start_run(run_id: str, request: Request) -> Any:
        body = await _require_dict_body(request)
        if str(body.get("kind") or "swarm") not in {"swarm", "idle"}:
            raise HTTPException(status_code=422, detail="unsupported run kind")
        _reject_temporarily_disabled_engine(body)
        if str(body.get("kind") or "swarm") == "swarm":
            from apps.web.task_contract import prepare_dispatch_contract
            from muteki.solver.gate import FlagFormatError

            try:
                body, _contract = prepare_dispatch_contract(body)
            except (FlagFormatError, ValueError) as exc:
                raise HTTPException(status_code=422, detail=str(exc)) from exc
        ensured = await _dispatch_run_command(
            "run.create", run_id, {"legacy": True, "run_id": run_id})
        if ensured.error is not None:
            return JSONResponse(
                {"detail": ensured.error.message,
                 "error": ensured.error.model_dump(mode="json")},
                status_code=_run_receipt_status(ensured),
            )
        command_id = str(body.pop("command_id", "") or "")
        idempotency_key = str(body.pop("idempotency_key", "") or "")
        receipt = await _dispatch_run_command(
            "run.start", run_id, body,
            command_id=command_id, idempotency_key=idempotency_key)
        if receipt.error is not None:
            return JSONResponse(
                {"detail": receipt.error.message,
                 "error": receipt.error.model_dump(mode="json")},
                status_code=_run_receipt_status(receipt),
            )
        return {"run_id": run_id, "started": True,
                "kind": body.get("kind", "swarm"),
                "receipt": receipt.model_dump(mode="json")}

    @app.post("/api/dispatch/parse")
    async def dispatch_parse_preflight(request: Request) -> Any:
        """Return the same canonical contract used by ``/start`` without launching."""
        body = await _require_dict_body(request)
        from apps.web.task_contract import prepare_dispatch_contract
        from muteki.solver.gate import FlagFormatError

        try:
            _prepared, contract = prepare_dispatch_contract(body)
        except (FlagFormatError, ValueError) as exc:
            raise HTTPException(status_code=422, detail=str(exc)) from exc
        return {
            "parsed": {
                "name": contract.title,
                "category": contract.category,
            },
            "task_contract": contract.model_dump(mode="json"),
        }

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
        ensured = await _dispatch_run_command(
            "run.create", run_id, {"legacy": True, "run_id": run_id})
        if ensured.error is not None:
            raise HTTPException(
                status_code=_run_receipt_status(ensured),
                detail=ensured.error.message)
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
        attached = await _dispatch_run_command(
            "run.artifact.attach", run_id, {"files": saved})
        if attached.error is not None:
            return JSONResponse(
                {"detail": attached.error.message,
                 "error": attached.error.model_dump(mode="json")},
                status_code=_run_receipt_status(attached))
        return {"files": attached.output.get("files", saved)}
