"""MutekiControlMcpServer —— Gateway 的 MCP 协议入口（CAP-02，设计 17.5）。

按核验后的现行规范 **MCP 2026-07-28** 实现，并提供 **双时代兼容层**
（见 docs/research/third_party_verification.md §MCP）：RUNTIME-02/03
实测本机 codex 0.147.0 / claude 2.1.234 / Cursor 2026.08.11 /
Grok 1.0.5 的 MCP 客户端仍停留在 initialize 握手时代（兼容版本范围为
2025-03-26 至 2025-11-25、不带新强制头），因此 server 同时服务两个协议时代，
时代判定集中在 ``_detect_protocol_era``：

- **无 initialize 握手、无 Mcp-Session-Id 会话**：能力发现走强制的
  ``server/discover`` RPC；协议版本经每请求的 ``MCP-Protocol-Version``
  头与 body ``params._meta["io.modelcontextprotocol/protocolVersion"]`` 协商，
  ``clientCapabilities`` 也在每个 ``params._meta`` 中必填。
- **Streamable HTTP 为 POST-only 单端点**：GET 返回 405；客户端必须带
  ``Accept: application/json, text/event-stream``（缺失返回 406）；
  ``MCP-Protocol-Version`` / ``Mcp-Method`` 为必需头，``tools/call`` 时
  ``Mcp-Name`` 也是必需头；不一致返回 400 + ``HeaderMismatch``
  （code ``-32020``），协议版本不支持返回 ``-32022``。
- **无 SSE 断流恢复**（Last-Event-ID 已移除）：本 server 只返回
  ``application/json`` 单响应，不开 SSE 流，天然满足「断流即取消、
  客户端用新 request id 重发」的语义。``subscriptions/listen``、MRTR
  多轮输入（``input_required``）属未确认/非必需特性，本保守子集不实现：
  工具结果恒为 ``resultType: "complete"``。
- ``tools/list`` 结果带 ``ttlMs`` / ``cacheScope: "private"``；
  ``tools/call`` 结果带 ``resultType`` + ``structuredContent``。
- 对所有 HTTP 连接校验 ``Origin``；默认仅接受本地 loopback
  来源和显式配置的来源，无 ``Origin`` 的 CLI Runtime 保持可用。
- **旧时代（2025-03-26 至 2025-11-25）路径**：接受
  ``initialize`` → ``notifications/initialized`` → ``tools/list`` →
  ``tools/call`` 握手序列；``initialize`` 响应带
  ``protocolVersion``/``capabilities``/``serverInfo``；放宽新时代必需头
  校验（缺 ``MCP-Protocol-Version`` / ``Mcp-Method`` / ``Mcp-Name`` 不报错，
  ``Accept`` 也不强制双类型）；``tools/list`` 结果为 ``result.tools``，
  ``tools/call`` 结果为 ``result.content`` + ``isError``（附
  ``structuredContent``，旧客户端忽略未知字段）。
- 全程无状态：不保存 ``Mcp-Session-Id`` 会话或 initialized 标记，每次
  请求独立按内容判定时代，多客户端并发不串。

职责边界（设计 17.5）：只做 tool schema、MCP transport、Bearer 认证上下文
和 Gateway 结果转换；不保存平台 Token、事件缓存、提交队列、限速队列或
Agent Session 业务状态；所有调用只经 AgentCapabilityGateway，不接触
Manager / Store / SharedGraph。工具清单由 ``gateway.describe`` 按
CapabilityBinding 动态过滤，不向任何 Thread 暴露全集。
"""

from __future__ import annotations

import ipaddress
import json
from typing import Any, Awaitable, Callable, Iterable, Mapping, Optional
from urllib.parse import urlsplit

from muteki.platform.command_handlers.base import CommandAPIError
from muteki.platform.contracts.capabilities import (
    BindingContext,
    CapabilityInvocation,
)
from muteki.platform.contracts.protocols import AgentCapabilityGateway

from .auth import BearerTokenError, extract_bearer_token, parse_bearer_token
from .http_jsonrpc import (
    INVALID_PARAMS,
    INVALID_REQUEST,
    METHOD_NOT_FOUND,
    PARSE_ERROR,
    BridgeResponse,
    capability_image_blocks,
    capability_result_payload,
    category_to_jsonrpc_code,
    envelope_data,
    jsonrpc_error,
    jsonrpc_result,
)

