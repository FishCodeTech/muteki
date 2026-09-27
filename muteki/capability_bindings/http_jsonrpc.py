"""结构化 HTTP/JSON-RPC Bridge（CAP-02，设计 17.5，任务书 10.11）。

``MutekiHttpJsonRpcBridge`` 是 AgentCapabilityGateway 的纯 JSON-RPC 2.0
入口，供不走 MCP 的 Runtime / 脚本 / CLI Agent 调用。只实现两个方法：

- ``muteki.describe``：返回该 Bearer 上下文可见的 CapabilityDescriptor
  （工具清单从 Gateway describe 动态生成，不维护独立命令目录）；
- ``muteki.invoke``：执行一次工具调用，返回与 MCP / Native Tool / CLI
  完全相同的 CapabilityResult payload（CommandReceipt / QueryResult /
  EventPage / WaitResult 与统一错误 envelope）。

无业务状态：不缓存事件、不保存令牌、不接触 Manager / Store；所有授权与
幂等由 Gateway / Command API 完成。

本模块同时提供 CAP-02 各协议入口共用的 JSON-RPC 信封与结果转换小工具
（``jsonrpc_result`` / ``jsonrpc_error`` / ``capability_result_payload``），
MCP Server 直接复用，避免两套错误映射。
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Mapping, Optional

from muteki.platform.command_handlers.base import CommandAPIError
from muteki.platform.contracts.capabilities import (
    CapabilityInvocation,
    CapabilityResult,
)
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.protocols import AgentCapabilityGateway

from .auth import BearerTokenError, extract_bearer_token, parse_bearer_token

#: JSON-RPC 2.0 标准错误码。
PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603
#: 服务器自定义错误段（-32099..-32000）：权限 / 状态 / 限速等业务拒绝。
SERVER_ERROR = -32001

#: Bridge 方法名（也是 Agent Plugin Skills-only 调用约定）。
METHOD_DESCRIBE = "muteki.describe"
METHOD_INVOKE = "muteki.invoke"


def category_to_jsonrpc_code(category: ErrorCategory) -> int:
    """统一错误分类 → JSON-RPC code（各入口同一映射）。"""
    if category in (ErrorCategory.VALIDATION, ErrorCategory.NOT_FOUND):
        return INVALID_PARAMS
    if category is ErrorCategory.INTERNAL:
        return INTERNAL_ERROR
    return SERVER_ERROR


def jsonrpc_result(request_id: Any, result: Any) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def jsonrpc_error(
    request_id: Any, code: int, message: str, data: Optional[dict[str, Any]] = None
) -> dict[str, Any]:
    error: dict[str, Any] = {"code": code, "message": message}
    if data is not None:
        error["data"] = data
    return {"jsonrpc": "2.0", "id": request_id, "error": error}


def envelope_data(error: ErrorEnvelope) -> dict[str, Any]:
    """把统一错误 envelope 放进 JSON-RPC error.data（各入口同构）。"""
    return {"error": error.model_dump(mode="json")}


def capability_result_payload(result: CapabilityResult) -> dict[str, Any]:
    """CapabilityResult → 协议中立 payload（receipt / result / error 同形）。"""
    payload: dict[str, Any] = {
        "ok": result.ok,
        "invocation_id": result.invocation_id,
    }
    if result.receipt is not None:
        payload["receipt"] = result.receipt.model_dump(mode="json")
    if result.result is not None:
        payload["result"] = result.result
    if result.error is not None:
        payload["error"] = result.error.model_dump(mode="json")
    return payload


@dataclass
class BridgeResponse:
    """协议入口的传输层响应（status + headers + body）。"""

    status: int
    body: bytes = b""
    headers: dict[str, str] = field(default_factory=dict)

    @classmethod
    def json(cls, status: int, payload: Any,
             headers: Optional[dict[str, str]] = None) -> "BridgeResponse":
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        all_headers = {"Content-Type": "application/json"}
        if headers:
            all_headers.update(headers)
        return cls(status=status, body=body, headers=all_headers)


class MutekiHttpJsonRpcBridge:
    """Gateway 的 HTTP/JSON-RPC 入口（POST-only，无状态）。"""

    def __init__(self, gateway: AgentCapabilityGateway) -> None:
        self._gateway = gateway

    async def handle_http(
        self, method: str, headers: Mapping[str, str], body: bytes
    ) -> BridgeResponse:
        """处理一次 HTTP 请求；不持有任何跨请求状态。"""
        if method.upper() != "POST":
            return BridgeResponse.json(
                405, {"error": "method not allowed"}, {"Allow": "POST"})

        context, auth_failure = self._authenticate(headers)
        if auth_failure is not None:
            return auth_failure

        try:
            request = json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            return BridgeResponse.json(400, jsonrpc_error(
                None, PARSE_ERROR, "request body is not valid JSON"))
        if not isinstance(request, dict):
            # 批请求（数组）不在保守子集内，统一按 invalid request 拒绝。
            return BridgeResponse.json(400, jsonrpc_error(
                None, INVALID_REQUEST, "batch requests are not supported"))
        if request.get("jsonrpc") != "2.0":
            return BridgeResponse.json(400, jsonrpc_error(
                request.get("id"), INVALID_REQUEST, "jsonrpc must be '2.0'"))
        if "id" not in request:
            # notification：本 Bridge 不定义任何 notification，确认语义下
            # 无响应体（202）。
            return BridgeResponse(status=202)
        request_id = request["id"]
        method_name = request.get("method")
        params = request.get("params") or {}
        if not isinstance(method_name, str) or not isinstance(params, dict):
            return BridgeResponse.json(400, jsonrpc_error(
                request_id, INVALID_REQUEST, "method/params have invalid types"))

        if method_name == METHOD_DESCRIBE:
            return await self._describe(request_id, context)
        if method_name == METHOD_INVOKE:
            return await self._invoke(request_id, context, params)
        return BridgeResponse.json(200, jsonrpc_error(
            request_id, METHOD_NOT_FOUND, f"unknown method {method_name!r}"))

    # -- 方法实现 ---------------------------------------------------------------

    async def _describe(self, request_id: Any, context: Any) -> BridgeResponse:
        try:
            descriptor = await self._gateway.describe(context.binding_id)
        except CommandAPIError as exc:
            return BridgeResponse.json(200, jsonrpc_error(
                request_id, category_to_jsonrpc_code(exc.error.category),
                exc.error.message, envelope_data(exc.error)))
        return BridgeResponse.json(200, jsonrpc_result(
            request_id, descriptor.model_dump(mode="json")))

    async def _invoke(
        self, request_id: Any, context: Any, params: dict[str, Any]
    ) -> BridgeResponse:
        tool_name = str(params.get("tool_name") or "").strip()
        if not tool_name:
            return BridgeResponse.json(200, jsonrpc_error(
                request_id, INVALID_PARAMS, "params.tool_name is required"))
        arguments = params.get("arguments") or {}
        if not isinstance(arguments, dict):
            return BridgeResponse.json(200, jsonrpc_error(
                request_id, INVALID_PARAMS, "params.arguments must be an object"))
        fields: dict[str, Any] = {"tool_name": tool_name, "arguments": arguments}
        if params.get("invocation_id"):
            fields["invocation_id"] = str(params["invocation_id"]).strip()
        if params.get("correlation_id"):
            fields["correlation_id"] = str(params["correlation_id"]).strip()
        invocation = CapabilityInvocation(**fields)
        # 业务结果（含拒绝 envelope）一律走 JSON-RPC result 返回，保持与
        # MCP isError 结果、Native Tool content 的 envelope 逐字节一致。
        result = await self._gateway.invoke(context, invocation)
        return BridgeResponse.json(200, jsonrpc_result(
            request_id, capability_result_payload(result)))

    # -- 认证 --------------------------------------------------------------------

    @staticmethod
    def _authenticate(
        headers: Mapping[str, str]
    ) -> tuple[Optional[Any], Optional[BridgeResponse]]:
        """Bearer → BindingContext；失败返回 401 + WWW-Authenticate。"""
        token = extract_bearer_token(headers)
        if token is None:
            return None, BridgeResponse.json(
                401, {"error": "missing bearer token"},
                {"WWW-Authenticate": 'Bearer realm="muteki-capability"'})
        try:
            return parse_bearer_token(token), None
        except BearerTokenError as exc:
            return None, BridgeResponse.json(
                401, {"error": exc.message, "code": exc.code},
                {"WWW-Authenticate":
                 'Bearer realm="muteki-capability", error="invalid_token"'})


__all__ = [
    "BridgeResponse",
    "INTERNAL_ERROR",
    "INVALID_PARAMS",
    "INVALID_REQUEST",
    "METHOD_DESCRIBE",
    "METHOD_INVOKE",
    "METHOD_NOT_FOUND",
    "MutekiHttpJsonRpcBridge",
    "PARSE_ERROR",
    "SERVER_ERROR",
    "capability_result_payload",
    "category_to_jsonrpc_code",
    "envelope_data",
    "jsonrpc_error",
    "jsonrpc_result",
]
