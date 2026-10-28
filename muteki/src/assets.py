from __future__ import annotations

import hashlib
import ipaddress
import json
from datetime import datetime, timezone
from typing import Any, Iterable, Optional
from urllib.parse import urlsplit

from muteki.platform.contracts.base import new_id, utcnow

from .contracts import ArtifactRef
from .store import SrcStoreError

KEY_VERSION = "asset-key.v1"
ASSET_TYPES = frozenset({
    "root_domain", "domain", "dns", "ip", "cidr", "port", "web_service",
    "non_web_service", "http_endpoint", "parameter", "app", "miniprogram",
    "cloud", "repository",
})
LIFECYCLE_ACTIVE = "active"


def _text(value: Any) -> str:
    return str(value if value is not None else "").strip()


def _time(value: Any) -> Optional[datetime]:
    if value in (None, ""):
        return None
    if isinstance(value, datetime):
        result = value
    elif isinstance(value, (int, float)) and not isinstance(value, bool):
        result = datetime.fromtimestamp(value, timezone.utc)
    else:
        result = datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    if result.tzinfo is None:
        result = result.replace(tzinfo=timezone.utc)
    return result.astimezone(timezone.utc)


def _iso(value: Any) -> Optional[str]:
    parsed = _time(value)
    return parsed.isoformat() if parsed is not None else None


def _host(value: Any) -> str:
    host = _text(value).rstrip(".").lower()
    if host.startswith("[") and host.endswith("]"):
        host = host[1:-1]
    try:
        return str(ipaddress.ip_address(host))
    except ValueError:
        return host.encode("idna").decode("ascii") if host else ""


def _endpoint_identity(value: Any) -> dict[str, Any]:
    if isinstance(value, dict) and value.get("scheme") and value.get("host") and not value.get("url"):
        scheme = _text(value["scheme"]).lower()
        host = _host(value["host"])
        if not host:
            raise SrcStoreError("asset_identity_invalid", "endpoint requires a complete site identity")
        port = int(value.get("port") or (80 if scheme == "http" else 443 if scheme == "https" else 0))
        return {
            "scheme": scheme, "host": host, "port": port,
            "path": _text(value.get("path")) or "/", "query": _text(value.get("query")),
        }
    raw = _text(value.get("url") if isinstance(value, dict) else value)
    parts = urlsplit(raw if "://" in raw else "https://" + raw)
    if not parts.hostname:
        raise SrcStoreError("asset_identity_invalid", "endpoint requires a complete site identity")
    scheme = parts.scheme.lower()
    port = parts.port or (80 if scheme == "http" else 443 if scheme == "https" else 0)
    return {
        "scheme": scheme, "host": _host(parts.hostname), "port": port,
        "path": parts.path or "/", "query": parts.query,
    }


