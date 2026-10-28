from __future__ import annotations

import csv
import hashlib
import io
import json
from typing import Any, Iterable, Optional
from urllib.parse import parse_qsl, urlsplit

from muteki.platform.contracts.base import new_id, utcnow

from .assets import SrcAssetService
from .store import SrcStoreError

PARSER_VERSION = "src-ingestion.v1"
TEXTUAL_HTTP = {"raw", "http", "burp", "caido"}


def _json_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), default=str).encode("utf-8")


def _mapping(mapping: dict[str, Any], data: dict[str, Any], path: str = "") -> Any:
    current: Any = data
    for part in path.split(".") if path else []:
        if not isinstance(current, dict) or part not in current:
            return None
        current = current[part]
    return current


def _first(data: dict[str, Any], *names: str) -> Any:
    for name in names:
        value = _mapping({}, data, name)
        if value not in (None, ""):
            return value
    return None


def _media_text(media_type: str) -> bool:
    return media_type.startswith("text/") or any(value in media_type for value in ("json", "xml", "http", "har"))


class SrcIngestionService:
    def __init__(self, src: Any, assets: Optional[SrcAssetService] = None) -> None:
        self.src = src
        self.store = src.store
        self.artifacts = src.artifacts
        self.assets = assets or SrcAssetService(src)

    def ingest(
        self, task_id: str, source_kind: str, content: bytes, *,
        media_type: str = "application/octet-stream", source_id: str = "",
        source_metadata: Optional[dict[str, Any]] = None,
        field_mapping: Optional[dict[str, Any]] = None,
        idempotency_key: str = "", observed_at: Any = None,
        cost: Optional[dict[str, Any]] = None,
        encoding: str = "",
    ) -> dict[str, Any]:
        self.src.binding(task_id)
        if not isinstance(content, bytes):
            raise SrcStoreError("import_invalid", "import content must be bytes")
        source_kind = source_kind.lower()
        source_metadata = dict(source_metadata or {})
        field_mapping = dict(field_mapping or {})
        idempotency_key = idempotency_key or new_id("simportkey")
        digest = hashlib.sha256(content).hexdigest()
        request = {
            "source_kind": source_kind, "sha256": digest, "media_type": media_type,
            "source_id": source_id, "source_metadata": source_metadata,
            "field_mapping": field_mapping, "observed_at": observed_at,
        }
        prior = self.store.idempotent(task_id, "ingest", idempotency_key, request)
        if prior is not None:
            return prior
        batch_id = new_id("sbatch")
        original = self.artifacts.put(
            task_id, content, media_type=media_type,
            name=source_metadata.get("filename") or f"{batch_id}-original",
        )
        batch = {
            "batch_id": batch_id, "task_id": task_id, "source_kind": source_kind,
            "source_id": source_id or batch_id, "source_metadata": source_metadata,
            "original_artifact": original.model_dump(mode="json"),
            "field_mapping": field_mapping, "mapping_version": PARSER_VERSION,
            "idempotency_key": idempotency_key, "created_at": utcnow().isoformat(),
            "status": "processing", "cost": cost or {"state": "not_applicable"},
            "scope": {"query": source_metadata.get("query"), "page": source_metadata.get("page"),
                      "cursor": source_metadata.get("cursor"), "upstream_limit": source_metadata.get("upstream_limit"),
                      "has_next_page": source_metadata.get("has_next_page"), "completeness": source_metadata.get("completeness") or "unknown"},
        }
        self.store.put(task_id, "import_batch", batch, record_id=batch_id)
        try:
            parsed = self._parse(content, source_kind, media_type, encoding=encoding)
        except SrcStoreError as exc:
            batch.update({"status": "failed", "error": {"code": exc.code, "message": exc.message, "detail": exc.detail}, "items": []})
            self.store.put(task_id, "import_batch", batch, record_id=batch_id)
            self.store.remember(task_id, "ingest", idempotency_key, request, batch)
            return batch
        items = self._commit_items(task_id, batch, parsed, observed_at=observed_at)
        batch["items"] = items
        batch["counts"] = self._counts(items)
        batch["status"] = self._status(items)
        batch["completed_at"] = utcnow().isoformat()
        self.store.put(task_id, "import_batch", batch, record_id=batch_id)
        self.store.remember(task_id, "ingest", idempotency_key, request, batch)
        return batch

    def _parse(self, content: bytes, source_kind: str, media_type: str, *, encoding: str = "") -> list[dict[str, Any]]:
        if not content:
            raise SrcStoreError("import_empty", "cannot import an empty source")
        codec = (encoding or "utf-8-sig").strip().lower()
        if source_kind in TEXTUAL_HTTP and not source_kind.endswith("har"):
            if b"\r\n\r\n" in content or b"\n\n" in content:
                return [{"index": 0, "format": "raw_http", "raw": content}]
        try:
            text = content.decode(codec) if _media_text(media_type) or source_kind in TEXTUAL_HTTP or codec != "utf-8-sig" else ""
        except (LookupError, UnicodeDecodeError) as exc:
            raise SrcStoreError("import_encoding_invalid", f"cannot decode import bytes as {codec}: {exc}") from exc
        if not text and media_type == "text/csv":
            try:
                text = content.decode(codec)
            except (LookupError, UnicodeDecodeError) as exc:
                raise SrcStoreError("import_encoding_invalid", f"cannot decode import bytes as {codec}: {exc}") from exc
        if not text:
            raise SrcStoreError("import_binary_unsupported", "binary input requires a versioned adapter")
        if media_type == "text/csv" or source_kind == "csv":
            try:
                reader = csv.DictReader(io.StringIO(text, newline=""))
                if not reader.fieldnames:
                    raise SrcStoreError("import_mapping_invalid", "CSV header is required")
                return [{"index": index, "format": "object", "data": dict(row), "unknown_fields": []}
                        for index, row in enumerate(reader)]
            except csv.Error as exc:
                raise SrcStoreError("import_csv_invalid", str(exc)) from exc
        if source_kind in {"jsonl", "bbot", "reconftw", "bbrf"} or media_type in {"application/jsonl", "application/x-ndjson"}:
            rows = []
            for index, line in enumerate(text.splitlines()):
                if not line.strip():
                    continue
                try:
                    value = json.loads(line)
                    if not isinstance(value, dict):
                        raise ValueError("entry must be an object")
                    rows.append({"index": index, "format": "object", "data": value})
                except ValueError as exc:
                    rows.append({"index": index, "error": {"code": "import_item_invalid", "message": str(exc), "recovery": "fix this JSONL row and retry failed items"}})
            return rows
        try:
            document = json.loads(text)
        except ValueError as exc:
            if source_kind in TEXTUAL_HTTP:
                return [{"index": 0, "format": "raw_http", "raw": content}]
            raise SrcStoreError("import_json_invalid", str(exc)) from exc
        if isinstance(document, dict) and isinstance(document.get("log"), dict) and isinstance(document["log"].get("entries"), list):
            return [{"index": index, "format": "har", "data": entry} for index, entry in enumerate(document["log"]["entries"])]
        if isinstance(document, dict) and isinstance(document.get("items"), list):
            document = document["items"]
        elif isinstance(document, dict):
            document = [document]
        if not isinstance(document, list):
            raise SrcStoreError("import_shape_invalid", "JSON import must be an object or array")
        rows = []
        for index, value in enumerate(document):
            if isinstance(value, dict):
                rows.append({"index": index, "format": "object", "data": value})
            else:
                rows.append({"index": index, "error": {"code": "import_item_invalid", "message": "entry must be an object", "recovery": "replace the entry with an object"}})
        return rows

    def _commit_items(self, task_id: str, batch: dict[str, Any], parsed: list[dict[str, Any]], *, observed_at: Any) -> list[dict[str, Any]]:
        results = []
        for item in parsed:
            index = item["index"]
            if item.get("error"):
                results.append({"index": index, "ok": False, **item["error"]})
                continue
            try:
                with self.store.platform.transaction():
                    if item["format"] == "raw_http":
                        result = self._raw_http(task_id, batch, item["raw"], index)
                    elif item["format"] == "har":
                        result = self._har(task_id, batch, item["data"], index)
                    else:
                        result = self._object(task_id, batch, item["data"], index, observed_at)
                results.append(result)
            except SrcStoreError as exc:
                results.append({"index": index, "ok": False, "code": exc.code,
                                "message": exc.message, "detail": exc.detail,
                                "field": "asset_type" if exc.code == "asset_type_invalid" else "identity" if exc.code == "asset_identity_invalid" else None,
                                "recovery": "correct this item and retry only failed items"})
            except (ValueError, TypeError, KeyError) as exc:
                results.append({"index": index, "ok": False, "code": "import_item_invalid",
                                "message": f"{type(exc).__name__}: {exc}",
                                "recovery": "correct this item and retry only failed items"})
        return results

    def _source(self, batch: dict[str, Any], index: int, data: Any) -> tuple[str, str]:
        source_id = str(_first(data, "uuid", "event_id", "id") or f"{batch['batch_id']}:{index}") if isinstance(data, dict) else f"{batch['batch_id']}:{index}"
        return f"{batch['source_kind']}:{batch['source_id']}", source_id

    def _object(self, task_id: str, batch: dict[str, Any], data: dict[str, Any], index: int, observed_at: Any) -> dict[str, Any]:
        mapping = batch.get("field_mapping") or {}
        source_kind, source_event_id = self._source(batch, index, data)
        item_artifact = self.artifacts.put(task_id, _json_bytes(data), media_type="application/json", name=f"{batch['batch_id']}-{index}.json")
        asset_type = str(_first(data, mapping.get("asset_type", "asset_type"), "type") or "").lower()
        type_map = {
            "dns_name": "domain", "domain": "domain", "subdomain": "domain",
            "ip_address": "ip", "ip": "ip", "cidr": "cidr",
            "open_tcp_port": "port", "port": "port", "url": "http_endpoint",
            "http_endpoint": "http_endpoint", "endpoint": "http_endpoint",
            "parameter": "parameter", "param": "parameter", "app": "app",
            "miniprogram": "miniprogram", "mini_program": "miniprogram",
            "cloud": "cloud", "repository": "repository", "repo": "repository",
            "web_service": "web_service", "service": "web_service",
            "non_web_service": "non_web_service", "dns": "dns",
        }
        asset_type = type_map.get(asset_type, asset_type)
        if asset_type in {"finding", "vulnerability"}:
            return {"index": index, "ok": True, "action": "skipped", "reason": "external finding is evidence, not a local Finding", "artifact": item_artifact.model_dump(mode="json")}
        identity = dict(data.get("identity") or mapping.get("identity") or {})
        if not identity:
            if asset_type in {"domain", "root_domain", "dns"}:
                identity = {"name": _first(data, mapping.get("name", "name"), "domain", "host", "data"),
                            "record_type": _first(data, "record_type", "dns_type"), "value": _first(data, "value", "record_value")}
            elif asset_type in {"ip", "cidr"}:
                identity = {"address": _first(data, mapping.get("address", "address"), "ip", "host", "data")}
            elif asset_type in {"port", "web_service", "non_web_service"}:
                identity = {"host": _first(data, "host", "ip", "domain"), "port": _first(data, "port"),
                            "protocol": _first(data, "protocol", "scheme"), "transport": _first(data, "transport")}
            elif asset_type in {"http_endpoint", "parameter"}:
                identity = {"url": _first(data, "url", "uri", "data"), "method": _first(data, "method") or "GET",
                            "location": _first(data, "location", "param_location", "in"),
                            "name": _first(data, "param_name", "parameter", "key"),
                            "identity_ref": _first(data, "identity_ref", "account"),
                            "tenant": _first(data, "tenant"), "action": _first(data, "action")}
            elif asset_type in {"app", "miniprogram"}:
                identity = {"platform": _first(data, "platform"), "stable_id": _first(data, "stable_id", "package_id", "bundle_id", "appid"),
                            "name": _first(data, "name", "product_name")}
            elif asset_type == "cloud":
                identity = {key: _first(data, key) for key in ("provider", "tenant", "region", "resource_id")}
            elif asset_type == "repository":
                identity = {"platform": _first(data, "platform", "host"), "stable_id": _first(data, "stable_id", "repository", "canonical", "full_name")}
        if isinstance(identity.get("url"), dict):
            identity.update(identity.pop("url"))
        attributes = dict(data.get("attributes") or mapping.get("attributes") or {})
        for key in ("title", "status_code", "content_length", "technology", "favicon", "ttl", "version", "description", "subject", "issuer", "record_type"):
            value = _first(data, key)
            if value is not None:
                attributes[key] = value
        result = self.assets.observe(
            task_id, asset_type, identity, source_kind=batch["source_kind"],
            source_id=batch["source_id"], attributes=attributes, raw_values=data,
            observed_at=_first(data, "observed_at", "timestamp", "time", "seen_at") or observed_at,
            artifact_refs=[item_artifact], source_run_id=_first(data, "run_id"),
            source_event_seq=_first(data, "seq", "event_seq"), parser_version=PARSER_VERSION,
            completeness="partial" if batch["scope"].get("has_next_page") else "complete",
            success=data.get("success") if isinstance(data.get("success"), bool) else None,
            source_event_id=source_event_id,
        )
        parent = _first(data, "parent_uuid", "parent_id", "parent")
        known_fields = set(identity) | set(attributes) | {
            "identity", "attributes", "type", "asset_type", "observed_at", "timestamp",
            "time", "seen_at", "success", "run_id", "seq", "event_seq", "uuid",
            "event_id", "id", "parent_uuid", "parent_id", "parent",
        }
        unknown_fields = sorted(set(data) - known_fields)
        if parent:
            result["observation"]["parent_source_event_id"] = parent
        return {"index": index, "ok": True, "action": result["action"],
                "asset_id": result["asset"]["asset_id"], "observation_id": result["observation"]["observation_id"],
                "source_event_id": source_event_id, "parent_source_event_id": parent,
                "unknown_fields": unknown_fields}

    def _raw_http(self, task_id: str, batch: dict[str, Any], raw: bytes, index: int) -> dict[str, Any]:
        separator = b"\r\n\r\n" if b"\r\n\r\n" in raw else b"\n\n"
        head, body = raw.split(separator, 1)
        lines = head.replace(b"\r\n", b"\n").split(b"\n")
        try:
            method, target, protocol = lines[0].decode("latin-1").split(" ", 2)
        except (ValueError, UnicodeDecodeError) as exc:
            raise SrcStoreError("import_http_invalid", "invalid raw HTTP request line", detail={"index": index}) from exc
        headers = []
        for line in lines[1:]:
            if b":" not in line:
                raise SrcStoreError("import_http_invalid", "invalid HTTP header", detail={"index": index})
            name, value = line.split(b":", 1)
            headers.append([name.decode("latin-1"), value.lstrip().decode("latin-1")])
        if "://" in target:
            url = target
        else:
            host = next((value for name, value in headers if name.lower() == "host"), "")
            scheme = str(batch["source_metadata"].get("scheme") or "")
            if not scheme or not host:
                raise SrcStoreError("import_target_unknown", "origin-form raw HTTP requires source_metadata.scheme and Host", detail={"index": index})
            url = f"{scheme}://{host}{target}"
        parts = urlsplit(url)
        if not parts.hostname:
            raise SrcStoreError("import_http_invalid", "absolute target URL is required")
        raw_ref = self.artifacts.put(task_id, raw, media_type="application/http", name=f"{batch['batch_id']}-{index}.http")
        data = {
            "method": method.upper(), "url": url, "protocol": protocol,
            "headers": headers, "body_sha256": hashlib.sha256(body).hexdigest(),
            "raw_http": True, "capture_source": "import", "live_replay": False,
        }
        result = self.assets.observe(
            task_id, "http_endpoint", {"url": url, "method": method},
            source_kind="import", source_id=batch["source_id"], attributes=data,
            raw_values={"headers": headers, "body_sha256": data["body_sha256"]},
            observed_at=batch["source_metadata"].get("captured_at"),
            artifact_refs=[raw_ref], parser_version=PARSER_VERSION,
            success=None, source_event_id=f"{batch['batch_id']}:{index}",
        )
        exchange = {
            "exchange_id": new_id("simportexchange"), "task_id": task_id,
            "source_kind": "import", "observed_url": url, "declared_target": url,
            "method": method.upper(), "protocol": protocol, "request_headers": headers,
            "request_raw_ref": raw_ref.model_dump(mode="json"), "response_raw_ref": None,
            "capture_state": "partial", "capture_limitations": ["import contains request only; no response or live execution"],
            "execution_state": "completed", "actor_ref": "human-import",
            "metadata": {"live_replay": False, "batch_id": batch["batch_id"]},
        }
        self.store.put(task_id, "import_exchange", exchange, record_id=exchange["exchange_id"], parent_id=batch["batch_id"], natural_key=f"{batch['batch_id']}:{index}")
        parameter_results = []
        for location, pairs in (("query", parse_qsl(parts.query, keep_blank_values=True)),):
            for position, (name, value) in enumerate(pairs):
                parameter = self.assets.observe(
                    task_id, "parameter", {"url": url, "method": method, "location": location, "name": name},
                    source_kind="import", source_id=batch["source_id"],
                    attributes={"example": value, "original_name": name, "occurrence": position},
                    raw_values={"name": name, "value": value, "location": location, "occurrence": position},
                    observed_at=batch["source_metadata"].get("captured_at"),
                    artifact_refs=[raw_ref], parser_version=PARSER_VERSION,
                    source_event_id=f"{batch['batch_id']}:{index}:parameter:{location}:{position}:{name}",
                )
                parameter_results.append(parameter["asset"]["asset_id"])
        return {"index": index, "ok": True, "action": result["action"],
                "asset_id": result["asset"]["asset_id"], "exchange_id": exchange["exchange_id"],
                "parameter_asset_ids": parameter_results, "live_replay": False}

    def _har(self, task_id: str, batch: dict[str, Any], entry: dict[str, Any], index: int) -> dict[str, Any]:
        request = entry.get("request") or {}
        response = entry.get("response") or {}
        method = str(request.get("method") or "GET").upper()
        url = str(request.get("url") or "")
        if not urlsplit(url).hostname:
            raise SrcStoreError("import_http_invalid", "HAR request URL is required", detail={"index": index})
        entry_ref = self.artifacts.put(task_id, _json_bytes(entry), media_type="application/json", name=f"{batch['batch_id']}-{index}-har.json")
        request_derived = self.artifacts.put(task_id, _json_bytes(request), media_type="application/json", name=f"{batch['batch_id']}-{index}-request-derived.json")
        response_derived = self.artifacts.put(task_id, _json_bytes(response), media_type="application/json", name=f"{batch['batch_id']}-{index}-response-derived.json")
        attributes = {
            "status_code": response.get("status"), "status_text": response.get("statusText"),
            "request_headers": request.get("headers") or [], "response_headers": response.get("headers") or [],
            "har_derived": True, "live_replay": False,
        }
        result = self.assets.observe(
            task_id, "http_endpoint", {"url": url, "method": method}, source_kind="import",
            source_id=batch["source_id"], attributes=attributes, raw_values=entry,
            observed_at=entry.get("startedDateTime"), artifact_refs=[entry_ref, request_derived, response_derived],
            parser_version=PARSER_VERSION, success=None,
            completeness="partial", source_event_id=f"{batch['batch_id']}:{index}",
        )
        exchange = {
            "exchange_id": new_id("simportexchange"), "task_id": task_id,
            "source_kind": "import", "observed_url": url, "declared_target": url,
            "method": method, "protocol": request.get("httpVersion") or "unknown",
            "request_headers": request.get("headers") or [], "response_headers": response.get("headers") or [],
            "request_raw_ref": request_derived.model_dump(mode="json"),
            "response_raw_ref": response_derived.model_dump(mode="json"),
            "status_code": response.get("status"), "capture_state": "partial",
            "capture_limitations": ["HAR is a structured capture; raw wire bytes were not supplied"],
            "execution_state": "completed", "actor_ref": "human-import",
            "metadata": {"derived_from_har": True, "live_replay": False, "batch_id": batch["batch_id"]},
        }
        self.store.put(task_id, "import_exchange", exchange, record_id=exchange["exchange_id"], parent_id=batch["batch_id"], natural_key=f"{batch['batch_id']}:{index}")
        return {"index": index, "ok": True, "action": result["action"],
                "asset_id": result["asset"]["asset_id"], "exchange_id": exchange["exchange_id"],
                "live_replay": False, "capture_state": "partial"}

    @staticmethod
    def _counts(items: list[dict[str, Any]]) -> dict[str, int]:
        counts = {"processed": len(items), "created": 0, "merged": 0, "updated": 0,
                  "skipped": 0, "failed": 0, "truncated": 0}
        for item in items:
            if not item.get("ok"):
                counts["failed"] += 1
            else:
                action = item.get("action")
                if action in counts:
                    counts[action] += 1
        return counts

    @staticmethod
    def _status(items: list[dict[str, Any]]) -> str:
        if not items:
            return "failed"
        failures = sum(not item.get("ok") for item in items)
        return "partial" if failures and failures < len(items) else "failed" if failures == len(items) else "completed"

    def retry_failed(self, task_id: str, batch_id: str) -> dict[str, Any]:
        batch = self.store.require(task_id, "import_batch", batch_id).payload
        if batch.get("cost", {}).get("state") == "unknown":
            batch = {**batch, "status": "external_unknown", "recovery": "reconcile the upstream receipt before any retry"}
            self.store.put(task_id, "import_batch", batch, record_id=batch_id)
            return batch
        failed_indexes = {item["index"] for item in batch.get("items") or [] if not item.get("ok")}
        from .contracts import ArtifactRef
        raw = self.artifacts.read(ArtifactRef.model_validate(batch["original_artifact"]), expected_task_id=task_id)
        parsed = [item for item in self._parse(raw, batch["source_kind"], batch["original_artifact"]["media_type"]) if item["index"] in failed_indexes]
        prior = {item["index"]: item for item in batch.get("items") or []}
        retried = self._commit_items(task_id, batch, parsed, observed_at=batch["source_metadata"].get("observed_at"))
        merged = [{**prior[index], **item} if (item.get("action") in {"created", "merged"} and prior.get(index, {}).get("action") in {"created", "merged"}) else item
                  for index, item in ((item["index"], item) for item in retried)]
        by_index = {**prior, **{item["index"]: item for item in merged}}
        batch["items"] = [by_index[key] for key in sorted(by_index)]
        batch["counts"] = self._counts(batch["items"])
        batch["status"] = self._status(batch["items"])
        batch["last_retry_at"] = utcnow().isoformat()
        self.store.put(task_id, "import_batch", batch, record_id=batch_id)
        return batch

    def reconcile_external(self, task_id: str, batch_id: str, receipt: dict[str, Any]) -> dict[str, Any]:
        record = self.store.require(task_id, "import_batch", batch_id)
        batch = record.payload
        cost = dict(receipt.get("cost") or {})
        batch["upstream_receipt"] = receipt
        batch["cost"] = {**cost, "state": "confirmed" if cost.get("state") != "unknown" else "unknown"}
        batch["status"] = "reconciled" if batch["cost"]["state"] == "confirmed" else "external_unknown"
        result = self.store.put(task_id, "import_batch", batch, record_id=batch_id, expected_revision=record.revision)
        return result.payload

    def external_event(self, task_id: str, event: dict[str, Any]) -> dict[str, Any]:
        event_id = str(event.get("event_id") or event.get("id") or "").strip()
        if not event_id:
            raise SrcStoreError("external_event_invalid", "external event id is required")
        payload = _json_bytes(event)
        prior = self.store.idempotent(task_id, "external-event", event_id, event)
        if prior is not None:
            return prior
        artifact = self.artifacts.put(task_id, payload, media_type="application/json", name=f"external-event-{event_id}.json")
        items = event.get("assets") if isinstance(event.get("assets"), list) else []
        result = None
        if items:
            result = self.ingest(
                task_id, "external_event", _json_bytes({"items": items}), media_type="application/json",
                source_id=str(event.get("source_id") or event_id), source_metadata={"event": event},
                idempotency_key=f"external-event-ingest:{event_id}", observed_at=event.get("observed_at"),
            )
        else:
            record = {
                "event_id": event_id, "task_id": task_id, "source_version": event.get("source_version"),
                "target_refs": event.get("target_refs") or [], "artifact": artifact.model_dump(mode="json"),
                "received_at": utcnow().isoformat(), "payload": event,
            }
            self.store.put(task_id, "external_event", record, record_id=event_id)
        response = {"event_id": event_id, "artifact": artifact.model_dump(mode="json"),
                    "batch": result, "executed": False, "run_created": False}
        self.store.remember(task_id, "external-event", event_id, event, response)
        return response


__all__ = ["PARSER_VERSION", "SrcIngestionService"]
