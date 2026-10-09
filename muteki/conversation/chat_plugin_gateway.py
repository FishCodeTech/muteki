"""Conversation plugin tools alongside, without replacing, the platform gateway."""
from __future__ import annotations

import logging

from muteki.platform.capability_gateway import AgentCapabilityGatewayImpl
from muteki.platform.contracts.capabilities import CapabilityResult, ToolDescription
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.objects import AgentSession
from muteki.platform.command_handlers.base import CommandAPIError
from muteki.conversation.browser_control import BrowserControlError
from muteki.conversation.computer_control import ComputerControlError, DROID_COMPUTER_DELIVERY_BYTES
from muteki.external_agents.factory import engine_for_adapter
from muteki.capability_management import enabled as capability_enabled

_log = logging.getLogger(__name__)
COMPUTER_RESOURCE = ("mcp", "computer-use")


class ChatPluginGateway(AgentCapabilityGatewayImpl):
    def __init__(self, *args, plugins, selection, browser=None, computer=None, current_turn=None, model_input=None, **kwargs):
        super().__init__(*args, **kwargs)
        self.plugins = plugins
        self.selection = selection
        self.browser = browser
        self.computer = computer
        self.current_turn = current_turn
        self.model_input = model_input

    async def _computer_tools(self) -> list[dict]:
        if self.computer is None or not capability_enabled(*COMPUTER_RESOURCE):
            return []
        try:
            return await self.computer.tools()
        except ComputerControlError as exc:
            # Missing Codex Computer Use only removes these tools; the card shows the reason.
            _log.warning("chat computer tools unavailable code=%s message=%s", exc.code, exc.message)
            return []

    async def describe(self, binding_id):
        descriptor = await super().describe(binding_id)
        binding = self._bindings.get_binding(binding_id)
        if binding.mode.value != "conversation":
            return descriptor
        engine = engine_for_adapter(self.selection(binding.thread_id).adapter_id)
        additions = [*(self.browser.tools() if self.browser else []),
                     *await self._computer_tools(),
                     *await self.plugins.prepare_tools(engine)]
        tools = descriptor.tools if self.plugins.control_enabled(engine) and capability_enabled("mcp", "muteki-control") else []
        return descriptor.model_copy(update={"tools": [*tools, *[
            ToolDescription(name=t["name"], description=t["description"], input_schema=t["input_schema"])
            for t in additions
        ]]})

    async def _invoke_browser(self, context, invocation, session):
        try:
            binding, _ = self._authorize(context, invocation.invocation_id)
        except CommandAPIError as exc:
            return CapabilityResult(ok=False, invocation_id=invocation.invocation_id, error=exc.error)
        if binding.mode.value != "conversation" or not session:
            return CapabilityResult(ok=False, invocation_id=invocation.invocation_id, error=ErrorEnvelope(
                code="chat.browser.mode_invalid", message="右侧浏览器只允许在聊天会话中调用", category=ErrorCategory.PERMISSION))
        try:
            result = await self.browser.invoke(binding.thread_id, invocation.tool_name, invocation.arguments)
        except BrowserControlError as exc:
            return CapabilityResult(ok=False, invocation_id=invocation.invocation_id, error=ErrorEnvelope(
                code=exc.code, message=exc.message, category=exc.category, retryable=exc.retryable,
                recovery_hint=exc.recovery_hint, detail=exc.detail))
        return CapabilityResult(ok=True, result=result, invocation_id=invocation.invocation_id)

    async def _invoke_computer(self, context, invocation, session):
        def failure(code, message, category, **extra):
            return CapabilityResult(ok=False, invocation_id=invocation.invocation_id, error=ErrorEnvelope(
                code=code, message=message, category=category, **extra))

        try:
            binding, _ = self._authorize(context, invocation.invocation_id)
        except CommandAPIError as exc:
            return CapabilityResult(ok=False, invocation_id=invocation.invocation_id, error=exc.error)
        if binding.mode.value != "conversation" or not session:
            return failure("chat.computer.mode_invalid", "电脑操控只允许在聊天会话中调用", ErrorCategory.PERMISSION)
        if not capability_enabled(*COMPUTER_RESOURCE):
            return failure("chat.computer.disabled", "聊天电脑操控已在能力管理中停用", ErrorCategory.PERMISSION)
        turn = self.current_turn(binding.thread_id) if self.current_turn is not None else None
        if turn is None or turn.status != "running" or turn.agent_session_id != context.agent_session_id:
            return failure("chat.computer.turn_invalid", "电脑操控请求不属于当前运行中的聊天回合", ErrorCategory.STATE)
        try:
            image_input = invocation.image_input
            if image_input is None and self.model_input is not None:
                inputs = self.model_input(binding.thread_id)
                if inputs is not None:
                    image_input = "image" in inputs
            outcome = await self.computer.invoke(
                binding.thread_id, turn.turn_id, invocation.invocation_id,
                invocation.tool_name, invocation.arguments, image_input=image_input,
                delivery_bytes=(DROID_COMPUTER_DELIVERY_BYTES if engine_for_adapter(session.adapter_id) == "droid" else None))
        except ComputerControlError as exc:
            return failure(exc.code, exc.message, exc.category, retryable=exc.retryable,
                           recovery_hint=exc.recovery_hint, detail=exc.detail)
        return CapabilityResult(ok=not outcome.is_error, result=outcome.result, images=outcome.images,
                                invocation_id=invocation.invocation_id)

    async def _invoke(self, context, invocation):
        # Platform commands keep their existing implementation and authorization.
        session = self._store.get(AgentSession, context.agent_session_id)
        engine = engine_for_adapter(session.adapter_id) if session else ""
        if self.browser and self.browser.handles(invocation.tool_name):
            return await self._invoke_browser(context, invocation, session)
        if self.computer and self.computer.handles(invocation.tool_name):
            return await self._invoke_computer(context, invocation, session)
        if not invocation.tool_name.startswith("chat_"):
            if context.mode.value != "conversation" or (self.plugins.control_enabled(engine) and capability_enabled("mcp", "muteki-control")):
                return await super()._invoke(context, invocation)
            return CapabilityResult(ok=False, invocation_id=invocation.invocation_id, error=ErrorEnvelope(
                code="chat.control.disabled", message="当前 Agent 的聊天平台工具已停用", category=ErrorCategory.PERMISSION))
        try:
            binding, _ = self._authorize(context, invocation.invocation_id)
            if binding.mode.value != "conversation" or not session:
                raise ValueError("聊天插件只允许在聊天会话中调用")
            result = await self.plugins.invoke(engine, invocation.tool_name, invocation.arguments)
            return CapabilityResult(ok=not result.get("isError", False), result=result, invocation_id=invocation.invocation_id)
        except CommandAPIError as exc:
            return CapabilityResult(ok=False, invocation_id=invocation.invocation_id, error=exc.error)
        except Exception:
            return CapabilityResult(ok=False, invocation_id=invocation.invocation_id, error=ErrorEnvelope(
                code="chat.plugin.unavailable", message="聊天插件调用失败或已停用，请在聊天插件页检查连接", category=ErrorCategory.VALIDATION))
