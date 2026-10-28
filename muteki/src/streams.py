from __future__ import annotations

import asyncio
import hashlib
import json
from dataclasses import dataclass, field
from typing import Any, AsyncIterator, Optional

import httpx
from websockets.asyncio.client import ClientConnection, connect as websocket_connect
from websockets.exceptions import ConnectionClosed

from muteki.platform.contracts.base import new_id, utcnow

from .contracts import ArtifactRef, ExecutionRequest
from .store import SrcStoreError


def _json_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), default=str).encode("utf-8")


def _headers(value: Any) -> list[list[str]]:
    if value is None:
        return []
    for name in ("raw_items", "multi_items", "items"):
        method = getattr(value, name, None)
        if method is not None:
            return [[str(key), str(item)] for key, item in method()]
    return []


@dataclass
class _SSEConnection:
    client: httpx.AsyncClient
    response: httpx.Response
    iterator: AsyncIterator[bytes]
    buffer: bytes = b""


@dataclass
class _WSConnection:
    connection: ClientConnection
    send_lock: asyncio.Lock = field(default_factory=asyncio.Lock)


class SrcStreamService:
    def __init__(self, src: Any) -> None:
        self.src = src
        self.store = src.store
        self.artifacts = src.artifacts
        self._websockets: dict[str, _WSConnection] = {}
        self._sse: dict[str, _SSEConnection] = {}
        self._locks: dict[str, asyncio.Lock] = {}
        self._frame_locks: dict[str, asyncio.Lock] = {}

    def _lock(self, stream_id: str) -> asyncio.Lock:
        return self._locks.setdefault(stream_id, asyncio.Lock())

    def _frame_lock(self, stream_id: str) -> asyncio.Lock:
        return self._frame_locks.setdefault(stream_id, asyncio.Lock())

    def _artifact(self, task_id: str, value: Any, name: str, *, original: bool = False, media_type: str = "application/json") -> ArtifactRef:
        raw = value if isinstance(value, bytes) else _json_bytes(value)
        ref = self.artifacts.put(task_id, raw, media_type=media_type, name=name)
        return ref if original else ref.model_copy(update={"original": False})

    def _request(self, task_id: str, stream_id: str, url: str, protocol: str, identity_ref: str, identity_revision: int, authorization_ref: str, extra: dict[str, Any]) -> ExecutionRequest:
        policy = self.src.execution.current_policy(task_id)
        arguments = {
            "url": url, "method": "GET", "protocol": protocol,
            "identity_ref": identity_ref, "authorization_ref": authorization_ref,
            "idempotent": True, **extra,
        }
        return ExecutionRequest(
            invocation_id=f"stream-connect:{stream_id}", task_id=task_id,
            thread_id=self.src.binding(task_id).thread_id,
            run_id=extra.get("run_id") or None,
            tool_name=f"src_{protocol}_connect", action="connect",
            arguments=arguments, arguments_hash=self.store.digest(arguments),
            resource_kind="http", identity_ref=identity_ref,
            identity_revision=identity_revision, policy_version=policy.version,
        )

    async def connect_websocket(
        self, task_id: str, url: str, *, identity_ref: str = "default",
        identity_revision: int = 0, authorization_ref: str,
        headers: Optional[dict[str, str]] = None, subprotocols: Optional[list[str]] = None,
        run_id: str = "", step_id: str = "", replay_of: str = "",
        idempotency_key: str = "", open_timeout: float = 20,
    ) -> dict[str, Any]:
        stream_id = new_id("sws")
        previous = self.store.require(task_id, "protocol_stream", replay_of) if replay_of else None
        root_id = str(previous.payload.get("root_stream_id") or replay_of) if previous else stream_id
        request = {
            "url": url, "identity_ref": identity_ref, "identity_revision": identity_revision,
            "authorization_ref": authorization_ref, "headers": headers or {},
            "subprotocols": subprotocols or [], "replay_of": replay_of,
        }
        if idempotency_key:
            prior = self.store.idempotent(task_id, "stream.connect", idempotency_key, request)
            if prior is not None:
                return prior
        connection_index = int(previous.payload.get("connection_index") or 0) + 1 if previous else 1
        execution_request = self._request(
            task_id, stream_id, url, "websocket", identity_ref, identity_revision,
            authorization_ref, {"run_id": run_id, "headers": headers or {}, "subprotocols": subprotocols or []},
        )

        async def open_connection(_request: ExecutionRequest) -> dict[str, Any]:
            connection = await websocket_connect(
                url, additional_headers=headers, subprotocols=subprotocols,
                open_timeout=open_timeout,
            )
            try:
                request_artifact = self._artifact(task_id, {
                    "method": "GET", "url": url, "headers": _headers(getattr(getattr(connection, "request", None), "headers", None)),
                    "representation": "derived-handshake-fields",
                }, f"{stream_id}-handshake-request")
                response = getattr(connection, "response", None)
                response_artifact = self._artifact(task_id, {
                    "status_code": getattr(response, "status_code", None),
                    "headers": _headers(getattr(response, "headers", None)),
                    "representation": "derived-handshake-fields",
                }, f"{stream_id}-handshake-response")
                now = utcnow()
                payload = {
                    "stream_id": stream_id, "root_stream_id": root_id,
                    "connection_index": connection_index, "protocol": "websocket",
                    "url": url, "task_id": task_id, "run_id": run_id, "step_id": step_id,
                    "identity_ref": identity_ref, "identity_revision": identity_revision,
                    "authorization_ref": authorization_ref, "state": "connected",
                    "capture_state": "complete", "opened_at": now.isoformat(),
                    "closed_at": None, "close": None, "next_seq": 0,
                    "request_ref": request_artifact.model_dump(mode="json"),
                    "response_ref": response_artifact.model_dump(mode="json"),
                    "subprotocol": getattr(connection, "subprotocol", None),
                    "replay_of": replay_of or None, "gaps": [],
                }
                self.store.put(
                    task_id, "protocol_stream", payload, record_id=stream_id,
                    parent_id=root_id, natural_key=f"{root_id}:{connection_index}",
                )
            except Exception:
                await connection.close()
                raise
            self._websockets[stream_id] = _WSConnection(connection)
            return self.store.require(task_id, "protocol_stream", stream_id).payload

        receipt = await self.src.execution.execute(execution_request, open_connection, wait_for_approval=True)
        if receipt.dispatch_state != "completed":
            raise SrcStoreError(receipt.error_code or "outcome_unknown", f"WebSocket connection did not complete: {receipt.dispatch_state}")
        if idempotency_key:
            self.store.remember(task_id, "stream.connect", idempotency_key, request, self.src.execution.read_result(task_id, receipt))
        return self.src.execution.read_result(task_id, receipt)

    async def send_websocket(self, task_id: str, stream_id: str, payload: str | bytes) -> dict[str, Any]:
        record = self.store.require(task_id, "protocol_stream", stream_id)
        holder = self._websockets.get(stream_id)
        if holder is None or record.payload["state"] != "connected":
            raise SrcStoreError("state_conflict", "WebSocket is not connected")
        async with holder.send_lock:
            try:
                await holder.connection.send(payload)
                return await self._append_event(task_id, stream_id, direction="outbound", payload=payload, state="sent")
            except Exception as exc:
                return await self._append_event(
                    task_id, stream_id, direction="outbound", payload=payload,
                    state="outcome_unknown", error=f"{type(exc).__name__}: {exc}",
                )

    async def receive_websocket(self, task_id: str, stream_id: str) -> dict[str, Any]:
        record = self.store.require(task_id, "protocol_stream", stream_id)
        holder = self._websockets.get(stream_id)
        if holder is None or record.payload["state"] != "connected":
            raise SrcStoreError("state_conflict", "WebSocket is not connected")
        try:
            async with self._frame_lock(stream_id):
                payload = await holder.connection.recv()
            return await self._append_event(task_id, stream_id, direction="inbound", payload=payload, state="received")
        except ConnectionClosed as exc:
            await self._close_record(task_id, stream_id, code=exc.code, reason=exc.reason, abnormal=exc.code != 1000)
            return {"state": "closed", "code": exc.code, "reason": exc.reason}

    async def _append_event(
        self, task_id: str, stream_id: str, *, direction: str,
        payload: str | bytes, state: str, error: str = "",
    ) -> dict[str, Any]:
        async with self._lock(stream_id):
            with self.store.platform.transaction():
                stream = self.store.require(task_id, "protocol_stream", stream_id)
                seq = int(stream.payload.get("next_seq") or 0) + 1
                binary = isinstance(payload, bytes)
                raw = payload if binary else payload.encode("utf-8")
                artifact = self.artifacts.put(
                    task_id, raw, media_type="application/octet-stream" if binary else "text/plain; charset=utf-8",
                    name=f"{stream_id}-{seq}",
                )
                event = {
                    "stream_id": stream_id, "root_stream_id": stream.payload["root_stream_id"],
                    "connection_index": stream.payload["connection_index"],
                    "seq": seq, "direction": direction, "opcode": 2 if binary else 1,
                    "payload_ref": artifact.model_dump(mode="json"), "payload_size": len(raw),
                    "state": state, "error": error, "captured_at": utcnow().isoformat(),
                    "server_acknowledged": False, "business_effect": "unverified",
                }
                self.store.put(
                    task_id, "protocol_stream_event", event,
                    record_id=f"{stream_id}:{seq}", parent_id=stream_id,
                    natural_key=f"{stream_id}:{seq}",
                )
                self.store.put(
                    task_id, "protocol_stream", {**stream.payload, "next_seq": seq},
                    record_id=stream_id, expected_revision=stream.revision,
                    parent_id=stream.parent_id, natural_key=stream.natural_key,
                )
            return event

    async def acknowledge(self, task_id: str, stream_id: str, event_seq: int, evidence_refs: list[str]) -> dict[str, Any]:
        event = self.store.require(task_id, "protocol_stream_event", f"{stream_id}:{event_seq}")
        if not evidence_refs:
            raise SrcStoreError("validation_error", "server acknowledgement requires actual response evidence")
        with self.store.platform.transaction():
            record = self.store.put(
                task_id, "protocol_ack", {
                    "stream_id": stream_id, "event_seq": event_seq,
                    "evidence_refs": evidence_refs, "recorded_at": utcnow().isoformat(),
                }, parent_id=stream_id, natural_key=f"{stream_id}:{event_seq}",
            )
            self.store.put(
                task_id, "protocol_stream_event", {
                    **event.payload, "server_acknowledged": True,
                    "ack_evidence_refs": list(evidence_refs),
                }, record_id=event.record_id, expected_revision=event.revision,
                parent_id=event.parent_id, natural_key=event.natural_key,
            )
        return record.payload

    async def close_websocket(self, task_id: str, stream_id: str, code: int = 1000, reason: str = "") -> None:
        holder = self._websockets.pop(stream_id, None)
        if holder is not None:
            await holder.connection.close(code=code, reason=reason)
        await self._close_record(task_id, stream_id, code=code, reason=reason, abnormal=code != 1000)

    async def _close_record(self, task_id: str, stream_id: str, *, code: int, reason: str, abnormal: bool) -> None:
        stream = self.store.require(task_id, "protocol_stream", stream_id)
        if stream.payload["state"] == "closed":
            return
        gaps = list(stream.payload.get("gaps") or [])
        if abnormal:
            gaps.append({"kind": "abnormal_disconnect", "code": code, "reason": reason})
        self.store.put(
            task_id, "protocol_stream", {
                **stream.payload, "state": "closed", "closed_at": utcnow().isoformat(),
                "capture_state": "partial" if abnormal else stream.payload["capture_state"],
                "close": {"code": code, "reason": reason}, "gaps": gaps,
            }, record_id=stream_id, expected_revision=stream.revision,
            parent_id=stream.parent_id, natural_key=stream.natural_key,
        )

    async def connect_sse(
        self, task_id: str, url: str, *, identity_ref: str = "default",
        identity_revision: int = 0, authorization_ref: str,
        headers: Optional[dict[str, str]] = None, run_id: str = "", step_id: str = "",
        replay_of: str = "", idempotency_key: str = "", timeout: float = 20,
    ) -> dict[str, Any]:
        stream_id = new_id("sse")
        previous = self.store.require(task_id, "protocol_stream", replay_of) if replay_of else None
        root_id = str(previous.payload.get("root_stream_id") or replay_of) if previous else stream_id
        last_event_id = str(previous.payload.get("last_event_id") or "") if previous else ""
        connection_index = int(previous.payload.get("connection_index") or 0) + 1 if previous else 1
        request_headers = {"Accept": "text/event-stream", **dict(headers or {})}
        if last_event_id:
            request_headers["Last-Event-ID"] = last_event_id
        request = {
            "url": url, "identity_ref": identity_ref, "identity_revision": identity_revision,
            "authorization_ref": authorization_ref, "headers": request_headers, "replay_of": replay_of,
        }
        if idempotency_key:
            prior = self.store.idempotent(task_id, "stream.connect", idempotency_key, request)
            if prior is not None:
                return prior
        execution_request = self._request(
            task_id, stream_id, url, "sse", identity_ref, identity_revision,
            authorization_ref, {"run_id": run_id, "headers": request_headers},
        )

        async def open_connection(_request: ExecutionRequest) -> dict[str, Any]:
            client = httpx.AsyncClient(timeout=httpx.Timeout(timeout, read=None), follow_redirects=False)
            try:
                response = await client.stream("GET", url, headers=request_headers).__aenter__()
                if response.status_code >= 400:
                    raise SrcStoreError("sse_http_error", f"SSE endpoint returned HTTP {response.status_code}")
                content_type = response.headers.get("content-type", "")
                if not content_type.lower().startswith("text/event-stream"):
                    raise SrcStoreError("sse_content_type_invalid", f"expected text/event-stream, got {content_type!r}")
                request_artifact = self._artifact(task_id, {
                    "method": "GET", "url": str(response.request.url),
                    "headers": _headers(response.request.headers),
                    "representation": "derived-request-fields",
                }, f"{stream_id}-request")
                response_artifact = self._artifact(task_id, {
                    "status_code": response.status_code, "headers": _headers(response.headers),
                    "http_version": response.http_version, "representation": "derived-response-fields",
                }, f"{stream_id}-response")
                payload = {
                    "stream_id": stream_id, "root_stream_id": root_id,
                    "connection_index": connection_index, "protocol": "sse", "url": url,
                    "task_id": task_id, "run_id": run_id, "step_id": step_id,
                    "identity_ref": identity_ref, "identity_revision": identity_revision,
                    "authorization_ref": authorization_ref, "state": "connected",
                    "capture_state": "complete", "opened_at": utcnow().isoformat(),
                    "closed_at": None, "close": None, "next_seq": 0,
                    "request_ref": request_artifact.model_dump(mode="json"),
                    "response_ref": response_artifact.model_dump(mode="json"),
                    "replay_of": replay_of or None, "last_event_id": last_event_id,
                    "content_encoding": response.headers.get("content-encoding", ""),
                    "payload_representation": "decoded-http-content", "gaps": [],
                }
                self.store.put(
                    task_id, "protocol_stream", payload, record_id=stream_id,
                    parent_id=root_id, natural_key=f"{root_id}:{connection_index}",
                )
            except Exception:
                await client.aclose()
                raise
            self._sse[stream_id] = _SSEConnection(client, response, response.aiter_bytes())
            return self.store.require(task_id, "protocol_stream", stream_id).payload

        receipt = await self.src.execution.execute(execution_request, open_connection, wait_for_approval=True)
        if receipt.dispatch_state != "completed":
            raise SrcStoreError(receipt.error_code or "outcome_unknown", f"SSE connection did not complete: {receipt.dispatch_state}")
        if idempotency_key:
            self.store.remember(task_id, "stream.connect", idempotency_key, request, self.src.execution.read_result(task_id, receipt))
        return self.src.execution.read_result(task_id, receipt)

    async def read_sse(self, task_id: str, stream_id: str) -> dict[str, Any]:
        stream = self.store.require(task_id, "protocol_stream", stream_id)
        connection = self._sse.get(stream_id)
        if connection is None or stream.payload["state"] != "connected":
            raise SrcStoreError("state_conflict", "SSE connection is not open")
        try:
            async with self._frame_lock(stream_id):
                raw = await self._next_sse_frame(connection)
        except StopAsyncIteration:
            partial = bool(connection.buffer)
            if partial:
                rem = connection.buffer
                connection.buffer = b""
                self.artifacts.put(
                    task_id, rem, media_type="text/event-stream; charset=utf-8",
                    name=f"{stream_id}-unterminated.sse", complete="partial",
                )
            await self._close_sse_record(task_id, stream_id, reason="unterminated_event" if partial else "eof", abnormal=partial)
            return {"state": "closed", "reason": "unterminated_event" if partial else "eof"}
        except (httpx.HTTPError, asyncio.TimeoutError, UnicodeError) as exc:
            if connection.buffer:
                rem = connection.buffer
                connection.buffer = b""
                self.artifacts.put(
                    task_id, rem, media_type="text/event-stream; charset=utf-8",
                    name=f"{stream_id}-partial.sse", complete="partial",
                )
            await self._close_sse_record(task_id, stream_id, reason=f"{type(exc).__name__}: {exc}", abnormal=True)
            return {"state": "closed", "reason": f"{type(exc).__name__}: {exc}", "capture_state": "partial"}
        async with self._lock(stream_id):
            with self.store.platform.transaction():
                current = self.store.require(task_id, "protocol_stream", stream_id)
                seq = int(current.payload.get("next_seq") or 0) + 1
                artifact = self.artifacts.put(task_id, raw, media_type="text/event-stream; charset=utf-8", name=f"{stream_id}-{seq}.sse")
                fields = self._parse_sse(raw)
                has_id = bool(fields.pop("has_id", False))
                duplicate = self._find_sse_event(task_id, current.payload["root_stream_id"], fields["id"])
                event = {
                    "stream_id": stream_id, "root_stream_id": current.payload["root_stream_id"],
                    "connection_index": current.payload["connection_index"],
                    "seq": seq, "direction": "inbound", "state": "received",
                    "payload_ref": artifact.model_dump(mode="json"), **fields,
                    "captured_at": utcnow().isoformat(), "delivery_state": "replayed" if duplicate else "received",
                    "duplicate_of": duplicate.record_id if duplicate else None,
                    "server_acknowledged": False, "business_effect": "unverified",
                }
                self.store.put(
                    task_id, "protocol_stream_event", event,
                    record_id=f"{stream_id}:{seq}", parent_id=stream_id,
                    natural_key=f"{stream_id}:{seq}",
                )
                next_last_id = fields["id"] if has_id else current.payload.get("last_event_id", "")
                self.store.put(
                    task_id, "protocol_stream", {
                        **current.payload, "next_seq": seq,
                        "last_event_id": next_last_id,
                    }, record_id=stream_id, expected_revision=current.revision,
                    parent_id=current.parent_id, natural_key=current.natural_key,
                )
            return event

    async def _next_sse_frame(self, connection: _SSEConnection) -> bytes:
        while True:
            earliest_idx = -1
            earliest_len = 0
            for delimiter in (b"\r\n\r\n", b"\n\n", b"\r\r"):
                index = connection.buffer.find(delimiter)
                if index >= 0 and (earliest_idx < 0 or index < earliest_idx):
                    earliest_idx = index
                    earliest_len = len(delimiter)
            if earliest_idx >= 0:
                end = earliest_idx + earliest_len
                raw, connection.buffer = connection.buffer[:end], connection.buffer[end:]
                return raw
            chunk = await connection.iterator.__anext__()
            if not chunk:
                raise StopAsyncIteration
            connection.buffer += chunk

    @staticmethod
    def _parse_sse(raw: bytes) -> dict[str, Any]:
        if raw.startswith(b"\xef\xbb\xbf"):
            raw = raw[3:]
        data: list[bytes] = []
        event_name = "message"
        event_id = ""
        has_id = False
        retry: Optional[int] = None
        comments: list[str] = []
        parse_errors: list[str] = []
        for line in raw.splitlines():
            if not line or line.startswith(b":"):
                comments.append(line.decode("utf-8", errors="replace"))
                continue
            name, separator, value = line.partition(b":")
            if not separator:
                name, value = line, b""
            elif value.startswith(b" "):
                value = value[1:]
            if name == b"data":
                data.append(value)
            elif name == b"event":
                event_name = value.decode("utf-8", errors="replace")
            elif name == b"id":
                if b"\x00" in value:
                    parse_errors.append("id_contains_null")
                else:
                    try:
                        event_id = value.decode("utf-8", errors="strict")
                        has_id = True
                    except UnicodeDecodeError:
                        parse_errors.append("id_invalid_utf8")
            elif name == b"retry":
                try:
                    parsed_retry = int(value)
                    if parsed_retry < 0:
                        parse_errors.append("retry_negative")
                    else:
                        retry = parsed_retry
                except ValueError:
                    parse_errors.append("retry_not_integer")
        return {
            "event": event_name, "id": event_id, "has_id": has_id, "retry": retry,
            "data_sha256": hashlib.sha256(b"\n".join(data)).hexdigest(),
            "comments": comments, "parse_errors": parse_errors,
        }

    def _find_sse_event(self, task_id: str, root_id: str, event_id: str) -> Any:
        if not event_id:
            return None
        for event in self.store.all(task_id, "protocol_stream_event"):
            if event.payload.get("root_stream_id") == root_id and event.payload.get("id") == event_id:
                return event
        return None

    async def close_sse(self, task_id: str, stream_id: str, reason: str = "operator_closed") -> None:
        connection = self._sse.pop(stream_id, None)
        if connection is not None:
            await connection.response.aclose()
            await connection.client.aclose()
        await self._close_sse_record(task_id, stream_id, reason=reason, abnormal=False)

    async def _close_sse_record(self, task_id: str, stream_id: str, *, reason: str, abnormal: bool) -> None:
        stream = self.store.require(task_id, "protocol_stream", stream_id)
        if stream.payload["state"] == "closed":
            return
        gaps = list(stream.payload.get("gaps") or [])
        if abnormal:
            gaps.append({"kind": "incomplete_stream", "reason": reason})
        self.store.put(
            task_id, "protocol_stream", {
                **stream.payload, "state": "closed", "closed_at": utcnow().isoformat(),
                "capture_state": "partial" if abnormal else stream.payload["capture_state"],
                "close": {"reason": reason}, "gaps": gaps,
            }, record_id=stream_id, expected_revision=stream.revision,
            parent_id=stream.parent_id, natural_key=stream.natural_key,
        )

    def _events_for_stream(self, task_id: str, stream_id: str) -> list[Any]:
        rows: list[Any] = []
        cursor = 0
        while True:
            page = self.store.list(task_id, "protocol_stream_event", parent_id=stream_id, after_rowid=cursor, limit=500)
            if not page:
                return rows
            rows.extend(page)
            cursor = self.store.rowid(page[-1])

    def export(self, task_id: str, root_stream_id: str) -> dict[str, Any]:
        root = self.store.require(task_id, "protocol_stream", root_stream_id)
        connections = [row for row in self.store.all(task_id, "protocol_stream") if row.payload.get("root_stream_id") == root_stream_id]
        connections.sort(key=lambda row: int(row.payload.get("connection_index") or 0))
        events = []
        for connection in connections:
            events.extend(self._events_for_stream(task_id, connection.record_id))
        events.sort(key=lambda row: (int(row.payload.get("connection_index") or 0) if row.payload.get("connection_index") else 0, row.payload.get("stream_id", ""), int(row.payload.get("seq") or 0)))
        return {
            "schema": "muteki.src.protocol-stream.v1", "root": root.payload,
            "connections": [row.payload for row in connections],
            "events": [row.payload for row in events],
            "gaps": [{"stream_id": row.record_id, "gap": gap} for row in connections for gap in row.payload.get("gaps", [])],
            "complete": all(row.payload.get("capture_state") == "complete" for row in connections),
        }

    def _any_stream(self, stream_id: str) -> Any:
        with self.store.platform.lock:
            row = self.store.platform.conn.execute(
                "SELECT * FROM src_records WHERE kind='protocol_stream' AND record_id=?",
                (stream_id,),
            ).fetchone()
        return self.store._decode(row) if row is not None else None

    async def close(self) -> None:
        for stream_id, holder in list(self._websockets.items()):
            await holder.connection.close()
            record = self._any_stream(stream_id)
            if record is not None:
                await self._close_record(record.task_id, stream_id, code=1001, reason="service_shutdown", abnormal=True)
        self._websockets.clear()
        for stream_id, connection in list(self._sse.items()):
            await connection.response.aclose()
            await connection.client.aclose()
            record = self._any_stream(stream_id)
            if record is not None:
                await self._close_sse_record(record.task_id, stream_id, reason="service_shutdown", abnormal=True)
        self._sse.clear()
        self._locks.clear()
        self._frame_locks.clear()


__all__ = ["SrcStreamService"]
