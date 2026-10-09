"""Measure built-in tool delivery and read-only round trips without model calls.

Run with the project's Python and write results to an ignored state directory:
    .venv/bin/python scripts/measure_builtin_tools.py --output state/tool-baselines/builtin.json

Optional --history-db and --plugin-db inspect local databases read-only. The
report keeps counts, sizes, statuses and timing; it never copies tool arguments,
result bodies, conversation text or credentials from those databases.
"""
from __future__ import annotations

import argparse
import ast
import asyncio
from collections import Counter, defaultdict
from contextlib import closing
from datetime import datetime, timedelta, timezone
from hashlib import sha256
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import socket
import sqlite3
import subprocess
import sys
import tempfile
from time import perf_counter
from types import SimpleNamespace
from zoneinfo import ZoneInfo

from fastapi import Request

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from muteki.capability_bindings.auth import issue_bearer_token, parse_bearer_token
from muteki.capability_bindings.http_jsonrpc import MutekiHttpJsonRpcBridge, capability_result_payload
from muteki.capability_bindings.mcp_server import MutekiControlMcpServer
from muteki.conversation.browser_control import BrowserControlBroker, BrowserControlError, TOOLS
from muteki.conversation.browser_control import REQUEST_TIMEOUT_SECONDS
from muteki.conversation.chat_plugin_gateway import ChatPluginGateway
from muteki.conversation.chat_plugins import ChatPluginService
from muteki.conversation.chat_providers import PROVIDERS
from muteki.conversation.visualizations import references
from muteki.external_agents.base import BaseExternalAgentAdapter
from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.capability_catalog import DEFAULT_CATALOG, MODE_TOOL_TEMPLATES
from muteki.platform.capability_gateway import register_capability_query_handlers
from muteki.platform.command_api import MutekiCommandApiImpl
from muteki.platform.command_api import DEFAULT_MAX_WAIT_SECONDS
from muteki.platform.contracts.capabilities import CapabilityInvocation, ThreadMode
from muteki.platform.contracts.external_agents import AgentCapabilities, SessionStart
from muteki.platform.contracts.objects import AgentSession, Project, Task, Thread
from muteki.platform.store import PlatformStore


def encoded(value) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode()


def size(value) -> int:
    return len(encoded(value))


def percentile(values: list[float], quantile: float) -> float | None:
    if not values:
        return None
    return round(sorted(values)[max(0, math.ceil(len(values) * quantile) - 1)], 3)


def timings(values: list[float]) -> dict:
    return {"samples": len(values), "median_ms": percentile(values, .5),
            "p95_ms": percentile(values, .95), "min_ms": round(min(values), 3) if values else None}


def wire_tools(rows) -> list[dict]:
    return [{"name": row.name, "description": row.description, "inputSchema": row.input_schema}
            for row in rows]


def fingerprint() -> dict:
    paths = ["scripts/measure_builtin_tools.py", "muteki/platform/capability_catalog.py",
             "muteki/platform/capability_gateway.py", "muteki/platform/capability_bindings.py",
             "muteki/conversation/chat_plugin_gateway.py", "muteki/conversation/chat_plugins.py",
             "muteki/conversation/browser_control.py", "muteki/conversation/visualizations.py",
             "muteki/conversation/executor.py", "muteki/external_agents/base.py",
             "muteki/external_agents/factory.py", "muteki/external_agents/pi.py", "muteki/external_agents/omp.py",
             "muteki/external_agents/codex.py", "muteki/external_agents/claude.py", "muteki/external_agents/opencode.py",
             "muteki/capability_bindings/mcp_server.py", "muteki/capability_bindings/native_tools.py",
             "muteki/agent_plugins/muteki-control/mcp/stdio_server.py",
             "muteki/platform/command_handlers/task_handlers.py",
             "muteki/platform/command_handlers/run_handlers.py", "muteki/platform/command_handlers/pagination.py",
             "muteki/platform/command_handlers/base.py", "muteki/platform/command_api.py",
             "muteki/agent_plugins/muteki-control/skills/muteki-control/scripts/muteki_client.py",
             "muteki/agent_plugins/muteki-control/skills/muteki-control/references/workflows.md",
             "pyproject.toml", "uv.lock", "apps/web/platform_stack.py"]
    digest = {path: sha256((ROOT / path).read_bytes()).hexdigest() for path in paths}
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    diff = subprocess.check_output(["git", "diff", "HEAD", "--", *paths], cwd=ROOT)
    return {"git_head": revision, "source_sha256": digest, "source_diff_sha256": sha256(diff).hexdigest(),
            "python": platform.python_version(), "system": platform.system(), "machine": platform.machine()}


