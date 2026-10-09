"""Adapter Registry：instance identity、注册、probe 与健康快照（RUNTIME-01）。

同一 Runtime 可注册多个实例（任务书 7.3）：身份为
``adapter_id + instance_id``；每个实例绑定 binary/endpoint、账户与环境
引用、能力快照。Registry 只做注册、查找、probe 与健康快照，不做调度准入
（账户并发、配额、冷却属于 Scheduler）。

probe 结果只存放在进程级 ``ProbeCache``（``probe_cache.py``）；
``AdapterInstanceRecord.last_probe`` 读取同一缓存，交互矩阵与设置 API
看到的是同一份结果。
"""

from __future__ import annotations

import asyncio
import threading
from dataclasses import dataclass, field, replace
from datetime import datetime
from typing import Any, Optional

from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.external_agents import ProbeRequest
from muteki.platform.contracts.protocols import ProbingAdapter

from .capabilities import CapabilityProbeReport, describe_injection_path
from .probe_cache import (
    BinaryIdentity,
    ProbeCache,
    ProbeEntry,
    ProbeKey,
    adapter_binary,
    config_hash,
    probe_cache,
)

_DEFAULT_REQUEST = ProbeRequest()


@dataclass
class AdapterInstanceRecord:
    """一个已注册的 Runtime 实例。"""

    adapter_id: str
    instance_id: str
    adapter: Any
    registered_at: datetime = field(default_factory=utcnow)
    # 实例绑定的 binary/endpoint、账户引用、环境引用等（引用，不含 secret 本体）
    metadata: dict[str, Any] = field(default_factory=dict)
    cache: ProbeCache = field(default_factory=probe_cache, repr=False, compare=False)

    @property
    def key(self) -> str:
        return f"{self.adapter_id}:{self.instance_id}"

    def probe_key(self, scope: str = "") -> ProbeKey:
        return ProbeKey(
            adapter_id=self.adapter_id,
            instance_id=self.instance_id,
            binary=BinaryIdentity.of(adapter_binary(self.adapter)),
            config_hash=config_hash({
                "metadata": self.metadata,
                "launch_args": list(self.adapter.launch_args),
            }),
            scope=scope,
        )

    @property
    def last_probe_entry(self) -> Optional[ProbeEntry]:
        """Cached default-scope entry for the current binary/config, any age."""
        return self.cache.peek(self.probe_key())

    @property
    def last_probe(self) -> Optional[CapabilityProbeReport]:
        entry = self.last_probe_entry
        return entry.report if entry is not None and entry.ok else None

    @property
    def last_probe_at(self) -> Optional[datetime]:
        entry = self.last_probe_entry
        return entry.probed_at if entry is not None else None


def _request_scope(request: ProbeRequest) -> str:
    """Non-default probe requests produce different reports; key them apart."""
    payload = request.model_dump(mode="json", exclude={"runtime_instance_id"})
    default = _DEFAULT_REQUEST.model_dump(mode="json", exclude={"runtime_instance_id"})
    return "" if payload == default else f"request:{config_hash(payload)}"


def _join_scope(*parts: str) -> str:
    return "|".join(part for part in parts if part)


def _snapshot(report: CapabilityProbeReport) -> CapabilityProbeReport:
    # Adapters keep mutating their own report object across probes; the
    # cache holds an independent copy of this probe's result.
    return replace(
        report,
        capabilities=report.capabilities.model_copy(deep=True),
        field_sources=dict(report.field_sources),
        degradations=list(report.degradations),
        model_catalog=dict(report.model_catalog) if report.model_catalog is not None else None,
    )


