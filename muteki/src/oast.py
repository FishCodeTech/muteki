from __future__ import annotations

import asyncio
import hashlib
import json
import secrets
from datetime import datetime, timezone
from typing import Any, Awaitable, Callable, Optional
from urllib.parse import urlsplit

import httpx

from muteki.platform.contracts.base import new_id, utcnow

from .store import SrcStoreError


def _json_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), default=str).encode("utf-8")


def _aware(value: Optional[datetime]) -> datetime:
    return value if value is not None and value.tzinfo is not None else (value or utcnow()).replace(tzinfo=timezone.utc)


class LocalOastReceiver:
    """Small raw-HTTP callback receiver used for controlled and private OAST deployments."""

    def __init__(self, service: "SrcOastService") -> None:
        self.service = service
        self.server: Optional[asyncio.AbstractServer] = None
        self.base_url = ""

    async def start(self, host: str = "127.0.0.1", port: int = 0) -> str:
        if self.server is not None:
            return self.base_url
        self.server = await asyncio.start_server(self._handle, host=host, port=port)
        sockets = self.server.sockets or []
        if not sockets:
            raise SrcStoreError("capability_unavailable", "OAST receiver has no listening socket")
        actual_port = sockets[0].getsockname()[1]
        self.base_url = f"http://{host}:{actual_port}"
        return self.base_url

    async def close(self) -> None:
        server, self.server = self.server, None
        if server is not None:
            server.close()
            await server.wait_closed()

    async def _handle(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            header = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), timeout=15)
            if len(header) > 65536:
                raise ValueError("headers too large")
            first = header.split(b"\r\n", 1)[0]
            parts = first.split(b" ")
            if len(parts) != 3:
                raise ValueError("invalid request line")
            method, target = parts[0].decode("ascii"), parts[1].decode("ascii")
            headers: list[tuple[str, str]] = []
            content_length = 0
            for line in header.split(b"\r\n")[1:]:
                if not line:
                    continue
                name, separator, value = line.partition(b":")
                if not separator:
                    raise ValueError("malformed header")
                text = value.strip().decode("latin-1")
                headers.append((name.decode("latin-1"), text))
                if name.lower() == b"content-length":
                    content_length = int(text)
            if content_length > 32 * 1024 * 1024:
                raise ValueError("body too large")
            body = await asyncio.wait_for(reader.readexactly(content_length), timeout=30) if content_length else b""
            raw = header + body
            path = target or "/"
            if path == "/health":
                response = b'{"state":"ready","capture":"raw-http"}'
            elif path.startswith("/"):
                token = path[1:].split("/", 1)[0].split("?", 1)[0]
                source = writer.get_extra_info("peername")
                await self.service.record_interaction(
                    token, protocol="http", raw=raw,
                    source_address=f"{source[0]}:{source[1]}" if source else "unknown",
                    request_path=path, metadata={"method": method, "headers": headers},
                )
                response = b'{"accepted":true}'
            else:
                response = b'{"error":"not_found"}'
            status = 200 if response != b'{"error":"not_found"}' else 404
            writer.write(
                f"HTTP/1.1 {status} {'OK' if status == 200 else 'Not Found'}\r\n".encode()
                + b"Content-Type: application/json\r\nConnection: close\r\n"
                + f"Content-Length: {len(response)}\r\n\r\n".encode() + response
            )
            await writer.drain()
        except (asyncio.IncompleteReadError, asyncio.LimitOverrunError, ValueError, UnicodeError, asyncio.TimeoutError):
            try:
                writer.write(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                await writer.drain()
            except (ConnectionError, RuntimeError):
                pass
        except SrcStoreError as exc:
            writer.write(_json_bytes({"error": exc.code}) + b"\n")
            await writer.drain()
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except (ConnectionError, RuntimeError):
                pass


class HttpOastProvider:
    """Versioned JSON-over-HTTP provider contract for a real remote OAST service."""

    def __init__(
        self, base_url: str, *, credential_ref: str = "",
        credential_resolver: Optional[Callable[[str], Awaitable[dict[str, str]]]] = None,
        timeout: float = 20,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.credential_ref = credential_ref
        self.credential_resolver = credential_resolver
        self.timeout = timeout

    async def _headers(self) -> dict[str, str]:
        return await self.credential_resolver(self.credential_ref) if self.credential_resolver else {}

    async def allocate(self, interaction: dict[str, Any]) -> dict[str, Any]:
        async with httpx.AsyncClient(timeout=self.timeout, follow_redirects=False) as client:
            response = await client.post(f"{self.base_url}/interactions", json=interaction, headers=await self._headers())
            raw = response.content
            if response.status_code >= 400:
                raise SrcStoreError("oast_provider_error", f"OAST allocation failed with HTTP {response.status_code}", detail={"response_sha256": hashlib.sha256(raw).hexdigest()})
            value = response.json()
            return {"receipt": value, "raw_sha256": hashlib.sha256(raw).hexdigest(), "status_code": response.status_code}

    async def poll(self, cursor: str = "") -> dict[str, Any]:
        async with httpx.AsyncClient(timeout=self.timeout, follow_redirects=False) as client:
            response = await client.get(f"{self.base_url}/events", params={"cursor": cursor}, headers=await self._headers())
            raw = response.content
            if response.status_code >= 400:
                raise SrcStoreError("oast_provider_error", f"OAST polling failed with HTTP {response.status_code}", detail={"response_sha256": hashlib.sha256(raw).hexdigest()})
            value = response.json()
            if not isinstance(value, dict) or not isinstance(value.get("events"), list):
                raise SrcStoreError("oast_provider_invalid", "OAST provider response has no events list")
            return {**value, "raw_sha256": hashlib.sha256(raw).hexdigest(), "status_code": response.status_code}

    async def health(self) -> dict[str, Any]:
        async with httpx.AsyncClient(timeout=self.timeout, follow_redirects=False) as client:
            response = await client.get(f"{self.base_url}/health", headers=await self._headers())
            return {"status_code": response.status_code, "healthy": response.status_code < 400, "raw_sha256": hashlib.sha256(response.content).hexdigest()}


class SrcOastService:
    def __init__(self, src: Any) -> None:
        self.src = src
        self.store = src.store
        self.artifacts = src.artifacts
        self._receiver: Optional[LocalOastReceiver] = None
        self._providers: dict[str, HttpOastProvider] = {}
        self._locks: dict[str, asyncio.Lock] = {}

    async def start_local_receiver(self, host: str = "127.0.0.1", port: int = 0) -> str:
        if self._receiver is None:
            self._receiver = LocalOastReceiver(self)
        return await self._receiver.start(host, port)

    def register_provider(self, service_id: str, provider: HttpOastProvider) -> None:
        self._providers[service_id] = provider

    async def close(self) -> None:
        if self._receiver is not None:
            await self._receiver.close()
            self._receiver = None
        self._providers.clear()

    async def allocate(
        self, task_id: str, *, run_id: str = "", step_id: str = "",
        trigger_exchange_ids: list[str], protocols: list[str], expires_at: datetime,
        authorization_ref: str, service_instance: str = "local",
        retention_seconds: int = 30 * 86400, idempotency_key: str = "",
    ) -> dict[str, Any]:
        request = {
            "task_id": task_id, "run_id": run_id, "step_id": step_id,
            "trigger_exchange_ids": trigger_exchange_ids, "protocols": protocols,
            "expires_at": _aware(expires_at).isoformat(), "service_instance": service_instance,
        }
        if idempotency_key:
            prior = self.store.idempotent(task_id, "oast.allocate", idempotency_key, request)
            if prior is not None:
                return prior
        if not protocols or any(item not in {"http", "dns", "tcp", "udp"} for item in protocols):
            raise SrcStoreError("validation_error", "OAST protocols must contain http, dns, tcp or udp")
        if _aware(expires_at) <= utcnow():
            raise SrcStoreError("validation_error", "OAST expiration must be in the future")
        if service_instance == "local" and self._receiver is None:
            await self.start_local_receiver()
        token = secrets.token_urlsafe(24)
        while self.store.by_natural_key(task_id, "oast_interaction", token) is not None:
            token = secrets.token_urlsafe(24)
        interaction_id = new_id("soast")
        if service_instance == "local":
            base = self._receiver.base_url if self._receiver else ""
            callback_url = f"{base}/{token}"
        else:
            provider = self._providers.get(service_instance)
            if provider is None:
                raise SrcStoreError("capability_unavailable", f"OAST provider {service_instance!r} is not configured")
            callback_url = f"{provider.base_url.rstrip('/')}/{token}"
        now = utcnow()
        payload = {
            **request, "interaction_id": interaction_id, "token": token,
            "callback_url": callback_url, "authorization_ref": authorization_ref,
            "created_at": now.isoformat(), "retention_seconds": int(retention_seconds),
            "state": "active", "last_successful_poll_at": None,
            "poll_cursor": "", "source_health": {"state": "unknown"},
        }
        if service_instance != "local":
            receipt = await self._providers[service_instance].allocate(payload)
            payload["provider_receipt"] = receipt
        self.store.put(task_id, "oast_interaction", payload, record_id=interaction_id, natural_key=token)
        if idempotency_key:
            self.store.remember(task_id, "oast.allocate", idempotency_key, request, payload)
        return payload

    def trigger_target(self, task_id: str, interaction_id: str, protocol: str) -> str:
        record = self.store.require(task_id, "oast_interaction", interaction_id)
        data = record.payload
        if data["state"] != "active":
            raise SrcStoreError("oast_expired", "OAST interaction no longer accepts new triggers")
        if datetime.fromisoformat(data["expires_at"]) <= utcnow():
            raise SrcStoreError("oast_expired", "OAST interaction has expired")
        if protocol not in data["protocols"]:
            raise SrcStoreError("validation_error", "OAST interaction was not allocated for this protocol")
        if protocol == "dns":
            callback = data["callback_url"]
            host = urlsplit(callback if "://" in callback else f"https://{callback}").hostname
            return f"{data['token']}.{host}"
        return data["callback_url"]

    def expire(self, task_id: str, interaction_id: str, *, delete_token: bool = False) -> dict[str, Any]:
        record = self.store.require(task_id, "oast_interaction", interaction_id)
        state = "deleted" if delete_token else "expired"
        updated = self.store.put(
            task_id, "oast_interaction", {**record.payload, "state": state},
            record_id=interaction_id, expected_revision=record.revision, natural_key=record.natural_key,
        )
        return updated.payload

    async def record_interaction(
        self, token: str, *, protocol: str, raw: bytes, source_address: str,
        request_path: str = "", source_event_id: str = "", received_at: Optional[datetime] = None,
        metadata: Optional[dict[str, Any]] = None,
        representation: str = "raw_wire", token_match: str = "exact-token",
        health_probe: bool = False,
    ) -> dict[str, Any]:
        candidates: list[Any] = []
        with self.store.platform.lock:
            rows = list(self.store.platform.conn.execute(
                "SELECT task_id FROM src_records WHERE kind='oast_interaction' AND natural_key=?", (token,),
            ).fetchall())
        for row in rows:
            record = self.store.by_natural_key(str(row["task_id"]), "oast_interaction", token)
            if record is not None:
                candidates.append(record)
        if len(candidates) != 1:
            raise SrcStoreError("oast_token_unmatched", "OAST token does not identify exactly one interaction")
        interaction = candidates[0]
        task_id = interaction.task_id
        received = _aware(received_at)
        late = received > datetime.fromisoformat(interaction.payload["expires_at"]) or interaction.payload["state"] != "active"
        event_id = source_event_id or hashlib.sha256(raw + received.isoformat().encode()).hexdigest()
        is_wire = representation == "raw_wire"
        artifact = self.artifacts.put(
            task_id, raw,
            media_type=f"application/x-oast-{protocol}" if is_wire else "application/json",
            name=f"oast-{interaction.record_id}-{event_id}",
            source_run_id=interaction.payload.get("run_id") or None,
            original=is_wire,
        )
        payload = {
            "interaction_id": interaction.record_id, "token": token,
            "source_event_id": event_id, "received_at": received.isoformat(),
            "protocol": protocol, "raw_ref": artifact.model_dump(mode="json"),
            "representation": representation,
            "source_address": source_address, "request_path": request_path,
            "token_match": token_match, "late": late,
            "metadata": dict(metadata or {}), "health_probe": bool(health_probe),
        }
        record_id = f"{interaction.record_id}:{event_id}"
        existing = self.store.get(task_id, "oast_event", record_id)
        if existing is not None:
            if existing.payload["raw_ref"]["sha256"] != artifact.sha256:
                raise SrcStoreError("version_conflict", "OAST source event id has different bytes")
            return {**existing.payload, "deduplicated": True}
        self.store.put(
            task_id, "oast_event", payload, record_id=record_id,
            parent_id=interaction.record_id, natural_key=f"{interaction.record_id}:{event_id}",
        )
        return {**payload, "deduplicated": False}

    async def poll(self, task_id: str, interaction_id: str, *, max_pages: int = 100) -> dict[str, Any]:
        interaction = self.store.require(task_id, "oast_interaction", interaction_id)
        provider = self._providers.get(interaction.payload["service_instance"])
        if provider is None:
            raise SrcStoreError("capability_unavailable", "OAST interaction has no polling provider; use its callback receiver")
        cursor = str(interaction.payload.get("poll_cursor") or "")
        prior_seq = int(self.store.source_watermark(task_id, f"oast:{interaction_id}").get("next_seq") or 0)
        pages = 0
        received = 0
        latest = interaction
        complete = False
        try:
            while pages < max_pages:
                result = await provider.poll(cursor)
                page = await self._store_provider_page(task_id, interaction.payload, cursor, result)
                pages += 1
                for event in result["events"]:
                    event_token = str(event.get("token") or interaction.payload["token"])
                    target_interaction = self.store.by_natural_key(task_id, "oast_interaction", event_token)
                    if target_interaction is None:
                        continue
                    has_raw = bool(event.get("raw_hex"))
                    await self.record_interaction(
                        event_token, protocol=str(event.get("protocol") or "http"),
                        raw=bytes.fromhex(event["raw_hex"]) if has_raw else _json_bytes(event),
                        source_address=str(event.get("source_address") or "unknown"),
                        request_path=str(event.get("request_path") or ""),
                        source_event_id=str(event.get("event_id") or ""),
                        received_at=datetime.fromisoformat(event["received_at"]) if event.get("received_at") else None,
                        metadata=event.get("metadata"),
                        representation="raw_wire" if has_raw else "provider_declared",
                        token_match="exact-token" if has_raw else "provider_declared",
                        health_probe=bool(event.get("health_probe", False)),
                    )
                    received += 1
                next_cursor = str(result.get("next_cursor") or "")
                if next_cursor and next_cursor == cursor:
                    raise SrcStoreError("oast_provider_invalid", "OAST provider cursor did not advance")
                cursor = next_cursor
                latest = self.store.put(
                    task_id, "oast_interaction", {
                        **latest.payload, "poll_cursor": cursor,
                        "last_successful_poll_at": utcnow().isoformat(),
                        "source_health": {"state": "ready", "last_page": page},
                    }, record_id=interaction_id, expected_revision=latest.revision,
                    natural_key=latest.natural_key,
                )
                self.store.advance_source(
                    task_id, f"oast:{interaction_id}", expected_generation=0,
                    next_seq=prior_seq + pages, extractor_version="src.oast-poll.v1",
                )
                if not result.get("has_more"):
                    complete = True
                    break
            if not complete:
                raise SrcStoreError("capture_partial", "OAST polling reached the page limit before completion")
        except Exception as exc:
            latest = self.store.put(
                task_id, "oast_interaction", {
                    **latest.payload,
                    "source_health": {"state": "unavailable", "error": f"{type(exc).__name__}: {exc}"},
                }, record_id=interaction_id, expected_revision=latest.revision,
                natural_key=latest.natural_key,
            )
            if isinstance(exc, SrcStoreError):
                raise
            raise SrcStoreError("oast_provider_error", f"OAST polling failed: {type(exc).__name__}: {exc}") from exc
        return {"interaction_id": interaction_id, "pages": pages, "events": received, "cursor": cursor, "complete": complete}

    async def _store_provider_page(self, task_id: str, interaction: dict[str, Any], cursor: str, result: dict[str, Any]) -> dict[str, Any]:
        artifact = self.artifacts.put(
            task_id, _json_bytes(result), media_type="application/json",
            name=f"oast-poll-{interaction['interaction_id']}-{cursor or 'initial'}",
        )
        return {"cursor": cursor, "artifact": artifact.model_dump(mode="json"), "raw_sha256": result.get("raw_sha256")}

    async def health_probe(self, task_id: str, interaction_id: str) -> dict[str, Any]:
        interaction = self.store.require(task_id, "oast_interaction", interaction_id)
        provider = self._providers.get(interaction.payload["service_instance"])
        if provider is None:
            raise SrcStoreError("capability_unavailable", "OAST provider is not configured")
        result = await provider.health()
        artifact = self.artifacts.put(task_id, _json_bytes(result), media_type="application/json", name="oast-health")
        record = self.store.put(
            task_id, "oast_health", {**result, "interaction_id": interaction_id,
                                   "artifact": artifact.model_dump(mode="json"), "checked_at": utcnow().isoformat()},
            parent_id=interaction_id,
        )
        return record.payload

    def events(self, task_id: str, interaction_id: str, *, after_rowid: int = 0, limit: int = 100) -> dict[str, Any]:
        self.store.require(task_id, "oast_interaction", interaction_id)
        rows = self.store.list(task_id, "oast_event", parent_id=interaction_id, after_rowid=after_rowid, limit=limit + 1)
        page = rows[:limit]
        return {
            "events": [row.payload for row in page],
            "next_rowid": self.store.rowid(page[-1]) if len(rows) > limit and page else None,
            "complete": len(rows) <= limit,
            "watermark": self.store.source_watermark(task_id, f"oast:{interaction_id}"),
        }


__all__ = ["SrcOastService", "LocalOastReceiver", "HttpOastProvider"]