#: 本 server 新时代路径支持的唯一协议版本（2026-07-28，现行 Current）。
PROTOCOL_VERSION = "2026-07-28"

#: 旧时代（initialize 握手时代）可识别的协议版本，按新到旧排列；
#: 第一个是本 server 旧时代路径宣告的最新版本。
LEGACY_PROTOCOL_VERSIONS = (
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
)

#: 协议时代标识（_detect_protocol_era 的返回值）。
ERA_NEW = "new"
ERA_LEGACY = "legacy"

#: 2026-07-28 新增 / 变更的协议错误码。
HEADER_MISMATCH = -32020
UNSUPPORTED_PROTOCOL_VERSION = -32022

#: _meta 中携带协议版本 / 客户端信息的键（SEP-2575）。
META_PROTOCOL_VERSION = "io.modelcontextprotocol/protocolVersion"
META_CLIENT_CAPABILITIES = "io.modelcontextprotocol/clientCapabilities"
META_SERVER_INFO = "io.modelcontextprotocol/serverInfo"

#: Mcp-Name 必需头的方法集合（规范还列 resources/read、prompts/get；
#: 本 server 不实现这两类，保留校验逻辑以备扩展）。
_METHODS_REQUIRING_NAME = frozenset({
    "tools/call", "resources/read", "prompts/get"})


