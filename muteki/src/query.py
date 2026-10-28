from __future__ import annotations

import csv
import hashlib
import io
import ipaddress
import json
import re
import zipfile
from datetime import datetime, timezone
from typing import Any, Callable, Iterable, Optional

from muteki.platform.command_handlers.cursor import assert_cursor_scope, encode_cursor, load_cursor_key
from muteki.platform.contracts.base import new_id, utcnow

from .assets import SrcAssetService
from .contracts import ArtifactRef
from .store import SrcStoreError

QUERY_VERSION = "src-asset-query.v1"
FIELDS = frozenset({
    "type", "company", "program", "task_id", "run_id", "domain", "ip", "cidr",
    "protocol", "port", "url", "method", "param_location", "param_name",
    "technology", "tag", "status", "source", "observed_after", "observed_before",
    "ingested_after", "ingested_before", "freshness", "kind", "text",
})
NUMERIC_FIELDS = frozenset({"port", "freshness"})
TIME_FIELDS = frozenset({"observed_after", "observed_before", "ingested_after", "ingested_before"})
TOKEN = re.compile(r"\s*(>=|<=|!=|[()=~<>]|\"(?:\\.|[^\"])*\"|'(?:\\.|[^'])*'|[^\s()=~<>!]+)")


def _tokenize(expression: str) -> list[str]:
    tokens = []
    position = 0
    while position < len(expression):
        match = TOKEN.match(expression, position)
        if not match:
            if expression[position:].strip():
                raise SrcStoreError("query_invalid", "invalid query expression", detail={"position": position})
            break
        tokens.append(match.group(1))
        position = match.end()
    return tokens


def _unquote(value: str) -> str:
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {"'", '"'}:
        if value[0] == '"':
            try:
                return json.loads(value)
            except ValueError as exc:
                raise SrcStoreError("query_invalid", "invalid quoted value") from exc
        return value[1:-1].replace("\\'", "'").replace("\\\\", "\\")
    return value


class _Parser:
    def __init__(self, expression: str) -> None:
        self.tokens = _tokenize(expression)
        self.position = 0

    def peek(self) -> str:
        return self.tokens[self.position].upper() if self.position < len(self.tokens) else ""

    def take(self) -> str:
        if self.position >= len(self.tokens):
            raise SrcStoreError("query_invalid", "unexpected end of expression", detail={"position": self.position})
        value = self.tokens[self.position]
        self.position += 1
        return value

    def parse(self) -> Any:
        if not self.tokens:
            return {"op": "true"}
        result = self.or_expr()
        if self.position != len(self.tokens):
            raise SrcStoreError("query_invalid", f"unexpected token {self.take()}")
        return result

    def or_expr(self) -> Any:
        left = self.and_expr()
        while self.peek() == "OR":
            self.take()
            left = {"op": "or", "items": [left, self.and_expr()]}
        return left

    def and_expr(self) -> Any:
        left = self.not_expr()
        while self.position < len(self.tokens) and self.peek() not in {"OR", ")"}:
            if self.peek() == "AND":
                self.take()
            left = {"op": "and", "items": [left, self.not_expr()]}
        return left

    def not_expr(self) -> Any:
        if self.peek() in {"NOT", "!"}:
            self.take()
            return {"op": "not", "item": self.not_expr()}
        return self.primary()

    def primary(self) -> Any:
        if self.peek() == "(":
            self.take()
            value = self.or_expr()
            if self.take() != ")":
                raise SrcStoreError("query_invalid", "missing closing parenthesis")
            return value
        first = self.take()
        if first in {"(", ")"}:
            raise SrcStoreError("query_invalid", "unexpected parenthesis")
        if self.peek() in {"=", "~", "!=", ">", "<", ">=", "<="}:
            operator = self.take()
            value = _unquote(self.take())
            return self.condition(first, operator, value)
        raw = _unquote(first)
        if raw.startswith("-") and len(raw) > 1:
            term = raw[1:]
            if ":" in term:
                field, value = term.split(":", 1)
                return {"op": "not", "item": self.condition(field, "=", value, implicit=True)}
            return {"op": "not", "item": self.condition("text", "~", term, implicit=True)}
        if ":" in raw:
            field, value = raw.split(":", 1)
            if value in {"contains", "exact", "exclude", "prefix"} and self.position < len(self.tokens):
                operator = {"contains": "~", "exact": "=", "exclude": "!=", "prefix": "prefix"}[value]
                return self.condition(field, operator, _unquote(self.take()))
            return self.condition(field, "=", value, implicit=True)
        return self.condition("text", "~", raw, implicit=True)

    def condition(self, field: str, operator: str, value: Optional[str], implicit: bool = False) -> dict[str, Any]:
        field = field.lower()
        if field not in FIELDS:
            raise SrcStoreError("query_field_unknown", f"unknown query field {field}", detail={"field": field})
        if value is None or value == "":
            raise SrcStoreError("query_value_missing", f"missing value for {field}", detail={"field": field})
        if field in NUMERIC_FIELDS:
            try:
                float(value)
            except ValueError as exc:
                raise SrcStoreError("query_value_invalid", f"{field} requires a number", detail={"field": field}) from exc
        if field in TIME_FIELDS:
            try:
                datetime.fromisoformat(value.replace("Z", "+00:00"))
            except ValueError as exc:
                raise SrcStoreError("query_value_invalid", f"{field} requires an ISO timestamp", detail={"field": field}) from exc
        return {"op": "condition", "field": field, "operator": operator, "value": value, "implicit": implicit}