def normalize_identity(asset_type: str, identity: dict[str, Any], source: Optional[dict[str, Any]] = None) -> dict[str, Any]:
    if asset_type not in ASSET_TYPES:
        raise SrcStoreError("asset_type_invalid", f"unsupported asset type {asset_type}")
    raw = dict(identity)
    data: dict[str, Any] = {}
    if asset_type in {"domain", "root_domain", "dns"}:
        name = _host(raw.get("name") or raw.get("domain") or raw.get("host"))
        if not name:
            raise SrcStoreError("asset_identity_invalid", "domain name is required")
        data["name"] = name
        if asset_type == "dns":
            data["record_type"] = _text(raw.get("record_type")).upper()
            data["value"] = _text(raw.get("value"))
            if not data["record_type"] or not data["value"]:
                raise SrcStoreError("asset_identity_invalid", "DNS record type and value are required")
    elif asset_type in {"ip", "cidr"}:
        value = _text(raw.get("address") or raw.get("cidr") or raw.get("ip"))
        try:
            parsed = ipaddress.ip_network(value, strict=False) if asset_type == "cidr" else ipaddress.ip_address(value)
        except ValueError as exc:
            raise SrcStoreError("asset_identity_invalid", "invalid IP/CIDR") from exc
        data["address"] = str(parsed)
        data["family"] = parsed.version
    elif asset_type in {"port", "web_service", "non_web_service"}:
        data["host"] = _host(raw.get("host"))
        data["port"] = int(raw.get("port") or 0)
        data["protocol"] = _text(raw.get("protocol") or ("tcp" if asset_type == "port" else "http")).lower()
        if not data["host"] or not 1 <= data["port"] <= 65535 or not data["protocol"]:
            raise SrcStoreError("asset_identity_invalid", "service requires host, protocol and valid port")
        if asset_type == "port":
            data["transport"] = _text(raw.get("transport") or "tcp").lower()
    elif asset_type == "http_endpoint":
        data.update(_endpoint_identity(raw.get("url") or raw))
        data["method"] = _text(raw.get("method") or "GET").upper()
    elif asset_type == "parameter":
        data.update(_endpoint_identity(raw.get("url") or raw.get("endpoint") or raw))
        data["method"] = _text(raw.get("method") or "GET").upper()
        data["location"] = _text(raw.get("location") or raw.get("param_location"))
        data["name"] = _text(raw.get("name") or raw.get("param_name"))
        if not data["location"] or not data["name"]:
            raise SrcStoreError("asset_identity_invalid", "parameter location and original name are required")
        for key in ("identity_ref", "tenant", "action"):
            data[key] = _text(raw.get(key))
    elif asset_type in {"app", "miniprogram"}:
        data["platform"] = _text(raw.get("platform")).lower()
        stable = _text(raw.get("stable_id") or raw.get("package_id") or raw.get("bundle_id") or raw.get("appid"))
        data["stable_id"] = stable
        if not data["platform"]:
            raise SrcStoreError("asset_identity_invalid", "mobile product platform is required")
        if not stable:
            marker = source or {}
            data["candidate_key"] = hashlib.sha256(json.dumps({
                "name": _text(raw.get("name")), "source": marker,
            }, sort_keys=True, ensure_ascii=False).encode()).hexdigest()
    elif asset_type == "cloud":
        for key in ("provider", "tenant", "region", "resource_id"):
            data[key] = _text(raw.get(key))
        if not data["provider"] or not data["resource_id"]:
            raise SrcStoreError("asset_identity_invalid", "cloud provider and resource id are required")
    elif asset_type == "repository":
        data["platform"] = _text(raw.get("platform") or raw.get("host")).lower()
        data["stable_id"] = _text(raw.get("stable_id") or raw.get("repository") or raw.get("canonical"))
        if not data["platform"] or not data["stable_id"]:
            raise SrcStoreError("asset_identity_invalid", "repository platform and stable id are required")
    return data


