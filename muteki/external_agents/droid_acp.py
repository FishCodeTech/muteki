"""Factory Droid ACP variant (``droid.acp``).

``droid exec --output-format acp`` starts every session at ``auto-high``.
Muteki access modes are therefore applied through the advertised
``autonomy_level`` config option on every open, resume included; a session
whose autonomy cannot be confirmed does not start. Model and effort use the
config options the session actually advertises. An empty
``session/set_config_option`` result is not treated as success; this transport
waits for the matching ``config_option_update``. The wire carries no token
usage, so ``usage_events`` is not declared.
"""

from __future__ import annotations

import asyncio
import os
from typing import Any, Optional

from muteki.platform.contracts.agent_events import RuntimeWarningPayload
from muteki.platform.contracts.capabilities import CapabilityInjectionPlan
from muteki.platform.contracts.external_agents import (
    ACCESS_MODE_VALUES, AccessMode, AgentEventType, AgentSessionRef,
    MessageInput, SessionStart,
)

from .acp import (
    AcpError, AcpTransport, BaseAcpAdapter, check_response,
)
from .droid_mcp import DroidMcp, DroidMcpError, stdio_mcp_servers
from .events import build_event

_AUTONOMY_OPTION = "autonomy_level"
_AUTONOMY_VALUES = {
    AccessMode.SUPERVISED.value: "normal",
    AccessMode.AUTO_ACCEPT_EDITS.value: "auto-low",
    AccessMode.AUTO.value: "auto-medium",
    AccessMode.FULL_ACCESS.value: "auto-high",
}


class DroidAcpError(AcpError):
    """Typed Droid ACP failure; callers branch on ``code``."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


class DroidAcpTransport(AcpTransport):
    """ACP transport that confirms Droid's empty set_config_option replies."""

    async def set_config_option(
        self, session_id: str, config_id: str, value: str, *,
        timeout: float = 30.0,
    ) -> None:
        result = check_response("session/set_config_option", await self._peer.request(
            "session/set_config_option",
            {"sessionId": session_id, "configId": config_id, "value": value},
            timeout=timeout,
        ))
        options = result.get("configOptions")
        if isinstance(options, list) and options:
            self._remember_options(session_id, options, config_id, value)
            return
        changed = self._config_changed.setdefault(session_id, asyncio.Event())
        loop = asyncio.get_running_loop()
        deadline = loop.time() + timeout
        while True:
            option = self._option(session_id, config_id)
            if option is not None and option.get("currentValue") == value:
                self._remember_mode(session_id, option, value)
                return
            remaining = deadline - loop.time()
            if remaining <= 0:
                raise DroidAcpError(
                    "droid.acp.config_unconfirmed",
                    f"Droid did not confirm {config_id}={value}")
            changed.clear()
            try:
                await asyncio.wait_for(changed.wait(), timeout=remaining)
            except asyncio.TimeoutError as exc:
                raise DroidAcpError(
                    "droid.acp.config_unconfirmed",
                    f"Droid did not confirm {config_id}={value}") from exc

    def _remember_options(
        self, session_id: str, options: list[Any], config_id: str, value: str,
    ) -> None:
        setup = self._session_setup.setdefault(session_id, {})
        setup["configOptions"] = options
        self._config_changed.setdefault(session_id, asyncio.Event()).set()
        option = next((item for item in options if item.get("id") == config_id), None)
        if option is None or option.get("currentValue") != value:
            raise DroidAcpError(
                "droid.acp.config_unconfirmed",
                f"session/set_config_option did not apply {config_id!r}={value!r}")
        self._remember_mode(session_id, option, value)

    def _remember_mode(self, session_id: str, option: dict[str, Any], value: str) -> None:
        # The shared launch path reads currentModeId as the session's default mode.
        if option.get("category") == "mode":
            self._session_setup.setdefault(session_id, {}).setdefault(
                "modes", {})["currentModeId"] = value

    def _option(self, session_id: str, config_id: str) -> Optional[dict[str, Any]]:
        options = self.session_setup(session_id).get("configOptions") or []
        found = next((item for item in options if item.get("id") == config_id), None)
        return found if isinstance(found, dict) else None