def parse_query(expression: str) -> dict[str, Any]:
    return _Parser(expression).parse()


def _fields(node: Any) -> list[str]:
    if not isinstance(node, dict):
        return []
    if node.get("op") == "condition":
        return [node["field"]]
    if "items" in node:
        return list(dict.fromkeys(field for item in node["items"] for field in _fields(item)))
    if "item" in node:
        return _fields(node["item"])
    return []


def _time(value: Any) -> Optional[datetime]:
    if not value:
        return None
    result = datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    return result.replace(tzinfo=timezone.utc) if result.tzinfo is None else result


def _url(identity: dict[str, Any]) -> str:
    if not identity.get("host"):
        return ""
    host = identity["host"]
    if ":" in host and not host.startswith("["):
        host = f"[{host}]"
    default = 80 if identity.get("scheme") == "http" else 443
    port = "" if identity.get("port") in {None, default} else f":{identity['port']}"
    return f"{identity.get('scheme', 'https')}://{host}{port}{identity.get('path') or '/'}" + (f"?{identity['query']}" if identity.get("query") else "")


def _values(asset: dict[str, Any], field: str) -> list[Any]:
    identity = asset.get("identity") or {}
    profile = asset.get("current_profile") or {}
    observations = asset.get("observations") or []
    judgements = asset.get("judgements") or {}
    direct = {
        "type": asset.get("type"), "kind": asset.get("type"),
        "company": (judgements.get("company_attribution") or {}).get("value"),
        "program": (judgements.get("program_coverage") or {}).get("value"),
        "task_id": asset.get("task_id"), "domain": identity.get("name") or identity.get("host"),
        "ip": identity.get("address") or identity.get("host"), "cidr": identity.get("address"),
        "protocol": identity.get("protocol") or identity.get("scheme"), "port": identity.get("port"),
        "url": _url(identity), "method": identity.get("method"),
        "param_location": identity.get("location"), "param_name": identity.get("name"),
        "technology": profile.get("technology") or profile.get("fingerprint"),
        "tag": asset.get("tags") or [], "status": asset.get("lifecycle"),
        "observed_after": asset.get("latest_observed"), "observed_before": asset.get("latest_observed"),
        "ingested_after": asset.get("updated_at"), "ingested_before": asset.get("updated_at"),
        "freshness": asset.get("latest_observed"),
    }
    if field == "run_id":
        return [row.get("source_run_id") for row in observations if row.get("source_run_id")]
    if field == "source":
        return [value for row in observations for value in (row.get("source_kind"), row.get("source_id")) if value]
    value = direct.get(field)
    return list(value) if isinstance(value, list) else [value]