def source_constant(path: str, name: str):
    for node in ast.parse((ROOT / path).read_text()).body:
        if isinstance(node, ast.Assign) and any(isinstance(target, ast.Name) and target.id == name for target in node.targets):
            return ast.literal_eval(node.value)
    raise RuntimeError(f"Measurement constant missing: {path}:{name}")


def invocation_metrics(connection: sqlite3.Connection) -> dict:
    tools = defaultdict(lambda: {"calls": 0, "ok": Counter(), "error_codes": Counter(),
                    "receipt_states": Counter(), "durations": [], "input_bytes": [], "result_bytes": []})
    for (raw,) in connection.execute("SELECT payload FROM domain_events WHERE event_type='core.capability.invoked' ORDER BY seq"):
        event = json.loads(raw).get("payload", {})
        row = tools[str(event.get("tool") or "unspecified")]
        row["calls"] += 1
        row["ok"][str(event.get("ok"))] += 1
        row["error_codes"][str(event.get("error_code"))] += 1
        row["receipt_states"][str(event.get("receipt_state"))] += 1
        for field, collection in (("duration_ms", "durations"), ("input_bytes", "input_bytes"), ("result_bytes", "result_bytes")):
            if isinstance(event.get(field), (int, float)):
                row[collection].append(event[field])
    public = {name: {"calls": row["calls"], "ok": dict(row["ok"]), "error_codes": dict(row["error_codes"]),
                    "receipt_states": dict(row["receipt_states"]), "host_timing": timings(row["durations"]),
                    "input_bytes_p95": percentile(row["input_bytes"], .95), "result_bytes_p95": percentile(row["result_bytes"], .95)}
              for name, row in sorted(tools.items())}
    return {"calls": sum(row["calls"] for row in tools.values()), "tools": public,
            "interpretation": "gateway completion and receipt state; accepted receipts and ok flags do not establish final task success"}


def catalog_baseline() -> dict:
    schema_check = None
    if importlib.util.find_spec("jsonschema"):
        from jsonschema import Draft202012Validator
        schema_check = Draft202012Validator.check_schema
    tools = []
    if schema_check:
        for tool in TOOLS:
            schema_check(tool["input_schema"])
    for name in DEFAULT_CATALOG.names():
        spec = DEFAULT_CATALOG.require(name)
        if schema_check:
            schema_check(spec.input_schema)
        operation = spec.command_type or spec.query_type or spec.target_kind.value
        tools.append({"name": name, "operation": operation,
                      "description_utf8_bytes": len(spec.description.encode()),
                      "schema_utf8_bytes": size(spec.input_schema),
                      "wire_descriptor_utf8_bytes": size(wire_tools([spec.describe()])[0]),
                      "required": spec.input_schema.get("required", []),
                      "properties": sorted(spec.input_schema.get("properties", {})),
                      "additional_properties": spec.input_schema.get("additionalProperties", "unspecified"),
                      "modes": [mode.value for mode, names in MODE_TOOL_TEMPLATES.items() if name in names]})
    modes = {}
    for mode, names in MODE_TOOL_TEMPLATES.items():
        rows = [spec.describe() for spec in DEFAULT_CATALOG.filter(list(names))]
        modes[mode.value] = {"tools": len(rows), "names": list(names),
                             "internal_descriptor_utf8_bytes": size([r.model_dump(mode="json") for r in rows]),
                             "mcp_tools_array_utf8_bytes": size(wire_tools(rows))}
    return {"total_control_tools": len(tools), "modes": modes, "tools": tools,
            "schema_validity": "all_checked" if schema_check else "not_run_jsonschema_unavailable",
            "schema_validity_tools_checked": len(tools) + len(TOOLS) if schema_check else 0,
            "browser": {"tools": len(TOOLS), "names": [t["name"] for t in TOOLS],
                        "internal_descriptor_utf8_bytes": size(TOOLS)},
            "byte_measurement": "compact sorted-key UTF-8 JSON; not provider token usage"}


class FixtureRuns:
    """Synthetic rows only; this fixture cannot launch or control a Run."""
    def __init__(self, count: int):
        self.rows = [{"run_id": f"run-baseline-{i:05d}", "name": f"Baseline record {i}",
                      "category": "fixture", "status": "completed", "solved": False} for i in range(count)]
        self.idle_run_ids: set[str] = set()

    async def list_runs(self, *, include_archived=True):
        return list(self.rows)

    async def snapshot(self, run_id: str):
        if run_id in self.idle_run_ids:
            return {"run_id": run_id, "status": "fixture_idle"}
        for row in self.rows:
            if row["run_id"] == run_id:
                return dict(row)
        raise LookupError("Synthetic fixture Run not found")


