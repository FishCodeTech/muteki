"""OpenCode 2's native API, selected explicitly by the OpenCode driver.

Protocol and lifecycle follow T3 nightly 611132c: /api/info, /api/event,
session inbox/execution events, forms, per-session rules and runtime MCP.
This is independent of the v1 session/status and message-part protocol.
"""
from __future__ import annotations

import asyncio
import hashlib
import json
import os
import tempfile
import time
from pathlib import Path
from typing import Any, AsyncIterator
from urllib.parse import quote, urlencode

from muteki.capability_bindings.acp_config import TOKEN_PLACEHOLDER
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import CapabilityInjectionPlan, InjectionKind
from muteki.platform.contracts.external_agents import (
    AccessMode, AgentCapabilities, AgentEvent, AgentEventType, AgentInput, AttachmentRef,
    AgentSessionRef, ApprovalResponseInput, MessageInput, ProbeRequest,
    SessionStart, SteerInput, UserInputResponseInput,
)
from muteki.platform.contracts.agent_events import (
    AgentNodePayload, AgentUpdatedPayload, FailureCategory, MessageDeltaPayload,
    MessageCompletedPayload, PlanPayload, ReasoningPayload, RuntimeCapabilitiesPayload, RuntimeWarningPayload,
    SessionPayload, ToolPayload, TurnCompletedPayload, TurnFailedPayload,
    TurnStartedPayload, UsagePayload, UserInputRequestedPayload, UserInputResolvedPayload,
    dump_payload,
)
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState

from .approvals import SessionApprovalGrants
from .base import TurnLimits, TurnRunner
from .capabilities import (
    AccessModeUnsupportedError, BOOL_CAPABILITY_FIELDS, CapabilityProbeReport,
    SOURCE_PROBE, SOURCE_REPORTED, conservative_capabilities,
)
from .events import build_event
from .opencode import (
    OpenCodeError, OpenCodeHttpClient, _SUPPORTED_ACCESS_MODES,
    _as_process, _opencode_major, _opencode_message_id, _pick_free_port,
)
from .probe_environment import subprocess_environment
from .process_supervisor import spawn_supervised
from .rpc import ProcessOutputLog
from .runtime_capabilities import RuntimeCapabilitySnapshot, dynamic_command_item
from .user_input_schema import _answer_values, expand_legacy_text_answers, normalize_question


def _data(body: Any, operation: str) -> Any:
    if not isinstance(body, dict) or "data" not in body:
        raise OpenCodeError(f"{operation} returned an invalid OpenCode 2 envelope: {json.dumps(body, ensure_ascii=False)}",
                            code="opencode2.protocol_invalid")
    return body["data"]


def _location(path: str, cwd: str) -> str:
    return path + "?" + urlencode({"location[directory]": cwd})


