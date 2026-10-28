from __future__ import annotations

import asyncio
import socket
import ssl
from dataclasses import dataclass, field
from typing import Any
from urllib.parse import urlsplit

from .contracts import ExecutionRequest, HttpExchange
from .exchanges import header_values, parse_http_request, parse_http_response
from .store import SrcStoreError
from muteki.platform.contracts.base import utcnow


class HttpExchangeError(RuntimeError):
    def __init__(
        self, code: str, message: str, *, exchange_id: str | None = None,
        outcome_known: bool = False, response_raw: bytes | None = None,
        capture_state: str = "unavailable", limitations: list[str] | None = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.exchange_id = exchange_id
        self.outcome_known = outcome_known
        self.response_raw = response_raw
        self.capture_state = capture_state
        self.limitations = limitations or []


@dataclass
class _TransportFailure(Exception):
    code: str
    message: str
    outcome_known: bool
    response_raw: bytes | None = None
    capture_state: str = "unavailable"
    limitations: list[str] = field(default_factory=list)
    request_bytes_sent: int = 0


class _Reader:
    def __init__(self, connection: socket.socket) -> None:
        self.connection = connection
        self.buffer = bytearray()
        self.captured = bytearray()

    def _receive(self) -> bytes:
        data = self.connection.recv(65536)
        if not data:
            raise EOFError
        self.buffer.extend(data)
        self.captured.extend(data)
        return data

    def readline(self, *, limit: int = 1048576) -> bytes:
        while True:
            index = self.buffer.find(b"\n")
            if index >= 0:
                if index + 1 > limit:
                    raise _TransportFailure("capture_unavailable", "HTTP line exceeds the parser limit", False, bytes(self.captured))
                line = bytes(self.buffer[:index + 1])
                del self.buffer[:index + 1]
                return line
            if len(self.buffer) > limit:
                raise _TransportFailure("capture_unavailable", "HTTP line exceeds the parser limit", False, bytes(self.captured))
            try:
                self._receive()
            except EOFError:
                raise _TransportFailure("outcome_unknown", "connection closed inside an HTTP line", False, bytes(self.captured)) from None

    def read_exact(self, size: int, *, limit: int | None = None) -> bytes:
        if limit is not None and size > limit:
            wanted = limit
            self.read_exact(wanted)
            raise _TransportFailure(
                "capture_partial", "response body exceeds the selected capture limit",
                False, bytes(self.captured), "partial", ["response_body_capture_limit"],
            )
        while len(self.buffer) < size:
            try:
                self._receive()
            except EOFError:
                self.buffer.clear()
                raise _TransportFailure("outcome_unknown", "connection closed before the complete response body", False, bytes(self.captured), "partial") from None
        data = bytes(self.buffer[:size])
        del self.buffer[:size]
        return data

    def read_to_eof(self, *, limit: int | None = None) -> bytes:
        while True:
            if limit is not None and len(self.buffer) >= limit:
                extra = len(self.buffer) - limit
                if extra > 0:
                    del self.captured[-extra:]
                self.buffer.clear()
                raise _TransportFailure(
                    "capture_partial", "response body exceeds the selected capture limit",
                    False, bytes(self.captured), "partial", ["response_body_capture_limit"],
                )
            try:
                self._receive()
            except EOFError:
                data = bytes(self.buffer)
                self.buffer.clear()
                return data


class ManagedHttpClient:
    def __init__(self, src: Any) -> None:
        self.src = src
        self.exchanges = getattr(src, "exchanges", None)

    def _service(self) -> Any:
        if self.exchanges is None:
            from .exchanges import ExchangeService
            self.exchanges = ExchangeService(self.src, http_client=self)
        return self.exchanges

    async def send(
        self, *, request: ExecutionRequest, raw_request: bytes,
        context_id: str, identity_revision: int, browser_generation: int,
        source_run_id: str | None, actor_ref: str, replay_of: str | None,
        redirect_parent: str | None, authorization_ref: str, decision_ref: str,
        capture_limit: int | None = None, timeout: float = 30.0,
    ) -> HttpExchange:
        service = self._service()
        target = str(request.arguments.get("url") or "")
        scheme = urlsplit(target).scheme or "http"
        try:
            parsed = parse_http_request(raw_request, default_scheme=scheme)
            if parsed["url"] != target:
                raise SrcStoreError("task_binding_invalid", "execution target differs from the raw request target")
            self._validate_framing(parsed)
            exchange = HttpExchange(
                task_id=request.task_id, source_run_id=source_run_id,
                source_kind="http_tool", actor_ref=actor_ref, context_id=context_id,
                identity_revision=identity_revision, browser_generation=browser_generation,
                declared_target=target, observed_url=parsed["url"], method=parsed["method"],
                started_at=request.created_at, request_raw_ref=self.src.artifacts.put(
                    request.task_id, raw_request, media_type="message/http",
                    name=f"{request.invocation_id}-request.http", source_run_id=source_run_id,
                ), protocol=parsed["version"].lower(), redirect_parent=redirect_parent,
                replay_of=replay_of, execution_state="dispatched", capture_state="unavailable",
                authorization_ref=authorization_ref, decision_ref=decision_ref,
                metadata={"capture": "managed-raw-socket", "request_bytes_sent": 0},
            )
            exchange = service.record_exchange(exchange)
        except Exception as exc:
            if isinstance(exc, (SrcStoreError, ValueError)):
                raise HttpExchangeError("capability_unavailable", str(exc), outcome_known=True) from exc
            raise
        try:
            response_raw, capture_state, limitations, transport = await asyncio.to_thread(
                self._perform, parsed, raw_request, timeout=timeout, capture_limit=capture_limit,
            )
        except _TransportFailure as exc:
            metadata = dict(exchange.metadata)
            metadata.update(self._transport_metadata(exc.response_raw))
            metadata["request_bytes_sent"] = exc.request_bytes_sent
            state = "completed" if exc.code == "capture_partial" else "failed" if exc.outcome_known else "outcome_unknown"
            capture_state = exc.capture_state
            if exc.response_raw:
                capture_state = "partial" if capture_state == "unavailable" else capture_state
            updated = exchange.model_copy(update={
                "response_raw_ref": self.src.artifacts.put(
                    request.task_id, exc.response_raw, media_type="message/http",
                    name=f"{exchange.exchange_id}-response-partial.http",
                    complete=capture_state,
                ) if exc.response_raw is not None else None,
                "execution_state": state, "capture_state": capture_state,
                "capture_limitations": exc.limitations,
                "finished_at": utcnow(),
                "metadata": metadata,
            })
            recorded = service.record_exchange(updated)
            if exc.code == "capture_partial":
                return recorded
            raise HttpExchangeError(
                exc.code, exc.message, exchange_id=exchange.exchange_id,
                outcome_known=exc.outcome_known, response_raw=exc.response_raw,
                capture_state=capture_state, limitations=exc.limitations,
            ) from exc
        metadata = dict(exchange.metadata)
        metadata.update(transport)
        completed = exchange.model_copy(update={
            "response_raw_ref": self.src.artifacts.put(
                request.task_id, response_raw, media_type="message/http",
                name=f"{exchange.exchange_id}-response.http", complete=capture_state,
            ),
            "execution_state": "completed", "capture_state": capture_state,
            "capture_limitations": limitations, "finished_at": utcnow(),
            "metadata": metadata,
        })
        return service.record_exchange(completed)

    @staticmethod
    def _validate_framing(parsed: dict[str, Any]) -> None:
        lengths = header_values(parsed["headers"], "content-length")
        if len(set(lengths)) > 1:
            raise SrcStoreError("capability_unavailable", "conflicting Content-Length values are not replayable")
        if lengths:
            try:
                if int(lengths[0]) < 0:
                    raise ValueError
            except ValueError as exc:
                raise SrcStoreError("capture_unavailable", "invalid Content-Length") from exc
        transfer = header_values(parsed["headers"], "transfer-encoding")
        if transfer and lengths:
            raise SrcStoreError("capability_unavailable", "Transfer-Encoding and Content-Length cannot be replayed together")

    @staticmethod
    def _transport_metadata(response_raw: bytes | None) -> dict[str, Any]:
        return {"response_bytes_received": len(response_raw or b"")}

    def _perform(
        self, parsed: dict[str, Any], raw_request: bytes, *,
        timeout: float, capture_limit: int | None,
    ) -> tuple[bytes, str, list[str], dict[str, Any]]:
        host = parsed["host"]
        port = parsed["port"]
        sent = 0
        reader: _Reader | None = None
        try:
            connection = socket.create_connection((host, port), timeout=timeout)
        except (OSError, socket.timeout) as exc:
            raise _TransportFailure("connection_failed", f"{type(exc).__name__}: {exc}", True) from exc
        try:
            connection.settimeout(timeout)
            if parsed["scheme"] == "https":
                context = ssl.create_default_context()
                connection = context.wrap_socket(connection, server_hostname=host)
            view = memoryview(raw_request)
            while sent < len(view):
                written = connection.send(view[sent:])
                if written <= 0:
                    raise _TransportFailure("outcome_unknown", "socket closed while sending request", False)
                sent += written
            reader = _Reader(connection)
            response_raw, state, limitations = self._read_response(reader, parsed["method"], capture_limit)
            peer = connection.getpeername()
            return response_raw, state, limitations, {
                "request_bytes_sent": sent, "response_bytes_received": len(response_raw),
                "observed_target": peer[0] if peer else "unknown", "observed_port": peer[1] if peer else port,
                "tls": parsed["scheme"] == "https", "response_complete": state == "complete",
            }
        except _TransportFailure as exc:
            exc.request_bytes_sent = sent
            if reader is not None and reader.captured:
                exc.response_raw = bytes(reader.captured)
                if exc.capture_state == "unavailable":
                    exc.capture_state = "partial"
            if sent and exc.code == "connection_failed":
                exc.code = "outcome_unknown"
                exc.outcome_known = False
            raise
        except (ssl.SSLError, socket.timeout, TimeoutError) as exc:
            captured = bytes(reader.captured) if reader is not None and reader.captured else None
            raise _TransportFailure(
                "outcome_unknown" if sent else "connection_failed",
                f"{type(exc).__name__}: {exc}", not bool(sent), captured,
                "partial" if captured else "unavailable", request_bytes_sent=sent,
            ) from exc
        except OSError as exc:
            captured = bytes(reader.captured) if reader is not None and reader.captured else None
            raise _TransportFailure(
                "outcome_unknown" if sent else "connection_failed",
                f"{type(exc).__name__}: {exc}", not bool(sent), captured,
                "partial" if captured else "unavailable", request_bytes_sent=sent,
            ) from exc
        finally:
            try:
                connection.close()
            except OSError:
                pass

    def _read_response(self, reader: _Reader, method: str, capture_limit: int | None) -> tuple[bytes, str, list[str]]:
        limitations: list[str] = []
        raw = bytearray()
        while True:
            head = bytearray()
            while True:
                line = reader.readline()
                head.extend(line)
                if line in {b"\r\n", b"\n"}:
                    break
            raw.extend(head)
            try:
                response = parse_http_response(bytes(head), method=method)
            except SrcStoreError as exc:
                raise _TransportFailure("capture_unavailable", str(exc), False, bytes(reader.captured), "partial") from exc
            if 100 <= response["status_code"] < 200 and response["status_code"] != 101:
                continue
            break
        status = response["status_code"]
        if status == 101:
            raise _TransportFailure("capability_unavailable", "protocol upgrade is not an HTTP response", False, bytes(reader.captured), "partial")
        headers = response["headers"]
        if status in {204, 304} or method.upper() == "HEAD" or 100 <= status < 200:
            return bytes(raw), "complete", limitations
        transfer = [item.strip().lower() for value in header_values(headers, "transfer-encoding") for item in value.split(",") if item.strip()]
        lengths = header_values(headers, "content-length")
        if len(set(lengths)) > 1:
            raise _TransportFailure("capture_unavailable", "conflicting response Content-Length", False, bytes(reader.captured), "partial")
        try:
            if transfer:
                if lengths:
                    raise _TransportFailure("capture_unavailable", "response has both Transfer-Encoding and Content-Length", False, bytes(reader.captured), "partial")
                if transfer[-1] != "chunked":
                    raise _TransportFailure("capability_unavailable", "unsupported response Transfer-Encoding", False, bytes(reader.captured), "partial")
                raw.extend(self._read_chunked(reader, capture_limit))
            elif lengths:
                length = int(lengths[0])
                if length < 0:
                    raise ValueError
                raw.extend(reader.read_exact(length, limit=capture_limit))
            else:
                raw.extend(reader.read_to_eof(limit=capture_limit))
        except ValueError as exc:
            raise _TransportFailure("capture_unavailable", "invalid response Content-Length", False, bytes(reader.captured), "partial") from exc
        except _TransportFailure as exc:
            exc.response_raw = bytes(reader.captured)
            exc.outcome_known = False
            raise
        return bytes(raw), "complete", limitations

    def _read_chunked(self, reader: _Reader, capture_limit: int | None) -> bytes:
        raw = bytearray()
        body_size = 0
        while True:
            size_line = reader.readline()
            raw.extend(size_line)
            try:
                size = int(size_line.strip().split(b";", 1)[0], 16)
            except ValueError as exc:
                raise _TransportFailure("capture_unavailable", "invalid chunk size", False, bytes(raw), "partial") from exc
            if size == 0:
                while True:
                    trailer = reader.readline()
                    raw.extend(trailer)
                    if trailer in {b"\r\n", b"\n"}:
                        return bytes(raw)
            body_size += size
            if capture_limit is not None and body_size > capture_limit:
                raise _TransportFailure(
                    "capture_partial", "chunked response exceeds the capture limit",
                    False, bytes(raw), "partial", ["response_body_capture_limit"],
                )
            raw.extend(reader.read_exact(size))
            raw.extend(reader.readline())


__all__ = ["ManagedHttpClient", "HttpExchangeError"]
