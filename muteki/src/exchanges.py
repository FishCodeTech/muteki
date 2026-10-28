from __future__ import annotations

import base64
import difflib
import gzip
import hashlib
import io
import ipaddress
import json
import re
import zipfile
import zlib
from datetime import datetime
from typing import Any, Iterable
from urllib.parse import parse_qsl, urlsplit

from muteki.platform.command_handlers.cursor import assert_cursor_scope, encode_cursor, load_cursor_key
from muteki.platform.contracts.base import new_id, utcnow

from .contracts import ExecutionRequest, HttpExchange, RequestDraft, ResourceFact
from .store import SrcStoreError

EXCHANGE_KIND = "http_exchange"
DRAFT_KIND = "request_draft"
COMPARISON_KIND = "exchange_comparison"
EXECUTION_KIND = "http_execution"
TEXT_MEDIA_PREFIXES = ("text/", "application/json", "application/xml", "application/javascript", "application/x-www-form-urlencoded")
IDEMPOTENT_METHODS = frozenset({"GET", "HEAD", "OPTIONS", "TRACE", "PUT", "DELETE"})


def _b64(data: bytes | None) -> str | None:
    return base64.b64encode(data).decode("ascii") if data is not None else None


def _json(value: Any) -> Any:
    if isinstance(value, bytes):
        return {"base64": _b64(value), "size": len(value), "sha256": hashlib.sha256(value).hexdigest()}
    if isinstance(value, datetime):
        return value.isoformat()
    if isinstance(value, dict):
        return {str(key): _json(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_json(item) for item in value]
    if hasattr(value, "model_dump"):
        return _json(value.model_dump(mode="json"))
    return value


def _split_message(raw: bytes) -> tuple[bytes, bytes, str]:
    for marker in (b"\r\n\r\n", b"\n\n"):
        index = raw.find(marker)
        if index >= 0:
            return raw[:index], raw[index + len(marker):], marker.decode("ascii")
    return raw, b"", ""


def _headers(raw_head: bytes, *, response: bool) -> tuple[list[str], list[list[str]]]:
    try:
        lines = raw_head.decode("iso-8859-1").splitlines()
    except UnicodeDecodeError as exc:
        raise SrcStoreError("capture_unavailable", "HTTP start line or headers are not ISO-8859-1") from exc
    if not lines or any(not line.strip() for line in lines[1:]):
        raise SrcStoreError("capture_unavailable", "HTTP headers are incomplete")
    start = lines[0]
    headers: list[list[str]] = []
    for line in lines[1:]:
        if line[:1] in {" ", "\t"} or ":" not in line:
            raise SrcStoreError("capability_unavailable", "folded or malformed HTTP headers cannot be normalized safely")
        name, value = line.split(":", 1)
        if not name or any(ord(char) <= 32 or ord(char) == 127 for char in name):
            raise SrcStoreError("capture_unavailable", "invalid HTTP header name")
        headers.append([name, value.lstrip(" \t")])
    return [start], headers


def _normalized_request_host(host: str) -> str:
    value = host.rstrip(".").lower()
    try:
        return str(ipaddress.ip_address(value))
    except ValueError:
        return value.encode("idna").decode("ascii")


def _request_authority(
    value: str, scheme: str, *, absolute: bool = False,
) -> tuple[str, str, int]:
    try:
        if not absolute and "://" in value:
            raise ValueError("Host header includes a scheme")
        parsed = urlsplit(value if absolute else f"{scheme}://{value}")
        port = parsed.port or (443 if parsed.scheme == "https" else 80)
    except ValueError as exc:
        raise SrcStoreError("capture_unavailable", "HTTP request authority is invalid") from exc
    if (parsed.scheme not in {"http", "https"} or not parsed.hostname
            or parsed.username is not None or parsed.password is not None
            or parsed.path or parsed.query or parsed.fragment):
        raise SrcStoreError("capture_unavailable", "HTTP request authority is invalid")
    return parsed.scheme, _normalized_request_host(parsed.hostname), port


def parse_http_request(raw: bytes, *, default_scheme: str = "http") -> dict[str, Any]:
    head, body, separator = _split_message(raw)
    if not separator:
        raise SrcStoreError("capture_partial", "request headers are incomplete")
    start, headers = _headers(head, response=False)
    parts = start[0].split(" ")
    if len(parts) != 3:
        raise SrcStoreError("capture_unavailable", "invalid HTTP request line")
    method, target, version = parts
    if version not in {"HTTP/1.0", "HTTP/1.1"}:
        raise SrcStoreError("capability_unavailable", "only HTTP/1.0 and HTTP/1.1 raw requests are supported")
    host_values = [value for name, value in headers if name.lower() == "host"]
    if len(host_values) > 1:
        raise SrcStoreError("capability_unavailable", "multiple Host headers require an explicit raw channel")
    if target.startswith("http://") or target.startswith("https://"):
        parsed = urlsplit(target)
        target_authority = _request_authority(
            f"{parsed.scheme}://{parsed.netloc}", parsed.scheme, absolute=True,
        )
        if host_values and _request_authority(host_values[0], parsed.scheme) != target_authority:
            raise SrcStoreError(
                "capability_unavailable",
                "absolute request target and Host header must identify the same origin",
            )
    else:
        if not target.startswith("/"):
            raise SrcStoreError("capability_unavailable", "only origin-form or absolute-form request targets are supported")
        if not host_values:
            raise SrcStoreError("capture_unavailable", "Host header is required to derive the target URL")
        _request_authority(host_values[0], default_scheme)
        parsed = urlsplit(f"{default_scheme}://{host_values[0]}{target}")
    if not parsed.hostname:
        raise SrcStoreError("capture_unavailable", "request target host is unavailable")
    return {
        "method": method, "target": target, "version": version, "headers": headers,
        "body": body, "url": parsed.geturl(), "host": parsed.hostname,
        "port": parsed.port or (443 if parsed.scheme == "https" else 80),
        "scheme": parsed.scheme, "path": parsed.path or "/", "query": parsed.query,
        "query_parameters": parse_qsl(parsed.query, keep_blank_values=True),
        "header_separator": separator,
    }


def _parse_status_line(line: str) -> tuple[str, int, str]:
    parts = line.split(" ", 2)
    if len(parts) < 2:
        raise SrcStoreError("capture_unavailable", "invalid HTTP response status line")
    version, status_text = parts[0], parts[1]
    reason = parts[2] if len(parts) > 2 else ""
    if version not in {"HTTP/1.0", "HTTP/1.1"}:
        raise SrcStoreError("capability_unavailable", "only HTTP/1.0 and HTTP/1.1 raw responses are supported")
    if len(status_text) != 3 or not status_text.isdigit():
        raise SrcStoreError("capture_unavailable", "invalid HTTP response status line")
    return version, int(status_text), reason


def parse_http_response(raw: bytes, *, method: str = "") -> dict[str, Any]:
    remaining = raw
    interim_responses: list[dict[str, Any]] = []
    consumed = 0
    while True:
        head, body, separator = _split_message(remaining)
        if not separator:
            raise SrcStoreError("capture_partial", "response headers are incomplete")
        start, headers = _headers(head, response=True)
        version, status_code, reason = _parse_status_line(start[0])
        consumed += len(head) + len(separator.encode("ascii"))
        if 100 <= status_code < 200 and status_code != 101 and body:
            interim_responses.append({
                "version": version, "status_code": status_code,
                "reason": reason, "headers": headers,
            })
            remaining = body
            continue
        return {
            "version": version, "status_code": status_code, "reason": reason,
            "headers": headers, "body": body, "body_offset": consumed,
            "method": method.upper(), "header_separator": separator,
            "interim_responses": interim_responses,
        }


def header_values(headers: Iterable[Iterable[str]], name: str) -> list[str]:
    return [value for key, value in headers if key.lower() == name.lower()]


def media_type(headers: Iterable[Iterable[str]]) -> str:
    values = header_values(headers, "content-type")
    return values[0].split(";", 1)[0].strip().lower() if values else ""


def is_textual(media: str, body: bytes) -> bool:
    return not b"\x00" in body and (media.startswith(TEXT_MEDIA_PREFIXES) or any(media.endswith(suffix) for suffix in ("+json", "+xml", "+text")))


def decoded_response_body(raw: bytes, headers: list[list[str]], *, method: str = "") -> tuple[bytes, list[str]]:
    body = parse_http_response(raw, method=method)["body"]
    limitations: list[str] = []
    if method.upper() == "HEAD":
        return body, limitations
    transfer = [item.strip().lower() for value in header_values(headers, "transfer-encoding") for item in value.split(",") if item.strip()]
    if transfer and transfer[-1] == "chunked":
        try:
            decoded = bytearray()
            offset = 0
            while True:
                ending = body.find(b"\r\n", offset)
                separator_size = 2
                if ending < 0:
                    ending = body.find(b"\n", offset)
                    separator_size = 1
                if ending < 0:
                    raise ValueError("missing chunk size line")
                size = int(body[offset:ending].split(b";", 1)[0], 16)
                offset = ending + separator_size
                if size == 0:
                    body = bytes(decoded)
                    break
                if len(body) < offset + size + separator_size:
                    raise ValueError("incomplete chunk data")
                decoded.extend(body[offset:offset + size])
                offset += size
                if body[offset:offset + separator_size] not in {b"\r\n", b"\n"}:
                    raise ValueError("missing chunk terminator")
                offset += separator_size
        except ValueError as exc:
            limitations.append(f"chunk_decode_failed:{exc}")
    encodings = [item.strip().lower() for value in header_values(headers, "content-encoding") for item in value.split(",") if item.strip()]
    try:
        for encoding in reversed(encodings):
            if encoding in {"identity", ""}:
                continue
            if encoding == "gzip":
                body = gzip.decompress(body)
            elif encoding == "deflate":
                body = zlib.decompress(body)
            else:
                limitations.append(f"unsupported_content_encoding:{encoding}")
                break
    except (gzip.BadGzipFile, zlib.error, EOFError) as exc:
        limitations.append(f"content_decode_failed:{type(exc).__name__}")
    return body, limitations


def replace_request_target(raw: bytes, target_url: str) -> bytes:
    parsed = parse_http_request(raw, default_scheme=urlsplit(target_url).scheme or "http")
    destination = urlsplit(target_url)
    if destination.scheme not in {"http", "https"} or not destination.hostname:
        raise SrcStoreError("task_binding_invalid", "redirect target must be a complete HTTP(S) URL")
    target = destination.path or "/"
    if destination.query:
        target += "?" + destination.query
    start = f"{parsed['method']} {target} {parsed['version']}".encode("iso-8859-1")
    head, _body, separator = _split_message(raw)
    lines = head.decode("iso-8859-1").splitlines()
    updated_headers: list[bytes] = []
    host_updated = False
    for line in lines[1:]:
        if line.lower().startswith("host:"):
            if not host_updated:
                updated_headers.append(f"Host: {destination.netloc}".encode("iso-8859-1"))
                host_updated = True
        else:
            updated_headers.append(line.encode("iso-8859-1"))
    if not host_updated:
        updated_headers.insert(0, f"Host: {destination.netloc}".encode("iso-8859-1"))
    return b"\r\n".join([start, *updated_headers]) + separator.encode("ascii") + parsed["body"]


def _origin(url: str) -> tuple[str, str, int]:
    parts = urlsplit(url)
    return (
        parts.scheme.lower(), _normalized_request_host(parts.hostname or ""),
        parts.port or (443 if parts.scheme == "https" else 80),
    )


def _without_origin_credentials(raw: bytes) -> bytes:
    head, body, separator = _split_message(raw)
    line_break = b"\r\n" if separator == "\r\n\r\n" else b"\n"
    lines = head.split(line_break)
    sensitive = {b"authorization", b"cookie", b"cookie2", b"proxy-authorization"}
    kept = [lines[0], *(line for line in lines[1:] if line.split(b":", 1)[0].lower() not in sensitive)]
    return line_break.join(kept) + separator.encode("ascii") + body


class ExchangeService:
    def __init__(self, src: Any, http_client: Any = None) -> None:
        self.src = src
        self.store = src.store
        self.artifacts = src.artifacts
        if http_client is None:
            from .http_client import ManagedHttpClient
            http_client = ManagedHttpClient(src)
        self.http = http_client
        self.http.exchanges = self

    def _task(self, task_id: str) -> None:
        self.src.binding(task_id)

    def _idempotency_begin(
        self, task_id: str, operation: str, key: str, request: Any, execution_id: str,
    ) -> tuple[bool, dict[str, Any] | None]:
        digest = self.store.digest(request)
        with self.store.platform.transaction():
            row = self.store.platform.conn.execute(
                "SELECT request_hash,response FROM src_idempotency WHERE task_id=? AND operation=? AND idempotency_key=?",
                (task_id, operation, key),
            ).fetchone()
            if row is not None:
                if row["request_hash"] != digest:
                    raise SrcStoreError("version_conflict", "idempotency key was used for different input")
                return False, json.loads(row["response"])
            self.store.platform.conn.execute(
                "INSERT INTO src_idempotency (task_id,operation,idempotency_key,request_hash,response,created_at) VALUES (?,?,?,?,?,?)",
                (task_id, operation, key, digest, json.dumps({
                    "state": "pending", "execution_id": execution_id,
                }), utcnow().isoformat()),
            )
        return True, None

    def _idempotency_finish(
        self, task_id: str, operation: str, key: str, request: Any, response: dict[str, Any],
    ) -> None:
        with self.store.platform.transaction():
            self.store.platform.conn.execute(
                "UPDATE src_idempotency SET response=? WHERE task_id=? AND operation=? AND idempotency_key=?",
                (json.dumps(_json(response), ensure_ascii=False), task_id, operation, key),
            )

    def record_exchange(
        self, exchange: HttpExchange, *, request_raw: bytes | None = None,
        response_raw: bytes | None = None,
    ) -> HttpExchange:
        self._task(exchange.task_id)
        if request_raw is not None:
            exchange = exchange.model_copy(update={"request_raw_ref": self.artifacts.put(
                exchange.task_id, request_raw, media_type="message/http",
                name=f"{exchange.exchange_id}-request.http", source_run_id=exchange.source_run_id,
                source_event_seq=exchange.source_event_seq,
            )})
        if response_raw is not None:
            exchange = exchange.model_copy(update={"response_raw_ref": self.artifacts.put(
                exchange.task_id, response_raw, media_type="message/http",
                name=f"{exchange.exchange_id}-response.http", source_run_id=exchange.source_run_id,
                source_event_seq=exchange.source_event_seq, complete=exchange.capture_state,
            )})
        existing = self.store.get(exchange.task_id, EXCHANGE_KIND, exchange.exchange_id)
        existing_revision = None
        if existing is not None:
            existing_revision = existing.revision
            current = HttpExchange.model_validate(existing.payload)
            if current.request_raw_ref.sha256 != exchange.request_raw_ref.sha256:
                raise SrcStoreError("version_conflict", "original request bytes are immutable")
            current_response = current.response_raw_ref.sha256 if current.response_raw_ref else None
            next_response = exchange.response_raw_ref.sha256 if exchange.response_raw_ref else None
            if current_response is not None and current_response != next_response:
                raise SrcStoreError("version_conflict", "original response bytes are immutable")
            if any(getattr(current, field) != getattr(exchange, field) for field in (
                "source_kind", "source_run_id", "source_event_seq", "replay_of", "redirect_parent",
            )):
                raise SrcStoreError("version_conflict", "exchange provenance is immutable")
            if current.execution_state in {"completed", "failed", "outcome_unknown", "cancelled", "denied"} and current.execution_state != exchange.execution_state:
                raise SrcStoreError("version_conflict", "a terminal exchange cannot be reopened")
        request_bytes = self.artifacts.read(exchange.request_raw_ref, expected_task_id=exchange.task_id)
        parsed_request = parse_http_request(request_bytes, default_scheme=urlsplit(exchange.observed_url).scheme or "http")
        if exchange.observed_url and exchange.observed_url != parsed_request["url"]:
            raise SrcStoreError("capture_unavailable", "observed URL does not match the raw request target")
        if not exchange.observed_url:
            exchange = exchange.model_copy(update={"observed_url": parsed_request["url"]})
        if exchange.method.upper() != parsed_request["method"].upper():
            raise SrcStoreError("capture_unavailable", "exchange method does not match raw request")
        exchange = exchange.model_copy(update={"request_headers": parsed_request["headers"]})
        metadata = dict(exchange.metadata)
        metadata.update({
            "request_target": parsed_request["target"], "request_body_sha256": hashlib.sha256(parsed_request["body"]).hexdigest(),
            "request_body_size": len(parsed_request["body"]), "query_parameters": parsed_request["query_parameters"],
            "raw_representation": "original" if exchange.protocol in {"http/1.0", "http/1.1"} else "collector-fields",
        })
        if exchange.response_raw_ref is not None:
            response_bytes = self.artifacts.read(exchange.response_raw_ref, expected_task_id=exchange.task_id)
            try:
                parsed_response = parse_http_response(response_bytes, method=exchange.method)
                exchange = exchange.model_copy(update={
                    "response_headers": parsed_response["headers"],
                    "status_code": exchange.status_code if exchange.status_code is not None else parsed_response["status_code"],
                })
                decoded, limitations = decoded_response_body(response_bytes, parsed_response["headers"], method=exchange.method)
                if limitations:
                    exchange = exchange.model_copy(update={"capture_limitations": [*exchange.capture_limitations, *limitations]})
                if decoded != parsed_response["body"]:
                    ref = self.artifacts.put(exchange.task_id, decoded, name=f"{exchange.exchange_id}-decoded-response")
                    metadata["decoded_body_ref"] = ref.model_dump(mode="json")
                metadata["response_body_sha256"] = hashlib.sha256(parsed_response["body"]).hexdigest()
                metadata["response_body_size"] = len(parsed_response["body"])
                metadata["media_type"] = media_type(parsed_response["headers"])
            except SrcStoreError as exc:
                if exchange.capture_state == "complete":
                    raise
                metadata["response_parse_error"] = exc.code
        exchange = exchange.model_copy(update={"metadata": metadata})
        self.store.put(
            exchange.task_id, EXCHANGE_KIND, exchange.model_dump(mode="json"),
            record_id=exchange.exchange_id, parent_id=exchange.source_run_id or "",
            generation=exchange.browser_generation, expected_revision=existing_revision,
        )
        record = self.store.require(exchange.task_id, EXCHANGE_KIND, exchange.exchange_id)
        self.store.advance_source(
            exchange.task_id, "http-exchanges", expected_generation=0,
            next_seq=self.store.rowid(record), extractor_version="HttpExchange.v1",
        )
        return exchange

    def record(self, task_id: str, **fields: Any) -> HttpExchange:
        request_raw = fields.pop("request_raw")
        response_raw = fields.pop("response_raw", None)
        request_ref = self.artifacts.put(task_id, request_raw, media_type="message/http", name="request.http")
        exchange = HttpExchange(task_id=task_id, request_raw_ref=request_ref, **fields)
        return self.record_exchange(exchange, response_raw=response_raw)

    def update_exchange(self, task_id: str, exchange_id: str, changes: dict[str, Any], *, expected_revision: int | None = None) -> HttpExchange:
        record = self.store.require(task_id, EXCHANGE_KIND, exchange_id)
        current = HttpExchange.model_validate(record.payload)
        updated = current.model_copy(update=changes)
        if updated.request_raw_ref != current.request_raw_ref or updated.response_raw_ref != current.response_raw_ref:
            raise SrcStoreError("version_conflict", "exchange raw artifacts are immutable")
        if any(getattr(current, field) != getattr(updated, field) for field in (
            "source_kind", "source_run_id", "source_event_seq", "actor_ref",
            "context_id", "identity_revision", "browser_generation",
            "declared_target", "observed_url", "method", "replay_of", "redirect_parent",
        )):
            raise SrcStoreError("version_conflict", "exchange provenance is immutable")
        if current.execution_state in {"completed", "failed", "outcome_unknown", "cancelled", "denied"} and updated.execution_state != current.execution_state:
            raise SrcStoreError("version_conflict", "a terminal exchange cannot be reopened")
        self.store.put(
            task_id, EXCHANGE_KIND, updated.model_dump(mode="json"), record_id=exchange_id,
            expected_revision=expected_revision if expected_revision is not None else record.revision,
            parent_id=record.parent_id, generation=record.generation, created_at=record.created_at,
        )
        return updated

    def read_exchange(self, task_id: str, exchange_id: str, *, revision: int | None = None, include_bytes: bool = True) -> dict[str, Any]:
        self._task(task_id)
        record = self.store.require(task_id, EXCHANGE_KIND, exchange_id, revision=revision)
        exchange = HttpExchange.model_validate(record.payload)
        request_raw = self.artifacts.read(exchange.request_raw_ref, expected_task_id=task_id)
        response_raw = self.artifacts.read(exchange.response_raw_ref, expected_task_id=task_id) if exchange.response_raw_ref else None
        result = {
            "exchange": exchange.model_dump(mode="json"), "record_revision": record.revision,
            "structured": {
                "request": _json(parse_http_request(request_raw, default_scheme=urlsplit(exchange.observed_url).scheme or "http")),
                "response": _json(parse_http_response(response_raw, method=exchange.method)) if response_raw and exchange.capture_state == "complete" else None,
            },
            "redirect_chain": self.redirect_chain(task_id, exchange_id),
        }
        if include_bytes:
            result["request_raw"] = request_raw
            result["response_raw"] = response_raw
        else:
            result["request_raw_base64"] = _b64(request_raw)
            result["response_raw_base64"] = _b64(response_raw)
        return result

    def redirect_chain(self, task_id: str, exchange_id: str) -> list[str]:
        chain: list[str] = []
        seen: set[str] = set()
        current: str | None = exchange_id
        while current:
            if current in seen:
                raise SrcStoreError("version_conflict", "redirect_parent contains a cycle")
            seen.add(current)
            chain.append(current)
            record = self.store.require(task_id, EXCHANGE_KIND, current)
            current = HttpExchange.model_validate(record.payload).redirect_parent
        chain.reverse()
        return chain

    def _matches(self, exchange: HttpExchange, filters: dict[str, Any]) -> bool:
        if filters.get("method") and exchange.method.upper() != str(filters["method"]).upper():
            return False
        if filters.get("url") and exchange.observed_url != filters["url"]:
            return False
        parsed = urlsplit(exchange.observed_url)
        if filters.get("host") and (parsed.hostname or "").lower() != str(filters["host"]).lower():
            return False
        if filters.get("path") and (parsed.path or "/") != filters["path"]:
            return False
        if filters.get("status_code") is not None and exchange.status_code != int(filters["status_code"]):
            return False
        if filters.get("source_run_id") is not None and exchange.source_run_id != filters["source_run_id"]:
            return False
        if filters.get("context_id") and exchange.context_id != filters["context_id"]:
            return False
        if filters.get("source_kind") and exchange.source_kind != filters["source_kind"]:
            return False
        if filters.get("capture_state") and exchange.capture_state != filters["capture_state"]:
            return False
        if filters.get("execution_state") and exchange.execution_state != filters["execution_state"]:
            return False
        if filters.get("tag") and filters["tag"] not in exchange.tags:
            return False
        if filters.get("media_type") and str(exchange.metadata.get("media_type") or "") != filters["media_type"]:
            return False
        if filters.get("started_after") and exchange.started_at < datetime.fromisoformat(filters["started_after"]):
            return False
        if filters.get("started_before") and exchange.started_at > datetime.fromisoformat(filters["started_before"]):
            return False
        parameter = filters.get("parameter") or filters.get("parameter_name")
        location = str(filters.get("parameter_location") or "any")
        if parameter:
            found = False
            if location in {"any", "query"}:
                found |= any(name == parameter for name, _ in parse_qsl(parsed.query, keep_blank_values=True))
            if location in {"any", "body"}:
                request_raw = self.artifacts.read(exchange.request_raw_ref, expected_task_id=exchange.task_id)
                body = parse_http_request(request_raw)["body"]
                found |= any(name == parameter for name, _ in parse_qsl(body.decode("utf-8", "ignore"), keep_blank_values=True))
            if location in {"any", "header"}:
                found |= any(name.lower() == str(parameter).lower() for name, _ in exchange.request_headers)
            if not found:
                return False
        if filters.get("body_text"):
            request_raw = self.artifacts.read(exchange.request_raw_ref, expected_task_id=exchange.task_id)
            body = parse_http_request(request_raw)["body"]
            media = media_type(exchange.request_headers)
            if not is_textual(media, body) or filters["body_text"] not in body.decode("utf-8", "replace"):
                return False
        return True

    def query_exchanges(self, task_id: str, filters: dict[str, Any] | None = None, *, cursor: str = "", limit: int = 50) -> dict[str, Any]:
        self._task(task_id)
        filters = dict(filters or {})
        limit = max(1, min(int(limit), 200))
        scope = self.store.digest(filters)
        key = load_cursor_key(self.store.platform.db_path)
        after = assert_cursor_scope(key, cursor, "src_exchange_query", task_id, scope=scope) if cursor else 0
        all_matching = [row for row in self.store.all(task_id, EXCHANGE_KIND) if self._matches(HttpExchange.model_validate(row.payload), filters)]
        matching = [row for row in all_matching if self.store.rowid(row) > after]
        page = matching[:limit + 1]
        has_more = len(page) > limit
        page = page[:limit]
        next_cursor = ""
        if has_more and page:
            next_cursor = encode_cursor(key, "src_exchange_query", task_id, self.store.rowid(page[-1]), scope=scope)
        return {
            "items": [row.model_dump(mode="json") for row in page],
            "total": len(all_matching), "returned": len(page), "next_cursor": next_cursor,
            "filters": filters, "semantic_version": "HttpExchange.query.v1",
            "watermarks": self.store.watermarks(task_id),
            "complete": not any(row.get("state") == "failed" for row in self.store.watermarks(task_id)),
        }

    def _draft_source(self, task_id: str, source_exchange_id: str) -> tuple[Any, HttpExchange, bytes, bool]:
        source = self.store.get(task_id, EXCHANGE_KIND, source_exchange_id)
        if source is None:
            source = self.store.require(task_id, "import_exchange", source_exchange_id)
        payload = dict(source.payload)
        for field in ("request_headers", "response_headers"):
            payload[field] = [
                [str(item.get("name") or ""), str(item.get("value") or "")] if isinstance(item, dict) else item
                for item in payload.get(field) or []
            ]
        exchange = HttpExchange.model_validate(payload)
        raw = self.artifacts.read(exchange.request_raw_ref, expected_task_id=task_id)
        try:
            parse_http_request(raw, default_scheme=urlsplit(exchange.observed_url).scheme or "http")
            return source, exchange, raw, False
        except SrcStoreError:
            pass
        try:
            document = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise SrcStoreError("capture_unavailable", "imported request has no replayable HTTP representation") from exc
        method = str(document.get("method") or exchange.method).upper()
        target = str(document.get("url") or exchange.observed_url)
        parts = urlsplit(target)
        if not parts.hostname:
            raise SrcStoreError("capture_unavailable", "imported request target is invalid")
        path = (parts.path or "/") + (f"?{parts.query}" if parts.query else "")
        header_rows = []
        has_host = False
        for item in document.get("headers") or []:
            if isinstance(item, dict):
                name, value = str(item.get("name") or ""), str(item.get("value") or "")
            else:
                name, value = str(item[0]), str(item[1])
            if not name:
                continue
            has_host = has_host or name.lower() == "host"
            header_rows.append(f"{name}: {value}\r\n")
        if not has_host:
            header_rows.append(f"Host: {parts.netloc}\r\n")
        post = document.get("postData") or {}
        if post.get("encoding") == "base64":
            body = base64.b64decode(str(post.get("text") or ""), validate=True)
        else:
            body = str(post.get("text") or "").encode("utf-8")
        raw_request = f"{method} {path} HTTP/1.1\r\n".encode("ascii") + "".join(header_rows).encode("latin-1") + b"\r\n" + body
        parse_http_request(raw_request, default_scheme=parts.scheme or "http")
        return source, exchange, raw_request, True

    def save_request_draft(
        self, task_id: str, source_exchange_id: str, base_revision: int, *,
        raw_request: bytes | None = None, changes: dict[str, Any] | None = None,
        editor: str, idempotency_key: str,
    ) -> dict[str, Any]:
        self._task(task_id)
        request = {"source_exchange_id": source_exchange_id, "base_revision": base_revision,
                   "raw_sha256": hashlib.sha256(raw_request).hexdigest() if raw_request is not None else None,
                   "changes": changes or {}, "editor": editor}
        reservation_id = new_id("sdraft")
        source, exchange, original, derived = self._draft_source(task_id, source_exchange_id)
        with self.store.platform.transaction():
            owned, prior = self._idempotency_begin(
                task_id, "save_request_draft", idempotency_key, request, reservation_id,
            )
            if not owned:
                return dict(prior or {})
            drafts = self.store.all(task_id, DRAFT_KIND)
            related = [row for row in drafts if row.payload.get("source_exchange_id") == source_exchange_id]
            latest = max(related, key=lambda row: int(row.payload.get("revision") or 0), default=None)
            current_revision = latest.payload["revision"] if latest else source.revision
            if current_revision != base_revision:
                raise SrcStoreError("version_conflict", "request draft base revision is stale", detail={"actual": current_revision, "expected": base_revision})
            if raw_request is not None:
                content = raw_request
            elif latest is not None:
                from .contracts import ArtifactRef
                content = self.artifacts.read(
                    ArtifactRef.model_validate(latest.payload["raw_request_ref"]),
                    expected_task_id=task_id,
                )
            else:
                content = original
            parsed = parse_http_request(content, default_scheme=urlsplit(exchange.observed_url).scheme or "http")
            revision = current_revision + 1 if latest else 1
            ref = self.artifacts.put(task_id, content, media_type="message/http", name=f"{source_exchange_id}-draft-v{revision}.http")
            draft = RequestDraft(
                draft_id=reservation_id, task_id=task_id, source_exchange_id=source_exchange_id,
                revision=revision, raw_request_ref=ref, target_url=parsed["url"],
                method=parsed["method"], changes={**dict(changes or {}), "derived_from_structured_capture": derived}, editor=editor,
            )
            record = self.store.put(task_id, DRAFT_KIND, draft.model_dump(mode="json"), record_id=draft.draft_id, parent_id=source_exchange_id)
            response = {"draft": draft.model_dump(mode="json"), "record_revision": record.revision}
            self._idempotency_finish(task_id, "save_request_draft", idempotency_key, request, response)
        return response

    save_draft = save_request_draft

    def read_draft(self, task_id: str, draft_id: str, *, revision: int | None = None, include_bytes: bool = True) -> dict[str, Any]:
        record = self.store.require(task_id, DRAFT_KIND, draft_id)
        draft = RequestDraft.model_validate(record.payload)
        if revision is not None and draft.revision != revision:
            raise SrcStoreError("version_conflict", "request draft revision is stale")
        raw = self.artifacts.read(draft.raw_request_ref, expected_task_id=task_id)
        return {"draft": draft.model_dump(mode="json"), "record_revision": record.revision,
                "raw_request": raw if include_bytes else None, "raw_request_base64": None if include_bytes else _b64(raw),
                "structured": _json(parse_http_request(raw, default_scheme=urlsplit(draft.target_url).scheme or "http"))}

    async def execute_request_draft(
        self, task_id: str, draft_id: str, draft_revision: int, *,
        identity_ref: str, credential_ref: str = "", expected_side_effect: str = "",
        authorization_version: int, idempotency_key: str, budget: dict[str, Any] | None = None,
        context_id: str = "default", identity_revision: int = 0, browser_generation: int = 0,
        source_run_id: str | None = None, actor_ref: str = "human",
        resource_facts: dict[str, ResourceFact] | None = None,
        wait_for_approval: bool = False,
    ) -> dict[str, Any]:
        self._task(task_id)
        if resource_facts:
            raise SrcStoreError("task_binding_invalid", "request execution facts must come from the saved draft")
        budget = dict(budget or {})
        idem_request = {
            "draft_id": draft_id, "draft_revision": draft_revision, "identity_ref": identity_ref,
            "credential_ref": credential_ref, "expected_side_effect": expected_side_effect,
            "authorization_version": authorization_version, "budget": budget,
            "context_id": context_id, "identity_revision": identity_revision,
            "browser_generation": browser_generation, "source_run_id": source_run_id,
            "resource_facts": {k: v.model_dump(mode="json") if hasattr(v, "model_dump") else v for k, v in (resource_facts or {}).items()},
            "idempotency_key": idempotency_key,
        }
        invocation_id = new_id("http-inv")
        draft_record = self.store.require(task_id, DRAFT_KIND, draft_id)
        draft = RequestDraft.model_validate(draft_record.payload)
        if draft.revision != draft_revision:
            raise SrcStoreError("version_conflict", "request draft revision is stale")
        raw = self.artifacts.read(draft.raw_request_ref, expected_task_id=task_id)
        parsed = parse_http_request(raw, default_scheme=urlsplit(draft.target_url).scheme or "http")
        if parsed["url"] != draft.target_url or parsed["method"] != draft.method:
            raise SrcStoreError("version_conflict", "draft raw request does not match its target contract")
        policy = self.src.execution.current_policy(task_id)
        if policy.version != authorization_version:
            raise SrcStoreError("authorization_stale", "authorization version is stale")
        owned, prior = self._idempotency_begin(
            task_id, "execute_request_draft", idempotency_key, idem_request, invocation_id,
        )
        if not owned:
            if (prior or {}).get("state") != "pending" and (prior or {}).get("execution_state") not in {"created", "waiting_approval"}:
                return dict(prior or {})
            invocation_id = str(prior["execution_id"])
        replay_of = draft.source_exchange_id
        try:
            result = await self._execute_hop(
                task_id=task_id, invocation_id=invocation_id, raw=raw, draft=draft,
                identity_ref=identity_ref, credential_ref=credential_ref,
                expected_side_effect=expected_side_effect, authorization_version=authorization_version,
                budget=budget, context_id=context_id, identity_revision=identity_revision,
                browser_generation=browser_generation, source_run_id=source_run_id,
                actor_ref=actor_ref, replay_of=replay_of,
                redirect_parent=None, wait_for_approval=wait_for_approval,
            )
        except Exception as exc:
            failure = {
                "execution_id": invocation_id, "draft_id": draft_id,
                "draft_revision": draft_revision, "execution_state": "error",
                "error": {"code": getattr(exc, "code", type(exc).__name__), "message": str(exc)},
            }
            self._idempotency_finish(task_id, "execute_request_draft", idempotency_key, idem_request, failure)
            raise
        chain = list(result.pop("redirect_results") or [])
        response = {**result, "execution_id": invocation_id, "draft_id": draft_id,
                    "draft_revision": draft_revision, "redirect_results": chain}
        self._idempotency_finish(task_id, "execute_request_draft", idempotency_key, idem_request, response)
        return response

    execute_draft = execute_request_draft

    async def _execute_hop(
        self, *, task_id: str, invocation_id: str, raw: bytes, draft: RequestDraft,
        identity_ref: str, credential_ref: str, expected_side_effect: str,
        authorization_version: int, budget: dict[str, Any], context_id: str,
        identity_revision: int, browser_generation: int, source_run_id: str | None,
        actor_ref: str, replay_of: str | None,
        redirect_parent: str | None, wait_for_approval: bool,
    ) -> dict[str, Any]:
        parsed = parse_http_request(raw, default_scheme=urlsplit(draft.target_url).scheme or "http")
        canonical = {
            "url": parsed["url"], "scheme": parsed["scheme"], "host": parsed["host"],
            "hostname": parsed["host"], "port": parsed["port"], "path": parsed["path"],
            "raw_path": parsed["path"] + (f"?{parsed['query']}" if parsed["query"] else ""),
            "method": parsed["method"], "headers": parsed["headers"],
            "body_sha256": hashlib.sha256(parsed["body"]).hexdigest(),
        }
        trusted_facts = {key: ResourceFact(value=value, origin="request") for key, value in canonical.items()}
        request = ExecutionRequest(
            invocation_id=invocation_id, task_id=task_id,
            thread_id=self.src.binding(task_id).thread_id, run_id=source_run_id,
            tool_name="src.http.request", action="request",
            arguments={
                "url": parsed["url"], "method": parsed["method"], "draft_id": draft.draft_id,
                "draft_revision": draft.revision, "raw_request_sha256": hashlib.sha256(raw).hexdigest(),
                "raw_request_ref": self.artifacts.put(
                    task_id, raw, media_type="message/http", name=f"{invocation_id}-request.http",
                ).model_dump(mode="json"),
                "credential_ref": credential_ref, "expected_side_effect": expected_side_effect,
                "budget": budget, "idempotent": parsed["method"].upper() in IDEMPOTENT_METHODS,
            },
            arguments_hash=hashlib.sha256(raw).hexdigest(), resource_kind="http",
            resource_facts=trusted_facts, identity_ref=identity_ref,
            identity_revision=identity_revision, environment_generation=browser_generation,
            policy_version=authorization_version,
        )
        outcome: dict[str, Any] = {"exchange_id": None}

        async def dispatch(_request: ExecutionRequest) -> Any:
            try:
                exchange = await self.http.send(
                    request=_request, raw_request=raw, context_id=context_id,
                    identity_revision=identity_revision, browser_generation=browser_generation,
                    source_run_id=source_run_id, actor_ref=actor_ref, replay_of=replay_of,
                    redirect_parent=redirect_parent,
                    authorization_ref=f"policy-v{authorization_version}",
                    decision_ref=_request.invocation_id,
                    capture_limit=budget.get("capture_limit"),
                    timeout=float(budget.get("timeout_seconds") or 30.0),
                )
                outcome["exchange_id"] = exchange.exchange_id
                return exchange.model_dump(mode="json")
            except Exception as exc:
                outcome["exchange_id"] = getattr(exc, "exchange_id", None)
                outcome["error"] = {"code": getattr(exc, "code", type(exc).__name__), "message": str(exc)}
                raise

        receipt = await self.src.execution.execute(request, dispatch, wait_for_approval=wait_for_approval)
        result = {
            "receipt": receipt.model_dump(mode="json"), "exchange_id": outcome["exchange_id"],
            "execution_state": receipt.dispatch_state, **({"error": outcome["error"]} if outcome.get("error") else {}),
        }
        execution_record = self.store.put(task_id, EXECUTION_KIND, {
            "invocation_id": invocation_id, "draft_id": draft.draft_id,
            "draft_revision": draft.revision, "exchange_id": outcome["exchange_id"],
            "replay_of": replay_of, "receipt": result["receipt"],
        }, record_id=invocation_id, parent_id=draft.draft_id)
        result["execution_record_revision"] = execution_record.revision
        result["redirect_results"] = []
        redirect_budget = int(budget.get("max_redirects") or 0)
        if receipt.dispatch_state == "completed" and outcome["exchange_id"] and redirect_budget > 0:
            exchange = HttpExchange.model_validate(self.store.require(task_id, EXCHANGE_KIND, outcome["exchange_id"]).payload)
            locations = header_values(exchange.response_headers, "location")
            if exchange.status_code in {301, 302, 303, 307, 308} and locations:
                from urllib.parse import urljoin
                target = urljoin(exchange.observed_url, locations[0])
                redirected_raw = replace_request_target(raw, target)
                cross_origin = _origin(exchange.observed_url) != _origin(target)
                if cross_origin:
                    redirected_raw = _without_origin_credentials(redirected_raw)
                redirected_draft = draft.model_copy(update={"target_url": target})
                next_result = await self._execute_hop(
                    task_id=task_id, invocation_id=new_id("http-inv"), raw=redirected_raw,
                    draft=redirected_draft, identity_ref=identity_ref,
                    credential_ref="" if cross_origin else credential_ref,
                    expected_side_effect=expected_side_effect, authorization_version=authorization_version,
                    budget={**budget, "max_redirects": redirect_budget - 1}, context_id=context_id,
                    identity_revision=identity_revision, browser_generation=browser_generation,
                    source_run_id=source_run_id, actor_ref=actor_ref,
                    replay_of=replay_of, redirect_parent=exchange.exchange_id,
                    wait_for_approval=wait_for_approval,
                )
                result["redirect_results"] = [next_result, *next_result.pop("redirect_results", [])]
                result["exchange_id"] = next_result["exchange_id"]
                result["execution_state"] = next_result["execution_state"]
                result["receipt"] = next_result["receipt"]
        return result

    def compare_exchanges(
        self, task_id: str, baseline_id: str, experiment_id: str, *,
        ignore_rules: dict[str, Any] | None = None,
        baseline_context: dict[str, Any] | None = None,
        experiment_context: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        baseline = self.read_exchange(task_id, baseline_id)
        experiment = self.read_exchange(task_id, experiment_id)
        rules = dict(ignore_rules or {})
        request_diff = self._side_diff(baseline["request_raw"], experiment["request_raw"], side="request")
        response_diff = self._side_diff(baseline["response_raw"], experiment["response_raw"], side="response")
        normalized_request = self._normalized_side(baseline["request_raw"], experiment["request_raw"], rules, request=True)
        normalized_response = self._normalized_side(baseline["response_raw"], experiment["response_raw"], rules, request=False)
        b_exchange = HttpExchange.model_validate(baseline["exchange"])
        e_exchange = HttpExchange.model_validate(experiment["exchange"])
        dimensions: list[str] = []
        unknown_dimensions: list[str] = []
        b_ctx = dict(baseline_context or {})
        e_ctx = dict(experiment_context or {})
        if b_ctx.get("identity", b_exchange.context_id) != e_ctx.get("identity", e_exchange.context_id):
            dimensions.append("identity")
        for name in ("tenant", "object_version", "target_version", "preconditions"):
            b_val = b_ctx.get(name)
            e_val = e_ctx.get(name)
            if b_val is None and e_val is None:
                unknown_dimensions.append(name)
            elif b_val != e_val:
                dimensions.append(name)
        if b_exchange.method != e_exchange.method:
            dimensions.append("method")
        b_parts = urlsplit(b_exchange.observed_url)
        e_parts = urlsplit(e_exchange.observed_url)
        if (b_parts.hostname or "").lower() != (e_parts.hostname or "").lower() or b_parts.port != e_parts.port:
            dimensions.append("host")
        if b_parts.path != e_parts.path:
            dimensions.append("path")
        if b_parts.query != e_parts.query:
            dimensions.append("query")
        if b_exchange.metadata.get("request_body_sha256") != e_exchange.metadata.get("request_body_sha256"):
            dimensions.append("body")
        result = {
            "baseline_id": baseline_id, "experiment_id": experiment_id,
            "raw": {"request": request_diff, "response": response_diff},
            "normalized": {"request": normalized_request, "response": normalized_response},
            "ignore_rules": rules, "changed_dimensions": dimensions,
            "unknown_dimensions": unknown_dimensions,
            "single_variable": len(dimensions) <= 1,
            "warnings": ["身份、租户、对象、目标版本或前置条件同时变化；这不是单变量对照"] if len(dimensions) > 1 else [],
            "context": {"baseline": _json(b_ctx), "experiment": _json(e_ctx)},
            "conclusion": "diff_only",
        }
        record = self.store.put(task_id, COMPARISON_KIND, _json(result), parent_id=f"{baseline_id}:{experiment_id}")
        result["comparison_id"] = record.record_id
        result["record_revision"] = record.revision
        return result

    compare = compare_exchanges

    def _side_diff(self, left: bytes | None, right: bytes | None, *, side: str) -> dict[str, Any]:
        text = self._unified_diff(left, right, f"baseline/{side}", f"experiment/{side}")
        return {
            "baseline_sha256": hashlib.sha256(left).hexdigest() if left is not None else None,
            "experiment_sha256": hashlib.sha256(right).hexdigest() if right is not None else None,
            "baseline_size": len(left) if left is not None else None,
            "experiment_size": len(right) if right is not None else None,
            "unified_diff": text, "identical": left == right,
        }

    @staticmethod
    def _unified_diff(left: bytes | None, right: bytes | None, left_name: str, right_name: str) -> str:
        if left is None or right is None:
            return "baseline unavailable" if left is None else "experiment unavailable"
        a = left.decode("utf-8", "surrogateescape").splitlines(keepends=True)
        b = right.decode("utf-8", "surrogateescape").splitlines(keepends=True)
        return "".join(difflib.unified_diff(a, b, fromfile=left_name, tofile=right_name))

    def _normalized_side(self, left: bytes | None, right: bytes | None, rules: dict[str, Any], *, request: bool) -> dict[str, Any]:
        def normalize(raw: bytes | None) -> Any:
            if raw is None:
                return None
            try:
                parsed = parse_http_request(raw) if request else parse_http_response(raw)
            except SrcStoreError:
                return raw
            ignored_headers = {str(name).lower() for name in rules.get("headers", [])}
            parsed["headers"] = [[name, value] for name, value in parsed["headers"] if name.lower() not in ignored_headers]
            body = parsed["body"]
            if rules.get("json_pointers"):
                try:
                    structured = json.loads(body.decode("utf-8"))
                    for pointer in rules["json_pointers"]:
                        self._remove_json_pointer(structured, str(pointer))
                    parsed["body"] = structured
                except (ValueError, UnicodeDecodeError, SrcStoreError):
                    parsed["body"] = body
            return parsed
        normalized_left = normalize(left)
        normalized_right = normalize(right)
        left_bytes = json.dumps(_json(normalized_left), ensure_ascii=False, sort_keys=True).encode() if normalized_left is not None else None
        right_bytes = json.dumps(_json(normalized_right), ensure_ascii=False, sort_keys=True).encode() if normalized_right is not None else None
        return self._side_diff(left_bytes, right_bytes, side="normalized")

    @staticmethod
    def _remove_json_pointer(value: Any, pointer: str) -> None:
        parts = [part.replace("~1", "/").replace("~0", "~") for part in pointer.split("/") if part]
        if not parts:
            return
        parent = value
        for part in parts[:-1]:
            if isinstance(parent, dict):
                parent = parent.get(part)
            elif isinstance(parent, list) and part.isdigit() and int(part) < len(parent):
                parent = parent[int(part)]
            else:
                return
        last = parts[-1]
        if isinstance(parent, dict):
            parent.pop(last, None)
        elif isinstance(parent, list) and last.isdigit() and int(last) < len(parent):
            parent.pop(int(last))

    def export_exchanges(
        self, task_id: str, *, exchange_ids: list[str] | None = None,
        filters: dict[str, Any] | None = None, format: str = "jsonl",
        allow_partial: bool = False,
    ) -> dict[str, Any]:
        self._task(task_id)
        if exchange_ids is None:
            selected = []
            cursor = ""
            while True:
                page = self.query_exchanges(task_id, filters, cursor=cursor, limit=200)
                selected.extend(page["items"])
                cursor = page["next_cursor"]
                if not cursor:
                    break
            ids = [row["record_id"] for row in selected]
        else:
            ids = list(dict.fromkeys(exchange_ids))
        omissions: list[dict[str, str]] = []
        exchanges: list[HttpExchange] = []
        raws: dict[str, dict[str, bytes | None]] = {}
        for exchange_id in ids:
            try:
                data = self.read_exchange(task_id, exchange_id)
                exchanges.append(HttpExchange.model_validate(data["exchange"]))
                raws[exchange_id] = {"request": data["request_raw"], "response": data["response_raw"]}
            except SrcStoreError as exc:
                omissions.append({"exchange_id": exchange_id, "code": exc.code, "message": exc.message})
        partial = bool(omissions)
        if partial and not allow_partial:
            raise SrcStoreError("artifact_missing", "export is incomplete", detail={"omissions": omissions})
        manifest = {
            "schema": "muteki.src.exchange-export.v1", "task_id": task_id,
            "format": format, "partial": partial, "expected": len(ids),
            "actual": len(exchanges), "omissions": omissions,
            "filters": filters or {}, "entries": [],
        }
        if format == "jsonl":
            rows = []
            for exchange in exchanges:
                raw = raws[exchange.exchange_id]
                rows.append(json.dumps({
                    "exchange": exchange.model_dump(mode="json"),
                    "request_raw_base64": _b64(raw["request"]),
                    "response_raw_base64": _b64(raw["response"]),
                }, ensure_ascii=False, default=str))
            content = ("\n".join(rows) + ("\n" if rows else "")).encode("utf-8")
            media = "application/x-ndjson"
        elif format == "har":
            content = json.dumps(self._har(exchanges, raws), ensure_ascii=False, indent=2).encode("utf-8")
            media = "application/har+json"
        elif format == "raw" and len(exchanges) == 1:
            raw = raws[exchanges[0].exchange_id]["request"]
            return {"content": raw, "media_type": "message/http", "filename": f"{exchanges[0].exchange_id}-request.http",
                    "manifest": manifest, "partial": partial}
        elif format == "raw" or format == "zip":
            output = io.BytesIO()
            with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as archive:
                for index, exchange in enumerate(exchanges, 1):
                    raw = raws[exchange.exchange_id]
                    for direction in ("request", "response"):
                        data = raw[direction]
                        if data is None:
                            continue
                        path = f"exchanges/{index:04d}-{exchange.exchange_id}/{direction}.http"
                        archive.writestr(path, data)
                        manifest["entries"].append({"path": path, "exchange_id": exchange.exchange_id,
                                                    "sha256": hashlib.sha256(data).hexdigest(), "size": len(data),
                                                    "media_type": "message/http", "original": exchange.protocol in {"http/1.0", "http/1.1"}})
                archive.writestr("manifest.json", json.dumps(manifest, ensure_ascii=False, indent=2))
            content = output.getvalue()
            media = "application/zip"
        else:
            raise SrcStoreError("capability_unavailable", f"unsupported exchange export format {format}")
        digest = hashlib.sha256(content).hexdigest()
        manifest["content_sha256"] = digest
        manifest["content_size"] = len(content)
        return {"content": content, "media_type": media, "filename": f"src-exchanges.{format}",
                "manifest": manifest, "partial": partial}

    export = export_exchanges

    def _har(self, exchanges: list[HttpExchange], raws: dict[str, dict[str, bytes | None]]) -> dict[str, Any]:
        entries = []
        for exchange in exchanges:
            raw = raws[exchange.exchange_id]
            request = parse_http_request(raw["request"], default_scheme=urlsplit(exchange.observed_url).scheme or "http")
            response = parse_http_response(raw["response"], method=exchange.method) if raw["response"] and exchange.capture_state == "complete" else None
            entries.append({
                "startedDateTime": exchange.started_at.isoformat(),
                "time": (exchange.finished_at - exchange.started_at).total_seconds() * 1000 if exchange.finished_at else 0,
                "request": {
                    "method": exchange.method, "url": exchange.observed_url,
                    "httpVersion": request["version"], "cookies": [],
                    "headers": [{"name": name, "value": value} for name, value in request["headers"]],
                    "queryString": [{"name": name, "value": value} for name, value in request["query_parameters"]],
                    "postData": {"mimeType": media_type(request["headers"]), "text": _b64(request["body"]), "encoding": "base64"},
                    "headersSize": -1, "bodySize": len(request["body"]),
                },
                "response": {
                    "status": exchange.status_code or 0, "statusText": response["reason"] if response else "",
                    "httpVersion": response["version"] if response else "",
                    "cookies": [], "headers": [{"name": name, "value": value} for name, value in exchange.response_headers],
                    "content": {"size": len(response["body"]) if response else 0,
                                "mimeType": media_type(exchange.response_headers),
                                "text": _b64(response["body"]) if response else None, "encoding": "base64"},
                    "redirectURL": header_values(exchange.response_headers, "location")[0] if header_values(exchange.response_headers, "location") else "",
                    "headersSize": -1, "bodySize": len(response["body"]) if response else -1,
                },
                "cache": {}, "timings": {"send": 0, "wait": 0, "receive": 0},
                "_muteki": {
                    "exchange_id": exchange.exchange_id, "capture_state": exchange.capture_state,
                    "request_raw_ref": exchange.request_raw_ref.model_dump(mode="json"),
                    "response_raw_ref": exchange.response_raw_ref.model_dump(mode="json") if exchange.response_raw_ref else None,
                    "raw_representation": exchange.metadata.get("raw_representation"),
                    "note": "HAR is an exchange representation, not proof of original wire bytes",
                },
            })
        return {"log": {"version": "1.2", "creator": {"name": "Muteki", "version": "HttpExchange.v1"}, "entries": entries}}


SrcExchangeService = ExchangeService

__all__ = [
    "ExchangeService", "SrcExchangeService", "parse_http_request", "parse_http_response",
    "header_values", "media_type", "decoded_response_body", "replace_request_target",
    "is_textual", "EXCHANGE_KIND", "DRAFT_KIND", "COMPARISON_KIND", "EXECUTION_KIND",
]