def _compare(actual: Any, operator: str, expected: str, field: str) -> bool:
    if actual is None:
        return False
    if field == "cidr":
        try:
            target_net = ipaddress.ip_network(expected, strict=False)
            text_actual = str(actual)
            if "/" in text_actual:
                actual_net = ipaddress.ip_network(text_actual, strict=False)
                in_net = actual_net.version == target_net.version and actual_net.subnet_of(target_net)
            else:
                in_net = ipaddress.ip_address(text_actual) in target_net
            return (not in_net) if operator == "!=" else in_net
        except ValueError:
            return False
    if field in NUMERIC_FIELDS:
        if field == "freshness":
            parsed = _time(actual)
            if parsed is None:
                return False
            actual = (datetime.now(timezone.utc) - parsed).total_seconds() / 86400
        left, right = float(actual), float(expected)
        return {"=": left == right, "!=": left != right, ">": left > right,
                "<": left < right, ">=": left >= right, "<=": left <= right}.get(operator, False)
    if field in TIME_FIELDS:
        left, right = _time(actual), _time(expected)
        if left is None or right is None:
            return False
        if field.endswith("_after"):
            return left >= right if operator in {"=", ">", ">="} else left < right
        return left <= right if operator in {"=", "<", "<="} else left > right
    case_sensitive = field in {"param_name", "url"}
    left = str(actual) if case_sensitive else str(actual).casefold()
    right = expected if case_sensitive else expected.casefold()
    if operator == "=":
        return left == right
    if operator == "!=":
        return left != right
    if operator == "~":
        return right in left
    if operator == "prefix":
        return left.startswith(right)
    if operator in {">", "<", ">=", "<="}:
        return {">": left > right, "<": left < right, ">=": left >= right, "<=": left <= right}[operator]
    return False


def _matches(node: dict[str, Any], asset: dict[str, Any]) -> bool:
    op = node.get("op")
    if op == "true":
        return True
    if op == "and":
        return all(_matches(item, asset) for item in node["items"])
    if op == "or":
        return any(_matches(item, asset) for item in node["items"])
    if op == "not":
        return not _matches(node["item"], asset)
    if op == "condition":
        values = _values(asset, node["field"])
        if node["field"] == "text":
            values.append(json.dumps(asset, ensure_ascii=False, sort_keys=True))
        if node["operator"] == "!=":
            return not any(_compare(value, "=", node["value"], node["field"]) for value in values)
        return any(_compare(value, node["operator"], node["value"], node["field"]) for value in values)
    return False


def _artifact_refs(value: Any) -> Iterable[dict[str, Any]]:
    if isinstance(value, dict):
        if {"sha256", "task_id", "size"}.issubset(value):
            yield value
        for child in value.values():
            yield from _artifact_refs(child)
    elif isinstance(value, list):
        for child in value:
            yield from _artifact_refs(child)


