"""共享 stdio JSONL / JSON-RPC 子进程 transport（RUNTIME-03，任务书 7.1/7.2）。

本模块是 ACP（``acp.py``，Cursor/Grok，RUNTIME-04 的 Kimi/OMP 复用）与
Pi JSONL RPC（``pi.py``）共用的底层传输，只做三件事：

1. **严格 LF 分帧**（``JsonLineFramer``）：只按 ``\\n`` 切分记录，容忍行尾
   ``\\r``；不使用按行读取器（Node ``readline`` 会把 JSON 字符串内合法的
   ``U+2028/U+2029`` 当换行，见 docs/research/third_party_verification.md
   §PI 影响①，ACP 侧同样按字节流分帧实现）。
2. **请求/响应关联**（``StdioJsonlPeer.request``）：出站消息分配单调
   ``id``，入站消息按 ``is_response`` 判定（ACP：带 ``id`` 且含
   ``result``/``error``；Pi：``type=="response"``）路由到等待方；
   其余入站消息（通知、事件流、Agent→Client 反向请求）交给
   ``on_message`` 回调。
3. **反向请求应答**（``respond``）：ACP ``session/request_permission``
   这类 Agent→Client 请求（同一条消息里既有 ``id`` 又有 ``method``）
   由回调处理后通过 ``respond`` 写回结果。

transport 层无任何业务状态：不认识 session、turn、tool 等概念，语义映射
全部在 ``acp.py`` / ``pi.py`` 完成。进程退出时不伪造任何完成事件，只把
EOF/错误传给等待中的请求（以异常形式失败）。
"""

from __future__ import annotations

import asyncio
import base64
import itertools
import json
import os
import signal
import subprocess
import uuid
from typing import Any, Awaitable, Callable, Optional

#: 出站消息 id 计数器（进程内单调，避免与 Agent 侧 id 冲突加前缀）。
_ID_COUNTER = itertools.count(1)

RPC_FRAME_BYTES = 1_048_576
RPC_REASSEMBLED_BYTES = 67_108_864
RPC_CHUNK_BYTES = 262_144


