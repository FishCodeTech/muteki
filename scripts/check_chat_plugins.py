"""Exercise chat package isolation with a real, harmless stdio MCP server.

Run from the repository root: python scripts/check_chat_plugins.py
Only a temporary directory is written; no model or network service is needed.
"""
from __future__ import annotations

import asyncio
from hashlib import sha256
import json
import os
from pathlib import Path
import platform
import shutil
import sys
import tempfile
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from muteki.conversation.chat_plugins import ChatPluginError, ChatPluginService, ENGINES
from muteki.conversation.chat_providers import provider_for
from muteki.conversation.composer_capabilities import ComposerCapabilityError, discover_skills, resolve_capability_refs

SERVER = '''import json,sys
for line in sys.stdin:
 r=json.loads(line)
 if "id" not in r: continue
 method=r.get("method")
 if method=="initialize": result={"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"check","version":"1"}}
 elif method=="tools/list": result={"tools":[{"name":"echo","description":"Echo test","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}
 elif method=="tools/call": result={"content":[{"type":"text","text":r["params"]["arguments"]["text"]}]}
 else: result={}
 print(json.dumps({"jsonrpc":"2.0","id":r["id"],"result":result}),flush=True)
'''


async def check() -> None:
    with tempfile.TemporaryDirectory(prefix="muteki-chat-check-") as temp:
        root = Path(temp)
        package = root / "source"
        skill = package / "skills" / "echo"
        skill.mkdir(parents=True)
        (skill / "SKILL.md").write_text("---\nname: echo\ndescription: Reply with the test marker.\n---\nReply TEST_OK.\n")
        (package / "server.py").write_text(SERVER)
        manifest = {"name": "test-chat", "version": "1", "mcpServers": {
            "echo": {"command": sys.executable, "args": ["${PLUGIN_ROOT}/server.py"], "env": {"TEST_SECRET": "not-a-real-secret"}}
        }}
        (package / "plugin.json").write_text(json.dumps(manifest))
        before = {str(p): sha256(p.read_bytes()).hexdigest() for p in package.rglob("*") if p.is_file()}
        service = ChatPluginService(root / "private")
        try:
            public = service.install({"path": str(package)})
            assert public["enabled"] and set(public["engines"]) == set(ENGINES)
            assert "TEST_SECRET" not in json.dumps(public) and "not-a-real-secret" not in json.dumps(public)
            for engine in ENGINES:
                rows = service.skill_rows(engine)
                assert len(rows) == 1
                _, context = resolve_capability_refs(rows, engine=engine, plugin_service=service)
                assert "Reply TEST_OK" in context
            native_home = root / "operator"
            native = native_home / ".codex/skills/native-only"
            native.mkdir(parents=True)
            (native / "SKILL.md").write_text("---\nname: native-only\n---\nTest only.\n")
            with patch("pathlib.Path.home", return_value=native_home):
                codex = next(row for row in discover_skills("codex") if row["name"] == "native-only")
                assert not any(row["name"] == "native-only" for row in discover_skills("kimi"))
                try:
                    resolve_capability_refs([codex], engine="kimi", plugin_service=service)
                except ComposerCapabilityError:
                    pass
                else:
                    raise AssertionError("Native Codex skill crossed into Kimi")
                for engine in ENGINES:
                    env = service.prepare_environment(engine, "test", {})
                    if not provider_for(engine).managed_environment:
                        assert env == {}, "Unmanaged runtimes must preserve their native session home"
                        continue
                    assert Path(env["HOME"]).is_relative_to(service.root)
                    (Path(env["HOME"]) / "runtime-state").write_text("test")
                assert not (native_home / "runtime-state").exists()
                # Cursor must not persist managed credentials to a keychain
                # under the private home, including credentials inherited by
                # the final subprocess. Native login and other engines retain
                # their storage selection.
                with patch.dict(os.environ, {}, clear=True):
                    for key in ("CURSOR_API_KEY", "CURSOR_AUTH_TOKEN"):
                        supplied = {key: "test-memory-only", "AGENT_CLI_CREDENTIAL_STORE": "file"}
                        env = service.prepare_environment("cursor", "credential-check", supplied)
                        assert env["AGENT_CLI_CREDENTIAL_STORE"] == "memory"
                        assert env[key] == "test-memory-only"
                        assert supplied["AGENT_CLI_CREDENTIAL_STORE"] == "file"
                        with patch.dict(os.environ, {key: "test-inherited"}):
                            env = service.prepare_environment("cursor", "credential-check", {})
                            assert env["AGENT_CLI_CREDENTIAL_STORE"] == "memory"
                            cleared = service.prepare_environment("cursor", "credential-check", {key: ""})
                            assert "AGENT_CLI_CREDENTIAL_STORE" not in cleared
                        other = service.prepare_environment("pi", "credential-check", supplied)
                        assert other["AGENT_CLI_CREDENTIAL_STORE"] == "file"
                    native = service.prepare_environment("cursor", "credential-check", {})
                    assert "AGENT_CLI_CREDENTIAL_STORE" not in native
                    assert "AGENT_CLI_CREDENTIAL_STORE" not in os.environ
                    for path in (service.root / "sessions").rglob("*"):
                        if path.is_file():
                            assert b"test-memory-only" not in path.read_bytes()
            # Probe subprocesses get task-local homes, including to_thread work;
            # resetting one probe must not alter its siblings or the host env.
            from muteki.external_agents.base import BaseExternalAgentAdapter
            from muteki.external_agents.probe_environment import subprocess_environment
            from muteki.platform.contracts.external_agents import ProbeRequest
            class Probe(BaseExternalAgentAdapter):
                async def probe(self, request):
                    return await asyncio.to_thread(subprocess_environment)
            host_home = os.environ.get("HOME")
            async def isolated_probe(name):
                adapter = Probe("test.probe")
                adapter.probe_environment_factory = lambda: {"HOME": str(root / name)}
                result = await adapter.probe_with_environment(ProbeRequest())
                assert result["HOME"] == str(root / name)
            await asyncio.gather(isolated_probe("one"), isolated_probe("two"))
            assert os.environ.get("HOME") == host_home and subprocess_environment() is None
            for p, digest in before.items():
                assert sha256(Path(p).read_bytes()).hexdigest() == digest
            service.set_control(False)
            assert all(not service.control_enabled(engine) for engine in ENGINES)
            service.set_control(True)
            service.remember_session("test-session", service.revision("codex"))
            assert service.session_revision("test-session") == service.revision("codex")

            # Verify a real OS process, protocol handshake and call, not a mock.
            if platform.system() == "Darwin" or shutil.which("bwrap"):
                tools = await service.prepare_tools("codex")
                assert tools, service._diagnostics
                result = await service.invoke("codex", tools[0]["name"], {"text": "MCP_OK"})
                assert result["content"][0]["text"] == "MCP_OK"
                for engine in ENGINES:
                    descriptions = await service.prepare_tools(engine)
                    if not provider_for(engine).gateway_tools:
                        assert not descriptions, "Portable skills must not claim unsupported Gateway injection"
                        continue
                    assert len(descriptions) == 1 and len(descriptions[0]["name"]) <= 43
                    delivered = await service.invoke(engine, descriptions[0]["name"], {"text": engine})
                    assert delivered["content"][0]["text"] == engine
                assert len(service._workers) == 1, "Global installation should share its MCP connection"
                old_name = tools[0]["name"]
                service.update("test-chat", enabled=False)
                await service.invalidate()
                assert not service._workers
                try:
                    await service.invoke("codex", old_name, {})
                except ChatPluginError:
                    pass
                else:
                    raise AssertionError("Disabled tool remained callable")
            else:
                print("stdio check skipped: no supported OS isolation backend")
            service.update("test-chat", enabled=False)
            assert all(not service.skill_rows(engine) for engine in ENGINES)
            manifest["version"] = "2"
            (package / "plugin.json").write_text(json.dumps(manifest))
            service.install({"path": str(package)})
            service.update("test-chat", rollback=True)
            assert service.get("test-chat")["version"] == "1"
            (package / "escape").symlink_to(root)
            try:
                service.install({"path": str(package)})
            except ChatPluginError:
                pass
            else:
                raise AssertionError("Package symlink was accepted")
            service.uninstall("test-chat")
            await service.invalidate()
            assert not service.records()
            if platform.system() == "Darwin" or shutil.which("bwrap"):
                resources = root / "resources"
                resources.mkdir()
                code = SERVER.replace('"capabilities":{"tools":{}}', '"capabilities":{"resources":{},"prompts":{}}')
                code = code.replace(' elif method=="tools/list":', ''' elif method=="resources/list": result={"resources":[{"uri":"test://resource","name":"test"}]}
 elif method=="resources/templates/list": result={"resourceTemplates":[]}
 elif method=="resources/read": result={"contents":[{"uri":"test://resource","text":"RESOURCE_OK"}]}
 elif method=="prompts/list": result={"prompts":[{"name":"greet"}]}
 elif method=="prompts/get": result={"messages":[{"role":"user","content":{"type":"text","text":"PROMPT_OK"}}]}
 elif method=="tools/list":''')
                (resources / "server.py").write_text(code)
                (resources / "plugin.json").write_text(json.dumps({"name":"resources-only", "mcpServers":{"resource-server":{
                    "command":sys.executable,"args":["${PLUGIN_ROOT}/server.py"]}}}))
                service.install({"path":str(resources)})
                for engine in ENGINES:
                    tools = await service.prepare_tools(engine)
                    if not provider_for(engine).gateway_tools:
                        assert not tools
                        continue
                    assert len(tools) == 5
                    reader = next(tool for tool in tools if tool.get("_method") == "read_resource")
                    result = await service.invoke(engine, reader["name"], {"uri":"test://resource"})
                    assert result["contents"][0]["text"] == "RESOURCE_OK"
                    prompt = next(tool for tool in tools if tool.get("_method") == "get_prompt")
                    result = await service.invoke(engine, prompt["name"], {"name":"greet"})
                    assert result["messages"][0]["content"]["text"] == "PROMPT_OK"
            visual = service.install_visualize()
            assert visual["mcp_servers"] == [] and visual["allowed_modes"] == ["chat"]
            service.set_control(False)
            for engine in ENGINES:
                assert any(row["name"] == "muteki-visualize:visualize" for row in service.skill_rows(engine))
                assert visual["compatibility"][engine]["status"] == "supported"
                assert not service.enabled(engine, "pentest")
            try:
                service.update("muteki-visualize", modes=["ctf"])
            except ChatPluginError as exc:
                assert exc.code == "chat_plugin.mode_unsupported"
            else:
                raise AssertionError("Chat-only visualization plugin was enabled for Workers")
            service.update("muteki-visualize", enabled=False)
            assert all(not any(row["name"] == "muteki-visualize:visualize" for row in service.skill_rows(engine)) for engine in ENGINES)
            print(f"PASS: {len(ENGINES)} skill projections, supported Gateway transports, independent chat-only visualization, lifecycle and isolation")
        finally:
            await service.close()


if __name__ == "__main__":
    asyncio.run(check())