class OpenCode2Runtime:
    """One protocol implementation, using its owner's shared event contracts."""

    def __init__(self, owner: Any) -> None:
        self.owner = owner

    def _client(self, url: str) -> OpenCodeHttpClient:
        return OpenCodeHttpClient(url, username="opencode", password=self.owner._server_password)

    async def _agents(self, client: OpenCodeHttpClient, cwd: str) -> list[dict[str, Any]]:
        # T3 waits for workspace inventories to settle. A new v2 location
        # initially returns an empty snapshot while built-in agents load.
        deadline = time.monotonic() + self.owner._startup_timeout
        while True:
            agents = _data(await client.get(
                _location("/api/agent", cwd),
                timeout=max(.1, deadline - time.monotonic())), "agent.list")
            if not isinstance(agents, list):
                raise OpenCodeError("agent.list is not an array", code="opencode2.protocol_invalid")
            if agents:
                return agents
            if time.monotonic() >= deadline:
                raise OpenCodeError("Native agent inventory did not settle", code="opencode2.inventory_timeout")
            await asyncio.sleep(.1)

    async def _start_server(self, cwd: str, env: dict[str, str], sid: str) -> tuple[Any, str, dict[str, Any]]:
        owner = self.owner
        if owner._attach_url:
            url, proc = owner._attach_url, None
        else:
            if not owner._manage_server:
                raise OpenCodeError("OpenCode 2 needs a configured server or managed server",
                                    code="opencode2.server_missing")
            port = _pick_free_port()
            url = f"http://127.0.0.1:{port}"
            server_env = owner._server_env(env)
            server_env.pop("OPENCODE_SERVER_PASSWORD", None)
            server_env.pop("OPENCODE_SERVER_USERNAME", None)
            server_env["OPENCODE_PASSWORD"] = owner._server_password
            output = ProcessOutputLog.create(owner._log_root, label=f"opencode2-serve-{port}")
            try:
                proc = await spawn_supervised(
                    owner._with_launch_args([owner._binary, "serve", "--hostname", "127.0.0.1", "--port", str(port)]),
                    adapter_id=owner.adapter_id, session_id=sid, label=f"opencode2-serve-{port}",
                    cwd=cwd, env=subprocess_environment(server_env),
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            except BaseException:
                await output.close()
                raise
            output.attach(proc.process)
            owner._server_outputs[proc.pid] = output
        client = self._client(url)
        try:
            deadline = time.monotonic() + owner._startup_timeout
            while True:
                process = _as_process(proc)
                if process is not None and process.returncode is not None:
                    raise OpenCodeError(f"OpenCode 2 server exited ({process.returncode}): {owner.server_output_detail(proc)}",
                                        code="opencode2.server_exited")
                try:
                    info = await client.get("/api/info", timeout=5)
                    if not isinstance(info, dict) or not isinstance(info.get("pid"), int) or (_opencode_major(str(info.get("version") or "")) or 0) < 2:
                        raise OpenCodeError(f"/api/info did not identify OpenCode 2: {json.dumps(info, ensure_ascii=False)}",
                                            code="opencode2.protocol_invalid")
                    return proc, url, info
                except OpenCodeError as exc:
                    if owner._attach_url or exc.status == 401 or exc.code == "opencode2.protocol_invalid":
                        raise
                    if time.monotonic() >= deadline:
                        raise OpenCodeError(f"OpenCode 2 server readiness timed out: {exc}\n{owner.server_output_detail(proc)}",
                                            code="opencode2.start_timeout") from exc
                    await asyncio.sleep(.2)
        except BaseException:
            if proc is not None:
                await owner._stop_server(proc)
            raise
        finally:
            try:
                await client.close()
            except BaseException:
                if proc is not None:
                    await owner._stop_server(proc)
                raise

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        # Inventory discovery scans its location. A health probe must not scan
        # the service checkout (including its unrelated native agent assets).
        with tempfile.TemporaryDirectory(prefix="muteki-opencode-probe-") as cwd:
            return await self._probe_at(request, cwd)

    async def _probe_at(self, request: ProbeRequest, cwd: str) -> AgentCapabilities:
        owner = self.owner
        proc = client = None
        caps = conservative_capabilities(transport_kind="http", capability_source=SOURCE_REPORTED)
        detail = ""
        try:
            proc, url, info = await self._start_server(cwd, {}, "")
            client = self._client(url)
            active = _data(await client.get("/api/session/active", timeout=5), "session.active")
            agents = await self._agents(client, cwd)
            if not isinstance(active, dict) or not isinstance(agents, list):
                raise OpenCodeError("OpenCode 2 inventory has an invalid shape", code="opencode2.protocol_invalid")
            stop, connected = asyncio.Event(), asyncio.Event()
            stream = asyncio.create_task(client.stream_sse("/api/event", lambda _name, _body: None, stop, connected))
            try:
                ready = asyncio.create_task(connected.wait())
                await asyncio.wait({stream, ready}, timeout=10, return_when=asyncio.FIRST_COMPLETED)
                if not connected.is_set():
                    if stream.done():
                        await stream
                    raise OpenCodeError("OpenCode 2 event subscription did not connect", code="opencode2.sse_connect_timeout")
            finally:
                stop.set()
                ready.cancel()
                stream.cancel()
                await asyncio.gather(stream, ready, return_exceptions=True)
            caps.runtime_version = str(info["version"])
            caps.protocol_version = "opencode-v2"
            caps.streaming = caps.tool_events = caps.usage_events = True
            caps.approval = caps.user_input = caps.interrupt = caps.steer = True
            caps.resume = caps.session_persistence = caps.subagents = True
            caps.mcp = not owner._attach_url
            caps.resume_continues_turn = False
            caps.plan = caps.compaction = True
            caps.plan_mode = any(isinstance(row, dict) and row.get("id") == "plan" for row in agents)
            caps.plan_mode_per_turn = True
            caps.access_modes = list(_SUPPORTED_ACCESS_MODES)
            if request.include_models:
                models = _data(await client.get(_location("/api/model", cwd), timeout=15), "model.list")
                if not isinstance(models, list):
                    raise OpenCodeError("OpenCode 2 model inventory is invalid", code="opencode2.protocol_invalid")
                caps.supported_models = [f"{row['providerID']}/{row['modelID']}" for row in models
                                         if isinstance(row, dict) and row.get("providerID") and row.get("modelID")]
            detail = "OpenCode 2 identity, session.active, agent inventory and event subscription verified"
        except (OpenCodeError, OSError, asyncio.TimeoutError) as exc:
            caps = conservative_capabilities(transport_kind="http", capability_source=SOURCE_REPORTED)
            detail = f"{getattr(exc, 'code', 'opencode2.probe_failed')}: {exc}"
        finally:
            if client is not None:
                await client.close()
            if proc is not None:
                await owner._stop_server(proc)
        owner._probe_cache = CapabilityProbeReport(
            adapter_id=owner.identity.adapter_id, instance_id=owner.identity.instance_id,
            capabilities=caps, binary_path=owner._binary,
            field_sources={field: SOURCE_REPORTED for field in BOOL_CAPABILITY_FIELDS},
            degradations=(["opencode2.mcp.external_server: runtime MCP injection is only enabled for a managed server"]
                          if caps.streaming and owner._attach_url else [] if caps.streaming else [detail]), detail=detail)
        return caps

    async def _rules(self, handle: dict[str, Any], plan: bool) -> list[dict[str, str]]:
        mode = handle["access_mode"]
        rules = ([{"action": "*", "resource": "*", "effect": "allow"}]
                 if mode == AccessMode.FULL_ACCESS.value else [
                     {"action": "shell", "resource": "*", "effect": "ask"},
                     {"action": "edit", "resource": "*", "effect": "allow" if mode == AccessMode.AUTO_ACCEPT_EDITS.value else "ask"},
                     {"action": "external_directory", "resource": "*", "effect": "ask"}])
        if plan:
            rules.append({"action": "edit", "resource": "*", "effect": "deny"})
        for agent in handle.get("agents", []):
            if agent.get("id") not in ({"build", "plan"} if plan else {"build"}):
                continue
            rules.extend(rule for rule in agent.get("permissions", []) if isinstance(rule, dict)
                         and rule.get("effect") == "allow" and rule.get("resource") != "*"
                         and rule.get("action") in {"edit", "external_directory"})
        if handle.get("mcp_name"):
            rules.extend([{"action": "muteki-*", "resource": "*", "effect": "deny"},
                          {"action": f"{handle['mcp_name']}_*", "resource": "*", "effect": "allow"}])
        return rules

    def _model(self, model: str | None, env: dict[str, str], effort: str | None) -> dict[str, str] | None:
        if not model:
            return None
        provider = env.get("MUTEKI_OPENCODE_PROVIDER") or self.owner._extra_env.get("MUTEKI_OPENCODE_PROVIDER")
        if not provider and "/" in model:
            provider, model = model.split("/", 1)
        if not provider:
            raise OpenCodeError("OpenCode 2 model selection needs provider/model", code="opencode2.model_invalid")
        variant = None
        if "#" in model:
            model, variant = model.split("#", 1)
        return {"id": model, "providerID": provider,
                **({"variant": variant or effort} if variant or effort and effort != "default" else {})}

    async def launch(self, request: SessionStart, plan: CapabilityInjectionPlan | None,
                     bearer_token: str | None) -> dict[str, Any]:
        owner = self.owner
        mode = str(request.access_mode or AccessMode.SUPERVISED.value)
        if mode not in _SUPPORTED_ACCESS_MODES:
            raise AccessModeUnsupportedError(owner.id, mode, _SUPPORTED_ACCESS_MODES,
                                             "OpenCode has no native auto-review mode")
        cwd = str(Path(request.options.cwd or os.getcwd()).resolve())
        env = dict(request.options.env)
        proc = client = None
        handle: dict[str, Any] = {
            "generation": "v2", "cwd": cwd, "options": request.options,
            "conversation_thread_id": request.thread_id, "access_mode": mode,
            "approval_grants": SessionApprovalGrants(mode), "pending_approvals": {},
            "pending_questions": {}, "approval_requests": {}, "background_tasks": set(),
            "event_sink": None, "current_turn_id": None, "turns": 0, "warnings": [],
            "child_sessions": {}, "parents": {}, "agent_nodes": {}, "tools": {},
            "texts": {}, "reasoning": {}, "delivered": set(), "admitted": set(),
            "resumed": bool(request.resume_handle), "interrupted": False,
            "model_ref": self._model(request.model, env, request.effort),
            "mcp_name": None, "mcp_injected": 0, "unsettled": False,
            "pending_steers": set(), "settled_steers": set(), "stranded_steers": set(),
            # T3 OpenCode2AdapterV2: SubagentCall, Wake, reports, stoppedChildren.
            "calls": {}, "wakes": [], "reports": {}, "stopped_children": set(),
            "busy": set(), "suppressed_executions": set(),
        }
        try:
            proc, url, info = await self._start_server(cwd, env, request.agent_session_id)
            client = self._client(url)
            handle.update(proc=proc, client=client, base_url=url, runtime_version=info["version"])
            agents = await self._agents(client, cwd)
            if not isinstance(agents, list):
                raise OpenCodeError("agent.list is not an array", code="opencode2.protocol_invalid")
            handle["agents"] = agents
            handle["plan_agent"] = any(row.get("id") == "plan" for row in agents if isinstance(row, dict))
            await self._inject_mcp(handle, plan, bearer_token)
            rules = await self._rules(handle, str(request.interaction_mode or "default") == "plan")
            model = handle["model_ref"]
            if request.options.fork_from:
                native = _data(await client.post(f"/api/session/{quote(request.options.fork_from, safe='')}/fork", {}), "session.fork")
            elif request.resume_handle:
                native = _data(await client.get(f"/api/session/{quote(request.resume_handle, safe='')}"), "session.get")
            else:
                native = _data(await client.post("/api/session", {
                    "location": {"directory": cwd}, "permissions": rules,
                    **({"model": model} if model is not None else {}),
                    **({"title": request.options.title} if request.options.title else {}),
                }), "session.create")
            if not isinstance(native, dict) or not isinstance(native.get("id"), str):
                raise OpenCodeError("session operation did not return a native id", code="opencode2.protocol_invalid")
            native_id = native["id"]
            handle.update(external_session_id=native_id, permissions=rules, agent=native.get("agent") or "build",
                          model_ref=model or native.get("model"))
            native_cwd = (native.get("location") or {}).get("directory")
            if native_cwd and native_cwd != cwd:
                await client.post(f"/api/session/{quote(native_id, safe='')}/move", {"directory": cwd})
            await client.patch(f"/api/session/{quote(native_id, safe='')}", {"permissions": rules})
            if request.resume_handle:
                await self._clear_stale_requests(handle)
            commands = _data(await client.get(_location("/api/command", cwd)), "command.list")
            if not isinstance(commands, list):
                raise OpenCodeError("command.list is not an array", code="opencode2.protocol_invalid")
            handle["available_commands"] = commands
            handle["mcp_statuses"] = _data(await client.get(_location("/api/mcp", cwd)), "mcp.list")
            stop, connected = asyncio.Event(), asyncio.Event()
            owner._sessions[request.agent_session_id] = handle
            task = asyncio.create_task(client.stream_sse(
                "/api/event", lambda name, body: self._dispatch(request.agent_session_id, name, body), stop, connected))
            handle.update(sse_task=task, sse_stop=stop)
            waiter = asyncio.create_task(connected.wait())
            try:
                await asyncio.wait({task, waiter}, timeout=owner._startup_timeout, return_when=asyncio.FIRST_COMPLETED)
                if not connected.is_set():
                    if task.done():
                        await task
                    raise OpenCodeError("OpenCode 2 event stream was not accepted", code="opencode2.sse_connect_timeout")
            finally:
                waiter.cancel()
                await asyncio.gather(waiter, return_exceptions=True)
            return {"external_session_id": native_id, "resume_handle": native_id}
        except BaseException:
            try:
                await owner._close_event_stream(handle)
            finally:
                try:
                    tasks = list(handle["background_tasks"])
                    for task in tasks:
                        task.cancel()
                    await asyncio.gather(*tasks, return_exceptions=True)
                    if client is not None:
                        await client.close()
                finally:
                    try:
                        if proc is not None:
                            await owner._stop_server(proc)
                    finally:
                        owner._sessions.pop(request.agent_session_id, None)
            raise

    async def _clear_stale_requests(self, handle: dict[str, Any]) -> None:
        client, sid = handle["client"], quote(handle["external_session_id"], safe="")
        permissions = _data(await client.get(f"/api/session/{sid}/permission"), "session.permission.list")
        forms = _data(await client.get(f"/api/session/{sid}/form"), "session.form.list")
        for request in permissions:
            await client.post(f"/api/session/{sid}/permission/{quote(request['id'], safe='')}/reply", {"decision": "reject"})
        for form in forms:
            await client.request("DELETE", f"/api/session/{sid}/form/{quote(form['id'], safe='')}")
        if permissions or forms:
            handle["warnings"].append("opencode2.runtime_restarted: stale native permission/form requests were cancelled")

    async def _inject_mcp(self, handle: dict[str, Any], plan: CapabilityInjectionPlan | None,
                          bearer_token: str | None) -> None:
        if plan is None:
            return
        if plan.injection_kind is not InjectionKind.MCP:
            raise OpenCodeError(
                f"OpenCode 2 requires MCP injection; capability probe selected {plan.injection_kind.value}",
                code="opencode2.mcp.injection_unsupported")
        if self.owner._attach_url:
            handle["warnings"].append("opencode2.mcp.external_server: Muteki's MCP is only added to a managed server")
            return
        name = "muteki-" + hashlib.sha256(handle["conversation_thread_id"].encode()).hexdigest()[:16]
        raw = (plan.runtime_config or {}).get("mcpServers") or {}
        entry = next(iter(raw.values()), {}) if isinstance(raw, dict) else {}
        headers = entry.get("headers") or {}
        if isinstance(headers, list):
            headers = {item["name"]: item["value"] for item in headers}
        headers = dict(headers)
        for key, value in headers.items():
            if TOKEN_PLACEHOLDER in str(value):
                if not bearer_token:
                    raise OpenCodeError("MCP requires this session's capability grant", code="opencode2.mcp.token_missing")
                headers[key] = str(value).replace(TOKEN_PLACEHOLDER, bearer_token)
        await handle["client"].request("PUT", _location(f"/api/experimental/mcp/{quote(name, safe='')}", handle["cwd"]), body={
            "config": {"type": "remote", "url": entry.get("url") or plan.gateway_endpoint,
                       "headers": headers, "oauth": False}})
        handle.update(mcp_name=name, mcp_injected=1)

    def _queue(self, handle: dict[str, Any], kind: Any, native: str, payload: Any) -> None:
        self.owner._queue_event(handle, kind, native, dump_payload(payload) if hasattr(payload, "model_dump") else payload)

    def _belongs(self, handle: dict[str, Any], sid: str) -> bool:
        return sid == handle.get("external_session_id") or sid in handle["child_sessions"]

    def _calls_above(self, handle: dict[str, Any], sid: str) -> list[dict[str, Any]]:
        # T3 callsAbove / requestTurn: the outermost background call owns requests.
        calls = []
        while sid in handle["parents"]:
            call = next((c for c in handle["calls"].values() if c.get("child") == sid), None)
            if call is not None:
                calls.append(call)
            sid = handle["parents"][sid]
        return calls

    def _request_turn(self, handle: dict[str, Any], sid: str) -> str | None:
        background = [c for c in self._calls_above(handle, sid) if c["background"]]
        if background:
            return background[-1]["turn_id"] if background[-1]["status"] == "running" else None
        return handle.get("conversation_turn_id")

    def _end_execution(self, handle: dict[str, Any], kind: str, data: dict[str, Any]) -> None:
        # T3 endExecution / endIfSettled.
        if kind == "session.execution.succeeded" and handle["pending_steers"] and not handle["interrupted"]:
            handle["held_terminal"] = (kind, data)
            return
        handle["execution_ended"] = True
        self._queue(handle, "__terminal__", kind, data)

    def _on_report(self, handle: dict[str, Any], sid: str, data: dict[str, Any]) -> None:
        # T3 onReport: a background launch returns before the report settles its call.
        payload = data["item"].get("payload") or {}
        metadata = payload.get("metadata") or {}
        child = metadata.get("childID")
        if metadata.get("source") != "subagent" or not child:
            return
        handle["reports"].setdefault(sid, {})[data["inboxID"]] = {
            "child": child, "text": payload.get("text") or "", "inbox": data["inboxID"]}
        for call in handle["calls"].values():
            if call.get("child") == child:
                call["status"] = ("cancelled" if child in handle["stopped_children"] or child in handle.get("rejected_sessions", set())
                                  else "failed" if metadata.get("state") == "failed"
                                  else "cancelled" if metadata.get("state") == "cancelled" else "completed")
                previous = handle.get("routing_turn_id")
                handle["routing_turn_id"] = call["turn_id"]
                self.owner._agent_patch(handle, child, {"status": call["status"], "result": payload.get("text")})
                handle["routing_turn_id"] = previous

    def _take_reports(self, handle: dict[str, Any], sid: str) -> tuple[list[dict[str, Any]], bool]:
        # T3 takeReports: a Stop's reports must not create another assistant turn.
        reports = list(handle["reports"].pop(sid, {}).values())
        stopped = bool(reports) and all(r["child"] in handle["stopped_children"] for r in reports)
        for report in reports:
            handle["stopped_children"].discard(report["child"])
        if stopped:
            handle["suppressed_executions"].add(sid)
            self.owner._spawn(handle, self._stop_report_execution(handle, sid))
        return reports, stopped

    async def _stop_report_execution(self, handle: dict[str, Any], sid: str) -> None:
        try:
            await handle["client"].post(f"/api/session/{quote(sid, safe='')}/interrupt", {"resume": False}, timeout=15)
        except OpenCodeError as exc:
            self.owner._emit_warning(handle, "opencode2.background_stop_failed", str(exc))

    def _on_wake(self, handle: dict[str, Any], reports: list[dict[str, Any]]) -> None:
        # T3 onWake / offerWake. No prompt is submitted for this execution.
        wake = {"id": new_id("wake"), "events": [], "running": True, "dropped": False,
                "detail": "\n\n".join(r["text"] for r in reports),
                "first": reports[0]["inbox"] if reports else None,
                "after": reports[-1]["inbox"] if reports else None}
        handle["wakes"].append(wake)
        if handle.get("continuation_handler") is not None:
            handle["continuation_handler"]()

    def _replay(self, sid: str, wake: dict[str, Any]) -> None:
        # T3 replay marks dispatchIfCurrent stale before replaying native events.
        wake["dropped"] = True
        for name, event in wake["events"]:
            self._dispatch_checked(sid, name, event, replay=True)

    def _dispatch(self, sid: str, name: str, event: dict[str, Any]) -> None:
        try:
            self._dispatch_checked(sid, name, event)
        except (TypeError, ValueError, KeyError, AttributeError) as exc:
            handle = self.owner._sessions.get(sid)
            if handle is not None:
                error = OpenCodeError(
                    f"Invalid native event ({type(exc).__name__}): {json.dumps(event, ensure_ascii=False)}",
                    code="opencode2.event_invalid")
                handle["fatal_event_error"] = error
                self._queue(handle, "__error__", name, error)

    def _dispatch_checked(self, sid: str, name: str, event: dict[str, Any], *, replay: bool = False) -> None:
        handle = self.owner._sessions.get(sid)
        if handle is None:
            return
        kind = str(event.get("type") or name)
        data = event.get("data")
        if not isinstance(data, dict):
            if kind.startswith(("session.", "permission.", "form.")):
                error = OpenCodeError(f"Invalid OpenCode 2 event: {json.dumps(event, ensure_ascii=False)}", code="opencode2.event_invalid")
                handle["fatal_event_error"] = error
                self._queue(handle, "__error__", kind, error)
            return
        native_sid = str(data.get("sessionID") or (data.get("form") or {}).get("sessionID") or "")
        ended = kind in {"session.execution.succeeded", "session.execution.failed", "session.execution.interrupted"}
        if kind == "session.execution.started":
            handle["busy"].add(native_sid)
        elif ended:
            handle["busy"].discard(native_sid)
        if kind == "session.created" and data.get("parentID") and self._belongs(handle, str(data["parentID"])):
            native_sid = str(data["sessionID"])
            handle["parents"][native_sid] = str(data["parentID"])
            handle["child_sessions"][native_sid] = native_sid
        if not self._belongs(handle, native_sid):
            return
        # T3 holderOf / handleEvent: root wake and newly announced descendants
        # wait for their own continuation; existing background children keep routing.
        if not replay and handle["wakes"] and handle["wakes"][-1]["running"]:
            ancestor = native_sid
            while ancestor in handle["parents"] and not any(c.get("child") == ancestor for c in handle["calls"].values()):
                ancestor = handle["parents"][ancestor]
            if ancestor == handle["external_session_id"]:
                wake = handle["wakes"][-1]
                wake["events"].append((name, event))
                if native_sid == handle["external_session_id"] and ended:
                    wake["running"] = False
                return
        previous = handle.get("routing_turn_id")
        handle["routing_turn_id"] = self._request_turn(handle, native_sid)
        try:
            self._route(sid, handle, kind, data, native_sid, ended)
        finally:
            handle["routing_turn_id"] = previous

    def _route(self, sid: str, handle: dict[str, Any], kind: str, data: dict[str, Any], native_sid: str, ended: bool) -> None:
        if native_sid in handle["suppressed_executions"]:
            if ended:
                handle["suppressed_executions"].discard(native_sid)
            return
        if kind == "session.created" and data.get("parentID"):
            self.owner._agent_patch(handle, native_sid, {
                "parent_id": data["parentID"] if data["parentID"] != handle["external_session_id"] else None,
                "session_ref": native_sid, "status": "running",
                "title": data.get("title"), "role": data.get("agent"),
                "model": (data.get("model") or {}).get("id"),
            })
            return
        if kind == "session.inbox.enqueued" and (data.get("item") or {}).get("type") == "synthetic":
            self._on_report(handle, native_sid, data)
            return
        child = native_sid != handle["external_session_id"]
        agent_id = native_sid if child else None
        if kind in {"session.inbox.delivered", "session.inbox.cancelled"}:
            handle["reports"].get(native_sid, {}).pop(data.get("inboxID"), None)
        if kind in {"session.inbox.delivered", "session.inbox.cancelled"} and not child:
            inbox_id = str(data.get("inboxID") or "")
            if inbox_id in handle["pending_steers"]:
                handle["pending_steers"].discard(inbox_id)
                handle["settled_steers"].add(inbox_id)
                if kind == "session.inbox.cancelled" and not handle["pending_steers"]:
                    held = handle.pop("held_terminal", None)
                    if held is not None:
                        self._end_execution(handle, held[0], held[1])
            handle["stranded_steers"].discard(inbox_id)
            if kind == "session.inbox.cancelled":
                return
            handle["delivered"].add(inbox_id)
            if handle.get("operation_input") == data.get("inboxID"):
                handle["operation_delivered"] = True
                pending = handle.pop("operation_pending", None)
                operation = handle.get("operation_terminal")
                if pending is not None and operation is not None and not operation.done():
                    operation.set_result(pending)
            if handle.get("terminal_pending") and handle.get("input_id") in handle["delivered"]:
                terminal = handle.pop("terminal_pending")
                self._end_execution(handle, terminal[0], terminal[1])
        elif kind.startswith("session.execution."):
            if kind == "session.execution.started":
                reports, stopped = self._take_reports(handle, native_sid)
                if stopped:
                    return
                if child:
                    self.owner._agent_patch(handle, native_sid, {"status": "running"})
                else:
                    if (not handle.get("current_turn_id") or handle.get("execution_ended")) and handle.get("operation_terminal") is None:
                        self._on_wake(handle, reports)
                        return
                    handle["native_execution_started"] = True
                    handle.pop("held_terminal", None)
                    if handle.get("operation_terminal") is not None:
                        handle["operation_started"] = True
                return
            if kind not in {"session.execution.succeeded", "session.execution.failed", "session.execution.interrupted"}:
                return
            if child:
                status = {"session.execution.succeeded": "completed", "session.execution.failed": "failed",
                          "session.execution.interrupted": "cancelled"}[kind]
                if native_sid in handle.get("rejected_sessions", set()):
                    status = "cancelled"
                result = "\n".join(text for key, text in handle["texts"].items() if key.startswith(native_sid + ":"))
                self.owner._agent_patch(handle, native_sid, {"status": status, "result": result or None,
                    "error": json.dumps(data["error"], ensure_ascii=False) if data.get("error") else None})
                return
            operation = handle.get("operation_terminal")
            if operation is not None and not operation.done() and handle.get("operation_started"):
                if handle.get("operation_delivered"):
                    operation.set_result((kind, data))
                else:
                    handle["operation_pending"] = (kind, data)
            if handle.get("settle_event") is not None:
                handle["settle_event"].set()
                handle["unsettled"] = False
            if handle.get("event_sink") is not None:
                if not handle.get("native_execution_started") and not handle.get("interrupted"):
                    return  # A prior idle execution cannot complete a new inbox item.
                if handle.get("input_id") in handle["delivered"] or handle.get("interrupted") or handle.get("compacting"):
                    self._end_execution(handle, kind, data)
                else:
                    handle["terminal_pending"] = (kind, data)
        elif kind.startswith(("session.text.", "session.reasoning.")):
            self._text_event(handle, kind, data, agent_id)
        elif kind.startswith("session.tool."):
            self._tool_event(handle, kind, data, agent_id)
        elif kind == "permission.asked":
            action = data.get("action")
            props = {**data, "permission": "bash" if action == "shell" else action,
                     "patterns": data.get("resources") or [], "always": data.get("save") or [],
                     "tool": {"callID": (data.get("source") or {}).get("id")},
                     "background": any(c["background"] for c in self._calls_above(handle, native_sid))}
            self.owner._handle_permission(handle, props)
        elif kind == "form.created":
            self._form_event(handle, data.get("form") or {})
        elif kind in {"permission.replied", "form.replied", "form.cancelled"}:
            # T3 route: requests answered elsewhere or cancelled by the server settle here.
            request_id = str(data.get("requestID") or data.get("id") or "")
            if request_id in handle.get("answering_requests", set()):
                return
            if kind == "permission.replied":
                pending = handle["pending_approvals"].pop(request_id, None)
                handle["approval_requests"].pop(request_id, None)
                if pending is not None:
                    from muteki.platform.contracts.agent_events import ApprovalResolvedPayload
                    self._queue(handle, AgentEventType.APPROVAL_RESOLVED, kind, ApprovalResolvedPayload(
                        approval_id=request_id, decision="deny" if data.get("reply") == "reject" else "allow"))
            elif handle["pending_questions"].pop(request_id, None) is not None:
                self._queue(handle, AgentEventType.USER_INPUT_RESOLVED, kind, UserInputResolvedPayload(
                    request_id=request_id, outcome="answered" if kind == "form.replied" else "cancelled"))
                if handle.get("presented_form") == request_id:
                    handle["presented_form"] = None
                    self._present_form(handle)
        elif kind in {"session.step.ended", "session.step.failed"} and isinstance(data.get("tokens"), dict):
            from muteki.core.usage import sum_token_buckets
            tokens, cache = data["tokens"], data["tokens"].get("cache") or {}
            values = {"input_tokens": sum_token_buckets(tokens.get("input"), cache.get("read"), cache.get("write")),
                      "output_tokens": sum_token_buckets(tokens.get("output"), tokens.get("reasoning")),
                      "reasoning_tokens": tokens.get("reasoning"), "cached_input_tokens": cache.get("read"),
                      "cache_write_tokens": cache.get("write")}
            cost = data.get("cost")
            payload = UsagePayload(scope="message", usage_id=str(data.get("assistantMessageID")),
                                   **values, cost_usd=cost if isinstance(cost, (int, float))
                                   and not isinstance(cost, bool) and cost > 0 else None)
            self._queue(handle, AgentEventType.USAGE_UPDATED, kind, payload)
        elif kind in {"session.compaction.completed", "session.compaction.ended"}:
            self.owner._context_compacted(sid)

    def _text_event(self, handle: dict[str, Any], kind: str, data: dict[str, Any], agent_id: str | None) -> None:
        reasoning = kind.startswith("session.reasoning.")
        key = f"{data['sessionID']}:{data.get('assistantMessageID')}:{data.get('ordinal')}"
        texts = handle["reasoning" if reasoning else "texts"]
        previous = texts.get(key, "")
        if kind.endswith(".delta"):
            delta = str(data.get("delta") or "")
            texts[key] = previous + delta
        elif kind.endswith(".ended"):
            full = str(data.get("text") or "")
            delta = full[len(previous):] if full.startswith(previous) else ""
            texts[key] = full
            if not full.startswith(previous):
                self._queue(handle, AgentEventType.RUNTIME_WARNING, kind, RuntimeWarningPayload(
                    kind="degraded", code="opencode2.text_reconciled",
                    message="Native completed text differs from its received deltas; the complete value is retained"))
        else:
            return
        if reasoning:
            text = texts[key] if kind.endswith(".ended") else delta
            if text:
                self._queue(handle, AgentEventType.REASONING_SUMMARY, kind, ReasoningPayload(
                    text=text, channel="thinking", partial=not kind.endswith(".ended"), item_id=key,
                    native={"agent_id": agent_id} if agent_id else {}))
        elif delta and not agent_id:
            self._queue(handle, AgentEventType.MESSAGE_DELTA, kind, MessageDeltaPayload(
                text=delta, message_id=str(data.get("assistantMessageID"))))
        if not reasoning and not agent_id and kind.endswith(".ended"):
            # Native ended events are authoritative, including after a lost
            # ephemeral delta. Retain the full value in the canonical stream.
            self._queue(handle, AgentEventType.MESSAGE_COMPLETED, kind, MessageCompletedPayload(
                text=texts[key], message_id=str(data.get("assistantMessageID"))))

    def _tool_event(self, handle: dict[str, Any], kind: str, data: dict[str, Any], agent_id: str | None) -> None:
        call = str(data.get("id") or "")
        if not call:
            return
        key = str(data["sessionID"]) + ":" + call
        tool = handle["tools"].setdefault(key, {"tool_call_id": call, "agent_id": agent_id,
                                              "message_id": data.get("assistantMessageID")})
        if kind == "session.tool.input.started":
            tool["name"] = str(data.get("name") or "tool")
            if tool["name"] == "subagent":
                handle["calls"][key] = {"turn_id": handle.get("routing_turn_id") or handle.get("conversation_turn_id"),
                                         "background": False, "status": "running", "parent": data["sessionID"]}
            tool["kind"] = ("agent" if tool["name"] == "subagent" else "command" if tool["name"] == "shell"
                            else "file_change" if tool["name"] in {"edit", "write", "apply_patch", "patch"} else "other")
            tool["status"] = "running"
            self._queue(handle, AgentEventType.TOOL_STARTED, kind, ToolPayload(**tool))
        elif kind == "session.tool.called":
            tool["input"] = data.get("input") or {}
            if key in handle["calls"]:
                handle["calls"][key]["background"] = tool["input"].get("background") is True
            self._queue(handle, AgentEventType.TOOL_PROGRESS, kind, ToolPayload(**tool))
            if tool.get("name") in {"todowrite", "todo"}:
                self.owner._emit_plan(handle, tool["input"].get("todos") or tool["input"].get("tasks") or [])
        elif kind in {"session.tool.progress", "session.tool.success", "session.tool.failed"}:
            metadata = data.get("metadata") or {}
            if tool.get("name") == "subagent" and metadata.get("sessionID"):
                child = str(metadata["sessionID"])
                handle["calls"][key]["child"] = child
                handle["child_sessions"][child] = child
                parent = str(data["sessionID"])
                handle["parents"][child] = parent
                self.owner._agent_patch(handle, child, {"call_id": call, "session_ref": child,
                    "parent_id": parent if parent != handle["external_session_id"] else None,
                    "request": (tool.get("input") or {}).get("prompt")})
            if kind == "session.tool.progress":
                self._queue(handle, AgentEventType.TOOL_PROGRESS, kind, ToolPayload(**tool, native={"metadata": metadata}))
                return
            # T3 onTurnEvent: background success means launched, not completed.
            if key in handle["calls"] and metadata.get("status") == "running":
                return
            tool["status"] = "completed" if kind.endswith(".success") else "failed"
            if key in handle["calls"]:
                handle["calls"][key]["status"] = tool["status"]
            tool["output"] = data.get("content")
            if data.get("error") is not None:
                tool["error"] = json.dumps(data["error"], ensure_ascii=False)
            self._queue(handle, AgentEventType.TOOL_COMPLETED, kind, ToolPayload(**tool))

    def _form_event(self, handle: dict[str, Any], form: dict[str, Any]) -> None:
        questions = []
        unsupported = []
        for index, field in enumerate(form.get("fields") or []):
            if field.get("type") not in {"string", "multiselect"} or field.get("hidden") or field.get("when"):
                unsupported.append(field.get("key"))
                continue
            question = normalize_question({
                "question_id": field["key"], "header": field.get("title") or form.get("title"),
                "question": field.get("description") or field.get("title") or form.get("title"),
                "multi_select": field["type"] == "multiselect", "allow_free_text": field.get("custom") is True or not field.get("options"),
                "options": [{"label": option.get("label") or option["value"], "value": option["value"],
                             "description": option.get("description") or ""} for option in field.get("options") or []],
            }, index=index)
            if question is not None:
                questions.append(question)
        if unsupported or not questions:
            self.owner._emit_warning(handle, "opencode2.form_unsupported",
                                     f"Form cannot be represented: {json.dumps(form, ensure_ascii=False)}")
            self.owner._spawn(handle, self._cancel_form(handle, form))
            return
        background = any(c["background"] for c in self._calls_above(handle, form["sessionID"]))
        handle["pending_questions"][form["id"]] = {**form, "normalized_questions": questions,
            "background": background, "origin_turn_id": handle.get("routing_turn_id")}
        self._present_form(handle)

    def _present_form(self, handle: dict[str, Any]) -> None:
        # Muteki has one form slot. Keep all native pending forms and present
        # the next one only after the current form is settled (never overwrite it).
        if handle.get("presented_form") or not handle["pending_questions"]:
            return
        form = next(iter(handle["pending_questions"].values()))
        handle["presented_form"] = form["id"]
        previous = handle.get("routing_turn_id")
        handle["routing_turn_id"] = form.get("origin_turn_id")
        self._queue(handle, AgentEventType.USER_INPUT_REQUESTED, "form.created", UserInputRequestedPayload(
            request_id=form["id"], user_input_kind="opencode2.form", questions=form["normalized_questions"],
            agent_id=form["sessionID"] if form["sessionID"] != handle["external_session_id"] else None,
            response_actions=["submit", "cancel"], native={"background": form["background"]}))
        handle["routing_turn_id"] = previous

    async def _cancel_form(self, handle: dict[str, Any], form: dict[str, Any]) -> None:
        await handle["client"].request("DELETE", f"/api/session/{quote(form['sessionID'], safe='')}/form/{quote(form['id'], safe='')}")

    async def reply_permission(self, handle: dict[str, Any], request_id: str, reply: str,
                               session_id: str = "", message: str = "") -> None:
        await handle["client"].post(
            f"/api/session/{quote(session_id or handle['external_session_id'], safe='')}/permission/{quote(request_id, safe='')}/reply",
            {"decision": reply, **({"message": message} if message else {})})

    def send(self, session: AgentSessionRef, input: AgentInput) -> AsyncIterator[AgentEvent]:
        if isinstance(input, ApprovalResponseInput):
            return self.owner._approval_response_stream(session, input)
        if isinstance(input, UserInputResponseInput):
            return self._answer_form(session, input)
        if isinstance(input, MessageInput):
            return self._turn_stream(session, input)
        return self.owner.unsupported_input_stream(session, input)

    def _event(self, session: AgentSessionRef, kind: AgentEventType, native: str, payload: Any) -> AgentEvent:
        owner = self.owner
        record = owner._tracker.get(session.agent_session_id)
        handle = owner._sessions[session.agent_session_id]
        return owner.emit(build_event(
            kind, owner.sequencer_for(session.agent_session_id),
            agent_session_id=session.agent_session_id, external_session_id=handle["external_session_id"],
            turn_id=handle.get("current_turn_id"), native_type=native, payload=payload,
            run_id=record.run_id if record else None,
            execution_generation=record.execution_generation if record else None))

    async def _answer_form(self, session: AgentSessionRef, input: UserInputResponseInput) -> AsyncIterator[AgentEvent]:
        owner = self.owner
        handle = owner._sessions[session.agent_session_id]
        form = handle["pending_questions"].get(input.payload.request_id)
        if form is None:
            yield self._event(session, AgentEventType.RUNTIME_WARNING, "form.stale", RuntimeWarningPayload(
                kind="notice", code="user_input.stale", message="OpenCode 2 form is no longer pending"))
            return
        answered = input.payload.decision == "submit"
        answers = dict(input.payload.answers)
        if answered and not answers:
            answers = expand_legacy_text_answers({"questions": form["normalized_questions"]}, input.text)
        native_answer = {}
        handle.setdefault("answering_requests", set()).add(form["id"])
        try:
            if answered:
                for field in form["fields"]:
                    values, text = _answer_values(answers.get(field["key"]))
                    if text and text not in values:
                        values.append(text)
                    if values:
                        native_answer[field["key"]] = values if field["type"] == "multiselect" else ", ".join(values)
                await handle["client"].post(
                    f"/api/session/{quote(form['sessionID'], safe='')}/form/{quote(form['id'], safe='')}/reply", {"answer": native_answer})
            else:
                await self._cancel_form(handle, form)
                if form.get("background"):
                    handle.setdefault("rejected_sessions", set()).add(form["sessionID"])
                else:
                    handle["turn_rejected"] = True
        finally:
            handle["answering_requests"].discard(form["id"])
        handle["pending_questions"].pop(form["id"], None)
        handle["presented_form"] = None
        yield self._event(session, AgentEventType.USER_INPUT_RESOLVED, "form.replied" if answered else "form.cancelled",
                          UserInputResolvedPayload(request_id=form["id"], outcome="answered" if answered else "cancelled",
                                                   answers=answers if answered else None))
        self._present_form(handle)

    async def _active_sessions(self, handle: dict[str, Any]) -> set[str]:
        active = _data(await handle["client"].get("/api/session/active", timeout=5), "session.active")
        if not isinstance(active, dict) or any(not isinstance(value, dict) or value.get("type") != "running" for value in active.values()):
            raise OpenCodeError(f"Invalid session.active value: {json.dumps(active, ensure_ascii=False)}",
                                code="opencode2.active_invalid")
        return set(active).intersection({handle["external_session_id"], *handle["child_sessions"]})

    async def _active(self, handle: dict[str, Any]) -> bool:
        return bool(await self._active_sessions(handle))

    async def _settle(self, handle: dict[str, Any], operation: str) -> None:
        deadline = time.monotonic() + 15
        while await self._active(handle):
            if time.monotonic() >= deadline:
                handle["unsettled"] = True
                raise OpenCodeError(f"OpenCode 2 {operation} did not settle within 15 seconds",
                                    code="opencode2.settle_timeout")
            await asyncio.sleep(.1)
        handle["unsettled"] = False

    async def _apply_mode(self, handle: dict[str, Any], mode: str) -> None:
        sid, client = quote(handle["external_session_id"], safe=""), handle["client"]
        agent = "plan" if mode == "plan" else "build"
        if mode == "plan" and not handle["plan_agent"]:
            raise OpenCodeError("Native OpenCode 2 plan agent is unavailable", code="opencode2.plan_unsupported")
        if agent != handle["agent"]:
            await client.post(f"/api/session/{sid}/agent", {"agent": agent})
            handle["agent"] = agent
        rules = await self._rules(handle, mode == "plan")
        if rules != handle["permissions"]:
            await client.patch(f"/api/session/{sid}", {"permissions": rules})
            handle["permissions"] = rules
        if handle["model_ref"]:
            await client.post(f"/api/session/{sid}/model", {"model": handle["model_ref"]})
        # Like T3, use a durable instruction entry: v2 prompts have no system field.
        instructions = ("Muteki conversation tools are available through the connected MCP server."
                        if handle.get("mcp_name") else "")
        if handle.get("instructions") != instructions:
            await client.request("PUT", f"/api/experimental/session/{sid}/instructions/entries/muteki",
                                 body={"value": instructions})
            handle["instructions"] = instructions
        handle["interaction_mode"] = mode

    async def _turn_stream(self, session: AgentSessionRef, input: MessageInput, *, wake_id: str | None = None) -> AsyncIterator[AgentEvent]:
        owner = self.owner
        handle = owner._sessions[session.agent_session_id]
        if handle.get("fatal_event_error") is not None:
            raise handle["fatal_event_error"]
        if handle.get("current_turn_id"):
            raise OpenCodeError("OpenCode 2 session already has a running turn", code="opencode2.turn_busy")
        if handle["sse_task"].done():
            await handle["sse_task"]
            raise OpenCodeError("OpenCode 2 event stream is closed", code="opencode2.sse_closed")
        if handle["unsettled"] and await self._active(handle):
            yield self._event(session, AgentEventType.TURN_FAILED, "opencode2.session_unsettled", TurnFailedPayload(
                error=owner.failure(FailureCategory.TRANSPORT, "opencode2.session_unsettled",
                                    message="The previous OpenCode 2 execution has not stopped")))
            return
        handle["unsettled"] = False
        await self._cancel_stranded_steers(handle)
        wake = next((w for w in handle["wakes"] if w["id"] == wake_id and not w["dropped"]), None)
        # T3 runWake uses the first report as the native history boundary.
        handle["current_turn_id"] = (wake.get("first") if wake else None) or _opencode_message_id()
        root_prefix = handle["external_session_id"] + ":"
        # T3 beginTurn: a new root turn owns fresh buffers; child turns survive.
        for field in ("texts", "reasoning", "tools"):
            handle[field] = {key: value for key, value in handle[field].items() if not key.startswith(root_prefix)}
        handle.update(input_id=handle["current_turn_id"], event_sink=asyncio.Queue(),
                      delivered=set(), admitted=set(), execution_ended=False,
                      interrupted=False, turn_rejected=False, terminal_pending=None,
                      native_execution_started=False, normal_terminal=False)
        handle["pending_steers"].clear()
        handle["settled_steers"].clear()
        handle.pop("held_terminal", None)
        for item in handle.pop("deferred_events", []):
            handle["event_sink"].put_nowait(item)
        if not handle["turns"]:
            yield self._event(session, AgentEventType.SESSION_RESUMED if handle["resumed"] else AgentEventType.SESSION_STARTED,
                              "opencode2.session.start", SessionPayload(transport="http+sse", adapter_id=owner.id,
                              instance_id=owner.identity.instance_id, cwd=handle["cwd"], native={
                                  "runtime_generation": "v2", "mcp_injected": handle["mcp_injected"],
                                  "warnings": handle["warnings"]}))
        for warning in handle["warnings"]:
            yield self._event(session, AgentEventType.RUNTIME_WARNING, "opencode2.runtime.warning",
                              RuntimeWarningPayload(kind="degraded", code="opencode2.runtime_setup", message=warning))
        handle["warnings"] = []
        yield self._event(session, AgentEventType.TURN_STARTED, "opencode2.turn.start", TurnStartedPayload())
        record = owner._tracker.get(session.agent_session_id)
        async def abort(_failure: Any) -> None:
            await self.interrupt(session)
        proc = _as_process(handle.get("proc"))
        runner = TurnRunner(owner, session, turn_id=handle["current_turn_id"],
                            limits=TurnLimits(overall_s=owner.conversation_turn_timeout(
                                handle.get("conversation_thread_id"), owner._prompt_timeout)), auto_ack=False,
                            exit_watch=proc.wait if proc is not None else None, on_abort=abort,
                            run_id=record.run_id if record else None,
                            execution_generation=record.execution_generation if record else None)
        handle["runner"] = runner
        try:
            async for event in runner.stream(self._raw_turn(session, input, runner, wake_id=wake_id)):
                yield event
        finally:
            try:
                if (not handle.get("normal_terminal") and handle.get("current_turn_id")
                        and await self._active(handle)):
                    await self.interrupt(session)
            finally:
                handle["turns"] += 1
                handle["event_sink"] = None
                handle["current_turn_id"] = None
                handle.pop("runner", None)

    async def _raw_turn(self, session: AgentSessionRef, input: MessageInput, runner: TurnRunner, *, wake_id: str | None = None) -> AsyncIterator[AgentEvent]:
        owner = self.owner
        handle = owner._sessions[session.agent_session_id]
        mode = str(input.payload.interaction_mode or "default")
        started = time.monotonic()
        try:
            if wake_id is None:
                await self._apply_mode(handle, mode)
            if handle["interrupted"]:
                yield self._event(session, AgentEventType.TURN_FAILED, "opencode2.interrupt_before_admission",
                                  TurnFailedPayload(error=owner.failure(FailureCategory.CANCELLED, "interrupted",
                                                                       message="Turn interrupted before admission")))
                return
            sid = quote(handle["external_session_id"], safe="")
            invocation = input.payload.runtime_capability.get("invocation") or {}
            runner.mark_sent()
            if wake_id is not None:
                # T3 runWake: replay the oldest held execution, never submit a prompt.
                wake = next((w for w in handle["wakes"] if w["id"] == wake_id and not w["dropped"]), None)
                handle["native_execution_started"] = True
                handle["delivered"].add(handle["input_id"])
                if wake is None:
                    self._end_execution(handle, "session.execution.succeeded", {})
                else:
                    handle["wakes"].remove(wake)
                    self._replay(session.agent_session_id, wake)
            elif invocation.get("transport") == "opencode2.command":
                await handle["client"].post(f"/api/session/{sid}/command", {
                    "name": invocation["command_name"], "text": input.text})
                handle["compacting"] = True
            else:
                # T3 takeRunningWake: a user prompt joins the still-running wake.
                wake = handle["wakes"][-1] if handle["wakes"] else None
                if wake is not None and wake["running"]:
                    handle["wakes"].pop()
                    handle["native_execution_started"] = True
                    self._replay(session.agent_session_id, wake)
                body = {"id": handle["input_id"], "text": input.text}
                if input.payload.attachments:
                    body["files"] = self._files(input.payload.attachments)
                accepted = _data(await handle["client"].post(f"/api/session/{sid}/prompt", body, timeout=30), "session.prompt")
                if not isinstance(accepted, dict) or accepted.get("id") != handle["input_id"]:
                    raise OpenCodeError("Prompt acknowledgement has a different input id", code="opencode2.admission_invalid")
            handle["admitted"].add(handle["input_id"])
            runner.ack()
            while True:
                read = asyncio.create_task(handle["event_sink"].get())
                try:
                    done, _ = await asyncio.wait({read, handle["sse_task"]}, return_when=asyncio.FIRST_COMPLETED)
                    if read not in done:
                        await handle["sse_task"]
                        raise OpenCodeError("OpenCode 2 event stream ended during a turn", code="opencode2.sse_closed")
                    kind, native, payload = read.result()
                finally:
                    if not read.done():
                        read.cancel()
                    await asyncio.gather(read, return_exceptions=True)
                if kind == "__error__":
                    raise payload
                if kind == "__terminal__":
                    if native == "session.execution.failed" and not handle["turn_rejected"]:
                        raise OpenCodeError(json.dumps(payload.get("error") or payload, ensure_ascii=False),
                                            code="opencode2.execution_failed")
                    text = "\n".join(value for key, value in handle["texts"].items()
                                     if key.startswith(handle["external_session_id"] + ":") and value)
                    if mode == "plan" and text:
                        yield self._event(session, AgentEventType.PLAN_UPDATED, "opencode2.plan.completed",
                                          PlanPayload(explanation=text, phase="completed", native={"agent": "plan"}))
                    cancelled = handle["turn_rejected"]
                    interrupted = handle["interrupted"] or native == "session.execution.interrupted"
                    if cancelled or interrupted:
                        category = FailureCategory.CANCELLED
                        reason = "approval_denied" if cancelled else "interrupted"
                        yield self._event(session, AgentEventType.TURN_FAILED, native, TurnFailedPayload(
                            error=owner.failure(category, reason, message="User declined the request" if cancelled else "Turn interrupted")))
                    else:
                        handle["normal_terminal"] = True
                        common = {"agent_session_id": session.agent_session_id,
                                  "external_session_id": handle["external_session_id"], "turn_id": handle["current_turn_id"]}
                        record = owner._tracker.get(session.agent_session_id)
                        common.update(run_id=record.run_id if record else None,
                                      execution_generation=record.execution_generation if record else None)
                        for event in owner.completed_turn_events(owner.sequencer_for(session.agent_session_id), text=text,
                            common=common, native_type=native,
                            payload=TurnCompletedPayload(stop_reason="stop", duration_ms=int((time.monotonic()-started)*1000))):
                            yield event
                    break
                if isinstance(kind, AgentEventType):
                    yield self._event(session, kind, native, payload)
        except (OpenCodeError, ValueError, OSError) as exc:
            yield self._event(session, AgentEventType.TURN_FAILED, "opencode2.turn.failed", TurnFailedPayload(
                error=owner.exception_failure(exc, FailureCategory.PROVIDER if getattr(exc, "code", "") == "opencode2.execution_failed"
                                              else FailureCategory.TRANSPORT,
                                              getattr(exc, "code", "opencode2.protocol_invalid"),
                                              message="OpenCode 2 回合执行失败")))
        finally:
            handle["stranded_steers"].update(handle["pending_steers"])
            handle.pop("compacting", None)
            # T3 finishTurn retains requests belonging to live background work.
            for field in ("pending_approvals", "pending_questions"):
                for request_id, request in list(handle[field].items()):
                    if not (handle.get("normal_terminal") and request.get("background")):
                        handle[field].pop(request_id, None)
                        handle["approval_requests"].pop(request_id, None)
            if handle.get("presented_form") not in handle["pending_questions"]:
                handle["presented_form"] = None

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        handle = self.owner._sessions[session.agent_session_id]
        handle["interrupted"] = True
        handle["stranded_steers"].update(handle["pending_steers"])
        await self._cancel_stranded_steers(handle)
        # Native background children may outlive the parent execution. T3
        # stops the owned children before the parent so no report wakes it.
        # T3 stopBackground marks each caller's reports before issuing Stop.
        handle["stopped_children"].update(handle["child_sessions"])
        for reports in handle["reports"].values():
            handle["stopped_children"].update(r["child"] for r in reports.values())
        for wake in handle["wakes"]:
            wake["dropped"] = True
        handle["wakes"].clear()
        for child in list(handle["child_sessions"]):
            try:
                await handle["client"].post(f"/api/session/{quote(child, safe='')}/interrupt", {"resume": False}, timeout=15)
            except OpenCodeError as exc:
                if exc.status != 404:
                    raise
            for call in handle["calls"].values():
                if call.get("child") == child:
                    call["status"] = "cancelled"
        await handle["client"].post(f"/api/session/{quote(handle['external_session_id'], safe='')}/interrupt", {"resume": False}, timeout=15)
        await self._settle(handle, "interrupt")
        return CommandReceipt(command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
                              aggregate=AggregateRef(type="agent_session", id=session.agent_session_id))

    async def steer(self, session: AgentSessionRef, input: SteerInput) -> CommandReceipt:
        handle = self.owner._sessions[session.agent_session_id]
        if not handle.get("current_turn_id"):
            return self.owner.unsupported_receipt("steer", "active_turn", session=session)
        client_id = input.payload.client_user_message_id
        message_id = ("msg_muteki_steer_" + hashlib.sha256(
            f"{handle['external_session_id']}:{client_id}".encode()).hexdigest()
            if client_id else _opencode_message_id())
        if message_id not in handle["settled_steers"] and message_id not in handle["admitted"]:
            body = {"id": message_id, "text": input.text, "delivery": "steer"}
            if input.payload.attachments:
                body["files"] = self._files(input.payload.attachments)
            # Register before the HTTP request: the prior execution can end
            # while admission is in flight, before this item is delivered.
            handle["pending_steers"].add(message_id)
            try:
                accepted = _data(await handle["client"].post(
                    f"/api/session/{quote(handle['external_session_id'], safe='')}/prompt", body), "session.prompt")
                if not isinstance(accepted, dict) or accepted.get("id") != message_id:
                    raise OpenCodeError("Steer acknowledgement has a different id", code="opencode2.admission_invalid")
                handle["admitted"].add(message_id)
            except BaseException:
                handle["stranded_steers"].add(message_id)
                handle["pending_steers"].discard(message_id)
                held = handle.pop("held_terminal", None) if not handle["pending_steers"] else None
                if held is not None:
                    self._end_execution(handle, held[0], held[1])
                raise
        return CommandReceipt(command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
                              aggregate=AggregateRef(type="agent_session", id=session.agent_session_id))

    @staticmethod
    def _files(attachments: list[AttachmentRef]) -> list[dict[str, str]]:
        files = []
        for attachment in attachments:
            path = attachment.path or attachment.workspace_path or attachment.cas_path
            if not path or not Path(path).is_absolute() or attachment.delivery == "native_image":
                raise OpenCodeError("OpenCode 2 attachment requires an authorized absolute file path",
                                    code="opencode2.attachment_invalid")
            files.append({"uri": Path(path).as_uri(), **({"name": attachment.name} if attachment.name else {})})
        return files

    async def _cancel_stranded_steers(self, handle: dict[str, Any]) -> None:
        for inbox_id in tuple(handle["stranded_steers"]):
            await handle["client"].request("DELETE",
                f"/api/session/{quote(handle['external_session_id'], safe='')}/inbox/{quote(inbox_id, safe='')}")
            handle["stranded_steers"].discard(inbox_id)
            handle["pending_steers"].discard(inbox_id)

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        handle = self.owner._sessions[session.agent_session_id]
        return self._turn_stream(session, MessageInput(text=handle["options"].resume_prompt or "Continue from where you left off."))

    async def operation(self, session: AgentSessionRef, name: str, arguments: str) -> dict[str, Any]:
        handle = self.owner._sessions[session.agent_session_id]
        name = name.strip().lstrip("/")
        client, sid = handle["client"], quote(handle["external_session_id"], safe="")
        if name in {"mcp", "agents", "status"}:
            path = _location("/api/mcp" if name == "mcp" else "/api/agent", handle["cwd"]) if name != "status" else f"/api/session/{sid}"
            return {"status": "completed", "result": _data(await client.get(path), name)}
        if name not in {"compact", "summarize"} or arguments or handle.get("current_turn_id"):
            raise OpenCodeError("Requested OpenCode 2 operation is unavailable", code="opencode2.operation_unsupported")
        terminal = asyncio.get_running_loop().create_future()
        handle["operation_terminal"] = terminal
        try:
            message_id = _opencode_message_id()
            handle.update(operation_input=message_id, operation_started=False, operation_delivered=False)
            accepted = _data(await client.post(f"/api/session/{sid}/compact", {"id": message_id}), "session.compact")
            if not isinstance(accepted, dict) or accepted.get("id") != message_id:
                raise OpenCodeError("Compaction acknowledgement has a different id", code="opencode2.admission_invalid")
            # HTTP accepts an inbox item before execution starts; an immediate
            # idle snapshot cannot prove that the compaction finished.
            done, _ = await asyncio.wait({terminal, handle["sse_task"]}, return_when=asyncio.FIRST_COMPLETED)
            if terminal not in done:
                await handle["sse_task"]
                raise OpenCodeError("Event stream ended during compaction", code="opencode2.sse_closed")
            kind, data = terminal.result()
            if kind != "session.execution.succeeded":
                raise OpenCodeError(json.dumps(data, ensure_ascii=False), code="opencode2.compaction_failed")
            await self._settle(handle, "compaction")
        finally:
            for key in ("operation_terminal", "operation_input", "operation_started", "operation_delivered", "operation_pending"):
                handle.pop(key, None)
            terminal.cancel()
        return {"status": "completed", "message": "上下文已由 OpenCode 原生压缩"}

    async def snapshot(self, session: AgentSessionRef | None) -> RuntimeCapabilitySnapshot:
        owner = self.owner
        from .base import BaseExternalAgentAdapter
        snapshot = await BaseExternalAgentAdapter.runtime_capability_snapshot(owner, session)
        if session is None:
            return snapshot
        handle = owner._sessions.get(session.agent_session_id)
        if handle is None:
            return snapshot
        items = list(snapshot.items)
        for row in handle.get("available_commands") or []:
            name = str(row.get("name") or row.get("id") or "")
            if name:
                items.append(dynamic_command_item(
                    adapter_id=owner.id, engine="opencode", name=name, channel="app_server_rpc",
                    description=str(row.get("description") or ""),
                    invocation={"transport": "opencode2.command", "command_name": name}))
        return RuntimeCapabilitySnapshot(**{**snapshot.model_dump(), "items": items})

    async def teardown(self, session: AgentSessionRef) -> str:
        owner = self.owner
        handle = owner._sessions[session.agent_session_id]
        handle["continuation_handler"] = None
        try:
            proc = _as_process(handle.get("proc"))
            reachable = proc is None or proc.returncode is None
            if reachable and await self._active(handle):
                await self.interrupt(session)
            if reachable and handle.get("mcp_name"):
                await handle["client"].request("DELETE", _location(f"/api/experimental/mcp/{quote(handle['mcp_name'], safe='')}", handle["cwd"]))
        finally:
            try:
                await owner._close_event_stream(handle)
                tasks = list(handle["background_tasks"])
                for task in tasks:
                    task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)
                await handle["client"].close()
            finally:
                try:
                    await owner._stop_server(handle.get("proc"))
                finally:
                    owner._sessions.pop(session.agent_session_id, None)
        return "closed"