def natural_key(asset_type: str, identity: dict[str, Any], source: Optional[dict[str, Any]] = None) -> str:
    normalized = normalize_identity(asset_type, identity, source)
    canonical = json.dumps(normalized, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    return f"{KEY_VERSION}:{asset_type}:{hashlib.sha256(canonical.encode('utf-8')).hexdigest()}"


def _refs(values: Iterable[Any]) -> list[Any]:
    result = []
    for value in values:
        if isinstance(value, str):
            result.append(value)
        else:
            result.append(value.model_dump(mode="json") if hasattr(value, "model_dump") else dict(value))
    return result


class SrcAssetService:
    def __init__(self, src: Any) -> None:
        self.src = src
        self.store = src.store
        self.artifacts = src.artifacts

    def _asset(self, task_id: str, asset_id: str) -> dict[str, Any]:
        record = self.store.require(task_id, "asset", asset_id)
        if record.payload.get("lifecycle") == "destroyed":
            raise SrcStoreError("artifact_missing", "asset was destroyed; only its tombstone remains")
        return record.payload

    def _observations(self, task_id: str, asset_id: str) -> list[dict[str, Any]]:
        return [row.payload for row in self.store.all(task_id, "asset_observation")
                if row.payload.get("asset_id") == asset_id and not row.payload.get("revoked_at")]

    def _project(self, asset: dict[str, Any], observations: list[dict[str, Any]]) -> dict[str, Any]:
        selected: dict[str, dict[str, Any]] = {}
        conflicts: dict[str, list[dict[str, Any]]] = {}
        first_observed: Optional[str] = None
        latest_observed: Optional[str] = None
        last_success: Optional[str] = None
        last_failure: Optional[str] = None
        for observation in sorted(observations, key=lambda row: (row.get("observed_at") or "", row["observation_id"])):
            observed = observation.get("observed_at")
            if observed:
                first_observed = observed if first_observed is None else min(first_observed, observed)
                latest_observed = observed if latest_observed is None else max(latest_observed, observed)
            if observation.get("success") is True:
                last_success = max(last_success or observed or "", observed or "") or None
            elif observation.get("success") is False:
                last_failure = max(last_failure or observed or "", observed or "") or None
            for field, value in (observation.get("attributes") or {}).items():
                is_manual = bool(observation.get("manual"))
                evidence = {
                    "observation_id": observation["observation_id"], "value": value,
                    "observed_at": observed, "source_kind": observation.get("source_kind"),
                    "source_id": observation.get("source_id"), "manual": is_manual,
                }
                existing = selected.get(field)
                if existing is None:
                    selected[field] = evidence
                elif is_manual:
                    selected[field] = evidence
                elif existing.get("manual"):
                    continue
                elif existing["value"] != value:
                    obs_t = _time(observed)
                    ext_t = _time(existing.get("observed_at"))
                    if obs_t is not None and (ext_t is None or obs_t > ext_t):
                        selected[field] = evidence
        for observation in observations:
            for field in set(observation.get("attributes") or {}) | set(selected):
                value = (observation.get("attributes") or {}).get(field)
                if value is None:
                    continue
                if not any(item["observation_id"] == observation["observation_id"] for item in conflicts.setdefault(field, [])):
                    conflicts[field].append({
                        "observation_id": observation["observation_id"], "value": value,
                        "observed_at": observation.get("observed_at"),
                        "source_kind": observation.get("source_kind"), "source_id": observation.get("source_id"),
                    })
        conflicts = {
            field: rows for field, rows in conflicts.items()
            if len({json.dumps(row["value"], sort_keys=True, ensure_ascii=False) for row in rows}) > 1
        }
        return {
            **asset,
            "current_profile": {field: row["value"] for field, row in selected.items()},
            "profile_sources": selected, "conflicts": conflicts,
            "first_observed": first_observed, "latest_observed": latest_observed,
            "last_success": last_success, "last_failure": last_failure,
            "observation_count": len(observations),
        }

    def observe(
        self, task_id: str, asset_type: str, identity: dict[str, Any], *,
        source_kind: str, source_id: str, attributes: Optional[dict[str, Any]] = None,
        raw_values: Optional[dict[str, Any]] = None, observed_at: Any = None,
        artifact_refs: Iterable[Any] = (), source_run_id: Optional[str] = None,
        source_event_seq: Optional[int] = None, parser_version: str = "asset-observation.v1",
        completeness: str = "complete", success: Optional[bool] = None,
        source_event_id: str = "", manual: bool = False, correction_of: Optional[str] = None,
        aliases: Iterable[str] = (), judgements: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        normalized = normalize_identity(asset_type, identity, {"source_kind": source_kind, "source_id": source_id})
        key = natural_key(asset_type, normalized, {"source_kind": source_kind, "source_id": source_id})
        observed_iso = _iso(observed_at)
        source_payload = {
            "task_id": task_id, "asset_type": asset_type, "identity": normalized,
            "source_kind": source_kind, "source_id": source_id,
            "source_run_id": source_run_id, "source_event_seq": source_event_seq,
            "observed_at": observed_iso, "attributes": attributes or {},
            "raw_values": raw_values or {}, "parser_version": parser_version,
            "source_event_id": source_event_id,
        }
        scoped_event_key = f"{source_kind}:{source_id}:{source_event_id}" if source_event_id else ""
        if scoped_event_key:
            prior = self.store.idempotent(task_id, "asset-observation", scoped_event_key, source_payload)
            if prior is not None:
                return prior
        with self.store.platform.transaction():
            record = self.store.by_natural_key(task_id, "asset", key)
            action = "merged" if record is not None else "created"
            if record is None:
                asset_id = new_id("asset")
                asset = {
                    "asset_id": asset_id, "type": asset_type, "key_version": KEY_VERSION,
                    "natural_key": key, "identity": normalized,
                    "original_identifiers": [normalized], "aliases": list(dict.fromkeys(aliases)),
                    "lifecycle": LIFECYCLE_ACTIVE, "judgements": judgements or {
                        "company_attribution": {"state": "unknown"},
                        "program_coverage": {"state": "unknown"},
                        "active_test_conditions": {"state": "unknown"},
                    },
                }
                record = self.store.put(task_id, "asset", asset, record_id=asset_id, natural_key=key)
            else:
                asset = dict(record.payload)
                if asset.get("lifecycle") == "merged":
                    target = asset.get("merged_into")
                    if target:
                        raise SrcStoreError("version_conflict", "asset was merged; use the target asset", detail={"asset_id": target})
                if asset.get("lifecycle") == "destroyed":
                    raise SrcStoreError("version_conflict", "asset was destroyed")
                if asset.get("lifecycle") == "archived":
                    asset["lifecycle"] = LIFECYCLE_ACTIVE
                asset["aliases"] = list(dict.fromkeys([*asset.get("aliases", []), *aliases]))
                if normalized not in asset.get("original_identifiers", []):
                    asset["original_identifiers"] = [*asset.get("original_identifiers", []), normalized]
            observation_id = new_id("sobs")
            observation = {
                "observation_id": observation_id, "task_id": task_id,
                "asset_id": record.record_id, "asset_type": asset_type,
                "source_kind": source_kind, "source_id": source_id,
                "source_run_id": source_run_id, "source_event_seq": source_event_seq,
                "source_event_id": source_event_id,
                "artifact_refs": _refs(artifact_refs), "observed_at": observed_iso,
                "ingested_at": utcnow().isoformat(), "attributes": attributes or {},
                "raw_values": raw_values or normalized, "parser_version": parser_version,
                "completeness": completeness, "success": success,
                "manual": bool(manual), "correction_of": correction_of,
            }
            self.store.put(task_id, "asset_observation", observation,
                           record_id=observation_id, parent_id=record.record_id,
                           natural_key=scoped_event_key)
            projected = self._project(asset, self._observations(task_id, record.record_id))
            record = self.store.put(task_id, "asset", projected, record_id=record.record_id,
                                    expected_revision=record.revision, natural_key=key)
            result = {"action": action, "asset": record.payload, "observation": observation}
            if scoped_event_key:
                self.store.remember(task_id, "asset-observation", scoped_event_key, source_payload, result)
            return result

    def correct(self, task_id: str, asset_id: str, attributes: dict[str, Any], *, actor_id: str, reason: str, correction_of: Optional[str] = None) -> dict[str, Any]:
        asset = self._asset(task_id, asset_id)
        return self.observe(
            task_id, asset["type"], asset["identity"], source_kind="manual", source_id=actor_id,
            attributes=attributes, raw_values={"reason": reason}, success=True,
            manual=True, correction_of=correction_of,
        )

    def relate(self, task_id: str, from_asset_id: str, to_asset_id: str, relation_type: str, *, origin: str, evidence_refs: Iterable[Any] = (), valid_from: Any = None, valid_until: Any = None) -> dict[str, Any]:
        self._asset(task_id, from_asset_id)
        self._asset(task_id, to_asset_id)
        if origin not in {"observed", "derived", "inferred"}:
            raise SrcStoreError("asset_relation_invalid", "unknown relation origin")
        relation = {
            "relation_id": new_id("srel"), "task_id": task_id,
            "from_asset_id": from_asset_id, "to_asset_id": to_asset_id,
            "relation_type": relation_type, "origin": origin,
            "evidence_refs": _refs(evidence_refs), "valid_from": _iso(valid_from),
            "valid_until": _iso(valid_until), "version": 1,
        }
        key = hashlib.sha256(json.dumps([from_asset_id, to_asset_id, relation_type, origin], sort_keys=True).encode()).hexdigest()
        existing = self.store.by_natural_key(task_id, "asset_relation", key)
        if existing is not None:
            return existing.payload
        self.store.put(task_id, "asset_relation", relation, record_id=relation["relation_id"], parent_id=from_asset_id, natural_key=key)
        return relation

    def derive_endpoint_relations(self, task_id: str, endpoint_asset_id: str) -> list[dict[str, Any]]:
        endpoint = self._asset(task_id, endpoint_asset_id)
        if endpoint["type"] not in {"http_endpoint", "parameter"}:
            raise SrcStoreError("asset_type_invalid", "relations can only be derived for an endpoint or parameter")
        identity = endpoint["identity"]
        host_identity = {"name": identity["host"]}
        try:
            host = self.observe(task_id, "domain", host_identity, source_kind="derived", source_id=endpoint_asset_id,
                                observed_at=None, success=None, raw_values={"derived_from": endpoint_asset_id})
        except SrcStoreError:
            host = self.observe(task_id, "ip", {"address": identity["host"]}, source_kind="derived", source_id=endpoint_asset_id,
                                observed_at=None, success=None, raw_values={"derived_from": endpoint_asset_id})
        service = self.observe(task_id, "web_service", {
            "host": identity["host"], "port": identity["port"], "protocol": identity["scheme"],
        }, source_kind="derived", source_id=endpoint_asset_id, success=None,
            raw_values={"derived_from": endpoint_asset_id})
        return [
            self.relate(task_id, endpoint_asset_id, host["asset"]["asset_id"], "hosted_by", origin="derived", evidence_refs=[endpoint_asset_id]),
            self.relate(task_id, endpoint_asset_id, service["asset"]["asset_id"], "served_by", origin="derived", evidence_refs=[endpoint_asset_id]),
        ]

    def merge(self, task_id: str, source_asset_id: str, target_asset_id: str, *, actor_id: str, reason: str) -> dict[str, Any]:
        if source_asset_id == target_asset_id:
            raise SrcStoreError("version_conflict", "cannot merge an asset into itself")
        source_record = self.store.require(task_id, "asset", source_asset_id)
        target_record = self.store.require(task_id, "asset", target_asset_id)
        source, target = source_record.payload, target_record.payload
        if source["type"] != target["type"]:
            raise SrcStoreError("asset_type_invalid", "only assets of the same type can be merged")
        with self.store.platform.transaction():
            for observation in self.store.all(task_id, "asset_observation"):
                if observation.payload.get("asset_id") == source_asset_id:
                    payload = {**observation.payload, "asset_id": target_asset_id, "original_asset_id": observation.payload.get("original_asset_id") or source_asset_id}
                    self.store.put(task_id, "asset_observation", payload, record_id=observation.record_id,
                                   expected_revision=observation.revision, parent_id=target_asset_id,
                                   natural_key=observation.natural_key)
            for relation in self.store.all(task_id, "asset_relation"):
                payload = dict(relation.payload)
                changed = False
                for field in ("from_asset_id", "to_asset_id"):
                    if payload.get(field) == source_asset_id:
                        payload[field] = target_asset_id
                        changed = True
                if changed:
                    payload["remapped_from"] = source_asset_id
                    self.store.put(task_id, "asset_relation", payload, record_id=relation.record_id,
                                   expected_revision=relation.revision, parent_id=payload["from_asset_id"], natural_key=relation.natural_key)
            merged = {
                **source, "lifecycle": "merged", "merged_into": target_asset_id,
                "merge_reason": reason, "merged_by": actor_id,
            }
            self.store.put(task_id, "asset", merged, record_id=source_asset_id,
                           expected_revision=source_record.revision, natural_key=source_record.natural_key)
            alias = {"source_asset_id": source_asset_id, "target_asset_id": target_asset_id,
                     "natural_key": source_record.natural_key, "created_at": utcnow().isoformat()}
            self.store.put(task_id, "asset_alias", alias, record_id=source_asset_id,
                           natural_key=source_record.natural_key)
            combined = {
                **target,
                "aliases": list(dict.fromkeys([*target.get("aliases", []), *source.get("aliases", []), source_record.natural_key])),
                "original_identifiers": [*target.get("original_identifiers", []), *source.get("original_identifiers", [])],
            }
            combined = self._project(combined, self._observations(task_id, target_asset_id))
            result = self.store.put(task_id, "asset", combined, record_id=target_asset_id,
                                    expected_revision=target_record.revision, natural_key=target_record.natural_key)
            return {"source": merged, "target": result.payload}

    def split(self, task_id: str, merged_asset_id: str, observation_ids: list[str], *, actor_id: str) -> dict[str, Any]:
        merged = self.store.require(task_id, "asset", merged_asset_id).payload
        if not observation_ids:
            raise SrcStoreError("asset_identity_invalid", "split requires selected observations")
        new_asset_id = new_id("asset")
        identity = dict(merged["identity"])
        key = natural_key(merged["type"], identity) + f":split:{new_asset_id}"
        asset = {
            **merged, "asset_id": new_asset_id, "natural_key": key,
            "lifecycle": LIFECYCLE_ACTIVE, "split_from": merged_asset_id,
            "split_by": actor_id, "aliases": [],
        }
        asset.pop("merged_into", None)
        with self.store.platform.transaction():
            self.store.put(task_id, "asset", asset, record_id=new_asset_id, natural_key=key)
            selected = set(observation_ids)
            moved = 0
            for observation in self.store.all(task_id, "asset_observation"):
                if observation.record_id in selected and observation.payload.get("asset_id") == merged_asset_id:
                    payload = {**observation.payload, "asset_id": new_asset_id,
                               "split_from_observation": observation.record_id}
                    self.store.put(task_id, "asset_observation", payload,
                                   record_id=observation.record_id, expected_revision=observation.revision,
                                   parent_id=new_asset_id, natural_key=observation.natural_key)
                    moved += 1
            if moved != len(selected):
                raise SrcStoreError("artifact_missing", "one or more selected observations do not belong to this asset")
            original_record = self.store.require(task_id, "asset", merged_asset_id)
            original_projected = self._project(dict(original_record.payload), self._observations(task_id, merged_asset_id))
            self.store.put(
                task_id, "asset", original_projected, record_id=merged_asset_id,
                expected_revision=original_record.revision, natural_key=original_record.natural_key,
            )
            projected = self._project(asset, self._observations(task_id, new_asset_id))
            result = self.store.put(task_id, "asset", projected, record_id=new_asset_id, natural_key=key)
            return result.payload

    def lifecycle(
        self, task_id: str, asset_id: str, action: str, *,
        actor_id: str, expected_revision: Optional[int] = None,
    ) -> dict[str, Any]:
        record = self.store.require(task_id, "asset", asset_id)
        if expected_revision is not None and record.revision != expected_revision:
            raise SrcStoreError("version_conflict", "asset revision is stale")
        if action == "detach":
            link = self.store.by_natural_key(task_id, "task_asset_link", asset_id)
            payload = dict(link.payload if link else {"asset_id": asset_id})
            payload.update({"state": "detached", "actor_id": actor_id, "updated_at": utcnow().isoformat()})
            self.store.put(task_id, "task_asset_link", payload, record_id=asset_id, natural_key=asset_id)
            return payload
        if action == "destroy":
            return self.destroy(task_id, asset_id, actor_id=actor_id)
        states = {"archive": "archived", "restore": LIFECYCLE_ACTIVE}
        if action not in states:
            raise SrcStoreError("asset_lifecycle_invalid", f"unsupported lifecycle action {action}")
        payload = {**record.payload, "lifecycle": states[action], "lifecycle_actor": actor_id}
        result = self.store.put(task_id, "asset", payload, record_id=asset_id,
                                expected_revision=record.revision, natural_key=record.natural_key)
        self.store.put(task_id, "asset_lifecycle", {
            "asset_id": asset_id, "action": action, "actor_id": actor_id,
            "before": record.payload, "after": result.payload, "at": utcnow().isoformat(),
        }, parent_id=asset_id)
        return result.payload

    def _references(self, asset_id: str) -> list[dict[str, Any]]:
        with self.store.platform.lock:
            rows = list(self.store.platform.conn.execute(
                "SELECT task_id,kind,record_id,payload FROM src_records"
            ).fetchall())
        references = []
        intrinsic = {"asset", "asset_observation", "asset_tombstone", "asset_alias", "asset_lifecycle"}
        for row in rows:
            if row["kind"] in intrinsic:
                continue
            payload = json.loads(row["payload"])
            if asset_id in json.dumps(payload, ensure_ascii=False):
                references.append({"task_id": row["task_id"], "kind": row["kind"],
                                   "record_id": row["record_id"], "payload": payload})
        return references

    def destroy(self, task_id: str, asset_id: str, *, actor_id: str) -> dict[str, Any]:
        record = self.store.require(task_id, "asset", asset_id)
        protected = [row for row in self._references(asset_id)
                     if not (row["kind"] == "task_asset_link" and row["task_id"] == task_id)]
        if protected:
            raise SrcStoreError("artifact_in_use", "asset is protected by shared, report, relation or baseline references",
                                detail={"references": [{key: row[key] for key in ("task_id", "kind", "record_id")} for row in protected]})
        tombstone = {
            "asset_id": asset_id, "type": record.payload.get("type"),
            "natural_key": record.natural_key, "lifecycle": "destroyed",
            "destroyed_by": actor_id, "destroyed_at": utcnow().isoformat(),
            "identity": record.payload.get("identity"),
        }
        with self.store.platform.transaction():
            for kind in ("asset_observation", "asset_relation", "task_asset_link"):
                for row in self.store.all(task_id, kind):
                    if row.payload.get("asset_id") == asset_id or row.payload.get("from_asset_id") == asset_id or row.payload.get("to_asset_id") == asset_id:
                        self.store.platform.conn.execute("DELETE FROM src_records WHERE kind=? AND record_id=?", (kind, row.record_id))
            self.store.platform.conn.execute("DELETE FROM src_records WHERE kind='asset' AND record_id=?", (asset_id,))
            self.store.put(task_id, "asset_tombstone", tombstone, record_id=asset_id, natural_key=record.natural_key)
        return tombstone

    def publish(self, task_id: str, target_task_id: str, selection: dict[str, Any], *, actor_id: str, purpose: str) -> dict[str, Any]:
        self.src.binding(task_id)
        self.src.binding(target_task_id)
        assets = []
        for item in selection.get("assets") or []:
            asset = self._asset(task_id, str(item["asset_id"] if isinstance(item, dict) else item))
            fields = set(item.get("fields") or []) if isinstance(item, dict) else set()
            observations = self._observations(task_id, asset["asset_id"])
            selected_observation_ids = set(selection.get("observation_ids") or [])
            if selected_observation_ids:
                observations = [row for row in observations if row["observation_id"] in selected_observation_ids]
            if fields:
                asset["current_profile"] = {key: value for key, value in asset.get("current_profile", {}).items() if key in fields}
                asset["profile_sources"] = {key: value for key, value in asset.get("profile_sources", {}).items() if key in fields}
                asset["conflicts"] = {key: value for key, value in asset.get("conflicts", {}).items() if key in fields}
                selected_artifact_hashes = {
                    ArtifactRef.model_validate(value).sha256
                    for value in (selection.get("artifact_refs") or [])
                }
                observations = [{
                    **row,
                    "attributes": {key: value for key, value in row.get("attributes", {}).items() if key in fields},
                    "raw_values": {key: value for key, value in row.get("raw_values", {}).items() if key in fields},
                    "artifact_refs": [
                        ref for ref in (row.get("artifact_refs") or [])
                        if isinstance(ref, dict) and ref.get("sha256") in selected_artifact_hashes
                    ],
                } for row in observations]
            assets.append({"entity": asset, "observations": observations})
        selected_asset_ids = {item["entity"]["asset_id"] for item in assets}
        selected_relation_ids = set(selection.get("relation_ids") or [])
        if selected_relation_ids:
            relations = [row.payload for row in self.store.all(task_id, "asset_relation")
                         if row.record_id in selected_relation_ids]
        else:
            relations = [row.payload for row in self.store.all(task_id, "asset_relation")
                         if {row.payload.get("from_asset_id"), row.payload.get("to_asset_id")}.issubset(selected_asset_ids)]
        artifact_refs = []
        for value in selection.get("artifact_refs") or []:
            ref = ArtifactRef.model_validate(value)
            artifact_refs.append(self.artifacts.pin(ref, target_task_id=target_task_id).model_dump(mode="json"))
        publication_id = new_id("spub")
        version = len(self.store.all(task_id, "asset_publication")) + 1
        publication = {
            "publication_id": publication_id, "version": version, "task_id": task_id,
            "target_task_id": target_task_id, "publisher": actor_id, "purpose": purpose,
            "published_at": utcnow().isoformat(), "assets": assets, "relations": relations,
            "artifact_refs": artifact_refs, "selection": selection, "revoked_at": None,
        }
        with self.store.platform.transaction():
            self.store.put(task_id, "asset_publication", publication, record_id=publication_id,
                           natural_key=f"{target_task_id}:{publication_id}")
            reference = {
                "reference_id": new_id("sassetref"), "task_id": target_task_id,
                "source_task_id": task_id, "publication_id": publication_id, "version": version,
                "created_at": utcnow().isoformat(), "revoked_at": None,
            }
            self.store.put(target_task_id, "asset_reference", reference,
                           record_id=reference["reference_id"], parent_id=publication_id,
                           natural_key=publication_id)
        return {"publication": publication, "reference": reference}

    def published(self, task_id: str, publication_id: str) -> dict[str, Any]:
        reference = self.store.by_natural_key(task_id, "asset_reference", publication_id)
        if reference is None or reference.payload.get("revoked_at"):
            raise SrcStoreError("source_revoked", "published asset reference is unavailable")
        source_task_id = reference.payload["source_task_id"]
        publication = self.store.require(source_task_id, "asset_publication", publication_id)
        if publication.payload.get("revoked_at"):
            raise SrcStoreError("source_revoked", "publication was revoked")
        return publication.payload

    def refresh_reference(self, task_id: str, publication_id: str, selection: dict[str, Any], *, actor_id: str, purpose: str) -> dict[str, Any]:
        current = self.published(task_id, publication_id)
        return self.publish(current["task_id"], task_id, selection, actor_id=actor_id, purpose=purpose)

    def revoke_publication(self, task_id: str, publication_id: str, *, actor_id: str) -> dict[str, Any]:
        record = self.store.require(task_id, "asset_publication", publication_id)
        payload = {**record.payload, "revoked_at": utcnow().isoformat(), "revoked_by": actor_id}
        result = self.store.put(task_id, "asset_publication", payload,
                                record_id=publication_id, expected_revision=record.revision,
                                natural_key=record.natural_key)
        return result.payload

    def detail(self, task_id: str, asset_id: str) -> dict[str, Any]:
        asset = self._asset(task_id, asset_id)
        observations = self._observations(task_id, asset_id)
        sources = sorted({(row.get("source_kind"), row.get("source_id")) for row in observations})
        return {
            "asset": asset, "observations": observations,
            "sources": [{"source_kind": kind, "source_id": source} for kind, source in sources],
            "history": observations,
            "relations": [row.payload for row in self.store.all(task_id, "asset_relation")
                          if asset_id in {row.payload.get("from_asset_id"), row.payload.get("to_asset_id")}],
        }

    def resolve(self, task_id: str, reference: str) -> dict[str, Any]:
        record = self.store.get(task_id, "asset", reference)
        if record is not None and record.payload.get("lifecycle") == "merged":
            target = self.store.require(task_id, "asset", record.payload["merged_into"])
            return {"state": "remapped", "alias": record.payload, "asset": target.payload}
        if record is not None:
            return {"state": "current", "asset": record.payload}
        alias = self.store.get(task_id, "asset_alias", reference)
        if alias is not None:
            target = self.store.require(task_id, "asset", alias.payload["target_asset_id"])
            return {"state": "remapped", "alias": alias.payload, "asset": target.payload}
        tombstone = self.store.get(task_id, "asset_tombstone", reference)
        if tombstone is not None:
            return {"state": "destroyed", "tombstone": tombstone.payload}
        natural = self.store.by_natural_key(task_id, "asset_alias", reference)
        if natural is not None:
            target = self.store.require(task_id, "asset", natural.payload["target_asset_id"])
            return {"state": "remapped", "alias": natural.payload, "asset": target.payload}
        raise SrcStoreError("artifact_missing", "asset reference is unavailable")

    def history(self, task_id: str, asset_id: str, *, cursor: int = 0, limit: int = 100) -> dict[str, Any]:
        self._asset(task_id, asset_id)
        observations = self._observations(task_id, asset_id)
        observations.sort(key=lambda row: (row.get("observed_at") or "", row.get("ingested_at") or "", row["observation_id"]))
        source_rows: dict[tuple[str, str], dict[str, Any]] = {}
        for observation in observations:
            key = (observation.get("source_kind") or "", observation.get("source_id") or "")
            source = source_rows.setdefault(key, {
                "source_kind": key[0], "source_id": key[1], "observation_ids": [],
                "source_run_ids": [], "artifact_refs": [],
            })
            source["observation_ids"].append(observation["observation_id"])
            if observation.get("source_run_id"):
                source["source_run_ids"].append(observation["source_run_id"])
            source["artifact_refs"].extend(observation.get("artifact_refs") or [])
        start = max(0, int(cursor))
        size = max(1, min(int(limit), 1000))
        page = observations[start:start + size]
        sources = sorted(source_rows.values(), key=lambda row: (row["source_kind"], row["source_id"]))
        source_page = sources[start:start + size]
        return {
            "items": page, "total": len(observations),
            "next_cursor": start + len(page) if start + len(page) < len(observations) else None,
            "sources": source_page, "source_total": len(sources),
            "source_next_cursor": start + len(source_page) if start + len(source_page) < len(sources) else None,
        }


__all__ = ["ASSET_TYPES", "KEY_VERSION", "SrcAssetService", "natural_key", "normalize_identity"]
