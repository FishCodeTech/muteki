from __future__ import annotations

import base64
import hashlib
import json
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Awaitable, Callable, Optional

import httpx

from .artifacts import SrcArtifactStore
from .connector_cloud import aws_ec2_page, fetch_repository
from .connector_files import parse_source
from .connector_manifest import (
    SEARCH_MANIFESTS, CapabilityManifest, ConnectorBatch, ConnectorError,
    UpstreamPage, unavailable,
)
from .store import SrcStore


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _file_manifest(connector_id: str, schema: str, types: dict[str, str]) -> CapabilityManifest:
    return CapabilityManifest(
        connector_id, "source-file.v1", schema, ("import_file", "read"),
        types, {}, {"kind": "complete_file", "reliable_full": True},
        {"required": False, "reference_fields": []},
        {"actual_cost": "not_applicable", "retry": "committed_items_not_repeated"},
        {"kind": "file", "executes_hooks": False},
        {"raw_source": True, "stable_source_link": True, "screenshots": False},
    )


def _manifests() -> dict[str, CapabilityManifest]:
    rows = dict(SEARCH_MANIFESTS)
    rows.update({
        "bbot": _file_manifest("bbot", "bbot.typed-events.v3+jsonl", {"DNS_NAME": "domain", "IP_ADDRESS": "ip", "OPEN_TCP_PORT": "service", "URL": "http_endpoint", "FINDING": "external_observation"}),
        "reconftw": _file_manifest("reconftw", "reconftw.assets+jsonl", {"domain": "domain", "ip": "ip", "url": "http_endpoint", "service": "service"}),
        "scopesentry": _file_manifest("scopesentry", "scopesentry.export+json", {"domain": "domain", "service": "service", "app": "app_candidate", "miniprogram": "miniprogram_candidate"}),
        "bbrf": _file_manifest("bbrf", "bbrf.export+jsonl", {"domain": "domain", "ip": "ip", "url": "http_endpoint"}),
        "burp": _file_manifest("burp", "burp.xml+har", {"http_exchange": "http_exchange"}),
        "caido": _file_manifest("caido", "caido.har+jsonl", {"http_exchange": "http_exchange"}),
        "cli": _file_manifest("cli", "cli.json+jsonl+csv", {"asset": "source_observation"}),
        "cloud_inventory": _file_manifest("cloud_inventory", "cloud.inventory+json+csv", {"resource": "cloud_resource"}),
    })
    rows["scopesentry"] = CapabilityManifest(
        **{**rows["scopesentry"].__dict__, "capabilities": ("import_file", "read", "query_when_endpoint_configured")}
    )
    for provider in ("github", "gitlab"):
        rows[provider] = CapabilityManifest(
            provider, "repository-read.v1", f"{provider}.repository+commit",
            ("read_repository",), {"repository": "repository"}, {},
            {"kind": "single_document", "reliable_full": True},
            {"required": False, "reference_fields": ["token_ref"], "permission": "read_repository"},
            {"actual_cost": "unknown_unless_upstream_receipt", "retry": "submitted_unknown_not_replayed"},
            {"kind": "https", "self_hosted_base_url_supported": True},
            {"raw_response": True, "stable_source_link": True, "screenshots": False},
        )
    rows["aws_ec2"] = CapabilityManifest(
        "aws_ec2", "aws-cli.describe-instances.v1", "aws.ec2.describe-instances.response+json",
        ("query", "read", "paginate"), {"instance": "cloud_resource"}, {},
        {"kind": "next_token", "reliable_full": "when_all_pages_succeed"},
        {"required": False, "reference_fields": ["credential_ref"], "permission": "ec2:DescribeInstances"},
        {"actual_cost": "unknown_unless_upstream_receipt", "retry": "submitted_unknown_not_replayed"},
        {"kind": "aws_cli", "required_executable": "aws", "hooks": False},
        {"raw_response": True, "stable_source_link": True, "screenshots": False},
    )
    rows["external_event"] = _file_manifest("external_event", "external.source-event.v1", {"event": "external_observation"})
    rows["external_event"] = CapabilityManifest(
        **{**rows["external_event"].__dict__, "capabilities": ("receive_event", "idempotent_events", "read")}
    )
    return rows


