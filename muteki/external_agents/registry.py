"""Adapter Registry：instance identity、注册、probe 缓存与健康快照（RUNTIME-01）。

同一 Runtime 可注册多个实例（任务书 7.3）：身份为
``adapter_id + instance_id``；每个实例绑定 binary/endpoint、账户与环境
引用、能力快照。Registry 只做注册、查找、probe 与健康快照，不做调度准入
（账户并发、配额、冷却属于 Scheduler）。
"""

from __future__ import annotations

import threading
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Optional

from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.external_agents import ProbeRequest

from .capabilities import CapabilityProbeReport, describe_injection_path


@dataclass
class AdapterInstanceRecord:
    """一个已注册的 Runtime 实例。"""

    adapter_id: str
    instance_id: str
    adapter: Any
    registered_at: datetime = field(default_factory=utcnow)
    # 实例绑定的 binary/endpoint、账户引用、环境引用等（引用，不含 secret 本体）
    metadata: dict[str, Any] = field(default_factory=dict)
    last_probe: Optional[CapabilityProbeReport] = None
    last_probe_at: Optional[datetime] = None

    @property
    def key(self) -> str:
        return f"{self.adapter_id}:{self.instance_id}"


class AdapterRegistry:
    """Adapter 实例注册表（线程安全）。"""

    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._records: dict[tuple[str, str], AdapterInstanceRecord] = {}

    # -- 注册 ---------------------------------------------------------------

    def register(
        self,
        adapter: Any,
        *,
        instance_id: Optional[str] = None,
        metadata: Optional[dict[str, Any]] = None,
    ) -> AdapterInstanceRecord:
        """注册一个 Adapter 实例；同 key 重复注册报错（不静默覆盖）。"""
        adapter_id = str(getattr(adapter, "id", "") or "")
        if not adapter_id:
            raise ValueError("adapter must expose a non-empty id")
        iid = instance_id or str(getattr(adapter, "instance_id", "") or "default")
        # 保持 Adapter 自身身份与注册身份一致。
        identity = getattr(adapter, "identity", None)
        if identity is not None and identity.instance_id != iid:
            raise ValueError(
                f"instance_id mismatch: registry {iid!r} vs adapter "
                f"{identity.instance_id!r}")
        key = (adapter_id, iid)
        with self._lock:
            if key in self._records:
                raise ValueError(f"adapter instance already registered: {key}")
            record = AdapterInstanceRecord(
                adapter_id=adapter_id,
                instance_id=iid,
                adapter=adapter,
                metadata=dict(metadata or {}),
            )
            self._records[key] = record
            return record

    def unregister(self, adapter_id: str, instance_id: str = "default") -> bool:
        with self._lock:
            return self._records.pop((adapter_id, instance_id), None) is not None

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

    async def probe(
        self,
        adapter_id: str,
        instance_id: str = "default",
        request: Optional[ProbeRequest] = None,
    ) -> CapabilityProbeReport:
        """对一个实例执行 capability probe 并缓存结果。"""
        record = self._records.get((adapter_id, instance_id))
        if record is None:
            raise KeyError(f"unknown adapter instance: {adapter_id}:{instance_id}")
        req = request or ProbeRequest(runtime_instance_id=instance_id)
        caps = await record.adapter.probe(req)
        report = getattr(record.adapter, "probe_report", lambda: None)()
        if report is None:
            report = CapabilityProbeReport(
                adapter_id=adapter_id, instance_id=instance_id, capabilities=caps)
        with self._lock:
            record.last_probe = report
            record.last_probe_at = utcnow()
        return report

    async def probe_all(self) -> list[CapabilityProbeReport]:
        reports: list[CapabilityProbeReport] = []
        for record in self.instances():
            reports.append(await self.probe(record.adapter_id, record.instance_id))
        return reports

    async def health_snapshot(
        self, adapter_id: str, instance_id: str = "default"
    ) -> dict[str, Any]:
        """实例健康快照：binary/endpoint 可用性、能力摘要与降级清单。

        probe 失败（异常）时返回 ``healthy=False`` 与错误 detail，不抛出。
        """
        record = self._records.get((adapter_id, instance_id))
        if record is None:
            raise KeyError(f"unknown adapter instance: {adapter_id}:{instance_id}")
        try:
            report = await self.probe(adapter_id, instance_id)
        except Exception as exc:  # noqa: BLE001
            return {
                "adapter_id": adapter_id,
                "instance_id": instance_id,
                "healthy": False,
                "detail": str(exc)[:200],
                "capabilities": None,
                "degradations": [],
            }
        caps = report.capabilities
        return {
            "adapter_id": adapter_id,
            "instance_id": instance_id,
            "healthy": report.healthy,
            "detail": report.detail,
            "binary_path": report.binary_path,
            "probed_at": report.probed_at.isoformat(),
            # 公共健康快照保留完整能力位，包括 MCP / Native
            # Tool / ACP-MCP / Agent Plugin / Structured HTTP-RPC。消费方
            # 不需要根据 Adapter ID 反向推断工具接入协议。
            "capabilities": caps.model_dump(mode="json"),
            "capability_injection": describe_injection_path(caps),
            "degradations": list(report.degradations),
        }


__all__ = ["AdapterInstanceRecord", "AdapterRegistry"]