async def measure(operation, repeat: int) -> dict:
    started = perf_counter()
    first = await operation()
    first_ms = (perf_counter() - started) * 1000
    samples = []
    for _ in range(repeat):
        started = perf_counter()
        await operation()
        samples.append((perf_counter() - started) * 1000)
    return {"first_call_ms": round(first_ms, 3), **timings(samples), "response_utf8_bytes": size(first)}


async def fixture_baseline(root: Path, repeat: int, count: int, *, verify_wait: bool = False) -> dict:
    store = PlatformStore(db_path=root / "platform.db")
    plugins = ChatPluginService(root / "plugins")
    browser = BrowserControlBroker(root / "captures")
    bindings = CapabilityBindingService(store)
    fixture_runs = FixtureRuns(count)
    api = MutekiCommandApiImpl.with_builtin_handlers(store, run_gateway=fixture_runs)
    register_capability_query_handlers(api)
    gateway = ChatPluginGateway(store, api, plugins=plugins, browser=browser, binding_service=bindings,
                                selection=lambda _: SimpleNamespace(adapter_id="codex.app_server"))
    project = Project(project_id="proj-baseline", name="Fixture project",
                      created_at=datetime(2026, 1, 1, tzinfo=timezone.utc),
                      updated_at=datetime(2026, 1, 1, tzinfo=timezone.utc))
    thread = Thread(thread_id="thr-baseline", project_id=project.project_id, title="Fixture conversation")
    session = AgentSession(agent_session_id="asess-baseline", thread_id=thread.thread_id,
                           adapter_id="codex.app_server")
    store.save(project)
    store.save(thread)
    store.save(session)
    for i in range(count):
        store.save(Task(task_id=f"task-baseline-{i:05d}", thread_id=thread.thread_id,
                        project_id=project.project_id, kind="fixture.read_only", title=f"Baseline record {i}",
                        created_at=datetime(2026, 1, 1, tzinfo=timezone.utc) + timedelta(seconds=i)))
    binding = bindings.issue_binding(thread.thread_id, "baseline", mode=ThreadMode.CONVERSATION)
    grant = bindings.issue_grant(binding, session.agent_session_id, audience="baseline")
    token = issue_bearer_token(binding, grant, session)
    context = parse_bearer_token(token)
    bridge = MutekiHttpJsonRpcBridge(gateway)
    mcp = MutekiControlMcpServer(gateway)
    headers = {"authorization": "Bearer " + token,
               "accept": "application/json, text/event-stream", "mcp-protocol-version": "2025-06-18"}

    async def direct(name="muteki_list_tasks", arguments=None):
        result = await gateway.invoke(context, CapabilityInvocation(tool_name=name, arguments=arguments or {}))
        return capability_result_payload(result)

    async def protocol(server, method, params):
        response = await server.handle_http("POST", headers, encoded({"jsonrpc": "2.0", "id": 1,
                                                                     "method": method, "params": params}))
        body = json.loads(response.body)
        if response.status != 200 or "error" in body:
            raise RuntimeError(f"baseline protocol request failed: status={response.status}, code={body.get('error', {}).get('code')}")
        result = body.get("result", {})
        if result.get("isError") or result.get("ok") is False:
            raise RuntimeError(f"baseline protocol tool failed: {result.get('error') or result.get('structuredContent', {}).get('error')}")
        return body

    try:
        descriptor = await gateway.describe(binding.binding_id)
        surfaces = {"http_gateway_control_on": {"tool_count": len(descriptor.tools),
                    "mcp_tools_array_utf8_bytes": size(wire_tools(descriptor.tools)),
                    "names": [t.name for t in descriptor.tools]}}
        plan_rows = []
        for flag in ("mcp", "native_tool_binding", "acp_mcp_config", "agent_plugin", "structured_http_rpc"):
            adapter = BaseExternalAgentAdapter("baseline.fixture")
            adapter.bind_capability_gateway(gateway)
            async def capabilities(*, refresh=False, flag=flag):
                return AgentCapabilities(**{flag: True})
            adapter._capabilities = capabilities
            request = SessionStart(agent_session_id=session.agent_session_id, thread_id=thread.thread_id,
                                   options={"thread_mode": "conversation", "chat_tools": [], "chat_control_enabled": True})
            plan = await adapter.prepare_capability_injection(request, binding, grant)
            missing = sorted({t.name for t in descriptor.tools} - {t.name for t in plan.tool_descriptions})
            plan_rows.append({"capability_flag": flag, "injection_kind": plan.injection_kind.value,
                              "plan_tool_count": len(plan.tool_descriptions), "gateway_tool_count": len(descriptor.tools),
                              "gateway_only_tools": missing,
                              "evidence": "real plan builder with synthetic probe flags; no provider process launched"})
        surfaces["injection_plans"] = plan_rows
        plugins.set_control(False)
        disabled = await gateway.describe(binding.binding_id)
        surfaces["http_gateway_control_off"] = {"tool_count": len(disabled.tools), "names": [t.name for t in disabled.tools]}
        plugins.set_control(True)

        reads = {}
        for name in ("muteki_list_projects", "muteki_list_threads", "muteki_list_tasks", "muteki_list_runs", "muteki_get_task"):
            args = {"task_id": "task-baseline-00000"} if name == "muteki_get_task" else {}
            value = await direct(name, args)
            if not value.get("ok"):
                raise RuntimeError(f"baseline read failed: {name}: {value.get('error', {}).get('code')}")
            metric = await measure(lambda name=name, args=args: direct(name, args), repeat)
            result = value.get("result")
            metric.update(total=result.get("total") if isinstance(result, dict) else None,
                          returned=result.get("returned") if isinstance(result, dict) else None,
                          continuation_fields=sorted(set(result) & {"cursor", "next_cursor", "next_offset", "has_more"}) if isinstance(result, dict) else [])
            reads[name] = metric

        recent = await direct("muteki_list_tasks", {"limit": 3})
        ids = [row["task_id"] for row in recent.get("result", {}).get("tasks", [])]
        expected = [f"task-baseline-{i:05d}" for i in range(count - 1, max(-1, count - 4), -1)]
        if ids != expected:
            raise RuntimeError("reference task-list fixture did not return the expected newest IDs")
        latest = await direct("muteki_list_tasks", {"limit": 1})
        latest_id = latest.get("result", {}).get("tasks", [{}])[0].get("task_id")
        detail = await direct("muteki_get_task", {"task_id": latest_id})
        if latest_id != ids[0] or detail.get("result", {}).get("task_id") != latest_id:
            raise RuntimeError("reference list-to-detail chain did not preserve the returned ID")
        selection_cases = [
            {"id": "recent_tasks", "prompt": "列出最近三个任务。", "context": f"{count}-row fixture",
             "expected_calls": [{"tool": "muteki_list_tasks", "arguments": {"limit": 3}}],
             "reference_outcome_verified": True, "model_outcome": "not_run"},
            {"id": "latest_task_detail", "prompt": "找出最新的任务并读取完整详情。", "context": f"{count}-row fixture",
             "expected_calls": [{"tool": "muteki_list_tasks", "arguments": {"limit": 1}},
                                {"tool": "muteki_get_task", "arguments_from": "returned task_id"}],
             "reference_outcome_verified": True, "model_outcome": "not_run"},
            {"id": "task_detail_by_id", "prompt": "读取 task-baseline-00000 的详情。", "context": f"{count}-row fixture",
             "expected_calls": [{"tool": "muteki_get_task", "arguments": {"task_id": "task-baseline-00000"}}],
             "reference_transport_verified": True, "model_outcome": "not_run"},
            {"id": "browser_web_read", "prompt": "读取右侧浏览器页面。", "context": "web client, cross-origin preview",
             "expected_outcome": "report desktop requirement after typed host_unsupported; no repeated failed calls",
             "reference_transport_verified": True, "model_outcome": "not_run"},
            {"id": "inline_visual", "prompt": "用结构图说明项目、任务和运行的关系。", "context": "visualize skill enabled, file writes unavailable",
             "expected_outcome": "complete muteki-visualize fenced fragment rendered from the reply, zero control tools required",
             "reference_outcome_verified": True, "model_outcome": "not_run"},
        ]

        invalid = []
        cases = [("missing_task_id", "muteki_get_task", {}),
                 ("unknown_tool", "muteki_baseline_unknown", {}),
                 ("competition_tool_outside_mode", "muteki_list_competitions", {}),
                 ("string_limit", "muteki_list_tasks", {"limit": "invalid"}),
                 ("zero_limit", "muteki_list_tasks", {"limit": 0}),
                 ("invalid_thread_mode", "muteki_list_threads", {"mode": "invalid"})]
        for label, name, args in cases:
            spec = DEFAULT_CATALOG.get(name)
            schema_status = "not_applicable" if spec is None else "not_run"
            if spec and importlib.util.find_spec("jsonschema"):
                from jsonschema import Draft202012Validator
                schema_status = "invalid" if list(Draft202012Validator(spec.input_schema).iter_errors(args)) else "valid"
            result = await direct(name, args)
            error = result.get("error", {})
            invalid.append({"case": label, "tool": name, "advertised_schema_input": schema_status,
                            "ok": result["ok"], "error_code": error.get("code"),
                            "error_category": error.get("category"), "recovery_hint_present": bool(error.get("recovery_hint")),
                            "returned": result.get("result", {}).get("returned") if isinstance(result.get("result"), dict) else None})

        browser_probes = []
        for host in ("no_client", "web"):
            subscriber = browser.subscribe(thread.thread_id, host) if host == "web" else None
            try:
                await browser.invoke(thread.thread_id, "chat_browser_read", {})
            except BrowserControlError as error:
                browser_probes.append({"host": host, "tool": "chat_browser_read", "error_code": error.code})
            else:
                raise RuntimeError("unsupported browser fixture unexpectedly accepted read")
            finally:
                if subscriber:
                    browser.unsubscribe(subscriber)

        transport = {"http_jsonrpc_handler": await measure(lambda: protocol(bridge, "muteki.invoke", {"tool_name": "muteki_list_tasks", "arguments": {}}), repeat),
                     "http_mcp_handler": await measure(lambda: protocol(mcp, "tools/call", {"name": "muteki_list_tasks", "arguments": {}}), repeat),
                     "http_mcp_list": await measure(lambda: protocol(mcp, "tools/list", {}), repeat)}
        mcp_result = (await protocol(mcp, "tools/call", {"name": "muteki_list_tasks", "arguments": {}}))["result"]
        transport["mcp_result_encoding"] = {"text_utf8_bytes": len(mcp_result["content"][0]["text"].encode()),
                         "structured_content_utf8_bytes": size(mcp_result["structuredContent"]),
                         "same_payload_in_text_and_structured_content": json.loads(mcp_result["content"][0]["text"]) == mcp_result["structuredContent"],
                         "interpretation": "wire duplication; provider prompt duplication has not been measured"}
        for label, server, method, params in (
            ("http_jsonrpc_handler", bridge, "muteki.invoke", {"tool_name": "muteki_list_tasks", "arguments": {}}),
            ("http_mcp_handler", mcp, "tools/call", {"name": "muteki_list_tasks", "arguments": {}}),
            ("http_mcp_list", mcp, "tools/list", {}),
        ):
            reply = await server.handle_http("POST", headers, encoded({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            transport[label]["actual_http_response_body_bytes"] = len(reply.body)
        wait_run_id = "run-baseline-idle" if verify_wait else None
        if wait_run_id:
            fixture_runs.idle_run_ids.add(wait_run_id)
        transport["portable_stdio"] = await stdio_baseline(root, bridge, repeat, wait_run_id=wait_run_id)
        portable_timeout = source_constant("muteki/agent_plugins/muteki-control/mcp/stdio_server.py", "GATEWAY_TIMEOUT_SECONDS")
        transport["timeout_budgets"] = {"run_wait_default_seconds": DEFAULT_MAX_WAIT_SECONDS,
                    "browser_request_seconds": REQUEST_TIMEOUT_SECONDS,
                    "portable_stdio_upstream_seconds": portable_timeout,
                    "portable_stdio_run_wait_margin_seconds": portable_timeout - DEFAULT_MAX_WAIT_SECONDS,
                    "portable_stdio_timeout_source": "stdio_server.GATEWAY_TIMEOUT_SECONDS",
                    "interpretation": "configured timeout budgets; read-only warm round trips measured separately"}

        prompt = {"empty_managed_skill_catalog_utf8_bytes": len(plugins.skill_catalog_context("codex").encode()),
                  "visualization_host_hint_utf8_bytes": len(plugins.visualization_context(thread.thread_id).encode()),
                  "injection_frequency": "with native compaction observability: once per live session, changed catalog/hint revision or compaction; other runtimes receive the shorter hint each turn",
                  "control_skill_utf8_bytes": (ROOT / "muteki/agent_plugins/muteki-control/skills/muteki-control/SKILL.md").stat().st_size,
                  "visualize_skill_utf8_bytes": (ROOT / "muteki/agent_plugins/muteki-visualize/skills/visualize/SKILL.md").stat().st_size,
                  "skill_body_caveat": "file size is not evidence that its body enters every model turn"}
        plugins.install_visualize()
        prompt["installed_visualize_catalog"] = {engine: {"skill_count": len(plugins.skill_rows(engine)),
                    "catalog_utf8_bytes": len(plugins.skill_catalog_context(engine).encode())} for engine in PROVIDERS}
        html = '<div id="baseline-visual"><strong>Baseline visualization</strong></div>'
        message = SimpleNamespace(role="assistant", message_id="msg-baseline", text="```muteki-visualize\n" + html + "\n```")
        reference = references(message.text)[0]
        started = perf_counter()
        plugins.publish_visualizations(thread.thread_id, [message], "codex")
        document = plugins.visualization_document(thread.thread_id, [message], "codex", reference.path, message.message_id)
        visual = {"publish_and_read_ms": round((perf_counter() - started) * 1000, 3),
                  "references": len(references(message.text)), "document_keys": sorted(document),
                  "source_utf8_bytes": len(html.encode()), "document_utf8_bytes": size(document),
                  "tool_calls_required": 0, "evidence": "real parser and immutable snapshot; browser rendering not measured"}
        if document.get("html") != html:
            raise RuntimeError("visualization fixture did not publish the complete source")
        with closing(sqlite3.connect(store.db_path)) as connection:
            telemetry = invocation_metrics(connection)
        return {"fixture": {"task_rows": count, "run_rows": count, "run_data_source": "synthetic read-only FixtureRuns",
                            "queries_use_real_store_gateway_handlers": True, "models_launched": 0},
                "delivery": surfaces, "reads": reads, "input_feedback": invalid,
                "read_only_selection_cases": selection_cases,
                "browser_feedback": browser_probes, "transport": transport, "prompt": prompt, "visualization": visual,
                "invocation_metadata": telemetry}
    finally:
        await plugins.close()
        store.close()


async def stdio_baseline(root: Path, bridge: MutekiHttpJsonRpcBridge, repeat: int, *, wait_run_id: str | None = None) -> dict:
    """Real stdio process -> localhost HTTP -> production JSON-RPC bridge."""
    import uvicorn
    from fastapi import FastAPI
    from fastapi.responses import Response
    app = FastAPI()
    @app.post("/capability")
    async def capability(request: Request):
        reply = await bridge.handle_http("POST", dict(request.headers), await request.body())
        return Response(content=reply.body, status_code=reply.status, headers=reply.headers)
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    server = uvicorn.Server(uvicorn.Config(app, log_level="critical", lifespan="off"))
    task = asyncio.create_task(server.serve(sockets=[sock]))
    process = None
    try:
        async with asyncio.timeout(5):
            while not server.started:
                if task.done():
                    await task
                    raise RuntimeError("baseline HTTP fixture did not start")
                await asyncio.sleep(.01)
        # The temporary credential belongs only to this synthetic fixture.
        bindings = bridge._gateway._bindings
        binding = bindings.active_binding_for_thread("thr-baseline", "baseline")
        grant = bindings.issue_grant(binding, "asess-baseline", audience="baseline-stdio")
        connection = root / "connection.json"
        connection.write_text(json.dumps({"endpoint": f"http://127.0.0.1:{port}/capability",
                                          "bearer_token": issue_bearer_token(binding, grant)}))
        connection.chmod(0o600)
        started = perf_counter()
        process = await asyncio.create_subprocess_exec(sys.executable, str(ROOT / "muteki/agent_plugins/muteki-control/mcp/stdio_server.py"),
                    "--config", str(connection), stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE, limit=8 * 1024 * 1024,
                    env={key: os.environ[key] for key in ("PATH", "LANG", "LC_ALL") if key in os.environ})
        next_id = 0
        last_wire_bytes = 0
        async def request(method, params, *, timeout=10):
            nonlocal next_id, last_wire_bytes
            next_id += 1
            process.stdin.write(encoded({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params}) + b"\n")
            await process.stdin.drain()
            line = await asyncio.wait_for(process.stdout.readline(), timeout=timeout)
            if not line:
                raise RuntimeError("baseline stdio bridge closed without reply")
            last_wire_bytes = len(line)
            result = json.loads(line)
            if "error" in result:
                raise RuntimeError(f"baseline stdio request failed: {result['error']}")
            if result.get("result", {}).get("isError"):
                raise RuntimeError("baseline stdio tool returned an error")
            return result
        await request("initialize", {"protocolVersion": "2025-06-18"})
        startup_ms = (perf_counter() - started) * 1000
        listing = await request("tools/list", {})
        list_wire_bytes = last_wire_bytes
        calls = await measure(lambda: request("tools/call", {"name": "muteki_list_tasks", "arguments": {}}), repeat)
        calls["actual_stdio_response_line_bytes"] = last_wire_bytes
        bounded_wait = {"status": "not_requested"}
        if wait_run_id:
            started = perf_counter()
            reply = await request("tools/call", {"name": "muteki_wait_run", "arguments": {"run_id": wait_run_id}}, timeout=50)
            result = reply["result"]["structuredContent"]
            elapsed = perf_counter() - started
            if result.get("ok") is not True or result.get("result", {}).get("state") != "timeout" or not 30 <= elapsed < 45:
                raise RuntimeError("Real stdio bounded wait did not return the expected timeout result within its transport budget")
            bounded_wait = {"status": "verified", "elapsed_seconds": round(elapsed, 3),
                            "state": "timeout", "fixture": "synthetic idle RunGateway view with an empty event stream; no Run executor launched"}
        return {"process_start_and_initialize_ms": round(startup_ms, 3), "tool_count": len(listing["result"]["tools"]),
                "tool_list_response_utf8_bytes": size(listing), "task_list_round_trip": calls,
                "actual_tool_list_stdio_line_bytes": list_wire_bytes,
                "measurement_reader_max_line_bytes": 8 * 1024 * 1024,
                "bounded_wait": bounded_wait,
                "network": "localhost only; actual stdio and HTTP; no provider or model"}
    finally:
        if process:
            process.stdin.close()
            try:
                await asyncio.wait_for(process.wait(), 3)
            except asyncio.TimeoutError:
                process.kill()
                await process.wait()
        server.should_exit = True
        await asyncio.wait_for(task, 5)
        sock.close()


def installed_plugins(path: Path | None) -> dict:
    if path is None:
        return {"status": "not_requested"}
    if not path.is_file():
        return {"status": "database_missing"}
    with closing(sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)) as connection:
        rows = []
        for (value,) in connection.execute("SELECT value FROM packages"):
            record = json.loads(value)
            rows.append({"id": record["id"], "enabled": record.get("enabled", True),
                         "modes": record.get("modes", []), "skill_count": len(record.get("skills", [])),
                         "mcp_server_count": len(record.get("mcp", {}))})
        control = connection.execute("SELECT value FROM policy WHERE id='control'").fetchone()
    return {"status": "read_only_snapshot", "packages": rows, "control_enabled": json.loads(control[0]) if control else True}


def business_result(output) -> dict | None:
    """Inspect only explicit structured business results, never error prose."""
    if isinstance(output, str):
        try:
            output = json.loads(output)
        except (ValueError, TypeError):
            return None
    if not isinstance(output, dict):
        return None
    if isinstance(output.get("ok"), bool):
        return output
    if isinstance(output.get("structuredContent"), dict):
        return business_result(output["structuredContent"])
    if isinstance(output.get("result"), dict):
        return business_result(output["result"])
    content = output.get("content")
    if isinstance(content, list) and len(content) == 1 and isinstance(content[0], dict):
        return business_result(content[0].get("text"))
    return None


def history_baseline(path: Path | None) -> dict:
    if path is None:
        return {"status": "not_requested"}
    if not path.is_file():
        return {"status": "database_missing"}
    names = set(DEFAULT_CATALOG.names()) | {t["name"] for t in TOOLS}
    rows = defaultdict(lambda: {"started_events": 0, "completed_events": 0, "statuses": Counter(),
                               "explicit_error_flags": Counter(), "business_outcomes": Counter(),
                               "business_error_codes": Counter(), "durations": [], "output_bytes": []})
    starts = {}
    seen_starts = set()
    seen_completions = set()
    counts = Counter()
    first = last = None
    with closing(sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)) as connection:
        telemetry = invocation_metrics(connection)
        for (event_type, occurred_at, thread_id, raw) in connection.execute("SELECT event_type,occurred_at,aggregate_id,payload FROM domain_events WHERE event_type IN ('core.tool.started','core.tool.completed') ORDER BY seq"):
            event = json.loads(raw).get("payload", {})
            tool = str(event.get("tool") or event.get("tool_name") or "")
            name = next((n for n in names if tool == n or tool.endswith("__" + n)), None)
            if name is None:
                continue
            first = min(first, occurred_at) if first else occurred_at
            last = max(last, occurred_at) if last else occurred_at
            item = rows[name]
            counts[event_type] += 1
            key = (thread_id, event.get("agent_session_id"), event.get("turn_id"), event.get("call_id"), name)
            if event_type == "core.tool.started":
                item["started_events"] += 1
                if key in seen_starts:
                    counts["duplicate_start_keys"] += 1
                seen_starts.add(key)
                starts[key] = occurred_at
            else:
                item["completed_events"] += 1
                if key in seen_completions:
                    counts["duplicate_completion_keys"] += 1
                seen_completions.add(key)
                item["statuses"][str(event.get("status", "unspecified"))] += 1
                item["explicit_error_flags"][str(event.get("is_error", event.get("isError", "unspecified")))] += 1
                if "output" in event:
                    item["output_bytes"].append(size(event["output"]))
                result = business_result(event.get("output"))
                item["business_outcomes"][str(result["ok"]) if result is not None else "unknown"] += 1
                if result is not None and isinstance(result.get("error"), dict):
                    item["business_error_codes"][str(result["error"].get("code", "unspecified"))] += 1
                if key in starts:
                    duration = (datetime.fromisoformat(occurred_at.replace("Z", "+00:00")) -
                                datetime.fromisoformat(starts[key].replace("Z", "+00:00"))).total_seconds() * 1000
                    if duration >= 0:
                        item["durations"].append(duration)
                        counts["paired_completion_events"] += 1
    public = {}
    for name, row in sorted(rows.items()):
        public[name] = {"started_events": row["started_events"], "completed_events": row["completed_events"],
                       "statuses": dict(row["statuses"]), "explicit_error_flags": dict(row["explicit_error_flags"]),
                       "business_outcomes": dict(row["business_outcomes"]),
                       "business_error_codes": dict(row["business_error_codes"]),
                       "paired_event_timing": timings(row["durations"]),
                       "output_size_samples": len(row["output_bytes"]),
                       "output_utf8_bytes_p95": percentile(row["output_bytes"], .95)}
    return {"status": "read_only_retrospective", "event_counts": dict(counts), "first_event": first,
            "last_event": last, "tools": public, "invocation_metadata": telemetry,
            "interpretation": "archived/imported/synthetic provenance and historical code revisions are not validated; completion events do not establish task success; no model selection accuracy denominator",
            "model_call_success_rate": None, "schema_token_cost": None,
            "saved_content": "aggregated metadata only; no prompts, arguments, outputs or credentials"}


async def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--history-db", type=Path)
    parser.add_argument("--plugin-db", type=Path)
    parser.add_argument("--repeat", type=int, default=30)
    parser.add_argument("--fixture-rows", type=int, default=1000)
    parser.add_argument("--verify-wait", action="store_true", help="Verify the real 30-second bounded wait on an isolated empty event stream")
    args = parser.parse_args()
    if not 5 <= args.repeat <= 200 or not 1 <= args.fixture_rows <= 5000:
        parser.error("repeat must be 5..200 and fixture-rows must be 1..5000")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    source = fingerprint()
    os.environ["MUTEKI_HOST_DISCOVERY"] = "0"
    report = {"baseline_schema_version": 1, "created_at": datetime.now(ZoneInfo("Asia/Shanghai")).isoformat(),
              "source": source, "catalog": catalog_baseline(),
              "installed_plugins": installed_plugins(args.plugin_db), "history": history_baseline(args.history_db)}
    report["measurement_scope"] = {"model_calls": 0, "selection_accuracy": "not_measured",
            "provider_token_usage": "not_measured", "real_browser_ui": "not_measured",
            "control_commands": "not_executed", "fixture_results": "read-only host and transport measurements",
            "reported_response_utf8_bytes": "compact JSON serialization; actual HTTP body and stdio line sizes recorded separately"}
    with tempfile.TemporaryDirectory(prefix="fixture-", dir=args.output.parent) as temporary:
        report["measured"] = await fixture_baseline(Path(temporary), args.repeat, args.fixture_rows, verify_wait=args.verify_wait)
    after = fingerprint()
    report["source_stable_during_measurement"] = source == after
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"output": str(args.output.resolve()), "control_tools": report["catalog"]["total_control_tools"],
                      "browser_tools": report["catalog"]["browser"]["tools"],
                      "source_stable": report["source_stable_during_measurement"]}, ensure_ascii=False))
    if not report["source_stable_during_measurement"]:
        raise SystemExit("Measured source files changed during baseline capture; repeat for a stable snapshot.")


if __name__ == "__main__":
    asyncio.run(main())
