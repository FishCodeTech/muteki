from __future__ import annotations

import base64
import csv
import hashlib
import io
import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional
from xml.etree import ElementTree

from .store import SrcStore


@dataclass
class ParsedSource:
    items: list[dict[str, Any]]
    errors: list[dict[str, Any]] = field(default_factory=list)
    unknown_fields: list[str] = field(default_factory=list)
    mapping: dict[str, Any] = field(default_factory=dict)
    media_type: str = "application/json"
    complete: bool = True


def _error(index: int, code: str, message: str, *, field: str = "") -> dict[str, Any]:
    return {"index": index, "field": field, "code": code, "message": message, "recovery": "修正该项后仅重试失败项"}


def _rows(data: bytes, name: str) -> tuple[list[tuple[int, Any]], str, list[dict[str, Any]]]:
    suffix = Path(name).suffix.lower()
    if suffix == ".csv":
        try:
            text = data.decode("utf-8-sig")
        except UnicodeDecodeError:
            text = data.decode("utf-8", errors="strict")
        return list(enumerate(csv.DictReader(io.StringIO(text)))), "text/csv", []
    if suffix in {".xml", ".burp"}:
        return [], "application/xml", []
    try:
        value = json.loads(data)
        items = value if isinstance(value, list) else [value]
        return list(enumerate(items)), "application/json", []
    except (json.JSONDecodeError, UnicodeDecodeError):
        pass
    rows: list[tuple[int, Any]] = []
    errors: list[dict[str, Any]] = []
    for index, raw in enumerate(data.splitlines()):
        if not raw.strip():
            continue
        try:
            rows.append((index, json.loads(raw)))
        except (json.JSONDecodeError, UnicodeDecodeError) as exc:
            errors.append(_error(index, "invalid_json", str(exc)))
    return rows, "application/x-ndjson", errors


def _stable(item: Any, index: int) -> str:
    if isinstance(item, dict):
        if item.get("url") and (item.get("started") or item.get("time")):
            return f"url:{item['url']}@{item.get('started') or item.get('time')}:{index}"
        for key in ("uuid", "id", "asset_id", "url", "domain", "ip", "package", "appid", "AppID"):
            if item.get(key):
                return f"{key}:{item[key]}"
    return f"sha256:{SrcStore.digest(item)}:{index}"


def _apply_mapping(item: dict[str, Any], mapping: Optional[dict[str, str]]) -> dict[str, Any]:
    if not mapping:
        return dict(item)
    mapped = dict(item)
    for source, target in mapping.items():
        if source in item:
            mapped[target] = item[source]
    return mapped


def _object(item: Any, index: int, mapping: Optional[dict[str, str]]) -> dict[str, Any]:
    if not isinstance(item, dict):
        return {"input_key": f"value:{index}", "mapped": {"value": item}, "raw": item}
    return {"input_key": _stable(item, index), "mapped": _apply_mapping(item, mapping), "raw": item}


def _har(value: dict[str, Any]) -> list[dict[str, Any]]:
    entries = ((value.get("log") or {}).get("entries") or [])
    items: list[dict[str, Any]] = []
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            items.append({"input_key": f"invalid:{index}", "error": _error(index, "invalid_har_entry", "entry must be an object"), "raw": entry})
            continue
        request = entry.get("request") or {}
        response = entry.get("response") or {}
        capture_state = "complete" if request and response else "partial"
        items.append({
            "input_key": _stable({"url": request.get("url"), "started": entry.get("startedDateTime")}, index),
            "mapped": {
                "type": "http_exchange", "method": request.get("method"),
                "url": request.get("url"), "status": response.get("status"),
                "capture_state": capture_state, "source_kind": "import",
            },
            "raw": {
                "request": request, "response": response,
                "startedDateTime": entry.get("startedDateTime"),
                "time": entry.get("time"), "timings": entry.get("timings"),
                "serverIPAddress": entry.get("serverIPAddress"),
                "comment": entry.get("comment"),
            },
        })
    return items


