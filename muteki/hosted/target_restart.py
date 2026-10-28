"""TSecBench runner-owned target lifecycle bridge for live hosted Swarms."""
from __future__ import annotations

import asyncio
import json
import os
import re
import secrets
import sqlite3
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Callable

import httpx

URL_ENV = "MUTEKI_TSECBENCH_RESTART_URL"
TOKEN_ENV = "MUTEKI_TSECBENCH_RESTART_TOKEN"


class RestartPreflightError(RuntimeError):
    """The lifecycle callback confirms that no platform mutation was attempted."""


def relaunch_tsecbench_target(platform: Any, code: str, wait_ready: Callable[[Any], bool]) -> list[str]:
    receipt = platform.close_challenge(code)
    if not receipt.released:
        raise RuntimeError("platform did not confirm target close")
    fresh = platform.start_challenge(code)
    if not wait_ready(fresh):
        raise RuntimeError("restarted target did not become reachable")
    return list(fresh.addresses)


class TargetRestartBridge:
    """Only registered visits can mutate targets; ambiguous attempts are fenced."""

    def __init__(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        self.path = path
        self.token = secrets.token_urlsafe(32)
        self.lock = threading.RLock()
        self.visits: dict[str, Callable[[], list[str]]] = {}
        self.run_locks: dict[str, threading.RLock] = {}
        with sqlite3.connect(path) as db:
            db.execute("CREATE TABLE IF NOT EXISTS restarts (run_id TEXT, request_id TEXT, result TEXT, PRIMARY KEY(run_id,request_id))")
        bridge = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                if self.path != "/restart" or self.headers.get("Authorization") != f"Bearer {bridge.token}":
                    self.send_error(403)
                    return
                try:
                    payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
                    result = bridge.restart(str(payload["run_id"]), str(payload["request_id"]))
                except Exception as exc:
                    result = {"ok": False, "code": "restart_bridge_failed", "detail": f"{type(exc).__name__}: {exc}"}
                body = json.dumps(result).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args: Any) -> None:
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}/restart"

    def register(self, run_id: str, relaunch: Callable[[], list[str]]) -> None:
        with self.lock:
            self.visits[run_id] = relaunch
            self.run_locks.setdefault(run_id, threading.RLock())

    def unregister(self, run_id: str) -> None:
        with self.lock:
            lock = self.run_locks.get(run_id)
        if lock is not None:
            with lock, self.lock:
                self.visits.pop(run_id, None)

    def restart(self, run_id: str, request_id: str) -> dict[str, Any]:
        with self.lock:
            lock = self.run_locks.setdefault(run_id, threading.RLock())
        with lock, sqlite3.connect(self.path) as db:
            if not request_id or run_id not in self.visits:
                return {"ok": False, "code": "restart_unavailable", "detail": "No active hosted visit for this run"}
            prior = db.execute("SELECT result FROM restarts WHERE run_id=? AND request_id=?", (run_id, request_id)).fetchone()
            if prior and prior[0]:
                return json.loads(prior[0])
            if db.execute("SELECT 1 FROM restarts WHERE run_id=? AND result IS NULL", (run_id,)).fetchone():
                return {"ok": False, "code": "restart_outcome_unknown", "detail": "A prior restart has an uncertain outcome; do not repeat the destructive request"}
            db.execute("INSERT INTO restarts VALUES (?,?,NULL)", (run_id, request_id))
            db.commit()  # Persist before touching the platform.
            try:
                addresses = self.visits[run_id]()
                if not addresses or any(not isinstance(item, str) or not item.strip() for item in addresses):
                    raise RuntimeError("platform restarted without target endpoints")
                result = {"ok": True, "targets": addresses}
            except RestartPreflightError as exc:
                result = {"ok": False, "code": "restart_preflight_failed", "detail": str(exc)}
            except Exception as exc:
                # Keep the durable pending row: close/start may have succeeded.
                return {"ok": False, "code": "restart_outcome_unknown", "detail": f"{type(exc).__name__}: {exc}"}
            db.execute("UPDATE restarts SET result=? WHERE run_id=? AND request_id=?", (json.dumps(result), run_id, request_id))
            return result

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def install_swarm_restart(swarm: Any) -> None:
    """Bind only when the hosted runner explicitly supplies its private bridge."""
    url, token = os.environ.get(URL_ENV, ""), os.environ.get(TOKEN_ENV, "")
    if not url or not token or swarm.challenge.mode != "ctf":
        return
    lock = asyncio.Lock()
    results: dict[str, dict[str, Any]] = {}

    async def restart_target(*, worker: Any, request_id: str, evidence: str) -> dict[str, Any]:
        async with lock:
            if request_id in results:
                return dict(results[request_id])
            if swarm._live_solvers.get(worker.solver_id) is not worker or worker.run_id != swarm.run_id:
                return {"ok": False, "code": "restart_identity_invalid", "detail": "Requester is not a live Worker of this run"}
            paused: list[Any] = []
            worker_clock_paused = False
            swarm._target_restart_in_progress = True
            try:
                if not getattr(worker, "_paused", False) and hasattr(worker, "_paused_event"):
                    worker._paused_event.set()
                    worker_clock_paused = True
                for solver in list(swarm._live_solvers.values()):
                    if solver is worker:
                        continue
                    if not solver._paused:
                        if not solver._set_paused(True):
                            return {"ok": False, "code": "restart_pause_failed", "detail": "Could not pause every live Worker"}
                        solver._restart_pause_owned = True
                        paused.append(solver)
                if swarm.shared_graph is not None:
                    await asyncio.to_thread(swarm.shared_graph.stop_runtime_resources, actor="coordinator", target_epoch=str(swarm._target_epoch))
                if hasattr(swarm, "_emit_bb"):
                    await swarm._emit_bb(
                        "target_restart_requested",
                        worker=worker.solver_id,
                        evidence=evidence,
                    )

                def call() -> dict[str, Any]:
                    with httpx.Client(trust_env=False, timeout=300) as client:
                        response = client.post(url, headers={"Authorization": f"Bearer {token}"}, json={"run_id": swarm.run_id, "request_id": request_id})
                        response.raise_for_status()
                        return response.json()

                try:
                    operation = asyncio.create_task(asyncio.to_thread(call))
                    # Keep the pause fence until the mutating call actually retires,
                    # even if the requesting Worker is cancelled mid-restart.
                    while not operation.done():
                        try:
                            await asyncio.shield(operation)
                        except asyncio.CancelledError:
                            continue
                    result = operation.result()
                except Exception as exc:
                    result = {"ok": False, "code": "restart_outcome_unknown", "detail": f"{type(exc).__name__}: {exc}"}
                if not result.get("ok") and result.get("code") != "restart_outcome_unknown":
                    results[request_id] = dict(result)
                    return result
                if not result.get("ok"):
                    swarm._operator_paused = True
                    swarm._target_restart_error = result.get("detail") or result.get("code")
                    await swarm.insight.guidance(
                        "Target restart outcome is uncertain. Stop using the old target and reconcile platform state before continuing: " + str(swarm._target_restart_error),
                        action="hint", target="global", standing=True,
                    )
                    results[request_id] = dict(result)
                    # Do not let peers keep using potentially destroyed target state.
                    for solver in list(swarm._live_solvers.values()):
                        if solver is not worker:
                            solver._steer_event.set()
                    return result
                targets = result["targets"]
                old_targets = str(swarm.challenge.scope or swarm.challenge.target or "").split(", ")
                swarm._visit_restart_count = int(getattr(swarm, "_visit_restart_count", 0) or 0) + 1
                swarm._target_epoch = int(swarm._target_epoch) + 1
                swarm._target_redirect = targets[0]
                swarm.challenge.target = targets[0]
                swarm.challenge.scope = ", ".join(targets)
                if swarm.shared_graph is not None:
                    swarm.shared_graph._target_epoch = str(swarm._target_epoch)
                old_block = "Target endpoints:\n" + "\n".join(
                    f"- {target}" for target in old_targets
                )
                new_block = "Target endpoints:\n" + "\n".join(
                    f"- {target}" for target in targets
                )
                addr_json = json.dumps(list(targets), ensure_ascii=False, indent=4)
                if "\n" in addr_json:
                    addr_json = addr_json[:-1] + "  ]"
                addr_replacement = f'"container_addr": {addr_json}'
                description = str(swarm.challenge.description or "")
                if '"container_addr"' in description:
                    swarm.challenge.description = re.sub(
                        r'"container_addr"\s*:\s*\[[\s\S]*?\](?=\s*[,}\n])',
                        lambda _: addr_replacement,
                        description,
                        count=1,
                    )
                elif old_block in description:
                    swarm.challenge.description = description.replace(
                        old_block, new_block, 1,
                    )
                else:
                    swarm.challenge.description = (
                        f"{description}\nCurrent target endpoints after restart "
                        f"(epoch {swarm._target_epoch}; earlier endpoints are stale):\n"
                        + "\n".join(f"- {target}" for target in targets)
                    )
                contract = getattr(swarm.challenge, "task_contract", None)
                if contract is not None:
                    raw_inst = str(getattr(contract, "raw_instruction", "") or "")
                    if '"container_addr"' in raw_inst:
                        contract.raw_instruction = re.sub(
                            r'"container_addr"\s*:\s*\[[\s\S]*?\](?=\s*[,}\n])',
                            lambda _: addr_replacement,
                            raw_inst,
                            count=1,
                        )
                    elif old_block in raw_inst:
                        contract.raw_instruction = raw_inst.replace(
                            old_block, new_block, 1,
                        )
                    elif raw_inst:
                        contract.raw_instruction = swarm.challenge.description
                    if hasattr(contract, "execution_target"):
                        contract.execution_target = targets[0]
                    if hasattr(contract, "authorization_scope"):
                        contract.authorization_scope = ", ".join(targets)
                if swarm.shared_graph is not None:
                    swarm.shared_graph.challenge = swarm.challenge
                    if hasattr(swarm.shared_graph, "sync_origin_fact"):
                        await asyncio.to_thread(
                            swarm.shared_graph.sync_origin_fact,
                            workspace_root=getattr(swarm, "workspace_root", None),
                        )
                restart_notice = (
                    f"【系统同步】队友已将靶机容器重启完毕，当前最新目标地址为 {targets[0]}"
                    f"（epoch {swarm._target_epoch}，连接与内存会话态已重置），"
                    "请直接针对新目标地址继续完成你负责的 Step。"
                )
                for solver in list(swarm._live_solvers.values()):
                    solver._target_override = targets[0]
                    solver._target_epoch = str(swarm._target_epoch)
                    if solver is not worker:
                        solver._pending_target_restart_notice = restart_notice
                        solver._steer_event.set()
                if hasattr(swarm, "_emit_bb"):
                    await swarm._emit_bb(
                        "target_restart_completed",
                        worker=worker.solver_id,
                        target=targets[0],
                        target_epoch=str(swarm._target_epoch),
                    )
                result = {**result, "target": targets[0], "target_epoch": str(swarm._target_epoch), "message": f"RESTARTED targets={json.dumps(targets)} epoch={swarm._target_epoch}; run context before continuing"}
                results[request_id] = dict(result)
                return result
            finally:
                swarm._target_restart_in_progress = False
                if worker_clock_paused and not getattr(worker, "_paused", False) and hasattr(worker, "_paused_event"):
                    worker._paused_event.clear()
                owned = {id(s): s for s in [*paused, *swarm._live_solvers.values()] if getattr(s, "_restart_pause_owned", False)}
                for solver in owned.values():
                    solver._restart_pause_owned = False
                    if not solver._set_paused(False):
                        raise RuntimeError(f"failed to resume Worker {solver.solver_id} after restart")

    swarm._target_restarter = restart_target
