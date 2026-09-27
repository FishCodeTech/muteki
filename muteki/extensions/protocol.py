"""Extension Host 子进程协议：版本化 JSON-RPC over stdio（任务书 11.2，EXT-01）。

传输约定（与核验文档「第三方默认子进程、JSON-RPC 版本化协议」原则一致）：

- 换行分隔的 JSON-RPC 2.0 消息（newline-delimited JSON），stdin/stdout 双向；
- 每个 request 的 params 携带 ``protocol_version``（整数主版本），握手时
  双方主版本必须一致，否则握手失败；
- Host → 扩展方法面：initialize、capabilities/list、config/validate、
  activate、health/read、command/handle、projection/read、deactivate、
  shutdown；
- 扩展 → Host 方法面：event/propose（事件提案，经 MutekiCommandAPI 准入）。

本模块同时提供：

- 宿主侧异步 ``JsonRpcPeer``：挂在一个已启动子进程的 stdin/stdout 上，
  发请求、按 id 匹配响应、把扩展发来的 request 交给宿主回调；
- 扩展侧同步 ``serve_stdio``：纯 stdlib 的行循环，Python 扩展可直接复用
  （第三方扩展也可以按上面约定自行实现，不依赖本仓库）。

Extension Host 不执行模型调用和 Agent Loop；协议里没有任何模型相关方法。
"""

from __future__ import annotations

import asyncio
import json
import logging
from typing import Any, Awaitable, Callable, Optional

LOG = logging.getLogger(__name__)

#: 协议主版本；握手时双方主版本必须一致。
PROTOCOL_VERSION = 1

#: Host → 扩展的方法面（任务书 11.2 清单，event/propose 方向相反）。
HOST_METHODS = frozenset({
    "initialize",
    "capabilities/list",
    "config/validate",
    "activate",
    "health/read",
    "command/handle",
    "projection/read",
    "deactivate",
    "shutdown",
})

#: 扩展 → Host 的方法面。
EXTENSION_METHODS = frozenset({"event/propose"})

#: 单条消息的最大字节数（防御异常输出的对端）。
MAX_MESSAGE_BYTES = 4 * 1024 * 1024

#: 默认 RPC 超时（秒）。
DEFAULT_REQUEST_TIMEOUT = 10.0


class ProtocolError(RuntimeError):
    """协议层错误（帧损坏、版本不兼容、对端行为异常）。"""


class ExtensionRpcError(RuntimeError):
    """对端返回的 JSON-RPC error。"""

    def __init__(self, code: int, message: str, data: Any = None) -> None:
        super().__init__(f"rpc error {code}: {message}")
        self.code = int(code)
        self.message = message
        self.data = data


class ExtensionUnavailable(RuntimeError):
    """子进程已退出或管道关闭，RPC 无法送达。"""


# ---------------------------------------------------------------------------
# 帧编解码（宿主与扩展共用同一约定）
# ---------------------------------------------------------------------------


def encode_request(request_id: int, method: str, params: dict[str, Any]) -> bytes:
    """编码一条 JSON-RPC request；params 自动补 protocol_version。"""
    body = {
        "jsonrpc": "2.0",
        "id": int(request_id),
        "method": method,
        "params": {**dict(params), "protocol_version": PROTOCOL_VERSION},
    }
    return json.dumps(body, ensure_ascii=False).encode("utf-8") + b"\n"


def encode_response(request_id: Any, result: Any = None, error: Any = None) -> bytes:
    """编码一条 JSON-RPC response（result 与 error 二选一）。"""
    body: dict[str, Any] = {"jsonrpc": "2.0", "id": request_id}
    if error is not None:
        body["error"] = error
    else:
        body["result"] = result if result is not None else {}
    return json.dumps(body, ensure_ascii=False).encode("utf-8") + b"\n"