class DroidAcpAdapter(BaseAcpAdapter):
    """Compatibility chat transport ``droid.acp``."""

    adapter_id = "droid.acp"
    supported_access_modes = tuple(ACCESS_MODE_VALUES)

    def __init__(self, *, binary: Optional[str] = None, env_extra: Optional[dict[str, str]] = None, **kwargs: Any) -> None:
        super().__init__(self.adapter_id, env_extra=env_extra, **kwargs)
        self._binary = binary or os.environ.get("MUTEKI_DROID_BIN") or "droid"
        self._mcp: dict[str, DroidMcp] = {}
        self._pending_notices: dict[str, list[tuple[str, str]]] = {}

    def _probe_extra_caps(self, caps: Any, hello: Any) -> None:
        del hello
        # Droid advertises no HTTP MCP; Muteki Control is injected over stdio.
        caps.mcp = True
        caps.acp_mcp_config = False
        caps.usage_events = False
        caps.steer = False
        caps.subagents = False
        caps.plan_mode = False

    def _mcp_http_degradation(self, hello: Any) -> Optional[str]:
        if hello.mcp_http:
            return None
        return (
            "mcpCapabilities.http=false; Muteki Control is injected "
            "through the stdio bridge"
        )

    def _agent_argv(self) -> list[str]:
        return [self._binary, "exec", "--output-format", "acp"]

    def _new_transport(self, argv: list[str], **kwargs: Any) -> AcpTransport:
        return DroidAcpTransport(argv, **kwargs)

    def _materialize_session_mcp(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> list[dict[str, Any]]:
        self._release_mcp(request.agent_session_id)
        try:
            mcp = stdio_mcp_servers(
                plan, bearer_token, gateway_endpoint=self._gateway_endpoint,
            )
        except DroidMcpError as exc:
            raise DroidAcpError(exc.code, str(exc)) from exc
        self._mcp[request.agent_session_id] = mcp
        self._pending_notices[request.agent_session_id] = [
            ("droid.mcp.http_not_injected", f"HTTP MCP server {name} was not injected")
            for name in mcp.dropped
        ]
        return mcp.servers

    def _release_mcp(self, agent_session_id: str) -> None:
        mcp = self._mcp.pop(agent_session_id, None)
        if mcp is not None:
            mcp.remove()
        self._pending_notices.pop(agent_session_id, None)

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        try:
            return await super()._launch(request, plan, bearer_token)
        except BaseException:
            self._release_mcp(request.agent_session_id)
            raise

    async def _teardown(self, session: AgentSessionRef) -> str:
        try:
            return await super()._teardown(session)
        finally:
            self._release_mcp(session.agent_session_id)

    async def _after_session_open(
        self, transport: AcpTransport, session_id: str, request: SessionStart,
    ) -> None:
        setup = transport.session_setup(session_id)
        advertised = {
            str(item.get("id"))
            for item in setup.get("configOptions") or []
            if isinstance(item, dict) and item.get("id")
        }
        access_mode = str(request.access_mode or AccessMode.SUPERVISED.value)
        autonomy = _AUTONOMY_VALUES.get(access_mode)
        if autonomy is None:
            raise DroidAcpError(
                "droid.acp.access_mode_unsupported",
                f"droid.acp has no autonomy level for access mode {access_mode!r}")
        if _AUTONOMY_OPTION not in advertised:
            raise DroidAcpError(
                "droid.acp.autonomy_unsupported",
                "Droid did not advertise autonomy_level; the access mode cannot be enforced")
        await transport.set_config_option(session_id, _AUTONOMY_OPTION, autonomy)
        if request.model:
            if "model" not in advertised:
                raise DroidAcpError(
                    "droid.acp.model_unsupported",
                    f"Droid did not advertise a model option; requested {request.model}")
            await transport.set_config_option(session_id, "model", str(request.model))
        effort = str(request.effort or "").strip()
        if effort and effort != "default":
            # The effort list depends on the model, so apply it after the model.
            options = transport.session_setup(session_id).get("configOptions") or []
            if not any(isinstance(item, dict) and item.get("id") == "reasoning_effort" for item in options):
                raise DroidAcpError(
                    "droid.acp.effort_unsupported",
                    f"Droid did not advertise reasoning_effort for this model; requested {effort}")
            await transport.set_config_option(session_id, "reasoning_effort", effort)

    def send(self, session: AgentSessionRef, input: Any):
        if isinstance(input, MessageInput):
            return self._prompt_with_notice(session, input)
        return super().send(session, input)

    async def _prompt_with_notice(
        self, session: AgentSessionRef, input: MessageInput,
    ):
        notices = self._pending_notices.pop(session.agent_session_id, [])
        seq = self.sequencer_for(session.agent_session_id)
        async for event in self._prompt_stream(session, input):
            yield event
            if notices and event.event_type is AgentEventType.TURN_STARTED:
                for code, message in notices:
                    yield self.emit(build_event(
                        AgentEventType.RUNTIME_WARNING, seq,
                        native_type=code,
                        payload=RuntimeWarningPayload(kind="degraded", code=code, message=message),
                        agent_session_id=session.agent_session_id,
                        external_session_id=event.external_session_id,
                        turn_id=event.turn_id,
                    ))
                notices = []
