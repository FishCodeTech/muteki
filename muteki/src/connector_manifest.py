from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Optional

from .store import SrcStoreError


class ConnectorError(SrcStoreError):
    pass


def unavailable(message: str, **detail: Any) -> ConnectorError:
    return ConnectorError("capability_unavailable", message, detail=detail)


@dataclass(frozen=True)
class CapabilityManifest:
    connector_id: str
    version: str
    source_schema: str
    capabilities: tuple[str, ...]
    asset_types: dict[str, str]
    field_mapping: dict[str, str]
    pagination: dict[str, Any]
    credentials: dict[str, Any]
    cost: dict[str, Any]
    runtime: dict[str, Any]
    evidence: dict[str, Any]

    def as_dict(self) -> dict[str, Any]:
        return {
            "connector_id": self.connector_id,
            "version": self.version,
            "source_schema": self.source_schema,
            "capabilities": list(self.capabilities),
            "asset_types": dict(self.asset_types),
            "field_mapping": dict(self.field_mapping),
            "pagination": dict(self.pagination),
            "credentials": dict(self.credentials),
            "cost": dict(self.cost),
            "runtime": dict(self.runtime),
            "evidence": dict(self.evidence),
        }


@dataclass
class UpstreamPage:
    items: list[dict[str, Any]]
    raw: bytes
    request: dict[str, Any]
    next_token: Any = None
    total: Optional[int] = None
    media_type: str = "application/json"
    quota: dict[str, Any] = field(default_factory=dict)


@dataclass
class ConnectorBatch:
    batch_id: str
    connector_id: str
    config_version: int
    task_id: str
    items: list[dict[str, Any]]
    source_refs: list[dict[str, Any]]
    errors: list[dict[str, Any]] = field(default_factory=list)
    unknown_fields: list[str] = field(default_factory=list)
    field_mapping: dict[str, Any] = field(default_factory=dict)
    acquisition: dict[str, Any] = field(default_factory=dict)
    counts: dict[str, int] = field(default_factory=dict)
    cost: dict[str, Any] = field(default_factory=dict)
    external_state: str = "completed"
    deduplicated: bool = False

    def as_dict(self) -> dict[str, Any]:
        counts = {
            "processed": len(self.items), "new": 0, "updated": 0,
            "merged": 0, "skipped": 0, "failed": len(self.errors),
            "truncated": int(bool(self.acquisition.get("truncated"))),
            **self.counts,
        }
        return {
            "schema": "muteki.src.connector-batch.v1",
            "batch_id": self.batch_id, "connector_id": self.connector_id,
            "config_version": self.config_version, "task_id": self.task_id,
            "product_mode": "src", "items": self.items,
            "source_refs": self.source_refs, "errors": self.errors,
            "unknown_fields": self.unknown_fields,
            "field_mapping": self.field_mapping,
            "acquisition": self.acquisition, "counts": counts,
            "cost": self.cost, "external_state": self.external_state,
            "deduplicated": self.deduplicated,
        }


SEARCH_MANIFESTS: dict[str, CapabilityManifest] = {
    "fofa": CapabilityManifest(
        "fofa", "api-v1.search-all.1", "fofa.search-all.response+json",
        ("query", "read", "paginate"),
        {"host": "service", "ip": "ip", "domain": "domain", "icp": "ownership_candidate"},
        {"host": "host", "ip": "ip", "port": "port", "protocol": "protocol", "title": "title", "domain": "domain", "icp": "icp"},
        {"kind": "page", "page_size_max": 10000, "reliable_full": False, "source_delay": "upstream"},
        {"required": True, "reference_fields": ["email_ref", "key_ref"], "permission": "search"},
        {"actual_cost": "unknown_unless_upstream_receipt", "retry": "submitted_unknown_not_replayed", "rate_limit": "upstream"},
        {"kind": "https", "base_url": "https://fofa.info"},
        {"raw_response": True, "stable_source_link": True, "screenshots": False},
    ),
    "quake": CapabilityManifest(
        "quake", "api-v3.quake-service.1", "quake.service.response+json",
        ("query", "read", "paginate"),
        {"service": "service", "ip": "ip", "domain": "domain", "app": "product_candidate"},
        {"service": "service", "ip": "ip", "port": "port", "transport": "transport", "domain": "domain"},
        {"kind": "offset", "page_size_max": 500, "reliable_full": False, "source_delay": "upstream"},
        {"required": True, "reference_fields": ["token_ref"], "permission": "quake_service_search"},
        {"actual_cost": "unknown_unless_upstream_receipt", "retry": "submitted_unknown_not_replayed", "rate_limit": "upstream"},
        {"kind": "https", "base_url": "https://quake.360.net"},
        {"raw_response": True, "stable_source_link": True, "screenshots": False},
    ),
    "shodan": CapabilityManifest(
        "shodan", "api.shodan-host-search.1", "shodan.host-search.response+json",
        ("query", "read", "paginate"),
        {"matches": "service", "ip_str": "ip", "domains": "domain_candidates"},
        {"matches": "matches", "ip_str": "ip", "port": "port", "transport": "transport", "domains": "domains"},
        {"kind": "page", "page_size_max": 100, "reliable_full": False, "source_delay": "upstream"},
        {"required": True, "reference_fields": ["key_ref"], "permission": "host_search"},
        {"actual_cost": "unknown_unless_upstream_receipt", "retry": "submitted_unknown_not_replayed", "rate_limit": "upstream"},
        {"kind": "https", "base_url": "https://api.shodan.io"},
        {"raw_response": True, "stable_source_link": True, "screenshots": False},
    ),
    "zoomeye": CapabilityManifest(
        "zoomeye", "api.host-search.1", "zoomeye.host-search.response+json",
        ("query", "read", "paginate"),
        {"data": "service", "ip": "ip", "site": "domain_candidate", "app": "product_candidate"},
        {"data": "data", "ip": "ip", "port": "port", "site": "site", "app": "app"},
        {"kind": "page", "page_size_max": 100, "reliable_full": False, "source_delay": "upstream"},
        {"required": True, "reference_fields": ["key_ref"], "permission": "host_search"},
        {"actual_cost": "unknown_unless_upstream_receipt", "retry": "submitted_unknown_not_replayed", "rate_limit": "upstream"},
        {"kind": "https", "base_url": "https://api.zoomeye.org"},
        {"raw_response": True, "stable_source_link": True, "screenshots": False},
    ),
}


__all__ = ["CapabilityManifest", "ConnectorBatch", "ConnectorError", "UpstreamPage", "SEARCH_MANIFESTS", "unavailable"]