MANIFESTS = _manifests()
_SECRET_KEYS = {
    "api_key", "apikey", "token", "secret", "secret_key", "password",
    "private_key", "credential", "credentials", "access_token", "refresh_token",
    "client_secret", "token_secret", "secret_access_key", "aws_secret_access_key",
    "authorization", "proxy_authorization",
}


def _reject_secret_config(value: Any, path: str = "") -> None:
    if isinstance(value, dict):
        for key, child in value.items():
            child_path = f"{path}.{key}" if path else str(key)
            if str(key).lower() in _SECRET_KEYS:
                raise ConnectorError("validation_error", f"{child_path} must be a credential reference, not raw secret material")
            _reject_secret_config(child, child_path)
    elif isinstance(value, list):
        for index, child in enumerate(value):
            _reject_secret_config(child, f"{path}[{index}]")


class ConnectorService:
    def __init__(
        self, store: SrcStore, artifacts: SrcArtifactStore, *,
        credential_resolver: Optional[Callable[[str, str, str], Awaitable[Any]]] = None,
        batch_sink: Optional[Callable[[dict[str, Any]], Awaitable[dict[str, Any]]]] = None,
        authorization_hook: Optional[Callable[[str, str, dict[str, Any]], Awaitable[None]]] = None,
    ) -> None:
        self.store = store
        self.artifacts = artifacts
        self.credential_resolver = credential_resolver
        self.batch_sink = batch_sink
        self.authorization_hook = authorization_hook

    def manifests(self) -> list[dict[str, Any]]:
        return [MANIFESTS[key].as_dict() for key in sorted(MANIFESTS)]

    def manifest(self, connector_id: str) -> CapabilityManifest:
        try:
            return MANIFESTS[connector_id]
        except KeyError as exc:
            raise unavailable(f"unknown connector {connector_id!r}") from exc

    def save_config(
        self, task_id: str, connector_id: str, config: dict[str, Any], *,
        created_by: str, executable_version: str = "",
    ) -> dict[str, Any]:
        manifest = self.manifest(connector_id)
        if not isinstance(config, dict):
            raise ConnectorError("validation_error", "connector config must be an object")
        _reject_secret_config(config)
        if connector_id == "aws_ec2":
            import shutil
            if not shutil.which("aws"):
                raise unavailable("AWS CLI is required by this configuration")
        if connector_id == "scopesentry" and config.get("query_enabled") and not config.get("endpoints"):
            raise unavailable("ScopeSentry online query requires explicit verified endpoints")
        previous = self.store.all(task_id, "tool_config")
        versions = [row.payload.get("version", 0) for row in previous if row.payload.get("connector_id") == connector_id]
        version = max(versions, default=0) + 1
        config_artifact = self.artifacts.put(
            task_id, json.dumps(config, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8"),
            media_type="application/json", name=f"{connector_id}-config-v{version}.json",
        )
        payload = {
            "connector_id": connector_id, "version": version,
            "tool_id": connector_id, "executable_version": executable_version or manifest.version,
            "config_schema_version": 1, "config": config,
            "config_artifact": config_artifact.model_dump(mode="json"),
            "created_by": created_by, "created_at": _now(), "revoked_at": None,
            "manifest": manifest.as_dict(),
        }
        record = self.store.put(
            task_id, "tool_config", payload,
            natural_key=f"{connector_id}:{version}", parent_id=connector_id,
        )
        return record.model_dump(mode="json")

    def config(self, task_id: str, connector_id: str, version: Optional[int] = None) -> dict[str, Any]:
        if version is not None:
            record = self.store.by_natural_key(task_id, "tool_config", f"{connector_id}:{version}")
            if record is None:
                raise ConnectorError("artifact_missing", "connector config version is unavailable")
        else:
            candidates = [row for row in self.store.all(task_id, "tool_config") if row.payload.get("connector_id") == connector_id]
            if not candidates:
                return {
                    "connector_id": connector_id, "version": 0, "config": {},
                    "manifest": self.manifest(connector_id).as_dict(), "revoked_at": None,
                }
            record = max(candidates, key=lambda row: int(row.payload.get("version") or 0))
        if record.payload.get("revoked_at"):
            raise ConnectorError("capability_unavailable", "connector config has been revoked")
        return record.payload

    def revoke_config(self, task_id: str, connector_id: str, version: int, *, base_revision: int) -> dict[str, Any]:
        record = self.store.by_natural_key(task_id, "tool_config", f"{connector_id}:{version}")
        if record is None:
            raise ConnectorError("artifact_missing", "connector config version is unavailable")
        if record.revision != base_revision:
            raise ConnectorError("version_conflict", "connector config revision is stale")
        return self.store.put(
            task_id, "tool_config", {**record.payload, "revoked_at": _now()},
            record_id=record.record_id, expected_revision=base_revision,
        ).model_dump(mode="json")

    async def resolve_secret(self, config: dict[str, Any], field: str, *, required: bool) -> Any:
        reference = str(config.get(field) or "").strip()
        if not reference:
            if required:
                raise unavailable(f"credential reference {field} is required")
            return None
        if self.credential_resolver is None:
            raise unavailable(f"no credential resolver is configured for {field}")
        value = await self.credential_resolver(str(config.get("task_id") or ""), reference, "connector")
        if value is None or value == "":
            raise unavailable(f"credential reference {field} could not be resolved")
        return value

    async def connector_environment(self, config: dict[str, Any]) -> dict[str, str]:
        environment = dict(os.environ)
        credentials = await self.resolve_secret(config, "credential_ref", required=False)
        if isinstance(credentials, str):
            try:
                credentials = json.loads(credentials)
            except json.JSONDecodeError as exc:
                raise unavailable("AWS credential resolver must return an environment object or JSON object") from exc
        if isinstance(credentials, dict):
            environment.update({str(key): str(value) for key, value in credentials.items()})
        return environment

    async def _authorize(self, task_id: str, operation: str, details: dict[str, Any]) -> None:
        if self.authorization_hook is not None:
            await self.authorization_hook(task_id, operation, details)

    def artifact_for_source(
        self, data: bytes, *, name: str, request: dict[str, Any], task_id: str,
        media_type: str = "application/json",
    ) -> dict[str, Any]:
        ref = self.artifacts.put(task_id, data, media_type=media_type, name=name)
        return {**ref.model_dump(mode="json"), "request": request, "fetched_at": _now()}

    async def fetch_json(self, task_id: str, url: str, *, headers: dict[str, str]) -> tuple[Any, bytes, dict[str, Any]]:
        request = {"method": "GET", "url": url, "header_names": sorted(headers)}
        async with httpx.AsyncClient(timeout=30.0, follow_redirects=False) as client:
            response = await client.get(url, headers=headers)
        raw = response.content
        request["response_headers"] = {
            key: value for key, value in response.headers.items()
            if key.lower().startswith(("x-ratelimit-", "x-quota-")) or key.lower() == "retry-after"
        }
        if response.status_code < 200 or response.status_code >= 300:
            ref = self.artifact_for_source(raw, name="connector-error-response", request=request, task_id=task_id)
            raise ConnectorError("upstream_http_error", f"upstream returned HTTP {response.status_code}", detail={"status": response.status_code, "artifact": ref})
        return self._response_json(task_id, response, request), raw, request

    def _response_json(self, task_id: str, response: httpx.Response, request: dict[str, Any]) -> Any:
        try:
            document = response.json()
        except (json.JSONDecodeError, UnicodeDecodeError) as exc:
            ref = self.artifact_for_source(response.content, name="connector-invalid-response", request=request, task_id=task_id)
            raise ConnectorError("upstream_response_invalid", "upstream response is not valid JSON", detail={"artifact": ref}) from exc
        if response.status_code < 200 or response.status_code >= 300:
            ref = self.artifact_for_source(response.content, name="connector-error-response", request=request, task_id=task_id)
            raise ConnectorError("upstream_http_error", f"upstream returned HTTP {response.status_code}", detail={"status": response.status_code, "artifact": ref})
        return document

    def _business_error(self, task_id: str, response: httpx.Response, request: dict[str, Any], message: str) -> ConnectorError:
        ref = self.artifact_for_source(response.content, name="connector-business-error", request=request, task_id=task_id)
        return ConnectorError("upstream_response_error", message, detail={"artifact": ref})

    async def import_file(
        self, task_id: str, connector_id: str, data: bytes, *, filename: str,
        created_by: str, config_version: Optional[int] = None,
        idempotency_key: str = "", field_mapping: Optional[dict[str, str]] = None,
    ) -> dict[str, Any]:
        manifest = self.manifest(connector_id)
        if "import_file" not in manifest.capabilities:
            raise unavailable(f"{connector_id} does not support file import")
        if not isinstance(data, bytes):
            raise ConnectorError("validation_error", "source file must be bytes")
        config = self.config(task_id, connector_id, config_version)
        digest = hashlib.sha256(data).hexdigest()
        key = idempotency_key or self.store.digest({"sha256": digest, "connector": connector_id, "config": config["version"], "mapping": field_mapping})
        request = {"filename": filename, "digest": digest, "mapping": field_mapping, "config_version": config["version"]}
        prior = self.store.idempotent(task_id, "connector.import", key, request)
        if prior is not None:
            return {**prior, "deduplicated": True}
        existing = self._uncommitted_batch(task_id, f"batch-{key[:24]}")
        if existing is not None:
            return existing
        await self._authorize(task_id, "connector.import", {"connector_id": connector_id, "config_version": config["version"]})
        parsed = parse_source(connector_id, data, filename=filename, field_mapping=field_mapping)
        source_ref = self.artifact_for_source(data, name=Path(filename).name or "source.bin", request={"filename": filename, "created_by": created_by}, task_id=task_id, media_type=parsed.media_type)
        batch = ConnectorBatch(
            batch_id=f"batch-{key[:24]}", connector_id=connector_id,
            config_version=int(config["version"]), task_id=task_id,
            items=parsed.items, source_refs=[source_ref], errors=parsed.errors,
            unknown_fields=parsed.unknown_fields, field_mapping=parsed.mapping,
            acquisition={"filename": filename, "complete": parsed.complete, "truncated": False},
            cost={"state": "not_applicable"},
            external_state="partial" if parsed.errors and parsed.items else "failed" if parsed.errors else "completed",
        )
        return await self._commit_batch(task_id, "connector.import", key, request, batch)

    def _uncommitted_batch(self, task_id: str, batch_id: str) -> Optional[dict[str, Any]]:
        record = self.store.get(task_id, "connector_batch", batch_id)
        if record is None:
            return None
        return {
            **record.payload, "external_state": "outcome_unknown",
            "recovery": "本地批次已存在但最终回执未提交；先核对入库水位，不重放上游调用",
        }

    async def _commit_batch(self, task_id: str, operation: str, key: str, request: Any, batch: ConnectorBatch) -> dict[str, Any]:
        result = batch.as_dict()
        self.store.put(task_id, "connector_batch", result, record_id=batch.batch_id, natural_key=batch.batch_id)
        if self.batch_sink is not None and batch.items:
            try:
                sink_result = await self.batch_sink(result)
                result["ingestion"] = sink_result
                result["counts"].update(sink_result.get("counts") or {})
            except Exception as exc:
                result["external_state"] = "outcome_unknown"
                result["errors"] = [*result["errors"], {
                    "code": "ingestion_commit_unknown", "message": f"{type(exc).__name__}: {exc}",
                    "recovery": "核对批次与入库水位，禁止重放已完成的上游调用",
                }]
            self.store.put(task_id, "connector_batch", result, record_id=batch.batch_id, expected_revision=1)
        self.store.remember(task_id, operation, key, request, result)
        return result

    async def _call(
        self, task_id: str, connector_id: str, config: dict[str, Any],
        operation: str, params: dict[str, Any],
        producer: Callable[[], Awaitable[UpstreamPage]], *,
        acquisition_id: str = "",
    ) -> dict[str, Any]:
        key = self.store.digest({
            "acquisition_id": acquisition_id, "connector": connector_id,
            "version": config["version"], "operation": operation, "params": params,
        })
        prior = self.store.idempotent(task_id, "connector.call", key, {"params": params, "config_version": config["version"], "acquisition_id": acquisition_id})
        if prior is not None:
            return {**prior, "deduplicated": True}
        call_record = self.store.get(task_id, "connector_call", key)
        if call_record is not None:
            return {
                "items": [], "external_state": "outcome_unknown",
                "error": {"code": "outcome_unknown", "message": "previous upstream call has no completed local receipt; reconcile before retry"},
                "call": call_record.payload,
            }
        await self._authorize(task_id, "connector.call", {"connector_id": connector_id, "operation": operation, "params": params})
        self.store.put(task_id, "connector_call", {
            "connector_id": connector_id, "config_version": config["version"],
            "operation": operation, "params": params, "state": "dispatched", "started_at": _now(),
        }, record_id=key)
        try:
            page = await producer()
            source_ref = self.artifact_for_source(page.raw, name=f"{connector_id}-response", request=page.request, task_id=task_id)
            result = {
                "items": page.items, "source_refs": [source_ref], "next_token": page.next_token,
                "total": page.total, "quota": page.quota, "external_state": "completed",
            }
            self.store.put(task_id, "connector_call", {
                "connector_id": connector_id, "config_version": config["version"],
                "operation": operation, "params": params, "state": "completed",
                "started_at": call_record.payload["started_at"] if call_record else _now(), "finished_at": _now(),
                "source_ref": source_ref, "next_token": page.next_token,
            }, record_id=key, expected_revision=1)
            self.store.remember(task_id, "connector.call", key, {"params": params, "config_version": config["version"], "acquisition_id": acquisition_id}, result)
            return result
        except Exception as exc:
            safe_message = f"{type(exc).__name__}: upstream transport failed" if isinstance(exc, httpx.HTTPError) else str(exc)
            self.store.put(task_id, "connector_call", {
                "connector_id": connector_id, "config_version": config["version"],
                "operation": operation, "params": params, "state": "failed",
                "finished_at": _now(), "error_code": getattr(exc, "code", type(exc).__name__),
                "error": safe_message,
            }, record_id=key, expected_revision=1)
            if isinstance(exc, ConnectorError):
                raise
            raise ConnectorError("upstream_call_failed", safe_message) from exc

    async def _http_page(
        self, task_id: str, connector_id: str, config: dict[str, Any],
        params: dict[str, Any],
    ) -> UpstreamPage:
        timeout = float(config.get("timeout") or 30)
        async with httpx.AsyncClient(timeout=timeout, follow_redirects=False) as client:
            if connector_id == "fofa":
                email = await self.resolve_secret(config, "email_ref", required=True)
                key = await self.resolve_secret(config, "key_ref", required=True)
                query = str(params["query"])
                page = int(params["page"])
                size = int(params["size"])
                fields = ",".join(config.get("fields") or ["host", "ip", "port", "protocol", "title", "domain", "icp"])
                request_params = {"email": email, "key": key, "qbase64": base64.b64encode(query.encode()).decode(), "page": page, "size": size, "fields": fields}
                response = await client.get("https://fofa.info/api/v1/search/all", params=request_params)
                request = {"method": "GET", "url": "https://fofa.info/api/v1/search/all", "params": {"qbase64": request_params["qbase64"], "page": page, "size": size, "fields": fields}, "credential_refs": ["email_ref", "key_ref"]}
                document = self._response_json(task_id, response, request)
                if document.get("error"):
                    raise self._business_error(task_id, response, request, str(document.get("errmsg") or "FOFA query failed"))
                items = [{"input_key": f"fofa:{page}:{index}:{self.store.digest(row)}", "mapped": dict(zip(fields.split(","), row)) if isinstance(row, list) else row, "raw": row} for index, row in enumerate(document.get("results") or [])]
                total = int(document.get("size") or 0)
                next_token = page + 1 if items and page * size < total else None
                return UpstreamPage(items, response.content, request, next_token, total, quota={key: value for key, value in document.items() if key not in {"results"}})
            if connector_id == "quake":
                token = await self.resolve_secret(config, "token_ref", required=True)
                start = int(params["start"])
                size = int(params["size"])
                body = {"query": str(params["query"]), "start": start, "size": size}
                response = await client.post("https://quake.360.net/api/v3/search/quake_service", json=body, headers={"X-QuakeToken": str(token)})
                request = {"method": "POST", "url": "https://quake.360.net/api/v3/search/quake_service", "body": body, "credential_refs": ["token_ref"]}
                document = self._response_json(task_id, response, request)
                if int(document.get("code") or 0) != 0:
                    raise self._business_error(task_id, response, request, str(document.get("message") or "Quake query failed"))
                rows = document.get("data") or []
                items = [{"input_key": f"quake:{start + index}:{row.get('id') or self.store.digest(row)}", "mapped": row, "raw": row} for index, row in enumerate(rows)]
                pagination = (document.get("meta") or {}).get("pagination") or {}
                total = int(pagination.get("total") or 0)
                next_token = start + len(rows) if rows and start + len(rows) < total else None
                return UpstreamPage(items, response.content, request, next_token, total, quota=pagination)
            if connector_id == "shodan":
                key = await self.resolve_secret(config, "key_ref", required=True)
                page = int(params["page"])
                query = str(params["query"])
                response = await client.get("https://api.shodan.io/shodan/host/search", params={"key": key, "query": query, "page": page})
                request = {"method": "GET", "url": "https://api.shodan.io/shodan/host/search", "params": {"query": query, "page": page}, "credential_refs": ["key_ref"]}
                document = self._response_json(task_id, response, request)
                if response.status_code != 200:
                    raise ConnectorError("upstream_http_error", str(document.get("error") or f"Shodan HTTP {response.status_code}"))
                rows = document.get("matches") or []
                items = [{"input_key": f"shodan:{row.get('ip_str')}:{row.get('port')}:{self.store.digest(row)}", "mapped": row, "raw": row} for row in rows]
                total = int(document.get("total") or 0)
                next_token = page + 1 if rows and page * 100 < total else None
                return UpstreamPage(items, response.content, request, next_token, total, quota={key: value for key, value in document.items() if key != "matches"})
            if connector_id == "zoomeye":
                key = await self.resolve_secret(config, "key_ref", required=True)
                page = int(params["page"])
                query = str(params["query"])
                response = await client.get("https://api.zoomeye.org/host/search", params={"query": query, "page": page}, headers={"API-KEY": str(key)})
                request = {"method": "GET", "url": "https://api.zoomeye.org/host/search", "params": {"query": query, "page": page}, "credential_refs": ["key_ref"]}
                document = self._response_json(task_id, response, request)
                if response.status_code != 200:
                    raise ConnectorError("upstream_http_error", str(document))
                rows = document.get("data") or []
                items = [{"input_key": f"zoomeye:{row.get('ip')}:{row.get('port')}:{self.store.digest(row)}", "mapped": row, "raw": row} for row in rows]
                total = int(document.get("total") or 0)
                next_token = page + 1 if rows and page * int(config.get("page_size") or 100) < total else None
                return UpstreamPage(items, response.content, request, next_token, total, quota={key: value for key, value in document.items() if key != "data"})
            if connector_id == "scopesentry":
                endpoints = config.get("endpoints") or {}
                asset_type = str(params.get("asset_type") or "domain")
                path = endpoints.get(asset_type)
                if not path:
                    raise unavailable(f"no verified ScopeSentry endpoint for {asset_type}")
                base = str(config.get("base_url") or "").rstrip("/")
                if not base:
                    raise unavailable("ScopeSentry base_url is required for online query")
                token = await self.resolve_secret(config, "token_ref", required=False)
                headers = {"Authorization": f"Bearer {token}"} if token else {}
                page = int(params["page"])
                size = int(params["size"])
                query_params = {"project_id": params.get("project_id"), "task_id": params.get("source_task_id"), "page": page, "size": size}
                response = await client.get(base + str(path), params=query_params, headers=headers)
                request = {"method": "GET", "url": base + str(path), "params": query_params, "header_names": sorted(headers)}
                document = self._response_json(task_id, response, request)
                if response.status_code != 200:
                    raise ConnectorError("upstream_http_error", f"ScopeSentry HTTP {response.status_code}")
                data_node = document.get("data") if isinstance(document, dict) else document
                rows = data_node.get("list") or data_node.get("records") or data_node.get("items") if isinstance(data_node, dict) else data_node
                rows = rows or []
                items = [{"input_key": f"scopesentry:{row.get('id') or self.store.digest(row)}", "mapped": row, "raw": row} for row in rows]
                total = int((data_node.get("total") if isinstance(data_node, dict) else document.get("total")) or 0)
                next_token = page + 1 if rows and page * size < total else None
                return UpstreamPage(items, response.content, request, next_token, total)
            if connector_id == "aws_ec2":
                return await aws_ec2_page(self, config, str(params.get("next_token") or ""))
        raise unavailable(f"online query is not supported for {connector_id}")

    async def query(
        self, task_id: str, connector_id: str, query: str, *,
        config_version: Optional[int] = None, max_pages: Optional[int] = None,
        max_items: Optional[int] = None, idempotency_key: str = "",
        extra: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        manifest = self.manifest(connector_id)
        config = self.config(task_id, connector_id, config_version)
        config = {**config, "config": {**config.get("config", {}), "task_id": task_id}}
        settings = config["config"]
        scopesentry_query = connector_id == "scopesentry" and settings.get("query_enabled") and settings.get("endpoints")
        if "query" not in manifest.capabilities and not scopesentry_query:
            raise unavailable(f"{connector_id} does not support online query with this configuration")
        from muteki.platform.contracts.base import new_id
        key = idempotency_key or new_id("sconnacq")
        request = {"query": query, "config_version": config["version"], "max_pages": max_pages, "max_items": max_items, "extra": extra}
        if idempotency_key:
            prior = self.store.idempotent(task_id, "connector.query", key, request)
            if prior is not None:
                return {**prior, "deduplicated": True}
            existing = self._uncommitted_batch(task_id, f"batch-{key[:24]}")
            if existing is not None:
                return existing
        pages_left = max(1, min(max_pages or int(settings.get("max_pages") or 10), 100))
        item_limit = max_items or int(settings.get("max_items") or 10000)
        items: list[dict[str, Any]] = []
        source_refs: list[dict[str, Any]] = []
        quota_reports: list[dict[str, Any]] = []
        errors: list[dict[str, Any]] = []
        next_token: Any = None
        total: Optional[int] = None
        page_number = 0
        external_state = "completed"
        while pages_left and len(items) < item_limit and (page_number == 0 or next_token is not None):
            page_number += 1
            if connector_id == "quake":
                params = {"query": query, "start": next_token or 0, "size": int(settings.get("page_size") or 100)}
            elif connector_id in {"aws_ec2"}:
                params = {"next_token": next_token or ""}
            elif connector_id == "scopesentry":
                params = {"query": query, "page": next_token or 1, "size": int(settings.get("page_size") or 100), **dict(extra or {})}
            else:
                params = {"query": query, "page": next_token or 1, "size": int(settings.get("page_size") or 100)}
            try:
                result = await self._call(
                    task_id, connector_id, config, "query_page", params,
                    lambda: self._http_page(task_id, connector_id, settings, params),
                    acquisition_id=key,
                )
            except Exception as exc:
                errors.append({"page": page_number, "code": getattr(exc, "code", "upstream_call_failed"), "message": str(exc)})
                external_state = "partial" if items else "failed"
                break
            if result.get("external_state") == "outcome_unknown":
                errors.append({"page": page_number, **result["error"]})
                external_state = "outcome_unknown"
                break
            items.extend(result["items"])
            source_refs.extend(result["source_refs"])
            if result.get("quota"):
                quota_reports.append(result["quota"])
            next_token = result.get("next_token")
            total = result.get("total") if result.get("total") is not None else total
            if result.get("deduplicated"):
                pages_left -= 1
                continue
            pages_left -= 1
        truncated = next_token is not None or len(items) > item_limit
        items = items[:item_limit]
        batch = ConnectorBatch(
            batch_id=f"batch-{key[:24]}", connector_id=connector_id,
            config_version=int(config["version"]), task_id=task_id,
            items=items, source_refs=source_refs, errors=errors,
            acquisition={"query": query, "pages": page_number, "total": total,
                         "next_token": next_token, "truncated": truncated,
                         "complete": not truncated and not errors},
            cost={"state": "unknown", "actual": None, "quota": quota_reports, "note": "上游未提供可核对实际费用"},
            external_state=external_state,
        )
        return await self._commit_batch(task_id, "connector.query", key, request, batch)

    async def repository(
        self, task_id: str, provider: str, *, config_version: Optional[int] = None,
        idempotency_key: str = "",
    ) -> dict[str, Any]:
        from muteki.platform.contracts.base import new_id
        if provider not in {"github", "gitlab"}:
            raise unavailable(f"unsupported repository provider {provider}")
        config_record = self.config(task_id, provider, config_version)
        config = {**config_record["config"], "task_id": task_id}
        key = idempotency_key or new_id("srepoacq")
        if idempotency_key:
            prior = self.store.idempotent(task_id, "connector.repository", key, config)
            if prior is not None:
                return {**prior, "deduplicated": True}
            existing = self._uncommitted_batch(task_id, f"batch-{key[:24]}")
            if existing is not None:
                return existing
            call = self.store.get(task_id, "connector_call", key)
            if call is not None:
                return {"external_state": "outcome_unknown", "call": call.payload, "items": []}
        await self._authorize(task_id, "connector.repository", {"provider": provider, "repository": config.get("repository")})
        self.store.put(task_id, "connector_call", {"state": "dispatched", "provider": provider, "started_at": _now()}, record_id=key)
        try:
            result = await fetch_repository(self, task_id, provider, config)
        except Exception as exc:
            self.store.put(task_id, "connector_call", {"state": "failed", "error": str(exc), "finished_at": _now()}, record_id=key, expected_revision=1)
            raise
        batch = ConnectorBatch(
            batch_id=f"batch-{key[:24]}", connector_id=provider,
            config_version=int(config_record["version"]), task_id=task_id,
            items=result["items"], source_refs=result["source_refs"],
            acquisition={"complete": True, "truncated": False},
            cost={"state": "unknown", "actual": None},
        )
        result = await self._commit_batch(task_id, "connector.repository", key, config, batch)
        self.store.put(task_id, "connector_call", {"state": "completed", "finished_at": _now(), "batch_id": batch.batch_id}, record_id=key, expected_revision=1)
        return result

    async def receive_external_event(
        self, task_id: str, *, source_id: str, event_id: str,
        source_version: str, payload: dict[str, Any], target_ref: str = "",
    ) -> dict[str, Any]:
        if not source_id or not event_id or not source_version:
            raise ConnectorError("validation_error", "source_id, event_id and source_version are required")
        raw = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        request = {"source_id": source_id, "event_id": event_id, "source_version": source_version, "target_ref": target_ref}
        prior = self.store.idempotent(task_id, "connector.external_event", event_id, request)
        if prior is not None:
            return {**prior, "deduplicated": True}
        existing = self._uncommitted_batch(task_id, f"batch-{self.store.digest(request)[:24]}")
        if existing is not None:
            return existing
        await self._authorize(task_id, "connector.external_event", request)
        ref = self.artifact_for_source(raw, name=f"{source_id}-{event_id}.json", request=request, task_id=task_id)
        item = {"input_key": f"{source_id}:{event_id}", "mapped": {"type": "external_observation", "target_ref": target_ref, "authorization_expanded": False}, "raw": payload}
        batch = ConnectorBatch(
            batch_id=f"batch-{self.store.digest(request)[:24]}", connector_id="external_event",
            config_version=0, task_id=task_id, items=[item], source_refs=[ref],
            acquisition={"source_id": source_id, "event_id": event_id, "source_version": source_version, "complete": True},
            cost={"state": "not_applicable"},
        )
        return await self._commit_batch(task_id, "connector.external_event", event_id, request, batch)


__all__ = ["ConnectorService", "MANIFESTS", "ConnectorError"]
