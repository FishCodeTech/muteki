"""Conversation plugin tools alongside, without replacing, the platform gateway."""
from __future__ import annotations

from muteki.platform.capability_gateway import AgentCapabilityGatewayImpl
from muteki.platform.contracts.capabilities import CapabilityResult, ToolDescription
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.objects import AgentSession
from muteki.external_agents.factory import engine_for_adapter


class ChatPluginGateway(AgentCapabilityGatewayImpl):
    def __init__(self, *args, plugins, selection, **kwargs):
        super().__init__(*args, **kwargs)
        self.plugins = plugins
        self.selection = selection

    async def describe(self, binding_id):
        descriptor = await super().describe(binding_id)
        binding = self._bindings.get_binding(binding_id)
        if binding.mode.value != "conversation":
            return descriptor
        engine = engine_for_adapter(self.selection(binding.thread_id).adapter_id)
        additions = await self.plugins.prepare_tools(engine)
        tools = descriptor.tools if self.plugins.control_enabled(engine) else []
        return descriptor.model_copy(update={"tools": [*tools, *[
            ToolDescription(name=t["name"], description=t["description"], input_schema=t["input_schema"])
            for t in additions
        ]]})

    async def invoke(self, context, invocation):
        # Platform commands keep their existing implementation and authorization.
        session = self._store.get(AgentSession, context.agent_session_id)
        engine = engine_for_adapter(session.adapter_id) if session else ""
        if not invocation.tool_name.startswith("chat_"):
            if context.mode.value != "conversation" or self.plugins.control_enabled(engine):
                return await super().invoke(context, invocation)
            return CapabilityResult(ok=False, error=ErrorEnvelope(
                code="chat.control.disabled", message="当前 Agent 的聊天平台工具已停用", category=ErrorCategory.PERMISSION))
        try:
            binding, _ = self._authorize(context, invocation.invocation_id)
            if binding.mode.value != "conversation" or not session:
                raise ValueError("聊天插件只允许在聊天会话中调用")
            result = await self.plugins.invoke(engine, invocation.tool_name, invocation.arguments)
            return CapabilityResult(ok=not result.get("isError", False), result=result, invocation_id=invocation.invocation_id)
        except Exception:
            return CapabilityResult(ok=False, invocation_id=invocation.invocation_id, error=ErrorEnvelope(
                code="chat.plugin.unavailable", message="聊天插件调用失败或已停用，请在聊天插件页检查连接", category=ErrorCategory.VALIDATION))
