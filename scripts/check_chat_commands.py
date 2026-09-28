"""Chat command routing regression checks. No model calls or host writes."""
from __future__ import annotations

import asyncio
from pathlib import Path
from types import SimpleNamespace
import sys
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from muteki.conversation.composer_capabilities import resolve_composer_catalog
from muteki.conversation.commands import _RUNTIME_INVOCATION_RE
from muteki.external_agents.command_providers import PROVIDERS, codex_review_target, native_prompt, public_operation_result
from muteki.external_agents.factory import DEFAULT_STRUCTURED_ADAPTER_BY_ENGINE
from muteki.external_agents.runtime_capabilities import RuntimeCapabilitySnapshot, dynamic_command_item
from muteki.external_agents.rpc_commands import rpc_operation, rpc_operation_items
from muteki.external_agents.omp import OmpRpcAdapter
from muteki.external_agents.pi import PiAdapter
from muteki.external_agents.grok import GrokAcpAdapter
from muteki.external_agents.acp import AcpTransport
from muteki.external_agents.codex import CodexAppServerAdapter
from muteki.platform.contracts.external_agents import AgentInput, AgentSessionRef


async def check():
    updates = []
    transport = AcpTransport(["fixture-unused"], on_update=lambda sid, update, replay: updates.append(replay))
    message = {"method": "session/update", "params": {"sessionId": "parent", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "background"}}}}
    await transport._on_message(message)
    transport._replaying_sessions.add("parent")
    await transport._on_message(message)
    assert updates == [False, True], "idle background update was discarded as replay"
    grok = GrokAcpAdapter()
    delivered = []
    handle = {"external_session_id": "parent", "available_commands": [], "capability_revision": 0,
              "event_sink": None, "background_handler": delivered.extend, "replay_events": []}
    grok._acp["fixture"] = handle
    grok._dispatch_update("fixture", "parent", {"sessionUpdate": "available_commands_update", "availableCommands": [{"name": "workflow", "description": "test"}]}, False)
    grok._dispatch_update("fixture", "child", {"sessionUpdate": "available_commands_update", "availableCommands": []}, False)
    assert handle["available_commands"][0]["name"] == "workflow"
    grok._dispatch_update("fixture", "parent", message["params"]["update"], False)
    assert any(row[0].value == "message.delta" for row in delivered)
    handle.update(access_mode="supervised", pending_approvals={})
    permission = asyncio.create_task(grok._request_permission("fixture", {
        "requestId": "bg-approval", "toolCall": {"title": "Read fixture", "kind": "read"},
        "options": [{"optionId": "allow", "kind": "allow_once"}]}))
    await asyncio.sleep(0)
    assert "bg-approval" in handle["pending_approvals"]
    assert any(row[0].value == "approval.requested" for row in delivered)
    handle["pending_approvals"]["bg-approval"]["future"].set_result("allow")
    assert await permission == "allow"
    native_calls = []
    async def native_request(method, params, **kwargs): native_calls.append((method, params)); return {}
    codex = CodexAppServerAdapter()
    codex._client_request_methods = {"thread/revert", "thread/rollback"}
    codex._runs["fixture"] = {"conn": SimpleNamespace(request=native_request), "thread_id": "native-thread"}
    await codex.rewind_session(AgentSessionRef(agent_session_id="fixture"), "native-turn")
    assert native_calls == [("thread/revert", {"threadId": "native-thread", "beforeTurnId": "native-turn"})]
    for engine in PROVIDERS:
        adapter = DEFAULT_STRUCTURED_ADAPTER_BY_ENGINE[engine]
        commands = [dynamic_command_item(adapter_id=adapter, engine=engine, name=f"test-{i}",
                    channel="provider_native") for i in range(120)]
        snapshot = RuntimeCapabilitySnapshot(adapter_id=adapter, items=commands)
        with patch("muteki.conversation.composer_capabilities.discover_skills", return_value=[]):
            rows = resolve_composer_catalog(engine=engine, trigger="/", runtime_snapshot=snapshot)
        assert all(any(row["name"] == command.name and row["invocable"] for row in rows) for command in commands)
        assert next(row for row in rows if row["name"] == "model")["action"] == "ui:model"
        assert next(row for row in rows if row["name"] == "quit")["invocable"] is False
        assert next(row for row in rows if row["name"] == "clear")["action"] == "new"
        for item in commands:
            assert native_prompt(item.model_dump(), "exact args\n第二行") == f"/{item.name} exact args\n第二行"
    match = _RUNTIME_INVOCATION_RE.fullmatch("/复盘:检查 保留参数\n第二行")
    assert match and match.group("args") == "保留参数\n第二行"
    assert _RUNTIME_INVOCATION_RE.fullmatch("/Users/example/file.txt") is None
    assert codex_review_target("") == {"type": "uncommittedChanges"}
    assert codex_review_target("--base main") == {"type": "baseBranch", "branch": "main"}
    assert codex_review_target("check text") == {"type": "custom", "instructions": "check text"}
    result = public_operation_result({"mcp": {"env": {"KEY": "private"}, "url": "https://user:secret@example.test/mcp?token=private"}})
    assert result == {"mcp": {"env": "[redacted]", "url": "https://example.test/mcp"}}
    calls = []
    async def command(peer, method, params, **kwargs):
        calls.append((method, params))
        return {"checked": True}
    session = AgentSessionRef(agent_session_id="test")
    for engine in ["pi", "omp"]:
        adapter = SimpleNamespace(id=f"{engine}.rpc", _rpc={"test": {"peer": object()}}, _cmd=command)
        for item in rpc_operation_items(adapter.id, engine):
            args = "on" if item.name == "autoretry" else ""
            await rpc_operation(adapter, session, item.name, args)
        await rpc_operation(adapter, session, "compact", "retain exact instructions")
        assert calls[-1] == ("compact", {"customInstructions": "retain exact instructions"})
        before = len(calls)
        try:
            await rpc_operation(adapter, session, "autoretry", "invalid")
        except ValueError:
            pass
        else:
            raise AssertionError("invalid arguments accepted")
        assert len(calls) == before
    # OMP emits command_output and completes in the prompt acknowledgement,
    # with no agent_end frame. Both output and completion must be delivered.
    adapter = OmpRpcAdapter()
    adapter._rpc["test"] = {"peer": object(), "turns": 1, "host_tool_tasks": set()}
    async def local_prompt(peer, method, params=None, **kwargs):
        assert method == "prompt" and params == {"message": "/dirs"}
        adapter._dispatch_event("test", {"type": "command_output", "text": "PRIVATE_DIRECTORY"})
        return {"agentInvoked": False}
    adapter._cmd = local_prompt
    async def consume():
        return [event async for event in adapter.send(session, AgentInput(text="/dirs"))]
    events = await asyncio.wait_for(consume(), 2)
    assert any(event.event_type.value == "message.completed" and "PRIVATE_DIRECTORY" in str(event.payload) for event in events)
    assert any(event.event_type.value == "turn.completed" for event in events)
    assert not any(event.event_type.value == "turn.failed" for event in events)
    # Pi acknowledges local extension handlers without an agent_end event.
    # Native get_state disambiguates local completion from a streaming prompt.
    for fails in [False, True]:
        pi = PiAdapter()
        pi._rpc["test"] = {"peer": object(), "turns": 1}
        async def pi_command(peer, method, params=None, **kwargs):
            if method == "prompt":
                pi._dispatch_event("test", {"type": "extension_error", "error": "TEST_FAILURE"} if fails else {
                    "type": "extension_ui_request", "method": "notify", "message": "PI_OK",
                })
                return {}
            assert method == "get_state"
            return {"isStreaming": False, "isCompacting": False, "pendingMessageCount": 0}
        pi._cmd = pi_command
        async def consume_pi():
            return [event async for event in pi.send(session, AgentInput(
                text="/local-extension", payload={"runtime_capability": {"verification": "verified"}}))]
        pi_events = await asyncio.wait_for(consume_pi(), 2)
        assert any(event.event_type.value == ("turn.failed" if fails else "turn.completed") for event in pi_events)
        if not fails:
            assert any(event.event_type.value == "message.completed" and "PI_OK" in str(event.payload) for event in pi_events)
    print("PASS: 8 provider catalogs, 120 native commands without truncation, Unicode/argument routing, RPC operations, Pi/OMP local output/completion/error")


if __name__ == "__main__":
    asyncio.run(check())
