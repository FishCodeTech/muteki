"""Local Devin CLI conversation adapter (ACP over stdio).

Uses the CLI's existing host login, or WINDSURF_API_KEY inherited by the
process. Conversation sessions and solving workers both use ACP; the latter
uses a JSONL bridge for the existing ``CliSolver`` event contract.
"""

from __future__ import annotations

import asyncio
from functools import lru_cache
import json
import os
from pathlib import Path
import shutil
import subprocess
import time
from typing import Any, AsyncIterator, Mapping, Optional

from muteki.platform.contracts.agent_events import (
    ApprovalOption, ApprovalRequestedPayload, UsagePayload, dump_payload, redact_secrets,
)
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import ThreadMode
from muteki.platform.contracts.external_agents import (
    ACCESS_MODE_VALUES, AccessMode, AgentCapabilities, AgentEvent,
    AgentEventType, AgentSessionRef, MessageInput, ProbeRequest, SessionStart,
)

from .acp import (
    AcpClientTerminals, AcpError, AcpRequestError, AcpTransport, BaseAcpAdapter,
    _content_text,
)


def default_devin_binary() -> str:
    return (os.environ.get("MUTEKI_DEVIN_BIN") or shutil.which("devin")
            or str(Path.home() / ".local/bin/devin"))


def devin_login_status(
    binary: Optional[str] = None, env: Optional[Mapping[str, str]] = None,
) -> str:
    source = dict(os.environ if env is None else env)
    if source.get("WINDSURF_API_KEY", "").strip():
        return "present"
    # ``devin auth status`` also refreshes remote account/team metadata.  A
    # transient network timeout must not make an existing host login disappear
    # while Worker settings are being saved.  This is the credential file path
    # reported and consumed by Devin CLI itself.
    try:
        data_home = source.get("XDG_DATA_HOME", "").strip()
        if data_home:
            credential_path = Path(data_home) / "devin" / "credentials.toml"
        else:
            home = source.get("HOME", "").strip()
            credential_path = (
                Path(home) if home else Path.home()
            ) / ".local" / "share" / "devin" / "credentials.toml"
        if credential_path.is_file() and credential_path.stat().st_size > 0:
            return "present"
    except OSError:
        pass
    try:
        result = subprocess.run(
            [binary or default_devin_binary(), "auth", "status"],
            stdin=subprocess.DEVNULL, capture_output=True, text=True,
            timeout=10, env=source,
        )
    except (OSError, subprocess.TimeoutExpired):
        return "unknown"
    status = (result.stdout + result.stderr).lower()
    if "not logged in" in status or "not authenticated" in status:
        return "absent"
    if result.returncode == 0 and "logged in" in status:
        return "present"
    return "unknown"


class DevinModelCatalogError(AcpError):
    def __init__(self, code: str, message: str, detail: str = "") -> None:
        super().__init__(redact_secrets(message))
        self.code = code
        self.detail = redact_secrets(detail)


def _catalog_output(stdout: Any, stderr: Any) -> str:
    def text(value: Any) -> str:
        return value.decode("utf-8", errors="replace") if isinstance(value, bytes) else str(value or "")
    return "\n".join(filter(None, (text(stdout), text(stderr))))


@lru_cache(maxsize=8)
def _model_families(binary: str, epoch: int) -> tuple[dict[str, Any], ...]:
    del epoch  # Short-lived cache of public metadata, never credentials.
    try:
        result = subprocess.run(
            [binary, "models", "list", "--format", "json"],
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=20,
        )
    except subprocess.TimeoutExpired as exc:
        raise DevinModelCatalogError(
            "devin.model_catalog_timeout", "Devin model catalog request timed out after 20s",
            _catalog_output(exc.stdout, exc.stderr)) from exc
    except OSError as exc:
        raise DevinModelCatalogError(
            "devin.model_catalog_launch_failed", f"Devin model catalog could not start: {exc}") from exc
    if result.returncode:
        raise DevinModelCatalogError(
            "devin.model_catalog_failed", f"Devin model catalog exited with code {result.returncode}",
            _catalog_output(result.stdout, result.stderr))
    try:
        data = json.loads(result.stdout)
        families = data.get("families") if isinstance(data, dict) else None
        if not isinstance(families, list) or any(
                not isinstance(family, dict) or not isinstance(family.get("variants"), list)
                or any(not isinstance(variant, dict) for variant in family["variants"])
                for family in families):
            raise ValueError("expected families with variant lists")
    except (ValueError, TypeError) as exc:
        raise DevinModelCatalogError(
            "devin.model_catalog_invalid", f"Devin model catalog has invalid metadata: {exc}",
            _catalog_output(result.stdout, result.stderr)) from exc
    return tuple(families)