def _burp_xml(data: bytes) -> list[dict[str, Any]]:
    root = ElementTree.fromstring(data)
    items: list[dict[str, Any]] = []
    for index, entry in enumerate(root.findall(".//item")):
        def text(name: str) -> str:
            node = entry.find(name)
            return str(node.text or "") if node is not None else ""
        request_b64 = text("request")
        response_b64 = text("response")
        for label, encoded in (("request", request_b64), ("response", response_b64)):
            if encoded:
                base64.b64decode(encoded, validate=True)
        items.append({
            "input_key": _stable({"url": text("url"), "time": text("time")}, index),
            "mapped": {"type": "http_exchange", "method": text("method"), "url": text("url"),
                       "status": text("status"), "source_kind": "import",
                       "capture_state": "complete" if request_b64 and response_b64 else "partial"},
            "raw": {"request_base64": request_b64, "response_base64": response_b64,
                    "time": text("time"), "extension": text("extension")},
        })
    return items


def _looks_like_raw_http(data: bytes) -> bool:
    first = data.lstrip().split(b"\n", 1)[0].upper()
    return any(first.startswith(method + b" ") for method in (
        b"GET", b"POST", b"PUT", b"DELETE", b"PATCH", b"OPTIONS", b"HEAD", b"TRACE", b"CONNECT",
    ))


def _raw_http(data: bytes) -> ParsedSource:
    request = data
    response = b""
    for sep in (b"\r\n\r\n", b"\n\n"):
        marker = sep + b"HTTP/"
        pos = data.find(marker)
        if pos >= 0:
            split_at = pos + len(sep)
            request = data[:split_at]
            response = data[split_at:]
            break
    header_block = request.split(b"\r\n\r\n", 1)[0] if b"\r\n\r\n" in request else request.split(b"\n\n", 1)[0]
    lines = header_block.replace(b"\r\n", b"\n").split(b"\n")
    parts = lines[0].split(b" ", 2)
    if len(parts) < 2:
        return ParsedSource(items=[], errors=[_error(0, "invalid_raw_http", "request line is malformed")], complete=False)
    headers: list[list[str]] = []
    for line in lines[1:]:
        name, separator, value = line.partition(b":")
        if separator:
            headers.append([name.decode("latin-1"), value.lstrip().decode("latin-1")])
    status = None
    if response:
        first_line = response.replace(b"\r\n", b"\n").split(b"\n", 1)[0]
        status_parts = first_line.split(b" ", 2)
        if len(status_parts) >= 2 and status_parts[1].isdigit():
            status = int(status_parts[1])
    item = {
        "input_key": f"raw-http:{hashlib.sha256(request).hexdigest()}",
        "mapped": {
            "type": "http_exchange", "method": parts[0].decode("latin-1"),
            "url": parts[1].decode("latin-1"), "status": status,
            "source_kind": "import", "capture_state": "complete" if response else "partial",
            "headers": headers,
        },
        "raw": {
            "request_base64": base64.b64encode(request).decode("ascii"),
            "response_base64": base64.b64encode(response).decode("ascii") if response else "",
            "protocol": "raw-http/1.x",
        },
    }
    return ParsedSource(
        items=[item], media_type="message/http",
        mapping={"event_schema": "raw-http.v1", "derived_view": True, "replayed_by_muteki": False},
    )


