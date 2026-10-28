from __future__ import annotations

import base64
import binascii
import gzip
import hashlib
import json
import re
from datetime import datetime, timezone
from typing import Any, Iterable, Optional
from urllib.parse import parse_qsl, urlsplit

import jsonschema
import yaml

from muteki.platform.contracts.base import new_id, utcnow

from .artifacts import SrcArtifactStore
from .contracts import ArtifactRef, HttpExchange, SrcRecord
from .store import SrcStore, SrcStoreError

OPENAPI_EXTRACTOR = "src.openapi.v1"
JS_EXTRACTOR = "src.javascript-static.v1"
HTTP_METHODS = frozenset({"get", "post", "put", "patch", "delete", "options", "head", "trace"})
PROVENANCE_KINDS = frozenset({"user_description", "document", "observation", "model_inference"})


def _json_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), default=str).encode("utf-8")


def _record_payload(record: SrcRecord) -> dict[str, Any]:
    return {
        "record_id": record.record_id, "kind": record.kind,
        "revision": record.revision, "parent_id": record.parent_id,
        "version": record.payload.get("version", record.revision),
        **record.payload,
    }


def _pointer(document: Any, pointer: str) -> Any:
    if pointer in {"", "#"}:
        return document
    if not pointer.startswith("#/"):
        raise SrcStoreError("openapi_ref_unresolved", f"unsupported JSON pointer {pointer}")
    current = document
    for raw in pointer[2:].split("/"):
        part = raw.replace("~1", "/").replace("~0", "~")
        try:
            if isinstance(current, list):
                current = current[int(part)]
            elif isinstance(current, dict):
                current = current[part]
            else:
                raise KeyError(part)
        except (KeyError, IndexError, ValueError, TypeError) as exc:
            raise SrcStoreError("openapi_ref_unresolved", f"JSON pointer does not exist: {pointer}") from exc
    return current


def _path_template_matches(template: str, actual: str) -> bool:
    template_parts = (template or "/").split("/")
    actual_parts = (actual or "/").split("/")
    if len(template_parts) != len(actual_parts):
        return False
    for expected, value in zip(template_parts, actual_parts):
        if expected.startswith("{") and expected.endswith("}") and len(expected) > 2:
            if not value:
                return False
        elif expected != value:
            return False
    return True


def _is_js_identifier(value: str) -> bool:
    if not value:
        return False
    first = value[0]
    if not (first.isalpha() or first in {"_", "$"}):
        return False
    return all(ch.isalnum() or ch in {"_", "$"} for ch in value[1:])


def _looks_like_endpoint(value: str) -> bool:
    text = value.strip()
    if not text or any(ch.isspace() for ch in text):
        return False
    if text.startswith("/") and len(text) > 1 and not text.startswith("//"):
        return True
    scheme_sep = text.find("://")
    if scheme_sep > 0:
        scheme = text[:scheme_sep]
        if scheme[0].isalpha() and all(ch.isalnum() or ch in {"+", ".", "-"} for ch in scheme[1:]):
            return len(text) > scheme_sep + 3
    return False


def _path_parameter_values(template: str, actual: str) -> dict[str, str]:
    template_parts = template.split("/")
    actual_parts = actual.split("/")
    if len(template_parts) != len(actual_parts):
        return {}
    values = {}
    for expected, value in zip(template_parts, actual_parts):
        if expected.startswith("{") and expected.endswith("}"):
            values[expected[1:-1]] = value
        elif expected != value:
            return {}
    return values


def _runtime_path_candidates(servers: list[dict[str, Any]], actual: Any) -> list[str]:
    candidates = []
    absolute_servers = [server for server in servers if urlsplit(str(server.get("url") or "")).hostname]
    for server in servers:
        parts = urlsplit(str(server.get("url") or ""))
        if parts.hostname and (parts.scheme.lower() != actual.scheme.lower() or parts.hostname.lower() != str(actual.hostname or "").lower()):
            continue
        base = parts.path.rstrip("/")
        if not base and (parts.hostname or not absolute_servers):
            candidates.append(actual.path or "/")
        elif base and actual.path.startswith(base + "/"):
            candidates.append(actual.path[len(base):] or "/")
        elif base == actual.path:
            candidates.append("/")
    if not absolute_servers:
        candidates.append(actual.path or "/")
    return list(dict.fromkeys(candidates))


def _json_body(raw: bytes, content_type: str = "") -> tuple[Any, Optional[str]]:
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return None, "body is not UTF-8 JSON"
    try:
        return json.loads(text), None
    except json.JSONDecodeError as exc:
        return None, f"invalid JSON at line {exc.lineno} column {exc.colno}: {exc.msg}"


def _response_body(raw: bytes, headers: Iterable[Iterable[str]] = ()) -> tuple[bytes, list[str]]:
    notes: list[str] = []
    header_map: dict[str, list[str]] = {}
    for pair in headers:
        if len(pair) >= 2:
            header_map.setdefault(str(pair[0]).lower(), []).append(str(pair[1]))
    body = raw
    if "chunked" in ",".join(header_map.get("transfer-encoding", [])).lower():
        try:
            decoded = bytearray()
            rest = body
            while True:
                line, rest = rest.split(b"\r\n", 1)
                size = int(line.split(b";", 1)[0], 16)
                if size == 0:
                    break
                chunk, rest = rest[:size], rest[size + 2:]
                decoded.extend(chunk)
            body = bytes(decoded)
            notes.append("chunked transfer decoding is a derived view; wire bytes are unchanged")
        except (ValueError, IndexError) as exc:
            return b"", [f"chunked body is incomplete: {exc}"]
    encodings = ",".join(header_map.get("content-encoding", [])).lower()
    if "gzip" in encodings:
        try:
            body = gzip.decompress(body)
            notes.append("gzip decoding is a derived view; wire bytes are unchanged")
        except OSError as exc:
            return b"", [f"gzip body is incomplete: {exc}"]
    return body, notes


def _schema_errors(schema: dict[str, Any], value: Any) -> list[dict[str, str]]:
    try:
        validator = jsonschema.Draft202012Validator(schema)
        return [
            {"path": "$" + "".join(f"[{part}]" if isinstance(part, int) else f".{part}" for part in error.absolute_path),
             "message": error.message}
            for error in validator.iter_errors(value)
        ]
    except (jsonschema.SchemaError, TypeError, ValueError) as exc:
        return [{"path": "$", "message": f"response schema is invalid: {exc}"}]