def decode_message(line: bytes) -> dict[str, Any]:
    """解码一行消息；非 JSON / 非对象抛 ProtocolError。"""
    if len(line) > MAX_MESSAGE_BYTES:
        raise ProtocolError(f"message too large: {len(line)} bytes")
    try:
        message = json.loads(line.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ProtocolError(f"invalid JSON frame: {exc}") from exc
    if not isinstance(message, dict):
        raise ProtocolError("frame must be a JSON object")
    return message


def check_protocol_version(params: dict[str, Any]) -> None:
    """握手 / 每请求的协议主版本一致性检查。"""
    peer = params.get("protocol_version")
    if peer is None:
        raise ProtocolError("params missing protocol_version")
    if int(peer) != PROTOCOL_VERSION:
        raise ProtocolError(
            f"protocol major mismatch: peer={peer}, host={PROTOCOL_VERSION}"
        )


def rpc_error_data(exc: BaseException) -> dict[str, Any]:
    """把扩展侧异常归一化为 JSON-RPC error 对象。"""
    if isinstance(exc, ExtensionRpcError):
        return {"code": exc.code, "message": exc.message, "data": exc.data}
    if isinstance(exc, (ProtocolError, ValueError, KeyError)):
        return {"code": -32602, "message": str(exc)}
    return {"code": -32603, "message": f"{type(exc).__name__}: {exc}"}


# ---------------------------------------------------------------------------
# 宿主侧：异步 peer
# ---------------------------------------------------------------------------

#: 扩展 → Host 请求的处理回调签名：``(method, params) -> result``。
HostRequestHandler = Callable[[str, dict[str, Any]], Awaitable[Any]]


class JsonRpcPeer:
    """宿主侧 JSON-RPC peer：管理一个扩展子进程的 stdin/stdout。

    后台读循环把收到的帧分为 response（按 id 唤醒等待中的 request）和
    request（扩展 → Host，例如 event/propose，交给 ``host_handler``）。
    """

    def __init__(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
        *,
        host_handler: Optional[HostRequestHandler] = None,
        request_timeout: float = DEFAULT_REQUEST_TIMEOUT,
        peer_label: str = "extension",
    ) -> None:
        self._reader = reader
        self._writer = writer
        self._host_handler = host_handler
        self._request_timeout = float(request_timeout)
        self._peer_label = peer_label
        self._next_id = 0
        self._pending: dict[int, asyncio.Future] = {}
        self._reader_task: Optional[asyncio.Task] = None
        self._closed = asyncio.Event()
        self._write_lock = asyncio.Lock()

    def start(self) -> None:
        """启动后台读循环（spawn 子进程后立即调用）。"""
        if self._reader_task is None:
            self._reader_task = asyncio.create_task(self._read_loop())

    async def close(self) -> None:
        """关闭 peer：取消读循环并让所有挂起请求失败。"""
        if self._reader_task is not None:
            self._reader_task.cancel()
            try:
                await self._reader_task
            except (asyncio.CancelledError, Exception):
                pass
            self._reader_task = None
        self._fail_pending(ExtensionUnavailable(f"{self._peer_label} peer closed"))

    async def request(
        self, method: str, params: Optional[dict[str, Any]] = None, *,
        timeout: Optional[float] = None,
    ) -> Any:
        """发送 Host → 扩展请求并等待响应；超时 / 对端错误抛异常。"""
        if self._reader_task is None:
            raise ExtensionUnavailable(f"{self._peer_label} peer not started")
        self._next_id += 1
        request_id = self._next_id
        loop = asyncio.get_running_loop()
        future: asyncio.Future = loop.create_future()
        self._pending[request_id] = future
        async with self._write_lock:
            try:
                self._writer.write(
                    encode_request(request_id, method, dict(params or {}))
                )
                await self._writer.drain()
            except (ConnectionError, BrokenPipeError) as exc:
                self._pending.pop(request_id, None)
                raise ExtensionUnavailable(
                    f"{self._peer_label} pipe broken: {exc}"
                ) from exc
        try:
            return await asyncio.wait_for(
                future, timeout or self._request_timeout
            )
        except asyncio.TimeoutError as exc:
            self._pending.pop(request_id, None)
            raise ProtocolError(
                f"{self._peer_label} did not answer {method} within "
                f"{timeout or self._request_timeout}s"
            ) from exc

    # -- 后台读循环 -----------------------------------------------------------

    async def _read_loop(self) -> None:
        try:
            while True:
                line = await self._reader.readline()
                if not line:
                    break  # 对端关闭 stdout（进程退出）
                try:
                    message = decode_message(line)
                except ProtocolError as exc:
                    LOG.warning("%s sent a bad frame: %s", self._peer_label, exc)
                    continue
                if "method" in message:
                    asyncio.create_task(self._handle_incoming(message))
                else:
                    self._resolve_response(message)
        except asyncio.CancelledError:
            raise
        except Exception:
            LOG.exception("%s read loop crashed", self._peer_label)
        finally:
            self._closed.set()
            self._fail_pending(
                ExtensionUnavailable(f"{self._peer_label} closed its stdout")
            )

    def _resolve_response(self, message: dict[str, Any]) -> None:
        request_id = message.get("id")
        future = self._pending.pop(request_id, None)
        if future is None or future.done():
            return
        error = message.get("error")
        if error is not None:
            future.set_exception(ExtensionRpcError(
                int(error.get("code", -32603)),
                str(error.get("message", "unknown error")),
                error.get("data"),
            ))
        else:
            future.set_result(message.get("result"))

    async def _handle_incoming(self, message: dict[str, Any]) -> None:
        """处理扩展 → Host 请求（event/propose 等）并回写响应。"""
        method = str(message.get("method") or "")
        request_id = message.get("id")
        params = message.get("params")
        params = params if isinstance(params, dict) else {}
        if method not in EXTENSION_METHODS:
            await self._send(encode_response(
                request_id,
                error={"code": -32601, "message": f"method not allowed: {method}"},
            ))
            return
        if self._host_handler is None:
            await self._send(encode_response(
                request_id,
                error={"code": -32601, "message": "no host handler registered"},
            ))
            return
        try:
            check_protocol_version(params)
            result = await self._host_handler(method, params)
            await self._send(encode_response(request_id, result=result))
        except Exception as exc:
            await self._send(encode_response(
                request_id, error=rpc_error_data(exc)))

    async def _send(self, payload: bytes) -> None:
        async with self._write_lock:
            try:
                self._writer.write(payload)
                await self._writer.drain()
            except (ConnectionError, BrokenPipeError):
                LOG.warning("%s pipe broken while responding", self._peer_label)

    def _fail_pending(self, exc: BaseException) -> None:
        for future in self._pending.values():
            if not future.done():
                future.set_exception(exc)
        self._pending.clear()

    @property
    def closed(self) -> bool:
        return self._closed.is_set()


# ---------------------------------------------------------------------------
# 扩展侧：同步 stdio 服务循环（纯 stdlib，供 Python 扩展直接复用）
# ---------------------------------------------------------------------------

#: 扩展方法处理器签名：``(params) -> result``；抛异常即返回 JSON-RPC error。
ExtensionMethodHandler = Callable[[dict[str, Any]], Any]

#: 扩展 → Host 请求的发送回调（由 serve_stdio 内部提供）。
HostRequestSender = Callable[[str, dict[str, Any]], Any]


def serve_stdio(
    handlers: dict[str, ExtensionMethodHandler],
    *,
    stdin: Any = None,
    stdout: Any = None,
    expected_protocol: int = PROTOCOL_VERSION,
    on_shutdown: Optional[Callable[[], None]] = None,
) -> None:
    """扩展侧服务循环：从 stdin 读行、分发、把响应写到 stdout。

    - 每个 request 的 params 必须带匹配的 ``protocol_version``（主版本），
      不匹配回 ``-32602``；
    - ``shutdown`` 处理后返回并结束循环（``on_shutdown`` 先行回调）；
    - 处理函数可以通过 ``request_host``（见 ``make_host_requester``）
      向宿主发 event/propose 并同步等待结果。
    """
    import sys

    in_stream = stdin if stdin is not None else sys.stdin
    out_stream = stdout if stdout is not None else sys.stdout
    buffer: dict[int, dict[str, Any]] = {}

    def _write(payload: bytes) -> None:
        out_stream.buffer.write(payload)
        out_stream.buffer.flush()

    def _handle_line(line: bytes) -> bool:
        """处理一行；返回 False 表示服务应退出。"""
        try:
            message = decode_message(line)
        except ProtocolError:
            return True
        if "method" not in message:
            # 是对 extend→host 请求的响应：暂存给等待中的 request_host
            buffer[int(message.get("id") or 0)] = message
            return True
        method = str(message.get("method") or "")
        request_id = message.get("id")
        params = message.get("params")
        params = params if isinstance(params, dict) else {}
        handler = handlers.get(method)
        if handler is None:
            _write(encode_response(
                request_id,
                error={"code": -32601, "message": f"unknown method: {method}"},
            ))
            return True
        try:
            if int(params.get("protocol_version", -1)) != expected_protocol:
                raise ProtocolError(
                    f"protocol major mismatch: host="
                    f"{params.get('protocol_version')}, extension={expected_protocol}"
                )
            result = handler(params)
            _write(encode_response(request_id, result=result))
        except Exception as exc:
            _write(encode_response(request_id, error=rpc_error_data(exc)))
        if method == "shutdown":
            if on_shutdown is not None:
                on_shutdown()
            return False
        return True

    next_host_id = [0]  # 扩展 → Host 的请求 id（与宿主 id 空间独立）

    def request_host(method: str, params: dict[str, Any]) -> Any:
        """向宿主发送请求并同步等待响应（供处理函数内使用）。"""
        next_host_id[0] += 1
        request_id = next_host_id[0]
        _write(encode_request(request_id, method, params))
        while True:
            line = in_stream.buffer.readline()
            if not line:
                raise ExtensionUnavailable("host closed stdin")
            buffered = buffer.pop(request_id, None)
            if buffered is not None:
                message = buffered
            else:
                try:
                    message = decode_message(line)
                except ProtocolError:
                    continue
                if "method" in message:
                    # 宿主在等待期间发来新请求：先处理它再继续等
                    if not _handle_line(line):
                        raise ExtensionUnavailable("host requested shutdown")
                    continue
                if int(message.get("id") or 0) != request_id:
                    buffer[int(message.get("id") or 0)] = message
                    continue
            error = message.get("error")
            if error is not None:
                raise ExtensionRpcError(
                    int(error.get("code", -32603)),
                    str(error.get("message", "unknown error")),
                    error.get("data"),
                )
            return message.get("result")

    serve_stdio.request_host = request_host  # type: ignore[attr-defined]

    while True:
        line = in_stream.buffer.readline()
        if not line:
            return
        if not _handle_line(line):
            return


__all__ = [
    "DEFAULT_REQUEST_TIMEOUT",
    "EXTENSION_METHODS",
    "HOST_METHODS",
    "MAX_MESSAGE_BYTES",
    "PROTOCOL_VERSION",
    "ExtensionMethodHandler",
    "ExtensionRpcError",
    "ExtensionUnavailable",
    "HostRequestHandler",
    "JsonRpcPeer",
    "ProtocolError",
    "check_protocol_version",
    "decode_message",
    "encode_request",
    "encode_response",
    "rpc_error_data",
    "serve_stdio",
]