class RpcChunkAssembler:
    """OMP RPC v2 ``rpc_chunk`` 的严格单序列重组器。"""

    def __init__(self) -> None:
        self._state: Optional[dict[str, Any]] = None

    def push(self, value: dict[str, Any]) -> Optional[dict[str, Any]]:
        if value.get("type") != "rpc_chunk":
            if self._state is not None:
                raise ValueError("rpc chunk sequence interrupted")
            return value
        chunk_id = value.get("chunkId")
        index = value.get("index")
        count = value.get("count")
        byte_length = value.get("byteLength")
        if (
            not isinstance(chunk_id, str) or not chunk_id
            or len(chunk_id) > 128
            or not isinstance(index, int) or isinstance(index, bool)
            or not isinstance(count, int) or isinstance(count, bool)
            or not isinstance(byte_length, int) or isinstance(byte_length, bool)
            or index < 0 or count < 2
            or count > ((RPC_REASSEMBLED_BYTES + RPC_CHUNK_BYTES - 1)
                        // RPC_CHUNK_BYTES)
            or index >= count
            or byte_length < RPC_FRAME_BYTES
            or byte_length > RPC_REASSEMBLED_BYTES
        ):
            raise ValueError("invalid rpc chunk metadata")
        data = value.get("data")
        if not isinstance(data, str) or not data:
            raise ValueError("invalid rpc chunk data")
        try:
            part = base64.b64decode(data, validate=True)
        except (ValueError, TypeError) as exc:
            raise ValueError("invalid rpc chunk data") from exc
        if len(part) > RPC_CHUNK_BYTES:
            raise ValueError("rpc chunk payload exceeds the transport limit")
        if self._state is None:
            if index != 0:
                raise ValueError("rpc chunk sequence must start at index 0")
            self._state = {
                "chunk_id": chunk_id,
                "count": count,
                "byte_length": byte_length,
                "next_index": 0,
                "chunks": [],
                "received": 0,
            }
        state = self._state
        assert state is not None
        if (
            state["chunk_id"] != chunk_id
            or state["count"] != count
            or state["byte_length"] != byte_length
            or state["next_index"] != index
        ):
            raise ValueError("rpc chunk sequence mismatch")
        state["chunks"].append(part)
        state["received"] += len(part)
        state["next_index"] += 1
        if state["received"] > byte_length:
            raise ValueError("rpc chunk sequence exceeds declared length")
        if state["next_index"] < count:
            return None
        self._state = None
        if state["received"] != byte_length:
            raise ValueError("rpc chunk sequence length mismatch")
        try:
            decoded = json.loads(b"".join(state["chunks"]).decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise ValueError("invalid reassembled rpc frame") from exc
        if not isinstance(decoded, dict):
            raise ValueError("rpc frame must be an object")
        return decoded


class JsonLineFramer:
    """严格 LF 分帧器：``feed`` 收字节流，吐出完整的 JSON 记录。

    - 仅以 ``\\n`` 为记录分隔符；行尾 ``\\r`` 去除；
    - ``U+2028/U+2029`` 是多字节 UTF-8 序列，不含 ``0x0A``，因此按字节
      切分天然安全；
    - 非 JSON 行（启动横幅、日志混入 stdout）以 ``("raw", str)`` 形式
      透出，由上层决定忽略或记为 warning，不在传输层静默丢弃。
    """

    def __init__(self) -> None:
        self._buf = bytearray()

    def feed(self, data: bytes) -> "list[tuple[str, Any]]":
        """返回 ``[("json", obj) | ("raw", line_text), ...]``。"""
        self._buf.extend(data)
        out: "list[tuple[str, Any]]" = []
        while True:
            idx = self._buf.find(b"\n")
            if idx < 0:
                break
            line = bytes(self._buf[:idx])
            del self._buf[: idx + 1]
            if line.endswith(b"\r"):
                line = line[:-1]
            if not line.strip():
                continue
            try:
                import json
                out.append(("json", json.loads(line.decode("utf-8"))))
            except (ValueError, UnicodeDecodeError):
                out.append(("raw", line.decode("utf-8", errors="replace")))
        return out

    def pending(self) -> bytes:
        """EOF 时仍未闭合的半行（进程异常截断的排障依据）。"""
        return bytes(self._buf)


def default_is_response(msg: Any) -> bool:
    """默认响应判定：ACP（id + result/error）或 Pi（type=="response"）。"""
    if not isinstance(msg, dict):
        return False
    if msg.get("type") == "response":
        return True
    # ACP：响应有 id 且无 method；反向请求有 id 且带 method，不算响应。
    return "id" in msg and "method" not in msg and (
        "result" in msg or "error" in msg)


class PeerClosedError(RuntimeError):
    """对端进程退出 / stdout EOF 后仍有未决请求时抛出。"""


class StdioJsonlPeer:
    """一个 stdio 换行分隔 JSON 子进程连接。

    参数：

    - ``argv``：进程命令行（cwd 隔离由调用方经 ``cwd`` 保证）；
    - ``on_message``：非响应入站消息回调（通知 / 事件 / 反向请求），
      在 reader task 的 event loop 上调用，可以是 async；
    - ``is_response``：响应判定函数，默认兼容 ACP 与 Pi 两种形态；
    - ``request_envelope``：把 ``(msg_id, method, params)`` 包装成出站
      消息体的函数；缺省为 JSON-RPC 2.0（ACP）；Pi 传自己的 envelope。
    """

    def __init__(
        self,
        argv: list[str],
        *,
        cwd: Optional[str] = None,
        env: Optional[dict[str, str]] = None,
        label: str = "",
        is_response: Callable[[Any], bool] = default_is_response,
        request_envelope: Optional[
            Callable[[str, str, Optional[dict]], dict[str, Any]]] = None,
        on_message: Optional[Callable[[dict[str, Any]], Awaitable[None]]] = None,
        on_raw_line: Optional[Callable[[str], None]] = None,
    ) -> None:
        self.argv = list(argv)
        self.cwd = cwd
        self.env = env
        self.label = label or (argv[0] if argv else "peer")
        self._is_response = is_response
        self._envelope = request_envelope or _jsonrpc_envelope
        self._on_message = on_message
        self._on_raw_line = on_raw_line
        self._proc: Optional[asyncio.subprocess.Process] = None
        self._framer = JsonLineFramer()
        self._reader_task: Optional[asyncio.Task] = None
        self._stderr_task: Optional[asyncio.Task] = None
        self._stderr_tail = bytearray()
        self._pending: dict[str, asyncio.Future] = {}
        # 仅保留最后一条 RPC 的无敏感诊断元数据。不得保存 params：其中可能
        # 包含用户输入、凭据或工具参数，错误投影会将本摘要写入持久化事件。
        self._last_request: Optional[dict[str, Any]] = None
        self._closed = False
        self._rpc_chunks_enabled = False
        self._rpc_chunks = RpcChunkAssembler()
        #: 排障计数：非 JSON 行、未匹配消息数（不进事件投影）。
        self.stats = {"raw_lines": 0, "unmatched": 0}

    # -- 生命周期 -----------------------------------------------------------

    @property
    def running(self) -> bool:
        return (self._proc is not None and self._proc.returncode is None
                and not self._closed)

    def diagnostics(self) -> dict[str, Any]:
        """返回可安全写入运行事件的 peer 诊断摘要。

        这里刻意不包含 argv、cwd、env 或 RPC 参数。前三者可能含路径/凭据，
        参数则会包含用户消息和工具输入；错误事件只需要确认实际命令、等待
        上限、进程存活状态及完整 stderr 尾部。
        """
        proc = self._proc
        return {
            "label": self.label,
            "running": self.running,
            "returncode": proc.returncode if proc is not None else None,
            "last_request": dict(self._last_request or {}),
            "stderr_tail": self._stderr_detail(),
            "stats": dict(self.stats),
        }

    async def start(self) -> None:
        """拉起子进程并启动 stdout reader task。"""
        if self.running:
            return
        env = None
        if self.env is not None:
            env = {**os.environ, **self.env}
        self._proc = await asyncio.create_subprocess_exec(
            *self.argv,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            cwd=self.cwd or None,
            env=env,
        )
        self._closed = False
        self._stderr_tail.clear()
        self._stderr_task = asyncio.ensure_future(self._read_stderr())
        self._reader_task = asyncio.ensure_future(self._read_loop())

    async def close(self) -> int:
        """终止进程并失败所有未决请求；返回退出码（无进程时为 -1）。"""
        self._closed = True
        proc = self._proc
        if proc is None:
            return -1
        descendants: list[int] = []
        if proc.returncode is None and proc.pid:
            # 只按 PPID 收子孙。ACP 子进程和 uvicorn 同一进程组，
            # 不能 killpg。
            descendants = _descendant_pids(proc.pid)
        if proc.returncode is None:
            try:
                proc.terminate()
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except (asyncio.TimeoutError, ProcessLookupError):
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
                await proc.wait()
        for pid in descendants:
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
        if self._reader_task is not None:
            self._reader_task.cancel()
            try:
                await self._reader_task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
        if self._stderr_task is not None:
            self._stderr_task.cancel()
            try:
                await self._stderr_task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
        self._fail_pending(PeerClosedError(f"{self.label} closed"))
        return int(proc.returncode if proc.returncode is not None else -1)

    # -- 出站 ----------------------------------------------------------------

    async def send(self, msg: dict[str, Any]) -> None:
        """写一条 JSON 记录（LF 结尾）。"""
        if not self.running or self._proc is None or self._proc.stdin is None:
            raise PeerClosedError(f"{self.label} is not running")
        encoded = json.dumps(msg, ensure_ascii=False).encode("utf-8")
        frames = [encoded]
        if self._rpc_chunks_enabled and len(encoded) > RPC_FRAME_BYTES:
            if len(encoded) > RPC_REASSEMBLED_BYTES:
                raise ValueError("rpc frame exceeds the reassembled transport limit")
            chunk_id = f"muteki-{uuid.uuid4().hex}"
            parts = [encoded[i:i + RPC_CHUNK_BYTES]
                     for i in range(0, len(encoded), RPC_CHUNK_BYTES)]
            frames = [json.dumps({
                "type": "rpc_chunk",
                "chunkId": chunk_id,
                "index": index,
                "count": len(parts),
                "byteLength": len(encoded),
                "data": base64.b64encode(part).decode("ascii"),
            }).encode("utf-8") for index, part in enumerate(parts)]
        for frame in frames:
            self._proc.stdin.write(frame + b"\n")
        await self._proc.stdin.drain()

    def enable_rpc_chunks(self) -> None:
        """在 OMP ``negotiate_protocol`` 成功后启用 v2 双向分块。"""
        self._rpc_chunks_enabled = True

    async def request(
        self,
        method: str,
        params: Optional[dict[str, Any]] = None,
        *,
        timeout: Optional[float] = 120.0,
    ) -> dict[str, Any]:
        """发送请求并等待响应；返回完整响应消息（含 result/error）。

        ``timeout`` 为 ``None`` 或 ``<= 0`` 时一直等到对端返回，不设上限。
        对话模式的 ``session/prompt`` 必须走这条路径：一整轮可以含多轮
        工具调用，不能用固定秒数切断。
        """
        msg_id = f"muteki-{next(_ID_COUNTER)}"
        loop = asyncio.get_running_loop()
        fut: asyncio.Future = loop.create_future()
        self._pending[msg_id] = fut
        wait_s = None if timeout is None or float(timeout) <= 0 else float(timeout)
        request_diagnostics = {
            "method": str(method),
            "timeout_seconds": wait_s,
            "state": "pending",
        }
        self._last_request = request_diagnostics
        try:
            await self.send(self._envelope(msg_id, method, params))
            request_diagnostics["state"] = "sent"
            if wait_s is None:
                response = await fut
            else:
                try:
                    response = await asyncio.wait_for(fut, timeout=wait_s)
                except asyncio.TimeoutError:
                    raise asyncio.TimeoutError(
                        f"{method} timed out after {wait_s:g}s"
                    ) from None
            request_diagnostics["state"] = "responded"
            return response
        except BaseException as exc:
            # 异常正文可能包含上游返回的敏感内容；事件里只记录异常类型。
            request_diagnostics["state"] = "failed"
            request_diagnostics["error_type"] = type(exc).__name__
            raise
        finally:
            self._pending.pop(msg_id, None)

    async def notify(self, method: str, params: Optional[dict[str, Any]] = None) -> None:
        """发送通知（无 id，不等响应；如 ACP ``session/cancel``）。"""
        await self.send({"jsonrpc": "2.0", "method": method,
                         **({"params": params} if params is not None else {})})

    async def respond(
        self,
        msg_id: Any,
        *,
        result: Optional[dict[str, Any]] = None,
        error: Optional[dict[str, Any]] = None,
    ) -> None:
        """应答 Agent→Client 反向请求（如 ``session/request_permission``）。"""
        msg: dict[str, Any] = {"jsonrpc": "2.0", "id": msg_id}
        if error is not None:
            msg["error"] = error
        else:
            msg["result"] = result if result is not None else {}
        await self.send(msg)

    # -- 入站 ----------------------------------------------------------------

    async def _read_stderr(self) -> None:
        """持续排空 stderr，并保留经过长度限制的启动错误尾部。"""
        assert self._proc is not None and self._proc.stderr is not None
        try:
            while True:
                chunk = await self._proc.stderr.read(4096)
                if not chunk:
                    return
                self._stderr_tail.extend(chunk)
                if len(self._stderr_tail) > 8192:
                    del self._stderr_tail[:-8192]
        except asyncio.CancelledError:
            raise

    def _stderr_detail(self) -> str:
        detail = bytes(self._stderr_tail).decode(
            "utf-8", errors="replace").strip()
        if not detail:
            return ""
        return detail[-2000:]

    async def _read_loop(self) -> None:
        assert self._proc is not None and self._proc.stdout is not None
        try:
            while True:
                chunk = await self._proc.stdout.read(65536)
                if not chunk:
                    break
                for kind, item in self._framer.feed(chunk):
                    if kind == "raw":
                        self.stats["raw_lines"] += 1
                        if self._on_raw_line is not None:
                            self._on_raw_line(str(item))
                        continue
                    await self._dispatch(item)
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001
            # reader 异常不能静默吞掉：记录原因并失败所有未决请求。
            self.stats["reader_error"] = str(exc)[:200]
        finally:
            # stderr has its own drain task; one loop tick lets the final startup
            # diagnostic reach the bounded tail before pending requests fail.
            await asyncio.sleep(0)
            detail = self._stderr_detail()
            self._fail_pending(
                PeerClosedError(
                    f"{self.label} reached stdout EOF"
                    + (f": {detail}" if detail else "")))

    async def _dispatch(self, msg: Any) -> None:
        if isinstance(msg, dict) and msg.get("type") == "rpc_chunk":
            if not self._rpc_chunks_enabled:
                raise ValueError("rpc chunk received before protocol negotiation")
        if isinstance(msg, dict) and self._rpc_chunks_enabled:
            msg = self._rpc_chunks.push(msg)
            if msg is None:
                return
        if (
            isinstance(msg, dict)
            and msg.get("type") == "response"
            and msg.get("command") == "negotiate_protocol"
            and msg.get("success") is True
            and isinstance(msg.get("data"), dict)
            and msg["data"].get("protocolVersion") == 2
        ):
            # 对端可能紧接响应发送分块帧，须在唤醒 request 等待方前启用。
            self._rpc_chunks_enabled = True
        if self._is_response(msg):
            fut = self._pending.pop(str(msg.get("id")), None)
            if fut is not None and not fut.done():
                fut.set_result(msg)
            else:
                self.stats["unmatched"] += 1
            return
        if isinstance(msg, dict) and self._on_message is not None:
            await self._on_message(msg)

    def _fail_pending(self, exc: Exception) -> None:
        for fut in self._pending.values():
            if not fut.done():
                fut.set_exception(exc)
        self._pending.clear()


def _descendant_pids(root_pid: int) -> list[int]:
    """按 PPID 收集 ``root_pid`` 的子孙。不使用 PGID。

    对话 ACP 子进程和 uvicorn 同组；按组杀会误伤 Web 服务。
    """
    try:
        output = subprocess.check_output(
            ["ps", "-axo", "pid=,ppid="], text=True, timeout=2)
    except (OSError, subprocess.SubprocessError):
        return []
    children: dict[int, list[int]] = {}
    for line in output.splitlines():
        parts = line.split()
        if len(parts) < 2:
            continue
        try:
            pid, ppid = int(parts[0]), int(parts[1])
        except ValueError:
            continue
        children.setdefault(ppid, []).append(pid)
    out: list[int] = []
    stack = list(children.get(root_pid, []))
    seen: set[int] = set()
    while stack:
        pid = stack.pop()
        if pid in seen or pid == root_pid:
            continue
        seen.add(pid)
        out.append(pid)
        stack.extend(children.get(pid, []))
    return out


def _jsonrpc_envelope(
    msg_id: str, method: str, params: Optional[dict[str, Any]]
) -> dict[str, Any]:
    """JSON-RPC 2.0 请求（ACP 形态）。"""
    return {"jsonrpc": "2.0", "id": msg_id, "method": method,
            **({"params": params} if params is not None else {})}


__all__ = [
    "JsonLineFramer",
    "PeerClosedError",
    "RpcChunkAssembler",
    "StdioJsonlPeer",
    "default_is_response",
]