class SrcResearchService:
    def __init__(self, store: SrcStore, artifacts: SrcArtifactStore) -> None:
        self.store = store
        self.artifacts = artifacts

    def _versioned(
        self, task_id: str, kind: str, parent_id: str, payload: dict[str, Any], *,
        base_revision: Optional[int] = None, natural_key: str = "",
    ) -> SrcRecord:
        parent_id = parent_id or new_id(kind.replace("_", "-"))
        prior = [row for row in self.store.all(task_id, kind) if row.parent_id == parent_id]
        latest = max(prior, key=lambda row: int(row.payload.get("version") or 0), default=None)
        latest_version = int(latest.payload.get("version") or 0) if latest else 0
        if base_revision is not None and latest_version != base_revision:
            raise SrcStoreError(
                "version_conflict", f"{kind} base revision is stale",
                detail={"expected": base_revision, "actual": latest_version},
            )
        version = int(latest.payload.get("version") or 0) + 1 if latest else 1
        record_id = f"{parent_id}:v{version}"
        body = {
            **payload, "version": version,
            "supersedes": latest.record_id if latest else None,
            "digest": self.store.digest(payload),
        }
        return self.store.put(
            task_id, kind, body, record_id=record_id, parent_id=parent_id,
            natural_key=natural_key or f"{parent_id}:v{version}",
        )

    def record(self, task_id: str, kind: str, record_id: str) -> SrcRecord:
        return self.store.require(task_id, kind, record_id)

    def records(self, task_id: str, kind: str = "", *, limit: int = 100, after_rowid: int = 0) -> list[SrcRecord]:
        return self.store.list(task_id, kind, after_rowid=after_rowid, limit=limit)

    def save_plan(
        self, task_id: str, payload: dict[str, Any], *, actor_id: str,
        plan_id: str = "", base_revision: Optional[int] = None,
    ) -> SrcRecord:
        required = ("question", "rationale", "gaps", "actions")
        missing = [key for key in required if key not in payload]
        if missing:
            raise SrcStoreError("plan_incomplete", f"plan is missing: {', '.join(missing)}")
        body = {
            **payload, "created_by": actor_id, "created_at": utcnow().isoformat(),
            "evidence_type": "Plan", "scheduler": False,
        }
        return self._versioned(task_id, "research_plan", plan_id, body, base_revision=base_revision)

    def current_plan(self, task_id: str, plan_id: str) -> SrcRecord:
        rows = [row for row in self.store.all(task_id, "research_plan") if row.parent_id == plan_id]
        if not rows:
            raise SrcStoreError("artifact_missing", "research plan is unavailable")
        return max(rows, key=lambda row: int(row.payload.get("version") or 0))

    def save_authorization_snapshot(
        self, task_id: str, *, source_refs: list[str], raw_artifact: Optional[ArtifactRef],
        clauses: list[dict[str, Any]], unknowns: list[dict[str, Any]],
        actor_id: str, snapshot_id: str = "", base_revision: Optional[int] = None,
    ) -> SrcRecord:
        if raw_artifact is not None:
            self.artifacts.read(raw_artifact, expected_task_id=task_id)
        body = {
            "source_refs": list(source_refs),
            "raw_artifact": raw_artifact.model_dump(mode="json") if raw_artifact else None,
            "clauses": clauses, "unknowns": unknowns, "created_by": actor_id,
            "created_at": utcnow().isoformat(), "authorization_is_not_execution_grant": True,
        }
        return self._versioned(task_id, "authorization_snapshot", snapshot_id, body, base_revision=base_revision)

    def register_skill_material(
        self, task_id: str, *, source_url: str, source_commit: str,
        content: bytes, required_capabilities: dict[str, str],
        available_capabilities: dict[str, str], selected_fragments: list[dict[str, Any]],
        distillation_version: str, actor_id: str, read_scope: str = "task",
        material_id: str = "", base_revision: Optional[int] = None,
    ) -> SrcRecord:
        if not source_url or not source_commit or not distillation_version:
            raise SrcStoreError("skill_source_incomplete", "source URL, commit and distillation version are required")
        digest = hashlib.sha256(content).hexdigest()
        artifact = self.artifacts.put(task_id, content, media_type="text/markdown", name="skill-source")
        mappings = []
        missing = []
        for capability, required_version in required_capabilities.items():
            actual_version = available_capabilities.get(capability)
            available = actual_version is not None
            row = {
                "capability": capability, "required_version": required_version,
                "actual_version": actual_version, "available": available,
            }
            if not available:
                row["code"] = "capability_unavailable"
                missing.append(capability)
            mappings.append(row)
        body = {
            "source_url": source_url, "source_commit": source_commit,
            "content_digest": digest, "distillation_version": distillation_version,
            "selected_fragments": selected_fragments, "capability_mappings": mappings,
            "missing_capabilities": missing, "read_scope": read_scope,
            "artifact": artifact.model_dump(mode="json"), "registered_by": actor_id,
            "registered_at": utcnow().isoformat(), "status": "partial" if missing else "available",
        }
        return self._versioned(
            task_id, "skill_material", material_id or self.store.digest({"url": source_url}),
            body, base_revision=base_revision,
        )

    def read_skill_material(self, task_id: str, record_id: str, *, fragment_index: Optional[int] = None) -> dict[str, Any]:
        record = self.store.require(task_id, "skill_material", record_id)
        body = dict(record.payload)
        artifact = ArtifactRef.model_validate(body["artifact"])
        content = self.artifacts.read(artifact, expected_task_id=task_id)
        result = {"record": _record_payload(record), "capability_mappings": body["capability_mappings"]}
        if fragment_index is None:
            result["original"] = content
            return result
        try:
            fragment = body["selected_fragments"][fragment_index]
            start, end = int(fragment["byte_start"]), int(fragment["byte_end"])
            if start < 0 or end < start or end > len(content):
                raise ValueError("outside original")
        except (IndexError, KeyError, TypeError, ValueError) as exc:
            raise SrcStoreError("skill_fragment_invalid", "fragment does not resolve to the original source") from exc
        result["fragment"] = {**fragment, "content": content[start:end]}
        return result

    def ingest_openapi(
        self, task_id: str, content: bytes, *, source_url: str,
        media_type: str = "application/json", source_run_id: Optional[str] = None,
        observed_at: Optional[datetime] = None,
    ) -> SrcRecord:
        source_digest = hashlib.sha256(content).hexdigest()
        artifact = self.artifacts.put(
            task_id, content, media_type=media_type, name="openapi-source",
            source_run_id=source_run_id,
        )
        key = f"{OPENAPI_EXTRACTOR}:{source_digest}"
        prior = self.store.idempotent(task_id, "openapi-ingest", key, {"sha256": source_digest})
        if prior is not None:
            return self.store.require(task_id, "research_document", prior["record_id"])
        errors: list[dict[str, Any]] = []
        unresolved: list[dict[str, Any]] = []
        try:
            document = yaml.safe_load(content)
        except (yaml.YAMLError, UnicodeDecodeError) as exc:
            document = None
            errors.append({"location": "$", "code": "openapi_parse_error", "message": str(exc)})
        if document is None and not errors:
            errors.append({"location": "$", "code": "openapi_empty", "message": "document is empty"})
        if document is not None and not isinstance(document, dict):
            errors.append({"location": "$", "code": "openapi_root_invalid", "message": "document root must be an object"})
            document = None
        declaration: dict[str, Any] = {
            "openapi": None, "servers": [], "operations": [], "security_schemes": {},
            "global_security": [],
        }
        status = "invalid" if errors else "available"

        def resolve(value: Any, location: str, seen: tuple[str, ...] = ()) -> Any:
            if isinstance(value, dict) and "$ref" in value:
                reference = str(value["$ref"])
                try:
                    if not reference.startswith("#"):
                        raise SrcStoreError("openapi_ref_unresolved", "external references are not fetched or granted authority")
                    if reference in seen:
                        raise SrcStoreError("openapi_ref_unresolved", f"cyclic reference: {' -> '.join((*seen, reference))}")
                    resolved = _pointer(document, reference)
                    return resolve(resolved, f"{location}({reference})", (*seen, reference))
                except SrcStoreError as exc:
                    unresolved.append({"location": location, "ref": reference, "code": exc.code, "message": exc.message})
                    return None
            if isinstance(value, dict):
                return {key: resolve(item, f"{location}.{key}", seen) for key, item in value.items()}
            if isinstance(value, list):
                return [resolve(item, f"{location}[{index}]", seen) for index, item in enumerate(value)]
            return value

        if document is not None:
            declaration["openapi"] = document.get("openapi") or document.get("swagger")
            swagger_server = ""
            if document.get("host"):
                schemes = document.get("schemes") or ["https"]
                swagger_server = f"{schemes[0]}://{document['host']}{document.get('basePath') or '/'}"
            declaration["servers"] = document.get("servers") or ([{"url": swagger_server}] if swagger_server else [])
            declaration["global_security"] = document.get("security") or []
            schemes = document.get("components", {}).get("securitySchemes") or document.get("securityDefinitions") or {}
            declaration["security_schemes"] = {name: resolve(schema, f"$.securitySchemes.{name}") for name, schema in schemes.items()}
            paths = document.get("paths") or {}
            if not isinstance(paths, dict):
                errors.append({"location": "$.paths", "code": "openapi_paths_invalid", "message": "paths must be an object"})
                paths = {}
            for path, path_item in paths.items():
                if not isinstance(path_item, dict):
                    errors.append({"location": f"$.paths.{path}", "code": "openapi_path_invalid", "message": "path item must be an object"})
                    continue
                shared = path_item.get("parameters") or []
                for method, operation in path_item.items():
                    if method.lower() not in HTTP_METHODS:
                        continue
                    location = f"$.paths.{path}.{method}"
                    if not isinstance(operation, dict):
                        errors.append({"location": location, "code": "openapi_operation_invalid", "message": "operation must be an object"})
                        continue
                    parameters = []
                    for index, parameter in enumerate([*shared, *(operation.get("parameters") or [])]):
                        parameter_location = f"{location}.parameters[{index}]"
                        resolved = resolve(parameter, parameter_location)
                        if resolved is None:
                            continue
                        if not isinstance(resolved, dict):
                            errors.append({"location": parameter_location, "code": "openapi_parameter_invalid", "message": "parameter must be an object"})
                            continue
                        parameters.append({
                            "name": resolved.get("name"), "in": resolved.get("in"),
                            "required": bool(resolved.get("required")),
                            "schema": resolve(resolved.get("schema") or {}, f"{parameter_location}.schema"),
                            "source_location": parameter_location,
                        })
                    body_parameter = next((item for item in parameters if item["in"] == "body"), None)
                    if body_parameter is not None:
                        consumes = operation.get("consumes") or document.get("consumes") or ["application/json"]
                        request_body = {
                            "required": body_parameter["required"],
                            "content": {media: {"schema": body_parameter["schema"]} for media in consumes},
                            "source_location": body_parameter["source_location"],
                        }
                    else:
                        request_body = resolve(operation.get("requestBody") or {}, f"{location}.requestBody") or {}
                    responses = {}
                    for status_code, response in (operation.get("responses") or {}).items():
                        response_location = f"{location}.responses.{status_code}"
                        resolved_response = resolve(response, response_location)
                        if resolved_response is None:
                            continue
                        content_map = resolved_response.get("content") or {}
                        response_schemas = {}
                        for response_type, response_content in content_map.items():
                            response_schemas[response_type] = resolve(
                                response_content.get("schema") or {},
                                f"{response_location}.content.{response_type}.schema",
                            )
                        if resolved_response.get("schema"):
                            produces = operation.get("produces") or document.get("produces") or ["application/json"]
                            response_schemas[produces[0]] = resolve(resolved_response["schema"], f"{response_location}.schema")
                        responses[str(status_code)] = {"schemas": response_schemas, "source_location": response_location}
                    declaration["operations"].append({
                        "operation_id": operation.get("operationId") or f"{method.upper()} {path}",
                        "method": method.upper(), "path": path,
                        "parameters": parameters, "request_body": request_body,
                        "responses": responses, "security": operation.get("security", declaration["global_security"]),
                        "source_location": location,
                    })
            if errors:
                status = "invalid"
            elif unresolved:
                status = "partial"
            else:
                status = "available"
        body = {
            "document_type": "openapi", "extractor_version": OPENAPI_EXTRACTOR,
            "source_url": source_url, "source_run_id": source_run_id,
            "source_digest": source_digest, "artifact": artifact.model_dump(mode="json"),
            "observed_at": observed_at.isoformat() if observed_at else None,
            "ingested_at": utcnow().isoformat(), "declaration": declaration,
            "unresolved_refs": unresolved, "errors": errors, "status": status,
            "runtime_reachability": "not_requested",
        }
        with self.store.platform.transaction():
            record = self.store.put(
                task_id, "research_document", body,
                record_id=f"openapi:{source_digest}:{OPENAPI_EXTRACTOR}",
                parent_id=source_digest, natural_key=f"openapi:{source_digest}:{OPENAPI_EXTRACTOR}",
            )
            for operation in declaration["operations"]:
                self.store.put(
                    task_id, "openapi_operation", {
                        "document_id": record.record_id, "document_digest": source_digest,
                        "declaration_state": "document_declared", **operation,
                    },
                    record_id=f"{record.record_id}:{operation['method']}:{operation['path']}",
                    parent_id=record.record_id,
                    natural_key=f"{record.record_id}:{operation['method']}:{operation['path']}",
                )
            self.store.remember(task_id, "openapi-ingest", key, {"sha256": source_digest}, {"record_id": record.record_id})
        return record

    def read_openapi(self, task_id: str, record_id: str) -> dict[str, Any]:
        record = self.store.require(task_id, "research_document", record_id)
        if record.payload.get("document_type") != "openapi":
            raise SrcStoreError("openapi_type_mismatch", "research document is not OpenAPI")
        artifact = ArtifactRef.model_validate(record.payload["artifact"])
        return {**_record_payload(record), "original": self.artifacts.read(artifact, expected_task_id=task_id)}

    @staticmethod
    def _exchange_dict(exchange: HttpExchange | dict[str, Any] | SrcRecord) -> dict[str, Any]:
        if isinstance(exchange, SrcRecord):
            return dict(exchange.payload)
        if isinstance(exchange, HttpExchange):
            return exchange.model_dump(mode="json")
        return dict(exchange)

    def _request_parameters(self, exchange: dict[str, Any]) -> list[dict[str, Any]]:
        url = urlsplit(str(exchange.get("observed_url") or exchange.get("declared_target") or ""))
        rows = [{"name": name, "in": "query", "value": value} for name, value in parse_qsl(url.query, keep_blank_values=True)]
        raw_ref = exchange.get("request_raw_ref")
        if isinstance(raw_ref, dict):
            try:
                raw = self.artifacts.read(ArtifactRef.model_validate(raw_ref), expected_task_id=str(exchange["task_id"]))
                body = raw.split(b"\r\n\r\n", 1)[1] if b"\r\n\r\n" in raw else b""
                content_type = next((value for key, value in exchange.get("request_headers") or [] if str(key).lower() == "content-type"), "")
                if "json" in content_type:
                    value, error = _json_body(body, content_type)
                    if error:
                        rows.append({"name": None, "in": "body", "error": error})
                    elif isinstance(value, dict):
                        rows.extend({"name": name, "in": "body", "value": item} for name, item in value.items())
                elif "application/x-www-form-urlencoded" in content_type:
                    rows.extend({"name": name, "in": "body", "value": value} for name, value in parse_qsl(body.decode("utf-8"), keep_blank_values=True))
            except (SrcStoreError, UnicodeDecodeError) as exc:
                rows.append({"name": None, "in": "body", "error": str(exc)})
        return rows

    def compare_openapi(
        self, task_id: str, document_id: str,
        exchanges: Iterable[HttpExchange | dict[str, Any] | SrcRecord], *,
        base_revision: Optional[int] = None,
    ) -> SrcRecord:
        document = self.store.require(task_id, "research_document", document_id)
        if document.payload.get("document_type") != "openapi":
            raise SrcStoreError("openapi_type_mismatch", "research document is not OpenAPI")
        rows = [self._exchange_dict(exchange) for exchange in exchanges]
        for exchange in rows:
            if str(exchange.get("task_id")) != task_id:
                raise SrcStoreError("task_binding_invalid", "exchange belongs to another Task")
        declaration = document.payload["declaration"]
        servers = declaration.get("servers") or []
        differences: list[dict[str, Any]] = []
        matched: list[dict[str, Any]] = []
        for exchange in rows:
            observed_url = str(exchange.get("observed_url") or "")
            parts = urlsplit(observed_url)
            operation = None
            operation_path = parts.path
            for candidate_path in _runtime_path_candidates(servers, parts):
                operation = next((item for item in declaration["operations"]
                                  if item["method"] == str(exchange.get("method") or "").upper()
                                  and _path_template_matches(item["path"], candidate_path)), None)
                if operation is not None:
                    operation_path = candidate_path
                    break
            if operation is None:
                differences.append({"kind": "undocumented_exchange", "exchange_id": exchange.get("exchange_id"),
                                    "method": exchange.get("method"), "url": observed_url,
                                    "meaning": "runtime observation only; not a vulnerability judgment"})
                continue
            match = {"exchange_id": exchange.get("exchange_id"), "operation_id": operation["operation_id"],
                     "source_location": operation["source_location"], "matched_path": operation_path,
                     "identity_revision": exchange.get("identity_revision"),
                     "observed_at": exchange.get("started_at"), "differences": []}
            parameters = self._request_parameters(exchange)
            parameters.extend({"name": name, "in": "path", "value": value}
                              for name, value in _path_parameter_values(operation["path"], operation_path).items())
            documented = {(item["name"], item["in"]): item for item in operation["parameters"]}
            has_body_declaration = any(location == "body" for _name, location in documented)
            for (name, location), parameter in documented.items():
                if location == "body":
                    continue
                if parameter["required"] and not any(row["name"] == name and row["in"] == location for row in parameters):
                    match["differences"].append({"kind": "missing_declared_parameter", "name": name, "in": location})
            for parameter in parameters:
                if parameter.get("error"):
                    match["differences"].append({"kind": "parameter_unavailable", "detail": parameter["error"]})
                elif parameter["in"] == "body" and has_body_declaration:
                    continue
                elif (parameter["name"], parameter["in"]) not in documented:
                    match["differences"].append({"kind": "undocumented_parameter", "name": parameter["name"], "in": parameter["in"]})
            request_spec = operation.get("request_body") or {}
            request_content = request_spec.get("content") or {}
            if request_spec or request_content:
                try:
                    raw = self.artifacts.read(ArtifactRef.model_validate(exchange["request_raw_ref"]), expected_task_id=task_id)
                    request_body = raw.split(b"\r\n\r\n", 1)[1] if b"\r\n\r\n" in raw else b""
                    request_type = next((value for key, value in exchange.get("request_headers") or [] if str(key).lower() == "content-type"), "")
                    media_type = request_type.split(";", 1)[0]
                    schema = (request_content.get(media_type) or {}).get("schema")
                    if request_spec.get("required") and not request_body:
                        match["differences"].append({"kind": "missing_request_body"})
                    if schema and request_body:
                        if "json" in media_type:
                            value, error = _json_body(request_body, request_type)
                            if error:
                                match["differences"].append({"kind": "request_body_unavailable", "detail": error})
                            else:
                                match["differences"].extend({"kind": "request_schema_mismatch", **item} for item in _schema_errors(schema, value))
                        elif media_type == "application/x-www-form-urlencoded":
                            value = dict(parse_qsl(request_body.decode("utf-8"), keep_blank_values=True))
                            match["differences"].extend({"kind": "request_schema_mismatch", **item} for item in _schema_errors(schema, value))
                except (SrcStoreError, UnicodeDecodeError) as exc:
                    match["differences"].append({"kind": "request_body_unavailable", "detail": str(exc)})
            documented_security = bool(operation.get("security"))
            actual_security = bool(exchange.get("metadata", {}).get("auth_scheme")) or any(
                str(key).lower() == "authorization" for key, _value in exchange.get("request_headers") or [])
            if documented_security != actual_security:
                match["differences"].append({"kind": "authentication_difference",
                                             "documented": documented_security, "observed": actual_security})
            response_spec = operation["responses"].get(str(exchange.get("status_code")))
            if response_spec is None:
                match["differences"].append({"kind": "undocumented_response_status", "status_code": exchange.get("status_code")})
            elif exchange.get("response_raw_ref"):
                try:
                    raw = self.artifacts.read(ArtifactRef.model_validate(exchange["response_raw_ref"]), expected_task_id=task_id)
                    body = raw.split(b"\r\n\r\n", 1)[1] if b"\r\n\r\n" in raw else raw
                    body, notes = _response_body(body, exchange.get("response_headers") or [])
                    if notes:
                        match["differences"].extend({"kind": "derived_response_view", "detail": note} for note in notes)
                    content_type = next((value for key, value in exchange.get("response_headers") or [] if str(key).lower() == "content-type"), "application/json")
                    schema = response_spec["schemas"].get(content_type.split(";", 1)[0]) or next(iter(response_spec["schemas"].values()), None)
                    if schema:
                        value, error = _json_body(body, content_type)
                        if error:
                            match["differences"].append({"kind": "response_body_unavailable", "detail": error})
                        else:
                            match["differences"].extend({"kind": "response_schema_mismatch", **item} for item in _schema_errors(schema, value))
                except SrcStoreError as exc:
                    match["differences"].append({"kind": "response_unavailable", "code": exc.code, "detail": exc.message})
            matched.append(match)
            differences.extend({"exchange_id": exchange.get("exchange_id"), **item} for item in match["differences"])
        documented_keys = {(item["method"], item["path"]) for item in declaration["operations"]}
        matched_keys = {(item["method"], item["path"]) for item in declaration["operations"]
                        if any(match["operation_id"] == item["operation_id"] for match in matched)}
        missing_operations = sorted(documented_keys - matched_keys)
        body = {
            "document_id": document_id, "document_digest": document.payload["source_digest"],
            "document_revision": document.revision,
            "exchange_refs": [{"exchange_id": row.get("exchange_id"), "identity_revision": row.get("identity_revision"),
                               "started_at": row.get("started_at"), "target_version": row.get("metadata", {}).get("target_version")} for row in rows],
            "matches": matched, "differences": differences,
            "missing_declared_operations": [{"method": method, "path": path} for method, path in missing_operations],
            "created_at": utcnow().isoformat(), "difference_is_not_vulnerability": True,
        }
        return self._versioned(
            task_id, "openapi_comparison", f"{document_id}:comparison", body,
            base_revision=base_revision,
        )

    @staticmethod
    def _javascript_tokens(content: str) -> list[dict[str, Any]]:
        tokens: list[dict[str, Any]] = []
        index = 0
        line, column = 1, 1
        state = "code"
        quote = ""
        start_line = start_column = 1
        buffer: list[str] = []
        multi_ops = ("===", "!==", ">>>", "=>", "==", "!=", "<=", ">=", "&&", "||", "??", "?.", "...")

        def advance(char: str) -> None:
            nonlocal line, column
            if char == "\n":
                line += 1
                column = 1
            else:
                column += 1

        def scan_code_segment(segment: str, seg_line: int, seg_col: int) -> None:
            i = 0
            cur_line, cur_col = seg_line, seg_col
            while i < len(segment):
                ch = segment[i]
                if ch.isspace():
                    if ch == "\n":
                        cur_line += 1
                        cur_col = 1
                    else:
                        cur_col += 1
                    i += 1
                    continue
                matched_op = next((op for op in multi_ops if segment.startswith(op, i)), None)
                if matched_op is not None:
                    tokens.append({
                        "kind": "symbol", "value": matched_op,
                        "line": cur_line, "column": cur_col,
                        "end_line": cur_line, "end_column": cur_col + len(matched_op),
                    })
                    cur_col += len(matched_op)
                    i += len(matched_op)
                    continue
                if ch.isalpha() or ch in {"_", "$"}:
                    start_c = cur_col
                    j = i + 1
                    while j < len(segment) and (segment[j].isalnum() or segment[j] in {"_", "$"}):
                        j += 1
                    ident = segment[i:j]
                    tokens.append({
                        "kind": "symbol", "value": ident,
                        "line": cur_line, "column": start_c,
                        "end_line": cur_line, "end_column": start_c + len(ident),
                    })
                    cur_col += len(ident)
                    i = j
                    continue
                if ch.isdigit():
                    start_c = cur_col
                    j = i + 1
                    while j < len(segment) and (segment[j].isalnum() or segment[j] in {".", "_"}):
                        j += 1
                    num = segment[i:j]
                    tokens.append({
                        "kind": "number", "value": num,
                        "line": cur_line, "column": start_c,
                        "end_line": cur_line, "end_column": start_c + len(num),
                    })
                    cur_col += len(num)
                    i = j
                    continue
                tokens.append({
                    "kind": "symbol", "value": ch,
                    "line": cur_line, "column": cur_col,
                    "end_line": cur_line, "end_column": cur_col + 1,
                })
                cur_col += 1
                i += 1

        def emit(kind: str, value: str, end_line: int, end_column: int) -> None:
            nonlocal buffer
            if kind == "code":
                if value:
                    scan_code_segment(value, start_line, start_column)
            else:
                tokens.append({
                    "kind": kind, "value": value, "line": start_line,
                    "column": start_column, "end_line": end_line, "end_column": end_column,
                })
            buffer = []

        while index < len(content):
            char = content[index]
            following = content[index + 1] if index + 1 < len(content) else ""
            if state == "code":
                if char in {'"', "'", "`"}:
                    emit("code", "".join(buffer), line, column)
                    state, quote = "template" if char == "`" else "string", char
                    start_line, start_column = line, column
                elif char == "/" and following == "/":
                    emit("code", "".join(buffer), line, column)
                    state = "line_comment"
                elif char == "/" and following == "*":
                    emit("code", "".join(buffer), line, column)
                    state = "block_comment"
                else:
                    if not buffer:
                        start_line, start_column = line, column
                    buffer.append(char)
            elif state == "line_comment":
                if char == "\n":
                    state = "code"
                    start_line, start_column = line + 1, 1
            elif state == "block_comment":
                if char == "*" and following == "/":
                    state = "code"
                    advance(char)
                    index += 1
                    advance("/")
                    index += 1
                    start_line, start_column = line, column
                    continue
            else:
                if char == "\\":
                    buffer.append(char)
                    if following:
                        buffer.append(following)
                        advance(char)
                        index += 1
                        advance(following)
                        index += 1
                        continue
                elif char == quote:
                    emit("string", "".join(buffer), line, column)
                    state = "code"
                    start_line, start_column = line, column + 1
                else:
                    buffer.append(char)
            advance(char)
            index += 1
        if state in {"string", "template"}:
            emit("unterminated_string", "".join(buffer), line, column)
        else:
            emit("code", "".join(buffer), line, column)
        return tokens

    @staticmethod
    def _source_map_positions(mappings: str) -> dict[tuple[int, int], tuple[int, int, int, int]]:
        alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        values = {char: index for index, char in enumerate(alphabet)}

        def vlq(segment: str, offset: int) -> tuple[int, int]:
            result = shift = 0
            while True:
                if offset >= len(segment) or segment[offset] not in values:
                    raise SrcStoreError("source_map_invalid", "invalid base64 VLQ mapping")
                digit = values[segment[offset]]
                offset += 1
                result += (digit & 31) << shift
                if not digit & 32:
                    sign = -1 if result & 1 else 1
                    return sign * (result >> 1), offset
                shift += 5

        positions: dict[tuple[int, int], tuple[int, int, int, int]] = {}
        generated_line = 0
        generated_column = source_index = original_line = original_column = name_index = 0
        for raw_line in mappings.split(";"):
            generated_column = 0
            if raw_line:
                for segment in raw_line.split(","):
                    if not segment:
                        continue
                    fields = []
                    offset = 0
                    while offset < len(segment):
                        value, offset = vlq(segment, offset)
                        fields.append(value)
                    if len(fields) not in {1, 4, 5}:
                        raise SrcStoreError("source_map_invalid", "mapping segment has an invalid field count")
                    generated_column += fields[0]
                    if len(fields) >= 4:
                        source_index += fields[1]
                        original_line += fields[2]
                        original_column += fields[3]
                        if len(fields) == 5:
                            name_index += fields[4]
                        positions[(generated_line + 1, generated_column + 1)] = (
                            source_index, original_line + 1, original_column + 1, name_index,
                        )
            generated_line += 1
        return positions

    @staticmethod
    def _javascript_function_scopes(tokens: list[dict[str, Any]]) -> list[dict[str, Any]]:
        scopes: list[dict[str, Any]] = []
        active: list[dict[str, Any]] = []
        pending: Optional[str] = None
        depth = 0
        for index, token in enumerate(tokens):
            if token["kind"] != "symbol":
                continue
            value = token["value"]
            if value == "function":
                name = next((tokens[next_index]["value"] for next_index in range(index + 1, min(index + 4, len(tokens)))
                             if _is_js_identifier(tokens[next_index]["value"])
                             and tokens[next_index]["value"] not in {"function", "async"}), "<anonymous>")
                pending = name
            elif value == "=>":
                name = "<arrow>"
                for previous in range(index - 1, -1, -1):
                    if tokens[previous]["value"] in {"=", ":"} and previous > 0 and _is_js_identifier(tokens[previous - 1]["value"]):
                        name = tokens[previous - 1]["value"]
                        break
                    if tokens[previous]["value"] in {";", "{", "}"}:
                        break
                pending = name
            elif value == "{":
                depth += 1
                if pending is not None:
                    active.append({"name": pending, "depth": depth, "start": index, "end": len(tokens)})
                    pending = None
            elif value == "}":
                if active and active[-1]["depth"] == depth:
                    scope = active.pop()
                    scope["end"] = index
                    scopes.append(scope)
                depth = max(0, depth - 1)
        scopes.extend(active)
        return scopes

    @staticmethod
    def _javascript_scope(index: int, scopes: list[dict[str, Any]]) -> str:
        matching = [scope for scope in scopes if scope["start"] <= index <= scope["end"]]
        if not matching:
            return "<top-level>"
        return min(matching, key=lambda scope: scope["end"] - scope["start"])["name"]

    def ingest_javascript(
        self, task_id: str, content: bytes, *, source_url: str,
        source_map: Optional[bytes] = None, source_run_id: Optional[str] = None,
    ) -> SrcRecord:
        digest = hashlib.sha256(content).hexdigest()
        key = f"{JS_EXTRACTOR}:{digest}:{hashlib.sha256(source_map or b'').hexdigest()}"
        prior = self.store.idempotent(task_id, "javascript-ingest", key, {"sha256": digest})
        if prior is not None:
            return self.store.require(task_id, "research_document", prior["record_id"])
        artifact = self.artifacts.put(task_id, content, media_type="text/javascript", name="javascript-source", source_run_id=source_run_id)
        try:
            text = content.decode("utf-8")
            decode_error = ""
        except UnicodeDecodeError as exc:
            text = ""
            decode_error = str(exc)
        errors = []
        if decode_error:
            errors.append({"location": "$", "code": "javascript_not_utf8", "message": decode_error})
        tokens = self._javascript_tokens(text) if text else []
        function_scopes = self._javascript_function_scopes(tokens)
        candidates: list[dict[str, Any]] = []
        calls: list[dict[str, Any]] = []
        js_keywords = {"if", "for", "while", "switch", "catch", "function", "return", "typeof", "delete", "void", "new", "await", "yield"}
        for index, token in enumerate(tokens):
            function_name = self._javascript_scope(index, function_scopes)
            if token["kind"] in {"string", "unterminated_string"}:
                raw = token["value"]
                value = raw
                if token["kind"] == "string" and "\\" in raw:
                    try:
                        value = json.loads(f'"{raw.replace(chr(34), chr(92) + chr(34))}"')
                    except Exception:
                        value = raw
                if _looks_like_endpoint(value):
                    candidates.append({
                        "candidate_type": "endpoint_string", "value": value,
                        "line": token["line"], "column": token["column"],
                        "end_line": token["end_line"], "end_column": token["end_column"],
                        "module": source_url, "function": function_name,
                        "evidence_state": "static_candidate", "reachability": "unknown",
                    })
            if token["kind"] == "unterminated_string":
                errors.append({"location": f"{token['line']}:{token['column']}",
                               "code": "javascript_unterminated_string", "message": "string literal is unterminated"})
            if token["kind"] == "symbol" and _is_js_identifier(token["value"]) and token["value"] not in js_keywords:
                before = tokens[index - 1]["value"] if index > 0 and tokens[index - 1]["kind"] == "symbol" else ""
                receiver = tokens[index - 2]["value"] if before == "." and index > 1 and tokens[index - 2]["kind"] == "symbol" else before
                after = tokens[index + 1]["value"] if index + 1 < len(tokens) and tokens[index + 1]["kind"] == "symbol" else ""
                if after in {"(", "="} or before == ".":
                    calls.append({
                        "name": token["value"], "receiver": receiver, "next_token": after,
                        "line": token["line"], "column": token["column"],
                        "module": source_url, "function": function_name,
                        "evidence_state": "static_call_site", "reachability": "unknown",
                    })
        map_artifact = None
        mapped_count = 0
        positions: dict[tuple[int, int], tuple[int, int, int, int]] = {}
        source_map_document: dict[str, Any] = {}
        if source_map is not None:
            map_artifact = self.artifacts.put(task_id, source_map, media_type="application/json", name="javascript-source-map", source_run_id=source_run_id)
            try:
                source_map_document = json.loads(source_map)
                if not isinstance(source_map_document.get("sources"), list) or not isinstance(source_map_document.get("mappings"), str):
                    raise SrcStoreError("source_map_invalid", "sources and mappings are required")
                positions = self._source_map_positions(source_map_document["mappings"])
            except (json.JSONDecodeError, UnicodeDecodeError, SrcStoreError) as exc:
                errors.append({"location": "$.sourceMap", "code": "source_map_invalid", "message": str(exc)})
        if positions:
            for candidate in candidates:
                generated_line, generated_col = candidate["line"], candidate["column"]
                same_line = [pos for pos in positions if pos[0] == generated_line and pos[1] <= generated_col]
                if not same_line:
                    continue
                source_index, original_line, original_column, _name_index = positions[max(same_line)]
                if not 0 <= source_index < len(source_map_document["sources"]):
                    continue
                candidate["original"] = {
                    "source": source_map_document["sources"][source_index],
                    "line": original_line, "column": original_column,
                }
                sources_content = source_map_document.get("sourcesContent") or []
                if source_index < len(sources_content) and sources_content[source_index] is not None:
                    candidate["original"]["content_available"] = True
                    candidate["original"]["content_sha256"] = hashlib.sha256(sources_content[source_index].encode()).hexdigest()
                else:
                    candidate["original"]["content_available"] = False
                mapped_count += 1
        map_suffix = f":{hashlib.sha256(source_map).hexdigest()[:16]}" if source_map is not None else ""
        doc_id = f"javascript:{digest}{map_suffix}:{JS_EXTRACTOR}"
        body = {
            "document_type": "javascript", "extractor_version": JS_EXTRACTOR,
            "source_url": source_url, "source_run_id": source_run_id,
            "source_digest": digest, "artifact": artifact.model_dump(mode="json"),
            "source_map_artifact": map_artifact.model_dump(mode="json") if map_artifact else None,
            "candidates": candidates, "call_sites": calls,
            "source_map": {"mapped_candidates": mapped_count, "unmapped_candidates": len(candidates) - mapped_count},
            "errors": errors,
            "parser_limitations": [
                "lexical extraction does not prove syntax validity, execution reachability or source-to-sink flow",
                "template interpolation and dynamic property access require runtime evidence",
                "source map positions identify original locations, not original runtime behavior",
            ],
            "status": "invalid" if decode_error else "partial" if errors else "available",
            "ingested_at": utcnow().isoformat(),
        }
        with self.store.platform.transaction():
            record = self.store.put(
                task_id, "research_document", body,
                record_id=doc_id, parent_id=digest,
                natural_key=doc_id,
            )
            for index, candidate in enumerate(candidates):
                self.store.put(
                    task_id, "static_candidate", {"document_id": record.record_id, **candidate},
                    record_id=f"{record.record_id}:{index}", parent_id=record.record_id,
                    natural_key=f"{record.record_id}:{candidate['line']}:{candidate['column']}:{candidate['value']}",
                )
            self.store.remember(task_id, "javascript-ingest", key, {"sha256": digest}, {"record_id": record.record_id})
        return record

    def read_javascript(self, task_id: str, record_id: str) -> dict[str, Any]:
        record = self.store.require(task_id, "research_document", record_id)
        if record.payload.get("document_type") != "javascript":
            raise SrcStoreError("javascript_type_mismatch", "research document is not JavaScript")
        artifact = ArtifactRef.model_validate(record.payload["artifact"])
        result = {**_record_payload(record), "original": self.artifacts.read(artifact, expected_task_id=task_id)}
        if record.payload.get("source_map_artifact"):
            map_ref = ArtifactRef.model_validate(record.payload["source_map_artifact"])
            result["source_map_original"] = self.artifacts.read(map_ref, expected_task_id=task_id)
        return result

    def register_runtime_observation(
        self, task_id: str, *, page_url: str, action: dict[str, Any],
        exchange_ids: list[str], identity_ref: str, identity_revision: int,
        browser_generation: int, evidence_refs: list[str],
        dom_state: Optional[dict[str, Any]] = None,
        storage_refs: Optional[list[str]] = None,
        script_calls: Optional[list[dict[str, Any]]] = None,
        execution_signal: Optional[dict[str, Any]] = None,
        observed_at: Optional[datetime] = None,
    ) -> SrcRecord:
        if not exchange_ids and not evidence_refs:
            raise SrcStoreError("runtime_evidence_missing", "runtime observation requires request or page evidence")
        active_injection = str(action.get("type") or "").lower() == "evaluate"
        body = {
            "evidence_type": "Observation", "evidence_layer": "runtime_observed",
            "page_url": page_url, "action": action, "exchange_ids": exchange_ids,
            "identity_ref": identity_ref, "identity_revision": identity_revision,
            "browser_generation": browser_generation, "evidence_refs": evidence_refs,
            "dom_state": dom_state or {}, "storage_refs": storage_refs or [],
            "script_calls": script_calls or [], "execution_signal": execution_signal or {},
            "execution_provenance": "active_injection" if active_injection else "page_observation",
            "target_page_execution_assessment": "not_target_execution" if active_injection else "not_determined",
            "observed_at": (observed_at or utcnow()).isoformat(),
            "upgrades_static_candidate": False,
        }
        return self.store.put(task_id, "runtime_observation", body, natural_key=self.store.digest(body))

    def _ref_exists(self, task_id: str, ref: str) -> bool:
        text = str(ref or "").strip()
        if not text:
            return False
        with self.store.platform.lock:
            row = self.store.platform.conn.execute(
                "SELECT 1 FROM src_records WHERE task_id=? AND record_id=? LIMIT 1",
                (task_id, text),
            ).fetchone()
        if row is not None:
            return True
        if len(text) == 64 and all(ch in "0123456789abcdef" for ch in text):
            return (self.artifacts.task_root(task_id) / text).is_file()
        return False

    def register_dataflow(
        self, task_id: str, *, source: dict[str, Any], transforms: list[dict[str, Any]],
        sink: dict[str, Any], static_refs: list[str], runtime_refs: list[str],
        normal_control_refs: list[str], frame_origin: str = "",
        required_interaction: str = "", target_version: str = "",
        assessment: Optional[dict[str, Any]] = None,
    ) -> SrcRecord:
        if not source.get("location") or not sink.get("location"):
            raise SrcStoreError("dataflow_location_missing", "source and sink require original file locations")
        runtime_verified = bool(runtime_refs) and all(self._ref_exists(task_id, ref) for ref in runtime_refs)
        control_verified = bool(normal_control_refs) and all(self._ref_exists(task_id, ref) for ref in normal_control_refs)
        status = (
            str(assessment.get("status"))
            if isinstance(assessment, dict) and assessment.get("status") and runtime_verified
            else "validated" if runtime_verified and control_verified
            else "candidate"
        )
        body = {
            "evidence_type": "Observation", "source": source, "transforms": transforms,
            "sink": sink, "static_refs": static_refs, "runtime_refs": runtime_refs,
            "normal_control_refs": normal_control_refs, "frame_origin": frame_origin,
            "required_interaction": required_interaction, "target_version": target_version,
            "status": status, "validated_claim": "dataflow_observed" if status == "validated" else "not_validated",
            "assessment": dict(assessment or {}),
            "created_at": utcnow().isoformat(),
        }
        return self.store.put(task_id, "dataflow", body, natural_key=self.store.digest(body))

    def register_page_action(
        self, task_id: str, *, exchange: HttpExchange | dict[str, Any] | SrcRecord,
        action: dict[str, Any], before_state: dict[str, Any], after_state: dict[str, Any],
        object_refs: Optional[dict[str, Any]] = None,
    ) -> SrcRecord:
        exchange_data = self._exchange_dict(exchange)
        if str(exchange_data.get("task_id")) != task_id:
            raise SrcStoreError("task_binding_invalid", "exchange belongs to another Task")
        if not exchange_data.get("observed_url"):
            raise SrcStoreError("runtime_target_unknown", "page action requires an actually observed request URL")
        body = {
            "evidence_type": "Observation", "action": action,
            "exchange_id": exchange_data.get("exchange_id"),
            "method": exchange_data.get("method"), "observed_url": exchange_data["observed_url"],
            "identity_ref": exchange_data.get("context_id"),
            "identity_revision": exchange_data.get("identity_revision"),
            "tenant": (object_refs or {}).get("tenant"),
            "object_refs": object_refs or {}, "before_state": before_state, "after_state": after_state,
            "observed_at": exchange_data.get("started_at"),
        }
        with self.store.platform.transaction():
            record = self.store.put(task_id, "page_action", body, natural_key=self.store.digest(body))
            for occurrence, parameter in enumerate(self._request_parameters(exchange_data)):
                if parameter.get("name") is None:
                    continue
                path = urlsplit(exchange_data["observed_url"]).path
                parameter_body = {
                    "action_id": record.record_id, "exchange_id": exchange_data.get("exchange_id"),
                    "method": exchange_data.get("method"), "path": path,
                    "name": parameter["name"], "location": parameter["in"], "value": parameter.get("value"),
                    "occurrence": occurrence,
                    "identity_ref": exchange_data.get("context_id"),
                    "tenant": (object_refs or {}).get("tenant"),
                    "identity_revision": exchange_data.get("identity_revision"),
                    "source": "runtime_request",
                }
                key = self.store.digest({
                    "action": record.record_id, "method": parameter_body["method"], "path": path,
                    "name": parameter["name"], "in": parameter["in"], "occurrence": occurrence,
                    "identity": parameter_body["identity_ref"], "tenant": parameter_body["tenant"],
                })
                self.store.put(task_id, "action_parameter", parameter_body,
                               record_id=key, parent_id=record.record_id, natural_key=key)
        return record

    def save_business_model(
        self, task_id: str, payload: dict[str, Any], *, model_id: str = "",
        base_revision: Optional[int] = None,
    ) -> SrcRecord:
        collections = ("roles", "accounts", "tenants", "object_types", "ownership", "state_transitions", "sensitive_actions", "constraints")
        for key in collections:
            rows = payload.get(key) or []
            if not isinstance(rows, list):
                raise SrcStoreError("business_model_invalid", f"{key} must be a list")
            for index, row in enumerate(rows):
                if not isinstance(row, dict) or row.get("provenance") not in PROVENANCE_KINDS:
                    raise SrcStoreError("business_provenance_invalid", f"{key}[{index}] requires a valid provenance")
                if row["provenance"] != "model_inference" and not row.get("source_refs"):
                    raise SrcStoreError("business_source_missing", f"{key}[{index}] requires source refs")
        return self._versioned(
            task_id, "business_model", model_id,
            {**payload, "updated_at": utcnow().isoformat(), "coverage_claim": "not_claimed"},
            base_revision=base_revision,
        )

    def add_permission_question(
        self, task_id: str, *, question: str, actor: dict[str, Any],
        object_context: dict[str, Any], action: dict[str, Any],
        expected_boundary: str, evidence_refs: list[str],
        missing_comparisons: list[dict[str, Any]], source_refs: list[str],
        question_id: str = "", base_revision: Optional[int] = None,
    ) -> SrcRecord:
        body = {
            "evidence_type": "Step", "question": question, "actor": actor,
            "object_context": object_context, "action": action,
            "expected_boundary": expected_boundary, "evidence_refs": evidence_refs,
            "missing_comparisons": missing_comparisons, "source_refs": source_refs,
            "updated_at": utcnow().isoformat(), "http_200_is_not_proof": True,
        }
        return self._versioned(task_id, "research_question", question_id, body, base_revision=base_revision)

    def research_backlog(self, task_id: str) -> list[dict[str, Any]]:
        questions = self.store.all(task_id, "research_question")
        all_records = self.store.all(task_id)
        result = []
        for question in questions:
            refs = set(question.payload.get("evidence_refs") or [])
            related = [row for row in all_records if row.record_id in refs or row.parent_id in refs]
            states = {row.kind for row in related}
            if states & {"review", "review_record"}:
                state = "reviewed"
            elif states & {"finding", "report"}:
                state = "finding"
            elif states & {"dead_end", "deadend"}:
                state = "excluded"
            elif question.payload.get("missing_comparisons"):
                state = "blocked"
            elif states & {"step", "tool_call", "exchange", "http_exchange", "experiment"}:
                state = "attempted"
            elif states & {"observation", "runtime_observation", "exchange", "http_exchange"}:
                state = "observed"
            else:
                state = "pending"
            result.append({
                **_record_payload(question), "derived_state": state,
                "state_evidence_refs": [row.record_id for row in related],
                "asset_fully_tested": False,
            })
        return result

    def research_history(
        self, task_id: str, *, run_id: Optional[str] = None,
        asset_id: Optional[str] = None, limit: int = 100, offset: int = 0,
    ) -> dict[str, Any]:
        relevant = {
            "step", "observation", "dead_end", "fact", "tool_call", "run_error",
            "report_version", "runtime_observation", "page_action", "experiment",
            "dataflow", "http_exchange", "exchange",
        }
        rows = []
        for record in self.store.all(task_id):
            if record.kind not in relevant:
                continue
            if run_id and record.parent_id != run_id and record.payload.get("source_run_id") != run_id:
                continue
            if asset_id:
                payload_refs = {
                    str(record.payload.get("asset_id") or ""),
                    str((record.payload.get("object_refs") or {}).get("asset_id") or ""),
                    *(str(item) for item in (record.payload.get("asset_ids") or [])),
                }
                if asset_id not in payload_refs and asset_id not in self.store._json(record.payload):
                    continue
            rows.append(_record_payload(record))
        rows.sort(key=lambda row: (str(row.get("observed_at") or row.get("source", {}).get("occurred_at") or ""), row["record_id"]))
        start = max(0, int(offset))
        page_limit = max(1, int(limit))
        selected = rows[start:start + page_limit]
        next_offset = start + page_limit if start + page_limit < len(rows) else None
        return {
            "items": selected,
            "total": len(rows),
            "returned": len(selected),
            "next_offset": next_offset,
            "complete": next_offset is None,
            "watermarks": self.store.watermarks(task_id),
            "cross_run_facts_are_local": False,
        }

    def record_experiment(
        self, task_id: str, *, hypotheses: list[dict[str, Any]],
        changed_variables: list[dict[str, Any]], baseline_exchange_ids: list[str],
        control_exchange_ids: list[str], experiment_exchange_ids: list[str],
        observation_refs: list[str], limitations: list[str],
        target_version: str, identity_refs: list[str],
        assessment: Optional[dict[str, Any]] = None,
    ) -> SrcRecord:
        gaps: list[str] = []
        has_evidence = (
            bool(observation_refs) and bool(baseline_exchange_ids) and bool(experiment_exchange_ids)
            and all(self._ref_exists(task_id, ref) for ref in [*observation_refs, *baseline_exchange_ids, *experiment_exchange_ids])
        )
        if not has_evidence:
            gaps.append("baseline, experiment and actual readable observations are required")
        if len(changed_variables) != 1:
            gaps.append("experiment changes multiple variables or does not declare a single variable")
        if not has_evidence:
            status = "inconclusive"
        elif isinstance(assessment, dict) and assessment.get("status"):
            status = str(assessment["status"])
        elif len(changed_variables) == 1:
            status = "differentiating"
        else:
            status = "observed_multi_variable"
        body = {
            "evidence_type": "Observation", "hypotheses": hypotheses,
            "changed_variables": changed_variables,
            "baseline_exchange_ids": baseline_exchange_ids,
            "control_exchange_ids": control_exchange_ids,
            "experiment_exchange_ids": experiment_exchange_ids,
            "observation_refs": observation_refs, "limitations": limitations,
            "target_version": target_version, "identity_refs": identity_refs,
            "status": status, "gaps": gaps,
            "assessment": dict(assessment or {}),
            "created_at": utcnow().isoformat(), "difference_is_not_vulnerability": True,
        }
        return self.store.put(task_id, "experiment", body, natural_key=self.store.digest(body))

    def record_capability_chain(
        self, task_id: str, *, links: list[dict[str, Any]],
        connections: list[dict[str, Any]], target_version: str,
        assessment: Optional[dict[str, Any]] = None,
    ) -> SrcRecord:
        gaps = []
        for index, link in enumerate(links):
            fact_refs = list(link.get("fact_step_refs") or [])
            ev_refs = list(link.get("evidence_refs") or [])
            if not fact_refs or not ev_refs or not all(self._ref_exists(task_id, ref) for ref in ev_refs):
                gaps.append({"link": index, "missing": "Fact/Step and actual readable evidence references"})
            if not link.get("identity_state"):
                gaps.append({"link": index, "missing": "identity and state preconditions"})
        if len(links) > 1 and not connections:
            gaps.append({"connection": 0, "missing": "multi-link capability chain requires verified connections"})
        for index, connection in enumerate(connections):
            conn_refs = list(connection.get("evidence_refs") or [])
            if not conn_refs or not all(self._ref_exists(task_id, ref) for ref in conn_refs):
                gaps.append({"connection": index, "missing": "end-to-end connection has not been verified"})
        body = {
            "evidence_type": "Observation", "links": links, "connections": connections,
            "target_version": target_version,
            "status": "validated" if not gaps and links else "incomplete",
            "gaps": gaps, "assessment": dict(assessment or {}),
            "created_at": utcnow().isoformat(),
            "node_semantics": "references_only_not_a_graph_primitive",
        }
        return self.store.put(task_id, "capability_chain", body, natural_key=self.store.digest(body))

    def record_precondition_trial(
        self, task_id: str, *, base_conditions: dict[str, Any],
        removed_or_simplified: dict[str, Any], result_exchange_ids: list[str],
        evidence_refs: list[str], remaining_conditions: dict[str, Any],
        outcome: str, target_version: str,
    ) -> SrcRecord:
        if outcome not in {"reproduced", "not_reproduced", "inconclusive"}:
            raise SrcStoreError("precondition_outcome_invalid", "outcome must be reproduced, not_reproduced or inconclusive")
        if outcome != "inconclusive" and (not result_exchange_ids or not evidence_refs):
            raise SrcStoreError("precondition_evidence_missing", "a conclusive precondition trial requires execution and evidence")
        if outcome == "not_reproduced":
            conclusion = "required_under_current_target_version"
        elif outcome == "reproduced":
            conclusion = "not_required_under_current_target_version"
        else:
            conclusion = "unknown"
        body = {
            "evidence_type": "Observation", "base_conditions": base_conditions,
            "removed_or_simplified": removed_or_simplified,
            "result_exchange_ids": result_exchange_ids, "evidence_refs": evidence_refs,
            "remaining_conditions": remaining_conditions, "outcome": outcome,
            "conclusion": conclusion, "target_version": target_version,
            "created_at": utcnow().isoformat(), "conclusion_is_environment_bound": True,
        }
        return self.store.put(task_id, "precondition_trial", body, natural_key=self.store.digest(body))

    def precondition_summary(self, task_id: str) -> dict[str, Any]:
        trials = self.store.all(task_id, "precondition_trial")
        required = []
        optional = []
        unknown = []
        for trial in trials:
            changed = trial.payload["removed_or_simplified"]
            row = {"change": changed, "target_version": trial.payload["target_version"],
                   "evidence_refs": trial.payload["evidence_refs"], "trial_id": trial.record_id}
            if trial.payload["conclusion"] == "required_under_current_target_version":
                required.append(row)
            elif trial.payload["conclusion"] == "not_required_under_current_target_version":
                optional.append(row)
            else:
                unknown.append(row)
        return {
            "required_with_evidence": required, "not_required_with_evidence": optional,
            "unknown": unknown, "target_versions": sorted({row.payload["target_version"] for row in trials}),
            "speculation_is_not_evidence": True,
        }


__all__ = ["SrcResearchService", "OPENAPI_EXTRACTOR", "JS_EXTRACTOR"]
