"""Claude SDK transport whose CLI belongs to the Muteki process ledger."""

from __future__ import annotations

import asyncio
from dataclasses import replace
import os
from pathlib import Path
import shlex
import sys
import uuid
from typing import Any

from .process_supervisor import (
    ProcessIdentityError, ProcessOwner, ProcessRecord, current_server_instance,
    default_ledger_path, default_process_ledger, process_start_time, terminate_tree,
)


def owned_claude_transport(options: Any, *, session_id: str) -> Any:
    # Keep the SDK an optional dependency until the Claude adapter is selected.
    from claude_agent_sdk._internal.transport.subprocess_cli import SubprocessCLITransport

    # ClaudeSDKClient configures a copy of the options for its default
    # transport. A preconstructed custom transport must apply that same SDK
    # configuration, or its CLI never receives --permission-prompt-tool stdio.
    transport_options = options
    if options.can_use_tool and not options.permission_prompt_tool_name:
        transport_options = replace(options, permission_prompt_tool_name="stdio")

    class OwnedTransport(SubprocessCLITransport):
        def __init__(self) -> None:
            super().__init__(prompt="", options=transport_options)
            self._owned_record: ProcessRecord | None = None
            self._owned_ledger = default_process_ledger()
            self._launcher: Path | None = None

        async def connect(self) -> None:
            if self._process is not None:
                return
            original_cli = self._cli_path or await asyncio.to_thread(self._find_cli)
            server = await current_server_instance()
            root = (self._owned_ledger.path if self._owned_ledger else default_ledger_path()).parent / "launchers"
            root.mkdir(parents=True, exist_ok=True, mode=0o700)
            launcher = root / f"claude-{uuid.uuid4().hex}"
            self._launcher = launcher
            command = [sys.executable, str(Path(__file__).with_name("process_guardian.py")),
                       str(server.pid), str(original_cli)]
            launcher.write_text("#!/bin/sh\nexec " + shlex.join(command) + ' "$@"\n', encoding="utf-8")
            launcher.chmod(0o700)
            self._cli_path = str(launcher.resolve())
            try:
                await super().connect()
                process = self._process
                if process is None:
                    raise ProcessIdentityError("Claude SDK did not expose its owned CLI process")
                deadline = asyncio.get_running_loop().time() + 5
                while process.returncode is None and os.getpgid(process.pid) != process.pid:
                    if asyncio.get_running_loop().time() >= deadline:
                        raise ProcessIdentityError("Claude guardian did not establish its own process group")
                    await asyncio.sleep(0.01)
                start = await process_start_time(process.pid)
                if start is None:
                    raise ProcessIdentityError("Claude CLI exited before ownership could be recorded")
                self._owned_record = ProcessRecord(
                    pid=process.pid, pgid=process.pid, start_time=start,
                    owner=ProcessOwner(server, "claude.sdk", session_id),
                    # SDK argv can contain MCP headers. Record only its binary.
                    command=(str(original_cli),), label="claude-sdk",
                )
                if self._owned_ledger is not None:
                    self._owned_ledger.add(self._owned_record)
            except BaseException:
                await self.close()
                raise

        async def close(self) -> None:
            async def cleanup() -> None:
                try:
                    await super(OwnedTransport, self).close()
                finally:
                    if self._owned_record is not None:
                        result = await terminate_tree(self._owned_record, require_leader=False)
                        if result.stale and self._owned_ledger is not None:
                            self._owned_ledger.remove(self._owned_record)
                        if not result.stopped:
                            raise ProcessIdentityError(f"{result.code}: {result.detail}")
                        self._owned_record = None
                    if self._launcher is not None:
                        self._launcher.unlink(missing_ok=True)
                        self._launcher = None

            task = asyncio.create_task(cleanup())
            try:
                await asyncio.shield(task)
            except asyncio.CancelledError:
                await task
                raise

    return OwnedTransport()
