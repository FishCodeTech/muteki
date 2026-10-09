"""Product-level Agent engine registry and support policy.

The registry deliberately distinguishes engines that remain recognizable in
historical data from engines that may be selected for new work.  DeepSeek
Harness (``dsh``) stays recognizable so old events and runs can still render,
but every mutation/execution boundary must reject it with the same stable error.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass
from typing import Any, Iterable


ENGINE_TEMPORARILY_UNSUPPORTED_CODE = "engine_temporarily_unsupported"
DSH_DISABLED_REASON = (
    "DeepSeek Harness 上游 CLI 暂未提供 Worker 所需的结构化事件、工具结果和会话恢复能力，"
    "因此暂不支持。待上游完善后开放。"
)


@dataclass(frozen=True)
class EngineDescriptor:
    id: str
    display_name: str
    support_status: str
    disabled_reason: str = ""
    configurable: bool = True
    executable: bool = True

    @property
    def enabled(self) -> bool:
        return self.support_status == "supported"

    def to_dict(self) -> dict[str, Any]:
        return {**asdict(self), "enabled": self.enabled}


ENGINE_DESCRIPTORS: tuple[EngineDescriptor, ...] = (
    EngineDescriptor("codex", "Codex", "supported"),
    EngineDescriptor("claude", "Claude Code", "supported"),
    EngineDescriptor("cursor", "Cursor", "supported"),
    EngineDescriptor("grok", "Grok", "supported"),
    EngineDescriptor("opencode", "OpenCode", "supported"),
    EngineDescriptor("pi", "Pi", "supported"),
    EngineDescriptor("kimi", "Kimi Code", "supported"),
    EngineDescriptor("omp", "OMP", "supported"),
    EngineDescriptor("devin", "Devin CLI", "supported"),
    EngineDescriptor("droid", "Droid", "supported"),
    EngineDescriptor(
        "dsh",
        "DeepSeek Harness",
        "temporarily_disabled",
        disabled_reason=DSH_DISABLED_REASON,
        configurable=False,
        executable=False,
    ),
)

ENGINE_DESCRIPTOR_BY_ID = {item.id: item for item in ENGINE_DESCRIPTORS}
KNOWN_ENGINE_IDS = tuple(item.id for item in ENGINE_DESCRIPTORS)
SUPPORTED_ENGINE_IDS = tuple(
    item.id for item in ENGINE_DESCRIPTORS if item.enabled
)
TEMPORARILY_DISABLED_ENGINE_IDS = tuple(
    item.id for item in ENGINE_DESCRIPTORS if not item.enabled
)

_ENGINE_ALIASES = {
    "deepseek_harness": "dsh",
    "deepseek-harness": "dsh",
    "dsh_sdk_worker": "dsh",
    "dsh-sdk-worker": "dsh",
    "cli.dsh": "dsh",
    "dsh.sdk": "dsh",
    "deepseek.harness": "dsh",
}


class EngineTemporarilyUnsupportedError(ValueError):
    """Raised when new configuration or execution targets a paused engine."""

    code = ENGINE_TEMPORARILY_UNSUPPORTED_CODE

    def __init__(self, engine: str = "dsh") -> None:
        self.engine = canonical_engine_id(engine) or str(engine or "dsh")
        descriptor = ENGINE_DESCRIPTOR_BY_ID.get(self.engine)
        self.reason = (
            descriptor.disabled_reason if descriptor is not None
            else "该 Agent 引擎暂不支持。"
        )
        super().__init__(self.reason)


def canonical_engine_id(value: Any) -> str:
    """Normalize engine, transport, adapter, and profile-like identifiers."""
    text = str(value or "").strip().lower()
    if not text:
        return ""
    # Runtime references use ``<adapter_id>:<instance_id>``.  Support checks
    # must evaluate the adapter portion as well as bare engine/adapter ids.
    text = text.split(":", 1)[0]
    if text in _ENGINE_ALIASES:
        return _ENGINE_ALIASES[text]
    if text in ENGINE_DESCRIPTOR_BY_ID:
        return text
    if text.startswith("cli."):
        candidate = text.removeprefix("cli.")
        return candidate if candidate in ENGINE_DESCRIPTOR_BY_ID else ""
    # Worker profile ids are conventionally ``<engine>-<kind>-<backend>``.
    prefix = text.split("-", 1)[0]
    return prefix if prefix in ENGINE_DESCRIPTOR_BY_ID else ""


def ensure_engine_supported(value: Any) -> str:
    """Return the normalized engine or raise for a temporarily disabled one."""
    engine = canonical_engine_id(value)
    if engine in TEMPORARILY_DISABLED_ENGINE_IDS:
        raise EngineTemporarilyUnsupportedError(engine)
    return engine


def first_temporarily_disabled_engine(values: Iterable[Any]) -> str:
    for value in values:
        engine = canonical_engine_id(value)
        if engine in TEMPORARILY_DISABLED_ENGINE_IDS:
            return engine
    return ""


_ENGINE_PAYLOAD_FIELDS = {
    "engine",
    "engines",
    "worker_engine",
    "target_engine",
    "transport",
    "adapter_id",
    "runtime_instance_ref",
    "runtime_key",
    "race_engines",
}


def temporarily_disabled_engine_in_payload(value: Any, *, field: str = "") -> str:
    """Find a paused engine in configuration-shaped request data.

    Only engine-bearing fields are inspected.  Free-form prompt text and LLM
    provider names are intentionally ignored so DeepSeek API usage for planner
    or titler remains valid.
    """
    if isinstance(value, dict):
        for key, item in value.items():
            name = str(key or "").strip().lower()
            if name in _ENGINE_PAYLOAD_FIELDS:
                if isinstance(item, (list, tuple, set)):
                    found = first_temporarily_disabled_engine(item)
                else:
                    found = first_temporarily_disabled_engine((item,))
                if found:
                    return found
            if isinstance(item, (dict, list, tuple)):
                found = temporarily_disabled_engine_in_payload(item, field=name)
                if found:
                    return found
        return ""
    if isinstance(value, (list, tuple)):
        for item in value:
            found = temporarily_disabled_engine_in_payload(item, field=field)
            if found:
                return found
        return ""
    if field in _ENGINE_PAYLOAD_FIELDS:
        return first_temporarily_disabled_engine((value,))
    return ""


def descriptor_payload() -> dict[str, Any]:
    engines = [item.to_dict() for item in ENGINE_DESCRIPTORS]
    return {
        "engines": engines,
        "supported": len(SUPPORTED_ENGINE_IDS),
        "registered": len(engines),
    }


__all__ = [
    "DSH_DISABLED_REASON",
    "ENGINE_DESCRIPTORS",
    "ENGINE_DESCRIPTOR_BY_ID",
    "ENGINE_TEMPORARILY_UNSUPPORTED_CODE",
    "EngineDescriptor",
    "EngineTemporarilyUnsupportedError",
    "KNOWN_ENGINE_IDS",
    "SUPPORTED_ENGINE_IDS",
    "TEMPORARILY_DISABLED_ENGINE_IDS",
    "canonical_engine_id",
    "descriptor_payload",
    "ensure_engine_supported",
    "first_temporarily_disabled_engine",
    "temporarily_disabled_engine_in_payload",
]
