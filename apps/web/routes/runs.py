"""runs routes moved from server.py."""
from __future__ import annotations

import asyncio
import hashlib
import html
import io
import json
import os
import re
import shutil
import sqlite3
import time
import zipfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from fastapi import (
    FastAPI,
    File,
    HTTPException,
    Request,
    UploadFile,
)
from fastapi.responses import JSONResponse, PlainTextResponse, Response
from pydantic import BaseModel, ConfigDict, Field, StrictBool, StrictInt, model_validator
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


class RunRetentionPolicyBody(BaseModel):
    model_config = ConfigDict(extra="forbid")

    archive_enabled: StrictBool
    archive_after_days: StrictInt = Field(ge=1, le=3650)
    delete_enabled: StrictBool
    delete_after_days: StrictInt = Field(ge=1, le=3650)

    @model_validator(mode="after")
    def validate_order(self) -> "RunRetentionPolicyBody":
        if (self.archive_enabled and self.delete_enabled
                and self.delete_after_days <= self.archive_after_days):
            raise ValueError(
                "delete_after_days must be greater than archive_after_days")
        return self

def register(app: FastAPI) -> None:
    h = app.state.route_helpers
    _reject_temporarily_disabled_engine = h._reject_temporarily_disabled_engine
    _dispatch_run_command = h._dispatch_run_command
    _run_receipt_status = h._run_receipt_status

    @app.get("/api/settings/run-retention")
    async def get_run_retention_settings() -> Any:
        return {"policies": app.state.manager.retention_policies.all()}

    @app.put("/api/settings/run-retention/{mode}")
    async def put_run_retention_settings(
        mode: str, policy: RunRetentionPolicyBody,
    ) -> Any:
        if mode not in {"ctf", "pentest"}:
            raise HTTPException(status_code=404, detail="unknown run mode")
        saved = app.state.manager.retention_policies.set(
            mode, policy.model_dump())
        return {"policy": saved}

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

    def _pentest_report_source(run_id: str) -> tuple[Any, Path, list[dict[str, Any]], Any]:
        from muteki.pentest.contract import PentestContract

        mgr: RunManager = app.state.manager
        run = mgr.get(run_id)
        if run is None:
            raise HTTPException(status_code=404, detail="unknown run")
        path = mgr.graph_dir(run_id) / "shared_graph.db"
        if not path.is_file():
            raise HTTPException(status_code=404, detail="pentest graph unavailable")
        connection = sqlite3.connect(f"{path.resolve().as_uri()}?mode=ro", uri=True)
        try:
            rows = connection.execute(
                "SELECT seq,ts,actor,kind,payload,verified FROM events "
                "WHERE challenge_id=? ORDER BY seq", (run_id,),
            ).fetchall()
        finally:
            connection.close()
        events = []
        contract = None
        for seq, ts, actor, kind, raw, verified in rows:
            try:
                payload = json.loads(raw or "{}")
            except (ValueError, TypeError):
                payload = {}
            if kind == "pentest_contract":
                contract = PentestContract.model_validate(payload)
            events.append({
                "seq": seq, "ts": ts, "actor": actor, "kind": kind,
                "payload": payload, "verified": bool(verified),
            })
        if contract is None:
            raise HTTPException(status_code=409, detail="run is not a Pentest engagement")
        return run, path, events, contract

    def _redact_known_report_secrets(run_id: str, run: Any, view: dict[str, Any]) -> None:
        """Remove exact run SecretStore values from reader-facing report prose.

        Stable evidence IDs and URLs remain intact so cited originals can still
        be checked through the authenticated evidence endpoints.
        """
        from apps.web.control_adapter import control_paths
        from muteki.control.secrets import SecretStore

        store = getattr(run, "control_secrets", None)
        if store is None:
            path = control_paths(app.state.manager.coordinator_control_dir(run_id))[1]
            if not path.is_dir():
                return
            store = SecretStore(path)
        values = sorted({store.resolve(item.reference) for item in store.list()} - {""},
                        key=len, reverse=True)
        if not values:
            return
        stable_keys = {"id", "poc_id", "artifact_id", "sha256", "filename", "url", "schema",
                       "finding_class", "review_status"}

        def walk(value: Any, key: str = "") -> Any:
            if isinstance(value, str):
                if key in stable_keys:
                    return value
                for secret in values:
                    value = value.replace(secret, "[已隐去]")
                return value
            if isinstance(value, list):
                return [walk(item) for item in value]
            if isinstance(value, dict):
                return {name: walk(item, name) for name, item in value.items()}
            return value

        view.update(walk(view))

    @app.get("/api/runs/{run_id}/pentest-report")
    async def pentest_report(run_id: str, report_seq: int | None = None) -> Any:
        """Read one immutable report version and the version list from the graph."""
        from muteki.pentest.judgement import report

        run, _path, events, contract = _pentest_report_source(run_id)
        terminal_reason = str(
            run.terminal_reason
            or run.termination_reasons.get(run.execution_generation)
            or (run.status() if run.finished else "")
        )
        try:
            view = report(events, contract, terminal_reason=terminal_reason,
                          report_seq=report_seq)
        except LookupError as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        from muteki.pentest.judgement import _evidenced_facts, contract_at
        evidence_contract = contract_at(events, contract, view["as_of_seq"])
        facts = _evidenced_facts(
            (event for event in events if event["seq"] <= view["as_of_seq"]), evidence_contract)
        selected_seq = view.get("report_seq")
        version_query = f"?report_seq={selected_seq}" if selected_seq is not None else ""
        image_suffixes = {".png", ".jpg", ".jpeg", ".webp", ".gif"}
        text_suffixes = {".txt", ".log", ".http", ".json", ".md"}
        artifact_root = app.state.manager.workspace_dir(run_id) / "arts"
        for fact in view.get("evidence_facts") or []:
            for evidence in fact.get("evidence") or []:
                seq = int(evidence.get("fact_seq") or 0)
                provenance = ((facts.get(seq) or {}).get("payload") or {}).get("evidence_provenance") or {}
                primary_id = str(provenance.get("artifact_id") or "")
                if (len(primary_id) == 12 and all(c in "0123456789abcdef" for c in primary_id)):
                    primary_matches = list(artifact_root.glob(f"{primary_id}.*"))
                    if len(primary_matches) == 1 and primary_matches[0].suffix.lower() in text_suffixes:
                        evidence["provenance"]["artifact_filename"] = primary_matches[0].name
                screenshots = []
                for ref in provenance.get("artifact_refs") or []:
                    if not isinstance(ref, dict):
                        continue
                    aid = str(ref.get("artifact_id") or "")
                    if len(aid) != 12 or any(c not in "0123456789abcdef" for c in aid):
                        continue
                    matches = list(artifact_root.glob(f"{aid}.*"))
                    if len(matches) == 1 and matches[0].suffix.lower() in image_suffixes:
                        screenshots.append({
                            "artifact_id": aid,
                            "url": f"/api/runs/{run_id}/pentest-evidence/{seq}/artifacts/{aid}/view{version_query}",
                            "filename": f"{aid}{matches[0].suffix.lower()}",
                        })
                evidence["screenshots"] = screenshots
        for finding in view.get("findings") or []:
            for note in finding.get("evidence_notes") or []:
                if not isinstance(note, dict):
                    continue
                note.pop("artifact_filename", None)
                seq = note.get("fact_seq")
                aid = str(note.get("artifact_id") or "")
                if type(seq) is not int or len(aid) != 12 or any(c not in "0123456789abcdef" for c in aid):
                    continue
                selected = next((item for item in finding.get("selected_artifacts") or []
                                 if item.get("fact_seq") == seq and item.get("artifact_id") == aid), None)
                if selected is not None:
                    note["artifact_filename"] = f"{aid}.txt"
                    continue
                provenance = ((facts.get(seq) or {}).get("payload") or {}).get("evidence_provenance") or {}
                if aid not in {str(ref.get("artifact_id") or "")
                               for ref in provenance.get("artifact_refs") or [] if isinstance(ref, dict)}:
                    continue
                matches = list(artifact_root.glob(f"{aid}.*"))
                if len(matches) == 1 and matches[0].suffix.lower() in text_suffixes:
                    note["artifact_filename"] = matches[0].name
            for screenshot in finding.get("screenshots") or []:
                poc_id = str(screenshot.get("poc_id") or "")
                if poc_id:
                    screenshot["url"] = f"/api/runs/{run_id}/pentest-report-images/{poc_id}{version_query}"
        _redact_known_report_secrets(run_id, run, view)
        return view

    def _pentest_export_findings(view: dict[str, Any]) -> list[tuple[dict[str, Any], dict[str, Any]]]:
        """Pair accepted source submissions with optional model-written reader prose."""
        sources = [item for item in view.get("findings") or []
                   if view.get("contract_version", 1) < 2 or item.get("review_status") == "accepted"]
        reader_rows = view.get("reader_findings")
        if reader_rows is None:
            return [(item, item) for item in sources]  # historical version or draft
        if (not isinstance(reader_rows, list) or len(reader_rows) != len(sources)
                or any(not isinstance(item, dict) or not isinstance(item.get("id"), str)
                       for item in reader_rows)):
            raise HTTPException(status_code=409, detail="reader report does not match accepted findings")
        by_id = {item.get("id"): item for item in reader_rows}
        if len(by_id) != len(sources) or set(by_id) != {item.get("id") for item in sources}:
            raise HTTPException(status_code=409, detail="reader report does not match accepted findings")
        return [(item, by_id[item["id"]]) for item in sources]

    def _pentest_markdown(
        view: dict[str, Any], *, controlled_bundle: bool = False,
        attachment_paths: set[str] | None = None,
    ) -> str:
        """Render a reader report while keeping the platform's review data separate."""
        def safe(value: Any) -> str:
            prose = re.sub(r"\bFact\s*#?\s*\d+\b", "原始证据", str(value or ""), flags=re.I)
            escaped = html.escape(prose, quote=False)
            escaped = re.sub(r"([\\`*_{}\[\]#|~])", r"\\\1", escaped)
            escaped = re.sub(r"(?m)^([ \t]*)([-+])(?=\s)", r"\1\\\2", escaped)
            return re.sub(r"(?m)^([ \t]*\d+)([.)])(?=\s)", r"\1\\\2", escaped)

        severity_labels = {
            "critical": "严重", "high": "高", "medium": "中", "low": "低",
            "informational": "提示", "unrated": "未评定",
        }
        severity_rank = {"critical": 0, "high": 1, "medium": 2, "low": 3,
                         "informational": 4, "unrated": 5}
        findings = sorted(
            _pentest_export_findings(view),
            key=lambda row: severity_rank.get(str(row[0].get("severity")), 5),
        )
        generated = view.get("delivery_status") == "generated"
        lines = [f"# {safe(view['title'])}", ""]
        if not generated:
            lines += ["**报告草稿｜尚未正式生成**", "",
                      "以下仅列出当前已有的可用发现；结论与文字整理仍待正式报告生成。", ""]
        elif view.get("reader_findings") is None and findings:
            lines += ["**历史报告说明：以下发现沿用当时的报告文字，未经过本次读者版改写。**", ""]
        if view.get("historical_snapshot"):
            lines += ["本报告反映生成时的测试结果，后续测试记录可能已有变化。", ""]
        if view.get("stale_after_report_seq"):
            lines += ["测试证据或发现已有更新，正式报告尚待重新生成。", ""]
        # The objective is the operator's full dispatch instruction. It may
        # contain Worker, Run, and tooling directions that have no place in a
        # document delivered to a reader. The authorized scope is the stable
        # target identity for this report.
        lines += [f"评估对象：{safe('、'.join(view['authorization']['scope']))}", ""]
        if generated:
            selected_version = next(
                (version for version in view.get("report_versions") or []
                 if version.get("seq") == view.get("report_seq")), None,
            )
            if selected_version and type(selected_version.get("created_at")) in (int, float):
                issued_at = datetime.fromtimestamp(selected_version["created_at"], timezone.utc)
                lines += [f"报告日期：{issued_at:%Y-%m-%d %H:%M} UTC", ""]
        if controlled_bundle:
            lines += ["本压缩包附有原始请求、响应与截图；附件可能包含敏感内容，请按授权范围保管。", ""]
        else:
            lines += ["原始请求、响应与截图可在同一运行的受控原始证据包中查看。", ""]
        if view.get("executive_summary") and generated:
            lines += ["## 结论", "", safe(view["executive_summary"]), ""]
        if findings:
            lines += ["## 发现一览", "",
                      "按报告风险级别由高到低列出，便于确定优先排查范围。", "",
                      "| 序号 | 风险级别 | 发现 | 受影响资源 |",
                      "| --- | --- | --- | --- |"]
            for index, (source, reader) in enumerate(findings, 1):
                lines.append(
                    f"| {index} | {severity_labels.get(str(source.get('severity') or ''), '未评定')} | "
                    f"{safe(reader.get('title') or source.get('title'))} | "
                    f"{safe('、'.join(source.get('affected_assets') or []))} |"
                )
            lines.append("")
        if view.get("methodology") and generated:
            lines += ["## 测试方法", "", safe(view["methodology"]), ""]
        if view.get("limitations") and generated:
            lines += ["## 范围与限制", ""]
            lines.extend(f"- {safe(item)}" for item in view["limitations"])
            lines.append("")
        lines += ["## 发现与建议", ""]
        if not findings:
            lines += ["本报告暂无可列出的独立漏洞发现。", ""]
        evidence_by_seq = {
            evidence.get("fact_seq"): evidence
            for fact in view.get("evidence_facts") or []
            for evidence in fact.get("evidence") or []
        }
        for index, (source, reader) in enumerate(findings, 1):
            title = reader.get("title") or source.get("title")
            severity = severity_labels.get(str(source.get("severity") or ""), "未评定")
            lines += [f"### {index}. {safe(title)}", "",
                      f"风险级别：{severity}",
                      f"受影响资源：{safe('、'.join(source.get('affected_assets') or []))}", ""]
            if reader.get("severity_rationale"):
                lines += ["**风险依据**", "", safe(reader["severity_rationale"]), ""]
            if reader.get("summary"):
                lines += [safe(reader["summary"]), ""]
            if reader.get("preconditions"):
                lines += ["**前置条件**", "", safe(reader["preconditions"]), ""]
            if reader.get("observed_impact"):
                lines += ["**已观察到的影响**", "", safe(reader["observed_impact"]), ""]
            if reader.get("potential_impact"):
                lines += ["**潜在影响（本次未直接验证）**", "", safe(reader["potential_impact"]), ""]
            lines += ["**复现方法**", ""]
            lines.extend(f"{number}. {safe(step)}" for number, step in enumerate(reader.get("reproduction_steps") or [], 1))
            lines += ["", "**修复建议**", "", safe(reader.get("remediation")), "",
                      "**复测方法**", ""]
            lines.extend(f"{number}. {safe(step)}" for number, step in enumerate(reader.get("retest_steps") or [], 1))
            lines += ["", "**证据摘要**", ""]
            shown_attachments: set[str] = set()
            reader_summaries = {row.get("fact_seq"): row.get("summary")
                                for row in reader.get("evidence_summaries") or []
                                if isinstance(row, dict)}
            for seq in source.get("evidence_fact_seqs") or []:
                note = next((row for row in source.get("evidence_notes") or []
                             if row.get("fact_seq") == seq), None)
                evidence = evidence_by_seq.get(seq) or {}
                summary = reader_summaries.get(seq)
                if summary:
                    lines.append(f"- {safe(summary)}")
                elif note:
                    lines.append(f"- {safe(note.get('observed'))} {safe(note.get('significance'))}")
                elif evidence:
                    fact = next((item for item in view.get("evidence_facts") or []
                                 if any(row.get("fact_seq") == seq for row in item.get("evidence") or [])), {})
                    lines.append(f"- {safe(fact.get('title'))}")
                filename = (note or {}).get("artifact_filename") or (
                    evidence.get("provenance") or {}).get("artifact_filename")
                if filename:
                    destination = f"evidence/{filename}"
                    if (controlled_bundle and destination in (attachment_paths or set())
                            and destination not in shown_attachments):
                        lines.append(f"  - [查看原始请求与响应](<{destination}>)")
                        shown_attachments.add(destination)
                for shot in evidence.get("screenshots") or []:
                    destination = f"images/{shot['filename']}"
                    if (controlled_bundle and destination in (attachment_paths or set())
                            and destination not in shown_attachments):
                        lines += ["", f"![测试截图](<{destination}>)", ""]
                        shown_attachments.add(destination)
            for shot in source.get("screenshots") or []:
                destination = f"images/{shot['artifact_id']}{Path(shot['name']).suffix.lower()}"
                if (controlled_bundle and destination in (attachment_paths or set())
                        and destination not in shown_attachments):
                    lines += ["", f"![测试截图](<{destination}>)", ""]
                    shown_attachments.add(destination)
            lines.append("")
        if generated:
            for section in view.get("custom_sections") or []:
                lines += [f"## {safe(section['title'])}", "", safe(section["body"]), ""]
        return "\n".join(lines)

    @app.get("/api/runs/{run_id}/pentest-report.md")
    async def pentest_report_markdown(run_id: str, report_seq: int | None = None) -> Any:
        """Export the selected report version without rewriting its evidence."""
        view = await pentest_report(run_id, report_seq=report_seq)
        filename_suffix = f"-v{view['report_seq']}" if view.get("report_seq") else ""
        return PlainTextResponse(
            _pentest_markdown(view), media_type="text/markdown; charset=utf-8",
            headers={"Content-Disposition": f'attachment; filename="pentest-{run_id}{filename_suffix}.md"',
                     "Cache-Control": "no-store"},
        )

    @app.get("/api/runs/{run_id}/pentest-report.zip")
    async def pentest_report_bundle(run_id: str, report_seq: int | None = None) -> Any:
        view = await pentest_report(run_id, report_seq=report_seq)
        selected_seq = view.get("report_seq")
        filename_suffix = f"-v{selected_seq}" if selected_seq else ""
        output = io.BytesIO()
        attachments: dict[str, dict[str, Any]] = {}
        export_sources = [source for source, _reader in _pentest_export_findings(view)]
        with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            def add_file(path: Path, destination: str, *, digest: str, size: int,
                         kind: str, fact_seq: int | None = None) -> None:
                # Freeze the exact bytes that were checked. A Worker can still
                # write its source workspace while this archive is assembled.
                content = path.read_bytes()
                if len(content) != size or hashlib.sha256(content).hexdigest() != digest:
                    raise HTTPException(status_code=409, detail="evidence changed during export")
                prior = attachments.get(destination)
                if prior is not None:
                    if prior["sha256"] != digest:
                        raise HTTPException(status_code=409, detail="evidence filename digest conflict")
                    if fact_seq is not None and fact_seq not in prior.setdefault("fact_seqs", []):
                        prior["fact_seqs"].append(fact_seq)
                    return
                archive.writestr(destination, content)
                entry: dict[str, Any] = {
                    "path": destination, "sha256": digest,
                    "size": size, "kind": kind,
                }
                if fact_seq is not None:
                    entry["fact_seqs"] = [fact_seq]
                attachments[destination] = entry

            facts_by_seq = {
                int(evidence["fact_seq"]): evidence
                for fact in view.get("evidence_facts") or []
                for evidence in fact.get("evidence") or []
                if type(evidence.get("fact_seq")) is int
            }
            private_root = app.state.manager.graph_dir(run_id) / "review-artifacts"
            for finding in export_sources:
                selected = {item["fact_seq"]: item for item in finding.get("selected_artifacts") or []
                            if isinstance(item, dict) and type(item.get("fact_seq")) is int}
                notes = {item["fact_seq"]: item for item in finding.get("evidence_notes") or []
                         if isinstance(item, dict) and type(item.get("fact_seq")) is int}
                for seq in finding.get("evidence_fact_seqs") or []:
                    chosen = selected.get(seq)
                    if chosen is not None:
                        aid = chosen.get("artifact_id")
                        digest = chosen.get("sha256")
                        if (not isinstance(aid, str) or len(aid) != 12
                                or any(c not in "0123456789abcdef" for c in aid)
                                or not isinstance(digest, str) or len(digest) != 64
                                or any(c not in "0123456789abcdef" for c in digest)):
                            raise HTTPException(status_code=409, detail="review artifact reference is invalid")
                        path = private_root / digest
                        if not path.is_file() or path.is_symlink():
                            raise HTTPException(status_code=409, detail="review artifact snapshot unavailable")
                        actual = hashlib.sha256()
                        with path.open("rb") as handle:
                            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                                actual.update(chunk)
                        if actual.hexdigest() != digest:
                            raise HTTPException(status_code=409, detail="review artifact snapshot changed")
                        add_file(path, f"evidence/{aid}.txt", digest=digest,
                                 size=path.stat().st_size, kind="reviewed_text_artifact", fact_seq=seq)
                        continue
                    if view.get("contract_version", 1) >= 2 and finding.get("review_status") == "accepted":
                        raise HTTPException(status_code=409, detail="accepted report has no reviewed artifact snapshot")
                    aid = (notes.get(seq) or {}).get("artifact_id") or (
                        (facts_by_seq.get(seq) or {}).get("provenance") or {}).get("artifact_id")
                    if not aid:
                        raise HTTPException(status_code=409, detail="cited Fact has no selected artifact")
                    path, digest, size, content = _pentest_artifact_source(
                        run_id, seq, str(aid), report_seq=selected_seq)
                    if content is not None:
                        add_file(path, f"evidence/{path.name}", digest=digest,
                                 size=size, kind="original_text_artifact", fact_seq=seq)
            cited = {int(seq) for finding in export_sources
                     for seq in finding.get("evidence_fact_seqs") or []}
            for fact in view.get("evidence_facts") or []:
                for evidence in fact.get("evidence") or []:
                    seq = int(evidence.get("fact_seq") or 0)
                    if seq not in cited:
                        continue
                    for screenshot in evidence.get("screenshots") or []:
                        aid = screenshot["artifact_id"]
                        filename = screenshot["filename"]
                        path, digest, size, _content = _pentest_artifact_source(
                            run_id, seq, aid, report_seq=selected_seq)
                        add_file(path, f"images/{filename}", digest=digest,
                                 size=size, kind="original_image_artifact", fact_seq=seq)
            for finding in export_sources:
                for screenshot in finding.get("screenshots") or []:
                    poc_id = str(screenshot.get("poc_id") or "")
                    path, filename, _media_type = _pentest_report_image_source(
                        run_id, poc_id, report_seq=selected_seq)
                    content = path.read_bytes()
                    add_file(path, f"images/{filename}",
                             digest=hashlib.sha256(content).hexdigest(), size=len(content),
                             kind="report_screenshot")
            markdown = _pentest_markdown(
                view, controlled_bundle=True, attachment_paths=set(attachments),
            ).encode("utf-8")
            archive.writestr("report.md", markdown)
            attachments["report.md"] = {
                "path": "report.md", "sha256": hashlib.sha256(markdown).hexdigest(),
                "size": len(markdown), "kind": "reader_report",
            }
            from muteki.pentest.judgement import _evidenced_facts, contract_at
            _run, _graph_path, events, contract = _pentest_report_source(run_id)
            source_events = [row for row in events if int(row.get("seq") or 0) <= view["as_of_seq"]]
            evidence_contract = contract_at(source_events, contract, view["as_of_seq"])
            source_facts = _evidenced_facts(source_events, evidence_contract)
            if cited - source_facts.keys():
                raise HTTPException(status_code=409, detail="cited evidence provenance unavailable")
            archive.writestr("manifest.json", json.dumps({
                "schema": "muteki.pentest.evidence-bundle.v1",
                "report_seq": selected_seq,
                "source_graph_seq": view["as_of_seq"],
                "classification": "controlled_original_evidence",
                "reader_report_redaction": "exact_known_secret_values_only",
                "findings": [{
                    "id": item.get("id"),
                    "review_status": item.get("review_status"),
                    "review_note": item.get("review_note"),
                    "evidence_fact_seqs": item.get("evidence_fact_seqs") or [],
                    "selected_artifacts": item.get("selected_artifacts") or [],
                } for item in export_sources],
                "source_facts": [{
                    "fact_seq": seq,
                    "fact": str((source_facts[seq].get("payload") or {}).get("fact") or ""),
                    "worker": source_facts[seq].get("actor"),
                    "provenance": (source_facts[seq].get("payload") or {}).get("evidence_provenance") or {},
                } for seq in sorted(cited)],
                "attachments": [attachments[key] for key in sorted(attachments)],
            }, ensure_ascii=False, indent=2))
        return Response(
            output.getvalue(), media_type="application/zip",
            headers={"Content-Disposition": f'attachment; filename="pentest-{run_id}{filename_suffix}-evidence.zip"',
                     "Cache-Control": "no-store"},
        )

    def _pentest_artifact_source(
        run_id: str, fact_seq: int, artifact_id: str, *, report_seq: int | None = None,
    ) -> tuple[Path, str, int, str | None]:
        """Resolve only an artifact cited by an evidenced Fact in this Run."""
        from muteki.pentest.judgement import _evidenced_facts, contract_at

        if len(artifact_id) != 12 or any(char not in "0123456789abcdef" for char in artifact_id):
            raise HTTPException(status_code=404, detail="evidence artifact unavailable")
        _run, _graph_path, events, contract = _pentest_report_source(run_id)
        if report_seq is not None:
            from muteki.pentest.judgement import report
            try:
                selected_view = report(events, contract, report_seq=report_seq)
            except LookupError as exc:
                raise HTTPException(status_code=404, detail=str(exc)) from exc
            events = [event for event in events if event["seq"] <= selected_view["as_of_seq"]]
        evidence_contract = contract_at(events, contract, int(events[-1]["seq"]) if events else 0)
        fact = _evidenced_facts(events, evidence_contract).get(fact_seq)
        if fact is None:
            raise HTTPException(status_code=404, detail="evidenced Fact unavailable")
        provenance = (fact.get("payload") or {}).get("evidence_provenance") or {}
        refs = {str(ref.get("artifact_id")): ref
                for ref in provenance.get("artifact_refs") or [] if isinstance(ref, dict)}
        ref = refs.get(artifact_id)
        if ref is None:
            raise HTTPException(status_code=404, detail="artifact is not cited by this Fact")
        root = app.state.manager.workspace_dir(run_id) / "arts"
        matches = list(root.glob(f"{artifact_id}.*"))
        if len(matches) != 1 or not matches[0].is_file() or matches[0].is_symlink():
            raise HTTPException(status_code=404, detail="evidence artifact unavailable")
        path = matches[0]
        try:
            path.resolve().relative_to(root.resolve())
        except ValueError as exc:
            raise HTTPException(status_code=404, detail="evidence artifact unavailable") from exc
        content = path.read_bytes()
        digest = hashlib.sha256(content).hexdigest()
        expected = str(ref.get("sha256") or "")
        if not expected and artifact_id == str(provenance.get("artifact_id") or ""):
            expected = str(provenance.get("artifact_sha256") or "")
        if (len(expected) != 64 or any(char not in "0123456789abcdef" for char in expected)):
            raise HTTPException(status_code=409, detail="evidence artifact has no durable digest")
        if digest != expected:
            raise HTTPException(status_code=409, detail="evidence artifact digest mismatch")
        text_content = (content.decode("utf-8", errors="replace")
                        if path.suffix.lower() in {".txt", ".log", ".http", ".json", ".md"}
                        else None)
        return path, digest, len(content), text_content

    def _pentest_report_image_source(
        run_id: str, poc_id: str, *, report_seq: int | None = None,
    ) -> tuple[Path, str, str]:
        from muteki.pentest.judgement import report

        _run, graph_path, events, contract = _pentest_report_source(run_id)
        try:
            view = report(events, contract, report_seq=report_seq)
        except LookupError as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        cited = next((shot for item in view["findings"] for shot in item.get("screenshots") or []
                      if shot.get("poc_id") == poc_id), None)
        if cited is None:
            raise HTTPException(status_code=404, detail="report screenshot unavailable")
        connection = sqlite3.connect(f"{graph_path.resolve().as_uri()}?mode=ro", uri=True)
        try:
            row = connection.execute(
                "SELECT name,path,artifact_id FROM pocs WHERE challenge_id=? AND poc_id=?",
                (run_id, poc_id),
            ).fetchone()
        finally:
            connection.close()
        if row is None or str(row[2] or "") != str(cited.get("artifact_id") or ""):
            raise HTTPException(status_code=404, detail="report screenshot unavailable")
        suffix = Path(str(row[0] or "")).suffix.lower()
        media_types = {".png": "image/png", ".jpg": "image/jpeg", ".jpeg": "image/jpeg",
                       ".webp": "image/webp"}
        if suffix not in media_types:
            raise HTTPException(status_code=404, detail="report attachment is not an image")
        root = app.state.manager.workspace_dir(run_id).resolve()
        path = (root / str(row[1] or "")).resolve()
        if not path.is_relative_to(root / "shared" / "objects") or not path.is_file():
            raise HTTPException(status_code=409, detail="report screenshot missing or changed")
        content = path.read_bytes()
        signatures = {
            ".png": content.startswith(b"\x89PNG\r\n\x1a\n"),
            ".jpg": content.startswith(b"\xff\xd8\xff"),
            ".jpeg": content.startswith(b"\xff\xd8\xff"),
            ".webp": content.startswith(b"RIFF") and content[8:12] == b"WEBP",
        }
        if hashlib.sha256(content).hexdigest() != str(row[2]) or not signatures[suffix]:
            raise HTTPException(status_code=409, detail="report screenshot missing or changed")
        return path, f"{row[2]}{suffix}", media_types[suffix]

    @app.get("/api/runs/{run_id}/pentest-report-images/{poc_id}")
    async def pentest_report_image(run_id: str, poc_id: str, report_seq: int | None = None) -> Any:
        path, filename, media_type = _pentest_report_image_source(
            run_id, poc_id, report_seq=report_seq)
        content = path.read_bytes()
        if hashlib.sha256(content).hexdigest() != filename.split(".", 1)[0]:
            raise HTTPException(status_code=409, detail="report screenshot changed during delivery")
        return Response(content, media_type=media_type,
                        headers={"Cache-Control": "no-store", "Content-Disposition": "inline"})

    @app.get("/api/runs/{run_id}/pentest-evidence/{fact_seq}/artifacts/{artifact_id}")
    async def pentest_evidence_artifact(run_id: str, fact_seq: int, artifact_id: str,
                                        report_seq: int | None = None) -> Any:
        path, digest, size, content = _pentest_artifact_source(
            run_id, fact_seq, artifact_id, report_seq=report_seq)
        return JSONResponse({
            "fact_seq": fact_seq, "artifact_id": artifact_id,
            "filename": path.name, "sha256": digest, "size": size,
            "content": content if path.suffix.lower() in {".txt", ".log", ".http", ".json", ".md"} else None,
        }, headers={"Cache-Control": "no-store"})

    @app.get("/api/runs/{run_id}/pentest-evidence/{fact_seq}/artifacts/{artifact_id}/download")
    async def download_pentest_evidence_artifact(run_id: str, fact_seq: int, artifact_id: str,
                                                  report_seq: int | None = None) -> Any:
        path, digest, size, _content = _pentest_artifact_source(
            run_id, fact_seq, artifact_id, report_seq=report_seq)
        content = path.read_bytes()
        if len(content) != size or hashlib.sha256(content).hexdigest() != digest:
            raise HTTPException(status_code=409, detail="evidence changed during delivery")
        return Response(content, media_type="application/octet-stream",
                        headers={"Cache-Control": "no-store",
                                 "Content-Disposition": f'attachment; filename="evidence-{artifact_id}{path.suffix}"'})

    @app.get("/api/runs/{run_id}/pentest-evidence/{fact_seq}/artifacts/{artifact_id}/view")
    async def view_pentest_image(run_id: str, fact_seq: int, artifact_id: str,
                                 report_seq: int | None = None) -> Any:
        path, digest, size, _content = _pentest_artifact_source(
            run_id, fact_seq, artifact_id, report_seq=report_seq)
        media_types = {".png": "image/png", ".jpg": "image/jpeg", ".jpeg": "image/jpeg",
                       ".webp": "image/webp", ".gif": "image/gif"}
        media_type = media_types.get(path.suffix.lower())
        if media_type is None:
            raise HTTPException(status_code=404, detail="evidence is not an image")
        content = path.read_bytes()
        if len(content) != size or hashlib.sha256(content).hexdigest() != digest:
            raise HTTPException(status_code=409, detail="evidence changed during delivery")
        return Response(content, media_type=media_type,
                        headers={"Cache-Control": "no-store", "Content-Disposition": "inline"})

    @app.post("/api/runs/{run_id}/pentest-report/regenerate")
    async def regenerate_pentest_report(run_id: str) -> Any:
        """Retry only the model report, preserving the completed test graph."""
        from apps.web.llm_credentials import resolve_llm_profile_credential
        from muteki.core.llm import LLMClient, llm_temperature_kwargs
        from muteki.models.solve_graph import Challenge
        from muteki.pentest.judgement import generate_report
        from muteki.swarm.shared_graph import SQLiteSharedGraph

        run, path, events, contract = _pentest_report_source(run_id)
        if not run.finished:
            raise HTTPException(status_code=409, detail="test is still running")
        mgr: RunManager = app.state.manager
        profile = dict((mgr.worker_config.get().get("llm_profiles") or {}).get("planner") or {})
        try:
            credential = resolve_llm_profile_credential(
                "planner", profile, sessions_root=mgr.state_root,
            )
        except ValueError as exc:
            raise HTTPException(status_code=503, detail="planner credential unavailable") from exc
        if not credential.api_key:
            raise HTTPException(status_code=503, detail="planner credential unavailable")
        kwargs: dict[str, Any] = dict(llm_temperature_kwargs(profile))
        kwargs["api_key"] = credential.api_key
        if credential.base_url:
            kwargs["base_url"] = credential.base_url
        challenge = Challenge(
            id=run_id, name=run.name or run_id, category=run.category or "web",
            target=contract.target, mode="pentest", goal=contract.goal,
            pentest_contract=contract,
        )
        graph = SQLiteSharedGraph.open(db_path=path, challenge=challenge)
        try:
            try:
                async with LLMClient(**kwargs) as llm:
                    generated = await generate_report(
                        llm, str(profile.get("model") or ""), events, contract,
                        terminal_reason=run.terminal_reason,
                        run_id=run_id, challenge_id=run_id,
                    )
                graph.record_pentest_report(payload=generated)
            except Exception as exc:
                graph.record_pentest_report(payload={
                    "code": "report_generation_failed",
                    "error_type": type(exc).__name__,
                    "detail": str(exc),
                    "raw_response": str(getattr(exc, "raw_response", "") or ""),
                }, error=True)
                raise HTTPException(status_code=502, detail={
                    "code": "report_generation_failed",
                    "error_type": type(exc).__name__,
                }) from exc
        finally:
            graph.close()
        return await pentest_report(run_id)

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
