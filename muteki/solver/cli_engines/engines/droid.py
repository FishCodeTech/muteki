"""Factory Droid worker CLI.

Events are the ``stream-json`` records observed from Droid 0.234.0:
``system``, ``message``, ``reasoning``, ``tool_call``, ``tool_result``,
and ``completion``. A turn is successful only when ``completion.finalText``
is present. ``--remove-tools web_search,fetch_url`` hides those tools; it
does not isolate the network. There is no verified stdin prompt that also
avoids a persisted session, so exact-secret delivery fails closed.
"""

from __future__ import annotations

import asyncio
import json
from concurrent.futures import ThreadPoolExecutor
from typing import Optional

from muteki.solver.cli_engines.base import CliDriver
from muteki.solver.cli_engines.types import (
    CliResult, LaunchContext, StreamStep, WORKER_LAUNCH,
)


class DroidResumeError(RuntimeError):
    """Resume was not started because the session is not a real Droid session."""

    code = "droid.resume_failed"


def _session_known(session_id: str) -> bool:
    """True only when the official SDK lists this session. Failure is closed."""
    from droid_sdk import list_sessions

    async def load() -> bool:
        rows = await list_sessions(all_workspaces=True)
        return any(getattr(row, "id", "") == session_id for row in rows)

    try:
        asyncio.get_running_loop()
    except RuntimeError:
        return asyncio.run(load())
    with ThreadPoolExecutor(max_workers=1) as pool:
        return pool.submit(lambda: asyncio.run(load())).result()


_WEB_TOOLS = ("web_search", "fetch_url")
_PERMISSIONS = {
    "supervised": [],
    "auto-accept-edits": ["--auto", "low"],
    "auto": ["--auto", "medium"],
    "full-access": ["--skip-permissions-unsafe"],
    "low": ["--auto", "low"],
    "medium": ["--auto", "medium"],
    "high": ["--auto", "high"],
    "skip-permissions-unsafe": ["--skip-permissions-unsafe"],
}


class DroidDriver(CliDriver):
    name = "droid"
    secure_prompt_transport = False
    offline_web_isolation = True

    def native_permission_modes(self) -> tuple[str, ...]:
        return tuple(_PERMISSIONS)

    def _permission_flags(self, launch: LaunchContext) -> list[str]:
        mode = self.validate_launch_context(launch)
        # Non-interactive Worker launches (做题 / 渗透 CliSolver) use
        # --skip-permissions-unsafe. Chat keeps the access mode selected
        # on the session and does not come through this driver.
        if not launch.interactive and mode in {"", "default"}:
            mode = "full-access"
        if mode in {"", "default"}:
            return []
        flags = _PERMISSIONS.get(mode)
        if flags is None:
            raise ValueError(f"droid unsupported native permission mode: {mode}")
        return list(flags)

    def _flags(self, *, web_access: bool, launch: LaunchContext) -> list[str]:
        flags = [
            "exec",
            *self._permission_flags(launch),
            "--output-format", "stream-json",
        ]
        if not web_access:
            flags.extend(["--remove-tools", ",".join(_WEB_TOOLS)])
        return flags

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del session, kb_access, stream
        return [self.bin, *self._flags(web_access=web_access, launch=launch), prompt]

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del kb_access, stream
        try:
            known = _session_known(session)
        except Exception as exc:
            raise DroidResumeError(
                "droid.resume_unverified: could not list Droid sessions"
            ) from exc
        if not known:
            raise DroidResumeError(
                "droid.resume_failed: Droid has no saved session with that id"
            )
        return [
            self.bin, "exec", "--session-id", session,
            *self._permission_flags(launch),
            "--output-format", "stream-json",
            *(["--remove-tools", ",".join(_WEB_TOOLS)] if not web_access else []),
            prompt,
        ]

    @staticmethod
    def _event(line: str) -> dict:
        try:
            value = json.loads(line)
        except (json.JSONDecodeError, TypeError):
            return {}
        return value if isinstance(value, dict) else {}

    def parse_stream_line(self, line: str) -> Optional[StreamStep]:
        event = self._event(line)
        kind = str(event.get("type") or "")
        if kind == "system" and event.get("session_id"):
            return StreamStep("session", session=str(event["session_id"]))
        if kind == "reasoning" and event.get("text"):
            return StreamStep("reasoning", text=str(event["text"]))
        if kind == "message" and event.get("role") == "assistant" and event.get("text"):
            return StreamStep("reasoning", text=str(event["text"]))
        if kind == "tool_call":
            return StreamStep(
                "tool",
                tool=str(event.get("toolId") or event.get("toolName") or "tool"),
                call_id=str(event.get("id") or ""),
                text=str(event.get("toolName") or ""),
            )
        if kind == "tool_result":
            raw = event.get("value")
            text = raw if isinstance(raw, str) else json.dumps(raw, ensure_ascii=False)
            return StreamStep(
                "tool_result",
                text=text[:600],
                raw=text,
                call_id=str(event.get("id") or ""),
            )
        return None

    def parse(self, stdout: str, stderr: str) -> CliResult:
        text = ""
        session = None
        usage: dict = {}
        turns = None
        error = ""
        saw_completion = False
        for line in stdout.splitlines():
            event = self._event(line)
            kind = event.get("type")
            if kind == "system" and event.get("session_id"):
                session = str(event["session_id"])
            elif kind == "completion":
                saw_completion = True
                final = event.get("finalText")
                text = final if isinstance(final, str) else ""
                turns = event.get("numTurns")
                if event.get("session_id"):
                    session = str(event["session_id"])
                if isinstance(event.get("usage"), dict):
                    usage = event["usage"]
            elif kind == "result" and event.get("is_error") is False and event.get("subtype") == "success":
                saw_completion = True
                final = event.get("result")
                text = final if isinstance(final, str) else ""
                turns = event.get("num_turns")
                if event.get("session_id"):
                    session = str(event["session_id"])
                if isinstance(event.get("usage"), dict):
                    usage = event["usage"]
            elif kind == "result" and event.get("is_error") is True:
                error = str(event.get("result") or "droid result error")
                if event.get("session_id"):
                    session = str(event["session_id"])
            elif kind == "error":
                error = str(event.get("message") or event.get("error") or "droid error")
        if not saw_completion and not error:
            error = "droid.completion_missing"
        return CliResult(
            text=text,
            session=session,
            input_tokens=_int(usage.get("input_tokens")),
            output_tokens=_int(usage.get("output_tokens")),
            cache_read_tokens=_int(usage.get("cache_read_input_tokens")),
            cache_write_tokens=_int(usage.get("cache_creation_input_tokens")),
            num_turns=_int(turns),
            raw_stderr=stderr,
            error=error,
        )

    def _hello_argv(self) -> list[str]:
        return self.build_execute(self.HELLO_PROMPT, None, web_access=False)


def _int(value: object) -> Optional[int]:
    if isinstance(value, bool) or value is None:
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float) and value.is_integer():
        return int(value)
    return None