def devin_model_ids(binary: Optional[str] = None) -> list[str]:
    return list(dict.fromkeys(str(variant["model_uid"])
                for family in _model_families(binary or default_devin_binary(), int(time.time() // 60))
                for variant in family["variants"] if variant.get("model_uid")))


def _config_choices(option: dict[str, Any]) -> list[dict[str, Any]]:
    return [choice for group in option.get("options", [])
            for choice in (group.get("options", []) if "group" in group else [group])]


def _terminal_command_line(params: dict[str, Any]) -> str:
    command = str(params.get("command") or "").strip()
    args = [str(item) for item in params.get("args") or []]
    if not args:
        return command
    return " ".join([command, *args]).strip()


def _permission_command(params: dict[str, Any]) -> str:
    """Exact command from a permission request. Empty means no grant is stored."""
    tool_call = params.get("toolCall") if isinstance(params.get("toolCall"), dict) else {}
    raw = tool_call.get("rawInput") if isinstance(tool_call.get("rawInput"), dict) else {}
    line = _terminal_command_line(raw) or _terminal_command_line(tool_call)
    if line:
        return line
    return str(tool_call.get("title") or "").strip()


def _permission_kind(params: dict[str, Any], option_id: Optional[str]) -> str:
    if not option_id:
        return ""
    for option in params.get("options") or []:
        if isinstance(option, dict) and str(option.get("optionId") or "") == option_id:
            return str(option.get("kind") or "")
    return ""


#: Client terminals run with our privileges. ``auto`` and ``full-access`` map
#: to Devin's native smart/bypass policies and may execute; the ask-shaped
#: modes still need an approval because Devin has no ask mode over ACP.
_TERMINAL_UNATTENDED_MODES = frozenset({
    AccessMode.AUTO.value,
    AccessMode.FULL_ACCESS.value,
})
_TERMINAL_ASK_MODES = frozenset({
    AccessMode.SUPERVISED.value,
    AccessMode.AUTO_ACCEPT_EDITS.value,
})
_STREAMING_MESSAGE_KINDS = frozenset({
    "agent_message_chunk",
    "agent_thought_chunk",
})


class DevinAcpAdapter(BaseAcpAdapter):
    adapter_id = "devin.acp"
    # 3000.11.3: Code/Smart/Bypass edit without request_permission; Ask is
    # read-only; Plan approval changes to Code/Bypass instead of gating edits.
    supported_access_modes = (
        AccessMode.AUTO_ACCEPT_EDITS.value,
        AccessMode.AUTO.value,
        AccessMode.FULL_ACCESS.value,
    )
    unsupported_access_mode_reasons = {
        AccessMode.SUPERVISED.value: (
            "Devin 3000.11.3 has no mode that requests approval for each file edit; "
            "Ask is read-only and Plan approval switches to Code/Bypass. "
            "Choose another access mode explicitly."
        ),
    }

    def __init__(self, *, binary: Optional[str] = None, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self._binary = binary or default_devin_binary()

    def _agent_argv(self) -> list[str]:
        return [self._binary, "acp"]

    def _agent_argv_for_request(self, request: SessionStart) -> list[str]:
        argv = self._agent_argv()
        if request.model:
            argv.extend(["--model", request.model])
        return argv

    def _select_session_auth_method(
        self, auth_methods: list[dict[str, Any]], env: dict[str, str],
    ) -> Optional[str]:
        # session/new reports invalid/missing credentials. Never start a browser
        # login flow from a server-side conversation.
        return None

    @staticmethod
    def _client_capabilities() -> dict[str, Any]:
        # Devin only emits ``cognition.ai/streamingMessageId`` when these flags
        # sit in ``clientCapabilities._meta``; the same flags in the
        # ``initialize`` request ``_meta`` are ignored (devin 3000.11.3).
        return {
            "fs": {"readTextFile": False, "writeTextFile": False},
            "terminal": True,
            "elicitation": {"form": {}},
            "_meta": {
                "cognition.ai/subagentSupport": True,
                "cognition.ai/messageGrouping": True,
            },
        }

    def _initialize_options(self) -> dict[str, Any]:
        return {"client_capabilities": self._client_capabilities()}

    def _session_transport_options(
        self, request: SessionStart, handle: dict[str, Any],
    ) -> dict[str, Any]:
        # ``AcpClientTerminals`` already refuses a cwd outside the session
        # workspace. Devin sends the shell source in ``command``.
        terminals = AcpClientTerminals(
            adapter_id=self.adapter_id,
            agent_session_id=request.agent_session_id,
            cwd=str(handle.get("cwd") or ""),
            env=dict(handle.get("env") or {}),
            shell_commands=True,
        )
        handle["terminals"] = terminals
        handlers = terminals.request_handlers()
        create = handlers["terminal/create"]

        async def create_terminal(params: dict[str, Any]) -> dict[str, Any]:
            await self._authorize_terminal(request.agent_session_id, params)
            return await create(params)

        handlers["terminal/create"] = create_terminal
        return {
            "client_capabilities": self._client_capabilities(),
            "request_handlers": handlers,
        }

    def _normalize_update(self, update: dict[str, Any]) -> dict[str, Any]:
        meta = update.get("_meta")
        if not isinstance(meta, dict):
            return update
        kind = str(update.get("sessionUpdate") or "")
        rewritten: Optional[dict[str, Any]] = None
        message_id = meta.get("cognition.ai/streamingMessageId")
        if (
            kind in _STREAMING_MESSAGE_KINDS
            and isinstance(message_id, str)
            and message_id
            and update.get("messageId") != message_id
        ):
            rewritten = dict(update)
            rewritten["messageId"] = message_id
        if kind == "tool_call":
            name = meta.get("cognition.ai/inferenceToolName")
            title = (rewritten if rewritten is not None else update).get("title")
            if isinstance(name, str) and name and title in (None, "", "Tool"):
                if rewritten is None:
                    rewritten = dict(update)
                rewritten["title"] = name
        return rewritten if rewritten is not None else update

    def _dispatch_update(
        self, agent_session_id: str, session_id: str,
        update: dict[str, Any], replay: bool,
    ) -> None:
        update = self._normalize_update(update)
        meta = update.get("_meta") if isinstance(update.get("_meta"), dict) else {}
        if not replay:
            completed = meta.get("cognition.ai/subagent_completed")
            handle = self._handle_for(agent_session_id)
            if isinstance(completed, dict) and handle is not None:
                agent_id = str(completed.get("agentId") or "")
                flushed = self._flush_child_activity(handle, agent_id) if agent_id else None
                if flushed is not None:
                    self._emit_handle_events(handle, [flushed])
        ctx = meta.get("cognition.ai/subagent_context")
        parent = str(ctx.get("parentAgentId") or "") if isinstance(ctx, dict) else ""
        kind = str(update.get("sessionUpdate") or "")
        if kind == "usage_update" and not replay:
            handle = self._handle_for(agent_session_id)
            if handle is not None:
                # Devin repeats each root context snapshot with root
                # subagent_context metadata. Deduplicate only identical
                # context snapshots within the same native owner. Child
                # snapshots and changed cost/token values remain observable.
                scope = parent or "root"
                snapshots = handle.setdefault("devin_context_usage", {})
                snapshot = {key: value for key, value in update.items() if key != "_meta"}
                usage_meta = {key: value for key, value in meta.items()
                              if key != "cognition.ai/subagent_context"}
                if usage_meta:
                    snapshot["_meta"] = usage_meta
                fingerprint = json.dumps(snapshot, sort_keys=True, ensure_ascii=False)
                if snapshots.get(scope) == fingerprint:
                    return
                snapshots[scope] = fingerprint
        if parent and parent != "root" and kind == "agent_message_chunk":
            # Same streamingMessageId stays one child activity; a new id flushes
            # the previous one. Tool attribution stays on the parent path.
            session_id = parent
        super()._dispatch_update(agent_session_id, session_id, update, replay)

    async def _request_permission(
        self, agent_session_id: str, params: dict[str, Any],
    ) -> Optional[str]:
        option_id = await super()._request_permission(agent_session_id, params)
        self._remember_execute_grant(agent_session_id, params, option_id)
        return option_id

    def _remember_execute_grant(
        self, agent_session_id: str, params: dict[str, Any], option_id: Optional[str],
    ) -> None:
        tool_call = params.get("toolCall") or {}
        if str(tool_call.get("kind") or "") != "execute":
            return
        kind = _permission_kind(params, option_id)
        if kind not in {"allow_once", "allow_always"}:
            return
        handle = self._handle_for(agent_session_id)
        if handle is None:
            return
        command = _permission_command(params)
        self._grant_terminal(
            handle, command, session=kind == "allow_always")

    def _grant_terminal(
        self, handle: dict[str, Any], command: str, *, session: bool,
    ) -> None:
        """Remember one exact command line. A session grant is not every command."""
        command = command.strip()
        if not command:
            return
        grant = handle.setdefault("terminal_execute", {
            "mode": handle.get("access_mode"),
            "session_commands": set(),
            "turn": None,
            "turn_commands": set(),
        })
        if grant.get("mode") != handle.get("access_mode"):
            grant["mode"] = handle.get("access_mode")
            grant["session_commands"] = set()
            grant["turn"] = None
            grant["turn_commands"] = set()
        if session:
            grant.setdefault("session_commands", set()).add(command)
            return
        turn = handle.get("current_turn_id")
        if grant.get("turn") != turn:
            grant["turn"] = turn
            grant["turn_commands"] = set()
        grant.setdefault("turn_commands", set()).add(command)

    def _terminal_granted(self, handle: dict[str, Any], command: str) -> bool:
        command = command.strip()
        grant = handle.get("terminal_execute") or {}
        if not command or grant.get("mode") != handle.get("access_mode"):
            return False
        if command in (grant.get("session_commands") or set()):
            return True
        turn = handle.get("current_turn_id")
        return bool(turn) and grant.get("turn") == turn and command in (
            grant.get("turn_commands") or set())

    def _terminal_approval(
        self, handle: dict[str, Any], command: str, approval_id: str,
    ) -> dict[str, Any]:
        cwd = str(handle.get("cwd") or "") or None
        return dump_payload(ApprovalRequestedPayload(
            approval_id=approval_id,
            approval_kind="command_execution",
            title=command,
            tool_name="terminal",
            command=command,
            cwd=cwd,
            options=[
                ApprovalOption(option_id="allow-once", label="Allow once", kind="allow_once"),
                ApprovalOption(
                    option_id="allow-session", label="Allow for session", kind="allow_always"),
                ApprovalOption(option_id="reject-once", label="Reject", kind="reject_once"),
                ApprovalOption(
                    option_id="reject-session", label="Reject for session", kind="reject_always"),
            ],
            scopes=["once", "session"],
            native={
                "tool_kind": "execute",
                "access_mode": handle.get("access_mode"),
            },
        ))

    async def _authorize_terminal(
        self, agent_session_id: str, params: dict[str, Any],
    ) -> None:
        handle = self._handle_for(agent_session_id)
        if handle is None:
            raise AcpRequestError(
                "terminal/create has no session",
                code=-32000, data={"reason": "access_mode"})
        mode = str(handle.get("access_mode") or AccessMode.SUPERVISED.value)
        if mode in _TERMINAL_UNATTENDED_MODES:
            return
        if mode not in _TERMINAL_ASK_MODES:
            raise AcpRequestError(
                f"terminal/create is not allowed in access mode {mode}",
                code=-32000, data={"reason": "access_mode"})
        command = _terminal_command_line(params)
        if command and self._terminal_granted(handle, command):
            return
        if not command:
            raise AcpRequestError("terminal/create requires a command", code=-32602)
        cwd = str(handle.get("cwd") or "")
        grants = handle.get("approval_grants")
        preview = self._terminal_approval(handle, command, "terminal-grant")
        if grants is not None and grants.covers(preview, cwd=cwd):
            return
        sink = self._interaction_sink(handle)
        if sink is None:
            raise AcpRequestError(
                "terminal/create requires approval and no approval channel is open",
                code=-32000, data={"reason": "access_mode"})
        approval_id = new_id("approval")
        requested = self._terminal_approval(handle, command, approval_id)
        loop = asyncio.get_running_loop()
        future: asyncio.Future[Optional[str]] = loop.create_future()
        delivery = loop.create_future()
        delivery.set_result({"ok": True})
        options = [
            {"optionId": "allow-once", "kind": "allow_once", "name": "Allow once"},
            {"optionId": "allow-session", "kind": "allow_always", "name": "Allow for session"},
            {"optionId": "reject-once", "kind": "reject_once", "name": "Reject"},
            {"optionId": "reject-session", "kind": "reject_always", "name": "Reject for session"},
        ]
        handle.setdefault("pending_approvals", {})[approval_id] = {
            "future": future,
            "options": options,
            "params": {"__muteki_rpc_request_id": approval_id},
            "delivery": delivery,
            "request_payload": requested,
        }
        sink.put_nowait((
            "approval", AgentEventType.APPROVAL_REQUESTED, "acp.terminal", requested,
        ))
        try:
            option_id = await future
        finally:
            handle.get("pending_approvals", {}).pop(approval_id, None)
        if option_id not in {"allow-once", "allow-session"}:
            raise AcpRequestError(
                "terminal/create was not approved",
                code=-32000, data={"reason": "access_mode"})
        self._grant_terminal(
            handle, command, session=option_id == "allow-session",
        )

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        caps = await super().probe(request)
        try:
            result = await asyncio.to_thread(
                subprocess.run, [self._binary, "--version"],
                stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10,
            )
            if result.returncode == 0:
                caps.runtime_version = result.stdout.strip()
        except (OSError, subprocess.TimeoutExpired):
            pass
        if request.include_models:
            try:
                caps.supported_models = await asyncio.to_thread(devin_model_ids, self._binary)
            except DevinModelCatalogError as exc:
                caps.supported_models = []
                if self._probe_cache is not None:
                    self._probe_cache.degradations.append(f"{exc.code}: {exc}\n{exc.detail}".rstrip())
        caps.user_input = False  # Provider-specific question requests unverified.
        # 本机实测（devin 3000.11.3）：run_subagent 委派经
        # _meta["cognition.ai/subagent_started|completed|subagent_context"]
        # 公布子智能体生命周期与工具归属。
        caps.subagents = True
        if self._probe_cache is not None:
            self._probe_cache.binary_path = self._binary
            self._probe_cache.field_sources["user_input"] = "static"
            login = await asyncio.to_thread(
                devin_login_status, self._binary, {**os.environ, **self._env_extra})
            self._probe_cache.healthy_override = bool(
                caps.protocol_version and caps.runtime_version and login == "present")
            if login != "present":
                self._probe_cache.detail = "本机 Devin 登录未确认；请执行 devin auth login"
        return caps

    async def _launch(self, request: SessionStart, plan: Any, bearer_token: Any) -> dict[str, Any]:
        if not request.thread_id or request.options.thread_mode not in (None, ThreadMode.CONVERSATION):
            raise ValueError("Devin ACP requires a Conversation thread")
        return await super()._launch(request, plan, bearer_token)

    async def _after_session_open(
        self, transport: AcpTransport, session_id: str, request: SessionStart,
    ) -> None:
        selected = request.permission_mode or {
            "supervised": "ask", "auto-accept-edits": "accept-edits",
            "auto": "smart", "full-access": "bypass",
        }.get(request.access_mode or "supervised", "ask")
        if request.sandbox_mode:
            raise AcpError("Devin ACP sandbox selection is not supported")
        if request.effort not in (None, "", "default"):
            raise AcpError("Choose a Devin model variant instead of a separate effort")
        # CLI variants and ACP selectors have different identities: ACP exposes
        # one family representative plus a separate thought_level selector.
        if request.model:
            await self._configure_model(transport, session_id, request.model)
        for category, value in (("mode", selected),):
            if not value:
                continue
            setup = transport.session_setup(session_id)
            option = next((item for item in setup.get("configOptions", [])
                           if item.get("category") == category
                           or item.get("id") == category), None)
            if option is None:
                raise AcpError(f"Devin did not advertise a {category} configuration option")
            choices = _config_choices(option)
            if option.get("type") != "select" or value not in {item.get("value") for item in choices}:
                advertised = [item.get("value") for item in choices]
                raise AcpError(
                    f"Devin does not advertise {category} {value!r}; "
                    f"current={option.get('currentValue')!r}, available={advertised!r}")
            if option.get("currentValue") != value:
                await transport.set_config_option(session_id, option["id"], value)

    async def _configure_model(self, transport: AcpTransport, session_id: str, model: str) -> None:
        families = await asyncio.to_thread(_model_families, self._binary, int(time.time() // 60))
        matches = [(family, variant) for family in families for variant in family["variants"]
                   if variant.get("model_uid") == model]
        if len(matches) != 1:
            raise AcpError(f"Devin CLI model metadata does not uniquely identify {model!r}")
        family, variant = matches[0]
        variant_ids = {str(item["model_uid"]) for item in family["variants"] if item.get("model_uid")}
        setup = transport.session_setup(session_id)
        option = next((item for item in setup.get("configOptions", [])
                       if item.get("category") == "model" or item.get("id") == "model"), None)
        if option is None or option.get("type") != "select":
            raise AcpError("Devin did not advertise a model selector")
        try:
            option = await transport.wait_config_option_values(
                session_id, option["id"], variant_ids, timeout=self._startup_timeout)
        except TimeoutError as exc:
            raise AcpError(f"Devin ACP did not make model family {family.get('family_uid')!r} available") from exc
        representatives = [item["value"] for item in _config_choices(option) if item.get("value") in variant_ids]
        if len(representatives) != 1:
            raise AcpError(f"Devin ACP model family is ambiguous for {model!r}")
        representative = representatives[0]
        # Apply even when the family is unchanged: a resumed session may still
        # carry a different variant. Every setter verifies the Agent's receipt.
        await transport.set_config_option(session_id, option["id"], representative)
        setup = transport.session_setup(session_id)
        thought = next((item for item in setup.get("configOptions", [])
                        if item.get("category") == "thought_level" or item.get("id") == "thought_level"), None)
        levels = [item["value"] for item in _config_choices(thought or {})
                  if variant.get("label") == f"{family.get('family_label')} {item.get('name')}"]
        if len(levels) == 1 and thought is not None:
            await transport.set_config_option(session_id, thought["id"], levels[0])
        elif model != representative:
            # Match native catalog labels exactly; never guess a reasoning
            # suffix or silently run the family's default variant.
            raise AcpError(f"Devin ACP cannot map the native variant {variant.get('label')!r} to its configuration selectors")

    async def _prompt_stream(
        self, session: AgentSessionRef, input: MessageInput,
    ) -> AsyncIterator[AgentEvent]:
        # Devin 3000.11.3 appends a system mode_transition after the first user
        # message, even when configuration completed before session/prompt.
        # Explain the host's presentation contract without filtering output or
        # changing tool permissions. Native slash commands must remain verbatim.
        if not input.payload.runtime_capability and not input.text.lstrip().startswith("/"):
            input = input.model_copy(update={"text": (
                "The ACP client applies and displays session configuration separately from the conversation. "
                "Treat runtime mode-transition notices as configuration context, not additional user requests. "
                "Do not acknowledge those notices unless the user asks about the configuration. "
                "Answer the user request below and follow its requested output format.\n\n"
                + input.text
            )})
        async for event in super()._prompt_stream(session, input):
            yield event

    # -- 子智能体（实测：devin 3000.11.3，2026-11） -----------------------------

    def _delegation_info(self, update: dict[str, Any]) -> Optional[dict[str, Any]]:
        meta = update.get("_meta") or {}
        if str(meta.get("cognition.ai/inferenceToolName") or "") != "run_subagent":
            return None
        raw = update.get("rawInput") or {}
        # node 等 subagent_started 以子 agentId 为锚，这里只标记委派。
        return {
            "title": str(raw.get("title") or "").strip() or None,
            "request": str(raw.get("task") or "").strip() or None,
            "role": str(raw.get("profile") or "").strip() or None,
        }

    def _tool_owner(self, update: dict[str, Any]) -> Optional[str]:
        ctx = (update.get("_meta") or {}).get("cognition.ai/subagent_context") or {}
        owner = str(ctx.get("parentAgentId") or "")
        return None if owner in ("", "root") else owner

    def _tool_meta_nodes(
        self, handle: dict[str, Any], update: dict[str, Any]
    ) -> list[dict[str, Any]]:
        meta = update.get("_meta") or {}
        started = meta.get("cognition.ai/subagent_started")
        if isinstance(started, dict):
            agent_id = str(started.get("agentId") or update.get("toolCallId") or "")
            if not agent_id:
                return []
            # subagent_started 不带父 call id；实测时序上它开始时恰好有
            # 一个进行中的 run_subagent 委派，按此结构关联。
            call_id = None
            for candidate, desc in handle["delegation_calls"].items():
                if desc.get("agent_id") is None:
                    desc["agent_id"] = agent_id
                    call_id = candidate
                    break
            patch: dict[str, Any] = {
                "agent_id": agent_id,
                "session_ref": agent_id,
                "title": str(started.get("title") or "").strip() or None,
                "request": str(started.get("task") or "").strip() or None,
                "role": str(started.get("profile") or "").strip() or None,
                "model": str(started.get("model") or "").strip() or None,
                "status": "running",
            }
            if call_id:
                patch["call_id"] = call_id
            return [patch]
        completed = meta.get("cognition.ai/subagent_completed")
        if isinstance(completed, dict):
            agent_id = str(completed.get("agentId") or update.get("toolCallId") or "")
            if not agent_id:
                return []
            success = bool(completed.get("success"))
            summary = str(completed.get("summary") or "").strip()
            patch = {
                "agent_id": agent_id,
                "status": "completed" if success else "failed",
            }
            if summary:
                patch["result" if success else "error"] = summary
            return [patch]
        return []

    def _delegation_result(
        self, handle: dict[str, Any], update: dict[str, Any],
        desc: dict[str, Any],
    ) -> Optional[dict[str, Any]]:
        agent_id = desc.get("agent_id")
        if not agent_id:
            return None
        patch: dict[str, Any] = {"agent_id": agent_id}
        # subagent_completed 先到且可能已置 failed，不能被这里的 completed 覆盖。
        existing = handle["agent_nodes"].get(agent_id) or {}
        if existing.get("status") == "failed":
            patch["status"] = "failed"
        output = _content_text(update.get("content"))
        prefix = "Subagent completed:\n"
        if output.startswith(prefix):
            output = output[len(prefix):]
        if output.strip():
            patch["result" if patch.get("status") != "failed" else "error"] = output
        return patch

    def _turn_result_usage(self, result: dict[str, Any]) -> Optional[UsagePayload]:
        usage = result.get("usage") or {}
        if not isinstance(usage, dict):
            return None

        def pick(*sources: str) -> Optional[int]:
            for source in sources:
                value = usage.get(source)
                if isinstance(value, int) and not isinstance(value, bool) and value >= 0:
                    return value
            return None

        # Devin 的 inputTokens 口径含缓存（沿用既有 input_includes_cache 标记）。
        model = UsagePayload(
            scope="turn",
            input_tokens=pick("inputTokens", "input_tokens"),
            output_tokens=pick("outputTokens", "output_tokens"),
            cached_input_tokens=pick(
                "cacheReadTokens", "cachedTokens", "cache_read_tokens"),
            cache_write_tokens=pick("cacheWriteTokens", "cache_write_tokens"),
            reasoning_tokens=pick("reasoningTokens", "reasoning_tokens"),
            cost_usd=(
                float(cost)
                if isinstance((cost := usage.get("costUsd", usage.get("cost_usd"))), (int, float))
                and not isinstance(cost, bool) and cost >= 0 else None
            ),
            native=dict(usage),
        )
        return model if model.model_fields_set - {"scope", "native"} else None