class MutekiControlMcpServer:
    """MCP 2026-07-28 Streamable HTTP 入口（无状态，可直接挂 ASGI）。"""

    def __init__(
        self,
        gateway: AgentCapabilityGateway,
        *,
        server_name: str = "muteki-control",
        server_version: str = "1.0.0",
        tools_ttl_ms: int = 5000,
        allowed_origins: Optional[Iterable[str]] = None,
    ) -> None:
        self._gateway = gateway
        self._server_name = server_name
        self._server_version = server_version
        # tools/list 的新鲜度提示；Binding 切换模式会让清单变化，客户端应
        # 按 ttlMs 重新拉取（2026-07-28 无 toolsListChanged 推送的保守替代）。
        self._tools_ttl_ms = int(tools_ttl_ms)
        self._allowed_origins = frozenset(
            self._normalize_origin(origin) for origin in (allowed_origins or ()))

    # -- HTTP 传输层 --------------------------------------------------------------

    async def handle_http(
        self, method: str, headers: Mapping[str, str], body: bytes
    ) -> BridgeResponse:
        """处理一次 HTTP 请求；不持有任何跨请求状态。"""
        normalized = {key.lower(): value for key, value in headers.items()}

        # Streamable HTTP 规范要求校验所有连接的 Origin，防止
        # 浏览器 DNS rebinding。非浏览器 Runtime 通常不发 Origin，规范
        # 允许此形态；默认接受 localhost / loopback 浏览器来源，
        # 保持 Muteki 本地生产流可用。非本地部署可显式传 allowed_origins。
        origin_failure = self._validate_origin(normalized)
        if origin_failure is not None:
            return origin_failure

        # POST-only 单端点（2026-07-28 移除了 GET 端点）。旧握手兼容只
        # 覆盖能经 POST 完成初始化和工具调用的 2025-03-26 至 2025-11-25；
        # 未实现旧 HTTP+SSE，故不宣称 2024-11-05 兼容。
        if method.upper() != "POST":
            return BridgeResponse.json(
                405, {"error": "method not allowed"}, {"Allow": "POST"})

        context, auth_failure = self._authenticate(normalized)
        if auth_failure is not None:
            return auth_failure

        try:
            request = json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            return BridgeResponse.json(400, jsonrpc_error(
                None, PARSE_ERROR, "request body is not valid JSON"))
        # 批请求（数组）自 2025-06-18 起已移除，按 invalid request 拒绝。
        if not isinstance(request, dict):
            return BridgeResponse.json(400, jsonrpc_error(
                None, INVALID_REQUEST, "batch requests are not supported"))
        if request.get("jsonrpc") != "2.0":
            return BridgeResponse.json(400, jsonrpc_error(
                request.get("id"), INVALID_REQUEST, "jsonrpc must be '2.0'"))
        method_name = request.get("method")
        params = request.get("params") or {}
        if not isinstance(method_name, str) or not isinstance(params, dict):
            return BridgeResponse.json(400, jsonrpc_error(
                request.get("id"), INVALID_REQUEST,
                "method/params have invalid types"))

        # 时代判定集中于此：旧时代放宽新时代必需头与 Accept 校验。
        era = self._detect_protocol_era(normalized, request)
        if era == ERA_NEW:
            # 新时代客户端必须同时接受 JSON 与 SSE（即使本 server 只回 JSON）。
            accept = normalized.get("accept", "")
            if ("application/json" not in accept
                    or "text/event-stream" not in accept):
                return BridgeResponse.json(
                    406, {"error": "Accept must include application/json and "
                                  "text/event-stream"})
            header_failure = self._check_protocol_headers(normalized, request)
            if header_failure is not None:
                return header_failure

        if "id" not in request:
            # 2026-07-28 的 HTTP core 没有客户端通知，因此该
            # notification 方法未实现，与其他现代未知方法一样
            # 返回 404/-32601。旧时代 notifications/initialized 与
            # notifications/cancelled 只需确认，保持 202 无响应体。
            if era == ERA_NEW:
                return BridgeResponse.json(404, jsonrpc_error(
                    None, METHOD_NOT_FOUND,
                    f"unknown notification method {method_name!r}"))
            return BridgeResponse(status=202)
        request_id = request["id"]

        if era == ERA_LEGACY:
            return await self._dispatch_legacy(
                request_id, context, method_name, params)
        if method_name == "server/discover":
            return BridgeResponse.json(200, jsonrpc_result(
                request_id, self._discover()))
        if method_name == "tools/list":
            return await self._tools_list(request_id, context, legacy=False)
        if method_name == "tools/call":
            return await self._tools_call(
                request_id, context, params, legacy=False)
        # 2026-07-28 Streamable HTTP 以 404 表示服务器不实现该
        # RPC，JSON-RPC body 仍保留 -32601，便于客户端诊断现代端点的
        # 未实现方法。
        return BridgeResponse.json(404, jsonrpc_error(
            request_id, METHOD_NOT_FOUND, f"unknown method {method_name!r}"))

    # -- 时代判定（双时代兼容的唯一入口） -------------------------------------------

    @staticmethod
    def _detect_protocol_era(
        headers: Mapping[str, str], request: dict[str, Any]
    ) -> str:
        """按单条请求内容判定协议时代；无状态，跨请求/跨客户端互不影响。

        旧时代（initialize 握手时代，2025-03-26 至 2025-11-25）判定条件
        （命中任一即为旧时代）：

        1. 请求方法为 ``initialize``——2026-07-28 已移除该方法，只有旧时代
           客户端会发送；
        2. 缺 ``MCP-Protocol-Version`` 头——该头自 2026-07-28 起为必需，
           缺失说明客户端不是新时代实现（实测 codex 0.147.0 /
           claude 2.1.234 / Cursor 2026.08.11 / Grok 1.0.5 均如此）；
        3. ``MCP-Protocol-Version`` 为已知的旧时代版本值（如 2025-11-25）
           ——旧客户端显式声明旧版本，按旧时代服务而不是回 ``-32022``。

        其余情况（版本头恰为 2026-07-28，或携带无法识别的未来版本）走
        新时代路径，由 ``_check_protocol_headers`` 做完整必需头校验，
        未知版本在那里统一返回 ``-32022``。
        """
        if request.get("method") == "initialize":
            return ERA_LEGACY
        version = (headers.get("mcp-protocol-version") or "").strip()
        if not version or version in LEGACY_PROTOCOL_VERSIONS:
            return ERA_LEGACY
        return ERA_NEW

    # -- 旧时代（initialize 握手时代）RPC --------------------------------------------

    async def _dispatch_legacy(
        self, request_id: Any, context: BindingContext,
        method_name: str, params: dict[str, Any],
    ) -> BridgeResponse:
        """旧时代方法分发；握手状态不落库，initialize 可重复调用。"""
        if method_name == "initialize":
            return BridgeResponse.json(200, jsonrpc_result(
                request_id, self._legacy_initialize(params)))
        if method_name == "ping":
            # 旧时代保活探测；无状态 server 直接回空结果。
            return BridgeResponse.json(200, jsonrpc_result(request_id, {}))
        if method_name == "server/discover":
            # 旧时代客户端不会调用，但返回发现信息无害，便于双时代探测。
            return BridgeResponse.json(200, jsonrpc_result(
                request_id, self._discover()))
        if method_name == "tools/list":
            return await self._tools_list(request_id, context, legacy=True)
        if method_name == "tools/call":
            return await self._tools_call(
                request_id, context, params, legacy=True)
        return BridgeResponse.json(200, jsonrpc_error(
            request_id, METHOD_NOT_FOUND, f"unknown method {method_name!r}"))

    def _legacy_initialize(self, params: dict[str, Any]) -> dict[str, Any]:
        """旧时代 initialize 响应：protocolVersion / capabilities / serverInfo。

        客户端请求的版本在已知旧时代版本内则原样回显（版本协商），否则
        回落到本 server 支持的最新旧时代版本；``Mcp-Session-Id`` 由客户端
        自行携带，本 server 不校验也不保存（无状态）。
        """
        requested = str(params.get("protocolVersion") or "").strip()
        version = (requested if requested in LEGACY_PROTOCOL_VERSIONS
                   else LEGACY_PROTOCOL_VERSIONS[0])
        return {
            "protocolVersion": version,
            "capabilities": {
                "tools": {"listChanged": False},
            },
            "serverInfo": {
                "name": self._server_name,
                "version": self._server_version,
            },
        }

    # -- 协议头校验（2026-07-28 必需头） ---------------------------------------------

    @staticmethod
    def _check_protocol_headers(
        headers: Mapping[str, str], request: dict[str, Any]
    ) -> Optional[BridgeResponse]:
        request_id = request.get("id")

        def mismatch(message: str) -> BridgeResponse:
            return BridgeResponse.json(400, jsonrpc_error(
                request_id, HEADER_MISMATCH, message,
                {"error": {"code": "mcp.header_mismatch", "message": message}}))

        version = (headers.get("mcp-protocol-version") or "").strip()
        if not version:
            return mismatch("MCP-Protocol-Version header is required")
        if version != PROTOCOL_VERSION:
            return BridgeResponse.json(400, jsonrpc_error(
                request_id, UNSUPPORTED_PROTOCOL_VERSION,
                f"unsupported protocol version {version!r}",
                {"supported": [PROTOCOL_VERSION], "requested": version}))

        # 2026-07-28 不再通过 initialize 保存协议与能力状态；
        # 每个请求都必须在 params._meta 重新声明。
        params = request.get("params")
        meta = params.get("_meta") if isinstance(params, dict) else None
        if not isinstance(meta, dict):
            return mismatch("params._meta is required")
        meta_version = meta.get(META_PROTOCOL_VERSION)
        if not isinstance(meta_version, str) or not meta_version:
            return mismatch(
                f"params._meta.{META_PROTOCOL_VERSION} is required")
        if meta_version != version:
            return mismatch(
                "MCP-Protocol-Version header does not match params._meta")
        client_capabilities = meta.get(META_CLIENT_CAPABILITIES)
        if not isinstance(client_capabilities, dict):
            return BridgeResponse.json(400, jsonrpc_error(
                request_id,
                INVALID_PARAMS,
                f"params._meta.{META_CLIENT_CAPABILITIES} must be an object",
            ))

        mcp_method = (headers.get("mcp-method") or "").strip()
        if not mcp_method:
            return mismatch("Mcp-Method header is required")
        if mcp_method != request["method"]:
            return mismatch("Mcp-Method header does not match request method")

        if request["method"] in _METHODS_REQUIRING_NAME:
            params = request.get("params") or {}
            name = params.get("name") if isinstance(params, dict) else None
            mcp_name = (headers.get("mcp-name") or "").strip()
            if not mcp_name:
                return mismatch(
                    f"Mcp-Name header is required for {request['method']}")
            if name is not None and mcp_name != str(name):
                return mismatch(
                    "Mcp-Name header does not match request params.name")
        return None

    # -- RPC 实现 --------------------------------------------------------------------

    def _discover(self) -> dict[str, Any]:
        """server/discover：协议版本、能力与身份（2026-07-28 强制 RPC）。"""
        return {
            "resultType": "complete",
            "supportedVersions": [PROTOCOL_VERSION],
            "capabilities": {
                # 本 server 只有 tools 能力；listChanged 推送依赖
                # subscriptions/listen（保守子集未实现），恒为 False。
                "tools": {"listChanged": False},
            },
            "_meta": {
                META_SERVER_INFO: {
                    "name": self._server_name,
                    "version": self._server_version,
                },
            },
            "ttlMs": self._tools_ttl_ms,
            "cacheScope": "private",
        }

    def _result_meta(self) -> dict[str, Any]:
        """2026-07-28 结果携带的 server identity（每响应 SHOULD）。"""
        return {
            META_SERVER_INFO: {
                "name": self._server_name,
                "version": self._server_version,
            }
        }

    async def _tools_list(
        self, request_id: Any, context: BindingContext, *, legacy: bool
    ) -> BridgeResponse:
        try:
            descriptor = await self._gateway.describe(context.binding_id)
        except CommandAPIError as exc:
            return BridgeResponse.json(200, jsonrpc_error(
                request_id, category_to_jsonrpc_code(exc.error.category),
                exc.error.message, envelope_data(exc.error)))
        tools = [
            {
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema or {"type": "object"},
            }
            for tool in descriptor.tools
        ]
        if legacy:
            # 旧时代形态：result.tools（无 resultType/ttlMs/cacheScope，
            # 这些是 2026-07-28 新增字段）。
            return BridgeResponse.json(200, jsonrpc_result(
                request_id, {"tools": tools}))
        return BridgeResponse.json(200, jsonrpc_result(request_id, {
            "tools": tools,
            "resultType": "complete",
            "ttlMs": self._tools_ttl_ms,
            # 工具清单按 Binding 过滤，属每 Thread 私有视图，禁止公共缓存。
            "cacheScope": "private",
            "_meta": self._result_meta(),
        }))

    async def _tools_call(
        self, request_id: Any, context: BindingContext,
        params: dict[str, Any], *, legacy: bool,
    ) -> BridgeResponse:
        name = str(params.get("name") or "").strip()
        if not name:
            return BridgeResponse.json(200, jsonrpc_error(
                request_id, INVALID_PARAMS, "params.name is required"))
        arguments = params.get("arguments") or {}
        if not isinstance(arguments, dict):
            return BridgeResponse.json(200, jsonrpc_error(
                request_id, INVALID_PARAMS, "params.arguments must be an object"))
        meta = params.get("_meta") or {}
        model = meta.get("muteki/modelCapabilities") if isinstance(meta, dict) else None
        image_input = model.get("imageInput") if isinstance(model, dict) else None
        if image_input is not None and not isinstance(image_input, bool):
            return BridgeResponse.json(200, jsonrpc_error(
                request_id, INVALID_PARAMS, "modelCapabilities.imageInput must be a boolean"))
        result = await self._gateway.invoke(
            context, CapabilityInvocation(tool_name=name, arguments=arguments, image_input=image_input))
        payload = capability_result_payload(result)
        if image_input is None and isinstance(result.result, dict):
            image_input = result.result.get("model_image_input")
        if image_input is False and result.images:
            payload["image_delivery"] = {"supported": False, "image_count": len(result.images),
                "message": "当前模型不支持图片输入。截图证据已保存；请依据辅助功能树操作，不能声称已目视核验。"}
        text = json.dumps(payload, ensure_ascii=False)
        content = [{"type": "text", "text": text},
                   *(capability_image_blocks(result) if image_input is not False else [])]
        # 业务失败（含 Grant 过期 / 权限拒绝 / 命令失败）统一走 isError 工具
        # 结果，structuredContent 内嵌与 Web / 其他入口相同的错误 envelope；
        # JSON-RPC error 只保留给协议层违规。两个时代同此约定。
        if legacy:
            # 旧时代形态：result.content + isError；structuredContent 自
            # 2025-06-18 起存在，更早的客户端会忽略未知字段，附带上可让
            # 2025-11-25 客户端直接取结构化 receipt。
            return BridgeResponse.json(200, jsonrpc_result(request_id, {
                "content": content,
                "structuredContent": payload,
                "isError": not result.ok,
            }))
        return BridgeResponse.json(200, jsonrpc_result(request_id, {
            "resultType": "complete",
            "content": content,
            "structuredContent": payload,
            "isError": not result.ok,
            "_meta": self._result_meta(),
        }))

    # -- Origin 校验 -------------------------------------------------------------

    @staticmethod
    def _normalize_origin(origin: str) -> tuple[str, str, Optional[int]]:
        """把 Origin 标准化为 (scheme, host, port)；非法值在配置期直接拒绝。"""
        raw = str(origin or "").strip()
        parsed = urlsplit(raw)
        if (
            parsed.scheme.lower() not in {"http", "https"}
            or not parsed.hostname
            or parsed.username is not None
            or parsed.password is not None
            or parsed.query
            or parsed.fragment
            or parsed.path not in {"", "/"}
        ):
            raise ValueError(f"invalid MCP allowed origin: {origin!r}")
        try:
            port = parsed.port
        except ValueError as exc:
            raise ValueError(f"invalid MCP allowed origin: {origin!r}") from exc
        if port is None:
            port = 80 if parsed.scheme.lower() == "http" else 443
        return parsed.scheme.lower(), parsed.hostname.lower().rstrip("."), port

    @staticmethod
    def _is_loopback_host(host: str) -> bool:
        normalized = host.lower().rstrip(".")
        if normalized == "localhost":
            return True
        try:
            return ipaddress.ip_address(normalized).is_loopback
        except ValueError:
            return False

    def _validate_origin(
        self, headers: Mapping[str, str]
    ) -> Optional[BridgeResponse]:
        origin = (headers.get("origin") or "").strip()
        if not origin:
            return None
        try:
            normalized = self._normalize_origin(origin)
        except ValueError:
            normalized = None
        if normalized is not None and (
            normalized in self._allowed_origins
            or self._is_loopback_host(normalized[1])
        ):
            return None
        return BridgeResponse.json(
            403,
            jsonrpc_error(
                None, INVALID_REQUEST, "Origin is not allowed for this MCP endpoint"),
            {"Vary": "Origin"},
        )

    # -- 认证 ------------------------------------------------------------------------

    @staticmethod
    def _authenticate(
        headers: Mapping[str, str]
    ) -> tuple[Optional[BindingContext], Optional[BridgeResponse]]:
        """Bearer → BindingContext；失败返回 401 + WWW-Authenticate。

        静态 bearer token 属核验结论确认的合规子集（内部 Runtime 接入）；
        对外暴露时需按 §MCP-5 补 RFC 9728 resource_metadata 挑战与
        audience 校验（本类不实现，属保守子集外特性）。
        """
        token = extract_bearer_token(headers)
        if token is None:
            return None, BridgeResponse.json(
                401, {"error": "missing bearer token"},
                {"WWW-Authenticate": 'Bearer realm="muteki-control"'})
        try:
            return parse_bearer_token(token), None
        except BearerTokenError as exc:
            return None, BridgeResponse.json(
                401, {"error": exc.message, "code": exc.code},
                {"WWW-Authenticate":
                 'Bearer realm="muteki-control", error="invalid_token"'})