class AdapterRegistry:
    """Adapter 实例注册表（线程安全）。"""

    def __init__(self, *, cache: Optional[ProbeCache] = None) -> None:
        self._lock = threading.RLock()
        self._records: dict[tuple[str, str], AdapterInstanceRecord] = {}
        self.cache = cache if cache is not None else probe_cache()
        # Different probe scopes of one adapter instance share the adapter's
        # mutable ``probe_report()``; they must not run concurrently.
        self._probe_locks: dict[tuple[str, str, int], asyncio.Lock] = {}

    # -- 注册 ---------------------------------------------------------------

    def register(
        self,
        adapter: Any,
        *,
        instance_id: Optional[str] = None,
        metadata: Optional[dict[str, Any]] = None,
    ) -> AdapterInstanceRecord:
        """注册一个 Adapter 实例；同 key 重复注册报错（不静默覆盖）。"""
        if not isinstance(adapter, ProbingAdapter):
            raise TypeError(
                f"{type(adapter).__name__} does not implement ProbingAdapter "
                "(derive from BaseExternalAgentAdapter)")
        adapter_id = str(adapter.id or "")
        if not adapter_id:
            raise ValueError("adapter must expose a non-empty id")
        iid = instance_id or adapter.identity.instance_id or "default"
        # 保持 Adapter 自身身份与注册身份一致。
        if adapter.identity.instance_id != iid:
            raise ValueError(
                f"instance_id mismatch: registry {iid!r} vs adapter "
                f"{adapter.identity.instance_id!r}")
        key = (adapter_id, iid)
        with self._lock:
            if key in self._records:
                raise ValueError(f"adapter instance already registered: {key}")
            record = AdapterInstanceRecord(
                adapter_id=adapter_id,
                instance_id=iid,
                adapter=adapter,
                metadata=dict(metadata or {}),
                cache=self.cache,
            )
            self._records[key] = record
            return record

    def unregister(self, adapter_id: str, instance_id: str = "default") -> bool:
        with self._lock:
            removed = self._records.pop((adapter_id, instance_id), None) is not None
        # A re-registered instance carries a new configuration; its old
        # results (and probes still in flight) must not be served.
        self.cache.invalidate(adapter_id, instance_id)
        return removed

    # -- 查找 ---------------------------------------------------------------

    def get(self, adapter_id: str, instance_id: str = "default") -> Any:
        record = self._records.get((adapter_id, instance_id))
        return record.adapter if record is not None else None

    def record(
        self, adapter_id: str, instance_id: str = "default"
    ) -> Optional[AdapterInstanceRecord]:
        return self._records.get((adapter_id, instance_id))

    def instances(self, adapter_id: Optional[str] = None) -> list[AdapterInstanceRecord]:
        """全部实例；给 ``adapter_id`` 时只列该 Runtime 的实例。"""
        with self._lock:
            records = list(self._records.values())
        if adapter_id is not None:
            records = [r for r in records if r.adapter_id == adapter_id]
        return records

    # -- probe 与健康快照 ------------------------------------------------------

    def _require(self, adapter_id: str, instance_id: str) -> AdapterInstanceRecord:
        record = self._records.get((adapter_id, instance_id))
        if record is None:
            raise KeyError(f"unknown adapter instance: {adapter_id}:{instance_id}")
        return record

    def _instance_lock(self, adapter_id: str, instance_id: str) -> asyncio.Lock:
        loop_id = id(asyncio.get_running_loop())
        with self._lock:
            return self._probe_locks.setdefault(
                (adapter_id, instance_id, loop_id), asyncio.Lock())

    def probe_key(
        self, adapter_id: str, instance_id: str = "default", *,
        request: Optional[ProbeRequest] = None, scope: str = "",
    ) -> ProbeKey:
        record = self._require(adapter_id, instance_id)
        req = request or ProbeRequest(runtime_instance_id=instance_id)
        return record.probe_key(_join_scope(scope, _request_scope(req)))

    async def run_probe(
        self,
        adapter_id: str,
        instance_id: str = "default",
        request: Optional[ProbeRequest] = None,
    ) -> CapabilityProbeReport:
        """Probe the adapter now, bypassing the cache (serialized per instance).

        Callers normally use ``probe``; this is the probe function the cache
        runs, exposed for scoped probes that prepare their own environment.
        """
        record = self._require(adapter_id, instance_id)
        req = request or ProbeRequest(runtime_instance_id=instance_id)
        async with self._instance_lock(adapter_id, instance_id):
            caps = await record.adapter.probe_with_environment(req)
            report = record.adapter.probe_report()
            if report is None:
                report = CapabilityProbeReport(
                    adapter_id=adapter_id, instance_id=instance_id, capabilities=caps)
            return _snapshot(report)

    async def probe_entry(
        self,
        adapter_id: str,
        instance_id: str = "default",
        request: Optional[ProbeRequest] = None,
        *,
        refresh: bool = False,
        scope: str = "",
    ) -> ProbeEntry:
        """Cached probe entry (single-flight); failures are typed entries."""
        key = self.probe_key(adapter_id, instance_id, request=request, scope=scope)
        return await self.cache.get(
            key, lambda: self.run_probe(adapter_id, instance_id, request),
            refresh=refresh)

    async def probe(
        self,
        adapter_id: str,
        instance_id: str = "default",
        request: Optional[ProbeRequest] = None,
        *,
        refresh: bool = False,
        scope: str = "",
    ) -> CapabilityProbeReport:
        """Capability report of one instance; raises ``ProbeFailedError``."""
        entry = await self.probe_entry(
            adapter_id, instance_id, request, refresh=refresh, scope=scope)
        return entry.require_report()

    async def probe_all(self, *, refresh: bool = False) -> list[ProbeEntry]:
        return [
            await self.probe_entry(record.adapter_id, record.instance_id, refresh=refresh)
            for record in self.instances()
        ]

    async def health_snapshot(
        self, adapter_id: str, instance_id: str = "default", *, refresh: bool = False,
    ) -> dict[str, Any]:
        """实例健康快照：binary/endpoint 可用性、能力摘要与降级清单。

        probe 失败时返回 ``healthy=False`` 与 typed error，不抛出。
        """
        entry = await self.probe_entry(adapter_id, instance_id, refresh=refresh)
        if not entry.ok:
            error = entry.error
            return {
                "adapter_id": adapter_id,
                "instance_id": instance_id,
                "healthy": False,
                "detail": error.describe() if error is not None else "",
                "error": {"code": error.code, "type": error.exception_type,
                          "message": error.message} if error is not None else None,
                "probed_at": entry.probed_at.isoformat(),
                "capabilities": None,
                "degradations": [],
            }
        report = entry.require_report()
        caps = report.capabilities
        return {
            "adapter_id": adapter_id,
            "instance_id": instance_id,
            "healthy": report.healthy,
            "detail": report.detail,
            "binary_path": report.binary_path,
            "probed_at": entry.probed_at.isoformat(),
            # 公共健康快照保留完整能力位，包括 MCP / Native
            # Tool / ACP-MCP / Agent Plugin / Structured HTTP-RPC。消费方
            # 不需要根据 Adapter ID 反向推断工具接入协议。
            "capabilities": caps.model_dump(mode="json"),
            "capability_injection": describe_injection_path(caps),
            "degradations": list(report.degradations),
        }


__all__ = ["AdapterInstanceRecord", "AdapterRegistry"]