class SrcQueryService:
    def __init__(self, src: Any, assets: Optional[SrcAssetService] = None) -> None:
        self.src = src
        self.store = src.store
        self.artifacts = src.artifacts
        self.assets = assets or SrcAssetService(src)

    def _publications(self, task_id: str) -> list[dict[str, Any]]:
        publications: list[dict[str, Any]] = []
        for reference in self.store.all(task_id, "asset_reference"):
            if reference.payload.get("revoked_at"):
                continue
            publication = self.store.require(
                reference.payload["source_task_id"], "asset_publication", reference.payload["publication_id"],
            )
            if not publication.payload.get("revoked_at"):
                publications.append(publication.payload)
        return publications

    def _accessible(self, task_id: str) -> dict[str, Any]:
        result: dict[str, Any] = {task_id: None}
        for publication in self._publications(task_id):
            result[publication["task_id"]] = publication
        return result

    def _detached_asset_ids(self, task_id: str) -> set[str]:
        return {
            row.record_id
            for row in self.store.all(task_id, "task_asset_link")
            if row.payload.get("state") == "detached"
        }

    def _asset_rows(self, task_id: str) -> list[dict[str, Any]]:
        rows = []
        detached = self._detached_asset_ids(task_id)
        for record in self.store.all(task_id, "asset"):
            if record.record_id in detached:
                continue
            if record.payload.get("lifecycle") not in {"destroyed", "merged"}:
                observations = [row.payload for row in self.store.all(task_id, "asset_observation") if row.payload.get("asset_id") == record.record_id]
                rows.append({**record.payload, "task_id": task_id, "observations": observations,
                             "access": "task", "updated_at": record.updated_at.isoformat()})
        for publication in self._publications(task_id):
            owner = publication["task_id"]
            for item in publication.get("assets") or []:
                entity = item["entity"]
                rows.append({**entity, "task_id": owner, "observations": item.get("observations") or [],
                             "access": "publication", "publication_id": publication["publication_id"],
                             "publication_version": publication["version"],
                             "updated_at": publication["published_at"]})
        rows.sort(key=lambda row: (row.get("latest_observed") or "", row["asset_id"]), reverse=True)
        return rows

    def _relation_rows(self, task_id: str) -> list[dict[str, Any]]:
        rows = [row.payload for row in self.store.all(task_id, "asset_relation")]
        for publication in self._publications(task_id):
            rows.extend(publication.get("relations") or [])
        return rows

    def _filter(self, task_id: str, expression: str) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        ast = parse_query(expression)
        rows = [row for row in self._asset_rows(task_id) if _matches(ast, row)]
        return rows, {"ast": ast, "matched_fields": _fields(ast)}

    def query(self, task_id: str, expression: str = "", *, limit: int = 50, cursor: Optional[str] = None) -> dict[str, Any]:
        self.src.binding(task_id)
        limit = max(1, min(int(limit), 1000))
        key = load_cursor_key(self.src.platform.db_path)
        if cursor:
            snapshot_id = cursor.split(":", 1)[0]
            snapshot = self.store.require(task_id, "query_snapshot", snapshot_id)
            scope = f"{task_id}:{snapshot_id}"
            offset = assert_cursor_scope(key, cursor.split(":", 1)[1], "src-assets", task_id, scope=scope)
            rows = snapshot.payload["items"]
            for row in rows:
                pub_id = row.get("publication_id")
                if pub_id:
                    self.assets.published(task_id, str(pub_id))
            query_expression = snapshot.payload["expression"]
            semantics = snapshot.payload["semantics"]
            watermarks = snapshot.payload["watermarks"]
            partial_reason = snapshot.payload.get("partial_reason")
        else:
            rows, semantics = self._filter(task_id, expression)
            query_expression = expression
            snapshot_id = new_id("squerysnap")
            scope = f"{task_id}:{snapshot_id}"
            watermarks = self.store.watermarks(task_id)
            partial_reason = "index_lagging" if any(row.get("state") in {"failed", "pending"} for row in watermarks) else None
            snapshot = self.store.put(task_id, "query_snapshot", {
                "snapshot_id": snapshot_id, "expression": query_expression,
                "semantic_version": QUERY_VERSION, "semantics": semantics,
                "items": rows, "watermarks": watermarks, "partial_reason": partial_reason,
                "created_at": utcnow().isoformat(),
            }, record_id=snapshot_id)
            offset = 0
        total = len(rows)
        selected = rows[offset:offset + limit]
        next_offset = offset + len(selected)
        next_cursor = f"{snapshot_id}:" + encode_cursor(key, "src-assets", task_id, next_offset, scope=scope) if next_offset < total else None
        return {
            "items": selected, "total": total, "returned": len(selected),
            "next_cursor": next_cursor, "snapshot_id": snapshot_id,
            "semantic_version": QUERY_VERSION, "expression": query_expression,
            "semantics": semantics, "watermarks": watermarks,
            "partial_reason": partial_reason,
        }

    def save_query(self, task_id: str, payload: dict[str, Any], *, actor_id: str, query_id: str = "", base_revision: Optional[int] = None) -> dict[str, Any]:
        query_id = query_id or new_id("ssavedquery")
        existing = self.store.get(task_id, "saved_query", query_id)
        if existing is not None and base_revision is not None and existing.revision != base_revision:
            raise SrcStoreError("version_conflict", "saved query revision is stale")
        expression = str(payload.get("expression") or "")
        parse_query(expression)
        record = {
            **(existing.payload if existing else {}), **payload,
            "query_id": query_id, "expression": expression,
            "semantic_version": QUERY_VERSION, "author": actor_id,
            "updated_at": utcnow().isoformat(), "deleted_at": None,
        }
        result = self.store.put(task_id, "saved_query", record, record_id=query_id,
                                expected_revision=base_revision)
        return result.payload

    def read_query(self, task_id: str, query_id: str, revision: Optional[int] = None) -> dict[str, Any]:
        return self.store.require(task_id, "saved_query", query_id, revision=revision).payload

    def delete_query(self, task_id: str, query_id: str, *, base_revision: int) -> dict[str, Any]:
        record = self.store.require(task_id, "saved_query", query_id)
        payload = {**record.payload, "deleted_at": utcnow().isoformat()}
        return self.store.put(task_id, "saved_query", payload, record_id=query_id, expected_revision=base_revision).payload

    def save_vocabulary(self, task_id: str, payload: dict[str, Any], *, actor_id: str, term_id: str = "", base_revision: Optional[int] = None) -> dict[str, Any]:
        term_id = term_id or new_id("svocab")
        if not str(payload.get("term") or "").strip():
            raise SrcStoreError("vocabulary_invalid", "term is required")
        record = {
            **(self.store.get(task_id, "target_vocabulary", term_id).payload if self.store.get(task_id, "target_vocabulary", term_id) else {}),
            **payload, "term_id": term_id, "author": actor_id,
            "status": payload.get("status") or "candidate", "updated_at": utcnow().isoformat(),
        }
        return self.store.put(task_id, "target_vocabulary", record, record_id=term_id, expected_revision=base_revision).payload

    def save_tag(self, task_id: str, payload: dict[str, Any], *, actor_id: str, tag_id: str = "", base_revision: Optional[int] = None) -> dict[str, Any]:
        tag_id = tag_id or new_id("stag")
        existing = self.store.get(task_id, "asset_tag", tag_id)
        if existing is not None and base_revision is not None and existing.revision != base_revision:
            raise SrcStoreError("version_conflict", "tag revision is stale")
        if not str(payload.get("name") or (existing.payload.get("name") if existing else "") or "").strip():
            raise SrcStoreError("tag_invalid", "tag name is required")
        record = {**(existing.payload if existing else {}), **payload,
                  "tag_id": tag_id, "author": actor_id, "updated_at": utcnow().isoformat()}
        return self.store.put(task_id, "asset_tag", record, record_id=tag_id, expected_revision=base_revision).payload

    def assign_tags(self, task_id: str, asset_ids: list[str], tag_ids: list[str], *, actor_id: str, detach: bool = False) -> dict[str, Any]:
        tags = [self.store.require(task_id, "asset_tag", tag_id).payload for tag_id in tag_ids]
        names = [row["name"] for row in tags if not row.get("deleted_at")]
        results = []
        for asset_id in asset_ids:
            try:
                record = self.store.require(task_id, "asset", asset_id)
                current = set(record.payload.get("tags") or [])
                values = (current - set(names)) if detach else (current | set(names))
                payload = {**record.payload, "tags": sorted(values), "annotation_actor": actor_id}
                result = self.store.put(task_id, "asset", payload, record_id=asset_id,
                                        expected_revision=record.revision, natural_key=record.natural_key)
                results.append({"asset_id": asset_id, "ok": True, "tags": result.payload["tags"]})
            except SrcStoreError as exc:
                results.append({"asset_id": asset_id, "ok": False, "code": exc.code, "message": exc.message})
        return {"items": results}

    def save_note(self, task_id: str, asset_id: str, payload: dict[str, Any], *, actor_id: str, note_id: str = "", base_revision: Optional[int] = None) -> dict[str, Any]:
        self.store.require(task_id, "asset", asset_id)
        note_id = note_id or new_id("snote")
        existing = self.store.get(task_id, "asset_note", note_id)
        if existing is not None and base_revision is not None and existing.revision != base_revision:
            raise SrcStoreError("version_conflict", "note revision is stale")
        record = {**(existing.payload if existing else {}), **payload,
                  "note_id": note_id, "asset_id": asset_id, "author": actor_id,
                  "origin": payload.get("origin") or "human", "updated_at": utcnow().isoformat()}
        return self.store.put(task_id, "asset_note", record, record_id=note_id, parent_id=asset_id, expected_revision=base_revision).payload

    def full_text(self, task_id: str, needle: str, *, kinds: Iterable[str] = (), limit: int = 100, cursor: Optional[str] = None) -> dict[str, Any]:
        self.src.binding(task_id)
        if not needle:
            raise SrcStoreError("query_value_missing", "full text query cannot be empty")
        limit = max(1, min(int(limit), 1000))
        key = load_cursor_key(self.src.platform.db_path)
        if cursor:
            snapshot_id = cursor.split(":", 1)[0]
            snapshot = self.store.require(task_id, "fulltext_snapshot", snapshot_id)
            offset = assert_cursor_scope(key, cursor.split(":", 1)[1], "src-fulltext", task_id, scope=snapshot_id)
            results = snapshot.payload["items"]
        else:
            snapshot_id = new_id("sfulltext")
            offset = 0
            allowed_kinds = set(kinds) or {"asset_observation", "run_event", "fact", "observation", "step", "dead_end", "tool_call", "import_item", "import_exchange", "report"}
            results = []
            lowered = needle.casefold()
            for owner, publication in self._accessible(task_id).items():
                if publication is not None:
                    records = [{"task_id": owner, "kind": "asset_observation", "record_id": row["observation_id"], "payload": row}
                               for item in publication.get("assets") or [] for row in item.get("observations") or []]
                else:
                    with self.store.platform.lock:
                        records = [dict(row) for row in self.store.platform.conn.execute(
                            "SELECT task_id,kind,record_id,payload FROM src_records WHERE task_id=?", (owner,),
                        ).fetchall()]
                for record in records:
                    if record["kind"] not in allowed_kinds:
                        continue
                    payload = json.loads(record["payload"]) if isinstance(record["payload"], str) else record["payload"]
                    text = json.dumps(payload, ensure_ascii=False, sort_keys=True)
                    artifacts = []
                    for raw_ref in _artifact_refs(payload):
                        try:
                            ref = ArtifactRef.model_validate(raw_ref)
                            if ref.task_id not in {owner, task_id}:
                                raise SrcStoreError("task_binding_invalid", "artifact is outside the selected publication scope")
                            data = self.artifacts.read(ref, expected_task_id=ref.task_id)
                            if ref.media_type.startswith(("text/", "application/json", "application/xml", "application/x-http", "application/har")):
                                text += "\n" + data.decode("utf-8", errors="replace")
                            artifacts.append(ref.model_dump(mode="json"))
                        except (ValueError, SrcStoreError):
                            artifacts.append({**raw_ref, "unavailable": True})
                    index = text.casefold().find(lowered)
                    if index < 0:
                        continue
                    results.append({
                        "task_id": owner, "kind": record["kind"], "record_id": record["record_id"],
                        "snippet": text[max(0, index - 100):index + len(needle) + 100],
                        "match_start": index, "artifact_refs": artifacts,
                        "full_record": payload,
                    })
            self.store.put(task_id, "fulltext_snapshot", {
                "snapshot_id": snapshot_id, "needle": needle, "items": results,
                "created_at": utcnow().isoformat(),
            }, record_id=snapshot_id)
        total = len(results)
        selected = results[offset:offset + limit]
        next_offset = offset + len(selected)
        next_cursor = f"{snapshot_id}:" + encode_cursor(key, "src-fulltext", task_id, next_offset, scope=snapshot_id) if next_offset < total else None
        return {
            "items": selected, "total": total, "returned": len(selected),
            "next_cursor": next_cursor, "snapshot_id": snapshot_id,
            "complete": next_cursor is None, "watermarks": self.store.watermarks(task_id),
        }

    def read_artifact(self, task_id: str, ref_data: dict[str, Any]) -> bytes:
        ref = ArtifactRef.model_validate(ref_data)
        if ref.task_id == task_id:
            return self.artifacts.read(ref, expected_task_id=task_id)
        allowed_hashes: set[str] = set()
        for publication in self._publications(task_id):
            if publication.get("task_id") != ref.task_id:
                continue
            for pub_ref in publication.get("artifact_refs") or []:
                if isinstance(pub_ref, dict) and pub_ref.get("sha256"):
                    allowed_hashes.add(str(pub_ref["sha256"]))
            for asset_item in publication.get("assets") or []:
                for obs in asset_item.get("observations") or []:
                    for obs_ref in obs.get("artifact_refs") or []:
                        if isinstance(obs_ref, dict) and obs_ref.get("sha256"):
                            allowed_hashes.add(str(obs_ref["sha256"]))
        if ref.sha256 not in allowed_hashes:
            raise SrcStoreError("task_binding_invalid", "artifact is outside the readable publication scope")
        return self.artifacts.read(ref, expected_task_id=ref.task_id)

    def pivot(self, task_id: str, asset_id: str, *, relation_type: str = "", limit: int = 50, cursor: int = 0) -> dict[str, Any]:
        assets = {row["asset_id"]: row for row in self._asset_rows(task_id)}
        rows = []
        for relation in self._relation_rows(task_id):
            if asset_id not in {relation.get("from_asset_id"), relation.get("to_asset_id")}:
                continue
            if relation_type and relation.get("relation_type") != relation_type:
                continue
            neighbor = relation["to_asset_id"] if relation["from_asset_id"] == asset_id else relation["from_asset_id"]
            if neighbor not in assets:
                continue
            rows.append({"relation": relation, "asset": assets[neighbor]})
        rows.sort(key=lambda row: row["relation"]["relation_id"])
        start = max(0, int(cursor))
        selected = rows[start:start + max(1, min(int(limit), 500))]
        return {
            "items": selected, "returned": len(selected), "total": len(rows),
            "next_cursor": start + len(selected) if start + len(selected) < len(rows) else None,
            "unexpanded": max(0, len(rows) - start - len(selected)),
        }

    def export(self, task_id: str, expression: str = "", *, export_format: str = "json", selected_ids: Optional[list[str]] = None, include_artifacts: bool = True, partial: bool = False) -> tuple[bytes, str, dict[str, Any]]:
        rows, semantics = self._filter(task_id, expression)
        if selected_ids is not None:
            allowed = set(selected_ids)
            rows = [row for row in rows if row["asset_id"] in allowed]
        snapshot_id = new_id("sexportsnap")
        manifest: dict[str, Any] = {
            "schema": "muteki.src.asset-export.v1", "snapshot_id": snapshot_id,
            "semantic_version": QUERY_VERSION, "expression": expression,
            "semantics": semantics, "task_id": task_id,
            "created_at": utcnow().isoformat(), "expected_entries": 0,
            "actual_entries": 0, "omissions": [], "attachments": [],
        }
        if export_format == "csv":
            output = io.StringIO(newline="")
            fields = ["asset_id", "type", "identity", "current_profile", "first_observed", "latest_observed", "last_success", "last_failure", "sources", "task_id", "access"]
            writer = csv.DictWriter(output, fieldnames=fields)
            writer.writeheader()
            for row in rows:
                writer.writerow({
                    "asset_id": row["asset_id"], "type": row["type"],
                    "identity": json.dumps(row.get("identity"), ensure_ascii=False),
                    "current_profile": json.dumps(row.get("current_profile"), ensure_ascii=False),
                    "first_observed": row.get("first_observed"), "latest_observed": row.get("latest_observed"),
                    "last_success": row.get("last_success"), "last_failure": row.get("last_failure"),
                    "sources": json.dumps(row.get("observations"), ensure_ascii=False),
                    "task_id": row.get("task_id"), "access": row.get("access"),
                })
            content = output.getvalue().encode("utf-8")
            media_type = "text/csv; charset=utf-8"
            manifest["roundtrip"] = "CSV preserves the selected projection only; it does not restore the graph or evidence"
        elif export_format == "jsonl":
            lines_out = [json.dumps(row, ensure_ascii=False) for row in rows]
            content = ("\n".join(lines_out) + ("\n" if lines_out else "")).encode("utf-8")
            media_type = "application/jsonl"
        elif export_format == "markdown":
            parts = [f"# SRC 资产导出 ({len(rows)} 项)\n"]
            for row in rows:
                parts.append(f"## {row.get('display_name') or row['asset_id']}")
                parts.append(f"- asset_id: {row['asset_id']}")
                parts.append(f"- type: {row['type']}")
                parts.append(f"- identity: {json.dumps(row.get('identity'), ensure_ascii=False)}")
                profile = row.get("current_profile") or {}
                if profile:
                    parts.append("- profile:")
                    for key, value in profile.items():
                        parts.append(f"  - {key}: {json.dumps(value, ensure_ascii=False)}")
                parts.append("")
            content = "\n".join(parts).encode("utf-8")
            media_type = "text/markdown; charset=utf-8"
        elif export_format in {"json", "zip"}:
            document = {
                "schema": "muteki.src.assets.v1", "snapshot_id": snapshot_id,
                "semantic_version": QUERY_VERSION, "assets": rows,
                "relations": [relation for relation in self._relation_rows(task_id)
                              if {relation.get("from_asset_id"), relation.get("to_asset_id")}.issubset({row["asset_id"] for row in rows})],
            }
            content = json.dumps(document, ensure_ascii=False, indent=2).encode("utf-8")
            media_type = "application/json"
        else:
            raise SrcStoreError("export_format_invalid", "asset export supports csv, json, jsonl, markdown or zip")
        if export_format != "zip":
            manifest["attachments"].append(self._attachment("data." + {"csv": "csv", "jsonl": "jsonl", "markdown": "md"}.get(export_format, "json"), content, media_type, "derived", "asset-projection"))
            manifest["expected_entries"] = manifest["actual_entries"] = len(manifest["attachments"])
            manifest_bytes = json.dumps(manifest, ensure_ascii=False, indent=2).encode("utf-8")
            return content, media_type, manifest
        output = io.BytesIO()
        attachments = list(manifest["attachments"])
        with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            archive.writestr("assets.json", content)
            attachments.append(self._attachment("assets.json", content, "application/json", "derived", "asset-projection"))
            if include_artifacts:
                seen: dict[str, str] = {}
                for row in rows:
                    for observation in row.get("observations") or []:
                        for raw_ref in _artifact_refs(observation.get("artifact_refs") or []):
                            try:
                                ref = ArtifactRef.model_validate(raw_ref)
                                data = self.artifacts.read(ref, expected_task_id=ref.task_id)
                                digest = hashlib.sha256(data).hexdigest()
                                if digest != ref.sha256 or len(data) != ref.size:
                                    raise SrcStoreError("artifact_hash_mismatch", "artifact changed during export")
                                path = f"evidence/{digest}"
                                previous = seen.get(path)
                                if previous and previous != digest:
                                    raise SrcStoreError("artifact_hash_mismatch", "export path collision")
                                if previous == digest:
                                    continue
                                seen[path] = digest
                                archive.writestr(path, data)
                                attachments.append(self._attachment(path, data, ref.media_type, "original", "selected-evidence", source=ref.model_dump(mode="json")))
                            except (ValueError, SrcStoreError) as exc:
                                if not partial:
                                    raise
                                manifest["omissions"].append({
                                    "ref": raw_ref,
                                    "code": getattr(exc, "code", "artifact_invalid"),
                                    "message": getattr(exc, "message", str(exc)),
                                })
            manifest["partial"] = bool(manifest["omissions"])
            manifest["attachments"] = attachments
            manifest["expected_entries"] = len(attachments) + len(manifest["omissions"]) + 1
            manifest["actual_entries"] = len(attachments) + 1
            manifest_data = json.dumps(manifest, ensure_ascii=False, indent=2).encode("utf-8")
            archive.writestr("manifest.json", manifest_data)
        return output.getvalue(), "application/zip", manifest

    @staticmethod
    def _attachment(path: str, data: bytes, media_type: str, classification: str, purpose: str, source: Optional[dict[str, Any]] = None) -> dict[str, Any]:
        return {
            "path": path, "media_type": media_type, "classification": classification,
            "purpose": purpose, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest(),
            "source": source,
        }


__all__ = ["FIELDS", "QUERY_VERSION", "SrcQueryService", "parse_query"]