def asgi_app(server: MutekiControlMcpServer) -> Callable[..., Awaitable[None]]:
    """把 server 包装为最小 ASGI 应用（供平台接线挂载，不修改 apps/web）。

    只处理 http scope；路径不敏感（单端点），由挂载方决定 path。
    """

    async def app(scope: dict[str, Any], receive: Any, send: Any) -> None:
        if scope["type"] != "http":
            raise RuntimeError("muteki control MCP server only handles http")
        body = b""
        while True:
            message = await receive()
            body += message.get("body", b"")
            if not message.get("more_body"):
                break
        headers = {
            key.decode("latin-1"): value.decode("latin-1")
            for key, value in scope.get("headers", [])
        }
        response = await server.handle_http(scope["method"], headers, body)
        await send({
            "type": "http.response.start",
            "status": response.status,
            "headers": [
                (key.encode("latin-1"), value.encode("latin-1"))
                for key, value in response.headers.items()
            ],
        })
        await send({"type": "http.response.body", "body": response.body})

    return app


__all__ = [
    "ERA_LEGACY",
    "ERA_NEW",
    "HEADER_MISMATCH",
    "LEGACY_PROTOCOL_VERSIONS",
    "META_CLIENT_CAPABILITIES",
    "META_PROTOCOL_VERSION",
    "META_SERVER_INFO",
    "PROTOCOL_VERSION",
    "UNSUPPORTED_PROTOCOL_VERSION",
    "MutekiControlMcpServer",
    "asgi_app",
]