def parse_source(
    connector_id: str, data: bytes, *, filename: str,
    field_mapping: Optional[dict[str, str]] = None,
) -> ParsedSource:
    if connector_id in {"burp", "caido", "cli"} and _looks_like_raw_http(data):
        return _raw_http(data)
    if connector_id == "burp" and Path(filename).suffix.lower() in {".xml", ".burp"}:
        try:
            return ParsedSource(items=_burp_xml(data), media_type="application/xml", mapping={"format": "burp-xml-v1"})
        except (ElementTree.ParseError, ValueError) as exc:
            return ParsedSource(items=[], errors=[_error(0, "invalid_burp_xml", str(exc))], complete=False)
    rows, media_type, errors = _rows(data, filename)
    unknown: list[str] = []
    items: list[dict[str, Any]] = []
    for index, row in rows:
        try:
            if connector_id in {"burp", "caido"} and isinstance(row, dict) and "log" in row:
                items.extend(_har(row))
                continue
            if connector_id == "bbot":
                if not isinstance(row, dict) or not row.get("type"):
                    errors.append(_error(index, "invalid_bbot_event", "BBOT event requires type", field="type"))
                    continue
                mapped = _object(row, index, field_mapping)
                mapped["mapped"]["event_type"] = row["type"]
                mapped["mapped"]["uuid"] = row.get("uuid") or row.get("id")
                mapped["mapped"]["parent_uuid"] = row.get("parent_uuid")
                mapped["mapped"]["scope"] = row.get("scope")
                items.append(mapped)
                continue
            if connector_id == "scopesentry":
                if isinstance(row, dict) and isinstance(row.get("data"), dict):
                    nested = row["data"]
                    candidates = nested.get("list") or nested.get("records") or nested.get("items") or []
                    if not isinstance(candidates, list):
                        raise ValueError("ScopeSentry data.list must be an array")
                    for child in candidates:
                        items.append(_object(child, len(items), field_mapping))
                    continue
                if isinstance(row, dict) and any(key in row for key in ("list", "records", "items")):
                    candidates = row.get("list") or row.get("records") or row.get("items")
                    if not isinstance(candidates, list):
                        raise ValueError("ScopeSentry list must be an array")
                    for child in candidates:
                        items.append(_object(child, len(items), field_mapping))
                    continue
            if connector_id == "cloud_inventory":
                if isinstance(row, dict) and isinstance(row.get("resources"), list):
                    for child in row["resources"]:
                        items.append(_object(child, len(items), field_mapping))
                    continue
            items.append(_object(row, index, field_mapping))
        except (TypeError, ValueError) as exc:
            errors.append(_error(index, "invalid_source_item", str(exc)))
    for item in items:
        mapped = item.get("mapped") or {}
        raw = item.get("raw")
        if isinstance(raw, dict) and isinstance(mapped, dict):
            kind = str(raw.get("type") or raw.get("asset_type") or raw.get("type_name") or "").lower()
            is_product = kind in {"app", "miniprogram", "mini_program", "mp", "wechat", "小程序"} or any(key in raw for key in ("app", "miniProgram", "miniprogram", "mp", "package", "bundle_id"))
            if connector_id == "scopesentry" and is_product:
                reliable = raw.get("AppID") or raw.get("appid") or raw.get("package") or raw.get("bundle_id") or raw.get("store_id")
                if not reliable:
                    mapped["identity_state"] = "candidate"
                    mapped["missing_fields"] = ["reliable_app_id"]
            if connector_id == "cloud_inventory":
                reliable = raw.get("resource_id") or raw.get("instance_id") or raw.get("arn") or raw.get("id")
                if not reliable:
                    mapped["identity_state"] = "candidate"
                    mapped["missing_fields"] = ["provider_resource_id"]
            if connector_id == "bbrf":
                mapped["program"] = raw.get("program") or raw.get("program_name")
                mapped["tags"] = raw.get("tags") or raw.get("tag") or []
    if connector_id == "bbot":
        mapping = {"event_schema": "bbot.typed-events.v3", "parent_chain": "uuid,parent_uuid"}
    elif connector_id == "reconftw":
        mapping = {"event_schema": "reconftw.assets.jsonl", "quickrescan_is_not_monitoring": True}
    elif connector_id in {"burp", "caido"}:
        mapping = {"event_schema": "har.http-archive.or.source-json", "replayed_by_muteki": False}
    else:
        mapping = {"event_schema": f"{connector_id}.source-json.v1"}
    if field_mapping:
        mapping["fields"] = dict(field_mapping)
    return ParsedSource(items=items, errors=errors, unknown_fields=unknown, mapping=mapping, media_type=media_type, complete=not errors)


__all__ = ["ParsedSource", "parse_source"]
