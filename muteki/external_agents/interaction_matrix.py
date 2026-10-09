"""Conversation 面向用户的交互能力矩阵（C24）。

把 Provider descriptor 声明基线、probe 布尔字段和会话目录 revision 收成统一的
supported / limited / unsupported / unknown / expired 行，供菜单与正文
控件共用；静态未探测不得显示成确定支持。
"""

from __future__ import annotations

from typing import Any, Literal, Optional

from pydantic import Field

from muteki.external_agents.capabilities import (
    BOOL_CAPABILITY_FIELDS,
    SOURCE_PROBE,
    SOURCE_STATIC,
    CapabilityProbeReport,
)
from muteki.external_agents.descriptors import (
    AdapterDescriptor,
    find_adapter_descriptor,
    find_descriptor_for_adapter,
    resolve_capabilities,
)
from muteki.external_agents.runtime_capabilities import RuntimeCapabilitySnapshot
from muteki.platform.contracts.base import ContractModel
from muteki.platform.contracts.external_agents import AgentCapabilities

# Operation method of Muteki's own history rebuild (not a native rewind).
HISTORY_REBUILD_METHOD = "muteki.history.rebuild"

InteractionCapabilityLevel = Literal[
    "supported", "limited", "unsupported", "unknown", "expired",
]
InteractionCapabilityKey = Literal[
    "steer",
    "interrupt",
    "attachments",
    "approval",
    "user_input",
    "plan",
    "native_history",
    "rewind",
    "fork",
    "command_catalog",
]

_MATRIX_KEYS: tuple[InteractionCapabilityKey, ...] = (
    "steer",
    "interrupt",
    "attachments",
    "approval",
    "user_input",
    "plan",
    "native_history",
    "rewind",
    "fork",
    "command_catalog",
)

_BOOL_KEY_MAP: dict[InteractionCapabilityKey, str] = {
    "steer": "steer",
    "interrupt": "interrupt",
    "approval": "approval",
    "user_input": "user_input",
    "plan": "plan",
    "native_history": "resume",
    "fork": "fork",
}

_DEFAULT_REASONS: dict[tuple[str, InteractionCapabilityLevel], str] = {
    ("steer", "unsupported"): "当前 Runtime 没有带内引导通道",
    ("steer", "unknown"): "尚未实测引导能力，不能当作已支持",
    ("interrupt", "unsupported"): "当前 Runtime 无法中断正在执行的回合",
    ("interrupt", "unknown"): "尚未实测中断能力",
    ("attachments", "limited"): "Runtime 不接受原生多模态附件",
    ("attachments", "unsupported"): "当前 Runtime 无法接收附件",
    ("attachments", "unknown"): "附件能力尚未实测",
    ("approval", "unsupported"): "当前 Runtime 不支持带内审批",
    ("approval", "unknown"): "审批能力尚未实测，不能当作已支持",
    ("user_input", "unsupported"): "当前 Runtime 不支持多问题 / 用户输入请求",
    ("user_input", "unknown"): "多问题能力尚未实测",
    ("plan", "unknown"): "计划能力尚未由 Runtime 确认",
    ("plan", "unsupported"): "当前 Runtime 不暴露结构化计划事件",
    ("plan", "supported"): "Runtime 会上报结构化计划 / 任务进度",
    ("plan", "expired"): "计划能力探测已过期，正在刷新",
    ("native_history", "unsupported"): "切换后无法继续同一原生会话历史",
    ("native_history", "unknown"): "原生历史连续性尚未实测",
    ("native_history", "limited"): "仅能尽力恢复会话，连续性不保证",
    ("rewind", "unknown"): "原生回退 / rewind 尚未确认",
    ("rewind", "unsupported"): "当前 Runtime 不支持原生回退",
    ("fork", "unsupported"): "当前 Runtime 不支持分叉会话",
    ("fork", "unknown"): "分叉能力尚未实测",
    ("command_catalog", "expired"): "命令目录已过期，正在刷新",
    ("command_catalog", "unknown"): "命令目录尚未就绪",
    ("command_catalog", "limited"): "目录刷新中，仅显示已确认项",
}

_DEFAULT_ALTERNATIVES: dict[InteractionCapabilityKey, str] = {
    "steer": "可改用停止执行，再发送一条新消息调整方向",
    "interrupt": "可等待当前回合结束，或关闭该 Runtime 会话后重开",
    "attachments": "可把文件放到工作区，并在消息里写明路径",
    "approval": "可在 Runtime 启动参数中固化权限模式，或改用支持审批的接入",
    "user_input": "可把多个问题写成一条消息依次说明",
    "plan": "可用普通消息描述计划步骤，待 Runtime 支持后再用专用入口",
    "native_history": "Muteki 会注入整理后的当前分支历史；需要原生连续性时请留在当前 Provider",
    "rewind": "可手动编辑草稿后重新发送，或开新 Thread",
    "fork": "可新建 Thread 并复制关键上下文",
    "command_catalog": "请等待刷新完成后再调用 `/` 命令",
}


class InteractionCapabilityRow(ContractModel):
    """矩阵中的一行交互能力。"""

    key: InteractionCapabilityKey
    level: InteractionCapabilityLevel
    reason: str = ""
    alternative: str = ""
    source: str = SOURCE_STATIC
    invocable: bool = False


class InteractionCapabilityMatrix(ContractModel):
    """与命令目录共用 revision 的交互能力矩阵。"""

    adapter_id: str = ""
    instance_id: str = "default"
    revision: int = 0
    stale: bool = False
    rows: list[InteractionCapabilityRow] = Field(default_factory=list)
    diagnostics: list[str] = Field(default_factory=list)

    def row(self, key: str) -> Optional[InteractionCapabilityRow]:
        needle = str(key or "").strip()
        return next((item for item in self.rows if item.key == needle), None)

    def level_of(self, key: str) -> InteractionCapabilityLevel:
        found = self.row(key)
        return found.level if found is not None else "unknown"

    def public_rows(self) -> list[dict[str, Any]]:
        return [item.model_dump(mode="json") for item in self.rows]

    def as_map(self) -> dict[str, dict[str, Any]]:
        return {item.key: item.model_dump(mode="json") for item in self.rows}


def _reason_for(
    key: InteractionCapabilityKey,
    level: InteractionCapabilityLevel,
) -> str:
    return _DEFAULT_REASONS.get((key, level), "")


def _alternative_for(
    key: InteractionCapabilityKey,
    level: InteractionCapabilityLevel,
) -> str:
    if level == "supported":
        return ""
    return _DEFAULT_ALTERNATIVES.get(key, "")


def _measured_level(
    *,
    value: bool,
    source: str,
    stale: bool,
    declared_absent: bool,
) -> InteractionCapabilityLevel:
    """Level of one boolean capability.

    ``declared_absent``: the adapter descriptor declares no code path for the
    field and nothing measured it, which is unsupported regardless of probe
    freshness. Other static values are unverified and stay ``unknown``.
    """
    if source == SOURCE_STATIC:
        return "unsupported" if declared_absent else "unknown"
    if stale:
        return "expired"
    return "supported" if value else "unsupported"


def _resolve(
    adapter_id: str,
    capabilities: AgentCapabilities | None,
    field_sources: dict[str, str] | None,
) -> tuple[AgentCapabilities, dict[str, str], Optional[AdapterDescriptor]]:
    """Overlay the probe on the adapter's declared baseline when it has one."""
    adapter = find_adapter_descriptor(adapter_id)
    descriptor = find_descriptor_for_adapter(adapter_id)
    if adapter is None or descriptor is None:
        caps = capabilities or AgentCapabilities(capability_source=SOURCE_STATIC)
        overall = str(caps.capability_source or SOURCE_STATIC)
        sources = {name: overall for name in BOOL_CAPABILITY_FIELDS}
        sources.update(field_sources or {})
        return caps, sources, None
    caps, sources = resolve_capabilities(
        descriptor,
        capabilities,
        adapter_id=adapter.adapter_id,
        probe_sources=field_sources,
    )
    return caps, sources, adapter


def build_interaction_matrix(
    *,
    adapter_id: str,
    instance_id: str = "default",
    capabilities: AgentCapabilities | None = None,
    field_sources: dict[str, str] | None = None,
    degradations: list[str] | None = None,
    revision: int = 0,
    stale: bool = False,
    snapshot: RuntimeCapabilitySnapshot | None = None,
    diagnostics: list[str] | None = None,
) -> InteractionCapabilityMatrix:
    """从 probe / 快照构造交互矩阵。

    规则：
    - 已知 adapter 先用 Provider descriptor 的声明基线叠加 probe；
    - ``static`` 来源的 True 最多落到 ``unknown``（未实测不宣称支持），
      descriptor 声明没有实现路径的 False 落到 ``unsupported``；
    - ``stale`` 时已有实测行降为 ``expired``；
    - ``attachments``：``image_input`` 为假但工作区路径可用 → ``limited``；
    - ``rewind``：会话目录里的 ``rewind`` operation 决定可用性，Muteki 历史
      重建（``muteki.history.rebuild``）只算 ``limited``；
    - probe degradations 原文进入 diagnostics，不按关键词映射到行。
    """
    caps, sources, adapter = _resolve(adapter_id, capabilities, field_sources)
    declared = adapter.capabilities if adapter is not None else None

    def declared_absent(field_name: str) -> bool:
        return declared is not None and not bool(getattr(declared, field_name))

    def bool_row(
        key: InteractionCapabilityKey, field_name: str,
    ) -> tuple[InteractionCapabilityLevel, str]:
        source = sources.get(field_name, SOURCE_STATIC)
        level = _measured_level(
            value=bool(getattr(caps, field_name)),
            source=source,
            stale=stale,
            declared_absent=declared_absent(field_name),
        )
        return level, source

    rows: list[InteractionCapabilityRow] = []
    for key in _MATRIX_KEYS:
        if key == "attachments":
            level, source = bool_row(key, "image_input")
            if level == "unsupported":
                # Conversation 仍可把附件落到工作区路径（C03）。
                level = "limited"
            rows.append(InteractionCapabilityRow(
                key=key,
                level=level,
                reason=_reason_for(key, level),
                alternative=_alternative_for(key, level),
                source=source,
                invocable=level in {"supported", "limited"},
            ))
            continue

        if key == "command_catalog":
            snap = snapshot
            if snap is None:
                level = "unknown"
                source = SOURCE_STATIC
            elif stale or snap.stale:
                level = "expired" if snap.revision > 0 else "unknown"
                source = SOURCE_PROBE if snap.revision > 0 else SOURCE_STATIC
            else:
                verified = any(
                    item.verification == "verified"
                    and item.kind in {"command", "skill", "operation"}
                    for item in snap.items
                )
                level = "supported" if verified else "limited"
                source = SOURCE_PROBE
            rows.append(InteractionCapabilityRow(
                key=key,
                level=level,
                reason=_reason_for(key, level),
                alternative=_alternative_for(key, level),
                source=source,
                invocable=level == "supported",
            ))
            continue

        if key == "rewind":
            operation = None
            if snapshot is not None:
                operation = next((
                    item for item in snapshot.items
                    if item.kind == "operation"
                    and item.name == "rewind"
                    and item.verification == "verified"
                ), None)
            if operation is None:
                level = "unknown"
                source = SOURCE_STATIC
            elif stale:
                level = "expired"
                source = SOURCE_PROBE
            elif operation.invocation.get("method") == HISTORY_REBUILD_METHOD:
                level = "limited"
                source = SOURCE_PROBE
            else:
                level = "supported"
                source = SOURCE_PROBE
            rows.append(InteractionCapabilityRow(
                key=key,
                level=level,
                reason=("回退会重建引擎会话并传入保留的聊天历史；工作区文件与外部操作保持原状"
                        if level == "limited" else _reason_for(key, level)),
                alternative=_alternative_for(key, level),
                source=source,
                invocable=level in {"supported", "limited"},
            ))
            continue

        field_name = _BOOL_KEY_MAP[key]
        level, source = bool_row(key, field_name)
        alternative = _alternative_for(key, level)
        if key == "steer" and level != "supported":
            interrupt_level, _ = bool_row("interrupt", "interrupt")
            if interrupt_level == "supported":
                alternative = "可使用停止执行中断当前回合，再发送新消息"
        rows.append(InteractionCapabilityRow(
            key=key,
            level=level,
            reason=_reason_for(key, level),
            alternative=alternative,
            source=source,
            invocable=level == "supported",
        ))

    diag = list(diagnostics or [])
    diag.extend(str(item) for item in degradations or [])
    if stale:
        diag.append("交互能力矩阵已过期，正在刷新；请勿把过期项当作确定支持")
    return InteractionCapabilityMatrix(
        adapter_id=adapter_id,
        instance_id=instance_id,
        revision=int(revision),
        stale=bool(stale),
        rows=rows,
        diagnostics=diag,
    )


def build_matrix_from_probe(
    report: CapabilityProbeReport | None,
    *,
    revision: int = 0,
    stale: bool = False,
    snapshot: RuntimeCapabilitySnapshot | None = None,
    diagnostics: list[str] | None = None,
) -> InteractionCapabilityMatrix:
    """从 ``CapabilityProbeReport`` 构造矩阵。"""
    if report is None:
        return build_interaction_matrix(
            adapter_id=snapshot.adapter_id if snapshot else "",
            instance_id=snapshot.instance_id if snapshot else "default",
            revision=revision,
            stale=True,
            snapshot=snapshot,
            diagnostics=diagnostics,
        )
    return build_interaction_matrix(
        adapter_id=report.adapter_id,
        instance_id=report.instance_id,
        capabilities=report.capabilities,
        field_sources=report.field_sources,
        degradations=report.degradations,
        revision=revision,
        stale=stale,
        snapshot=snapshot,
        diagnostics=diagnostics,
    )


def matrix_delta(
    before: InteractionCapabilityMatrix | None,
    after: InteractionCapabilityMatrix | None,
    *,
    keys: tuple[InteractionCapabilityKey, ...] = (
        "native_history", "attachments", "approval", "plan", "rewind",
    ),
) -> list[dict[str, Any]]:
    """Provider 切换时的行级变化（验收标准 2）。"""
    changes: list[dict[str, Any]] = []
    before_map = before.as_map() if before is not None else {}
    after_map = after.as_map() if after is not None else {}
    for key in keys:
        left = before_map.get(key) or {
            "key": key, "level": "unknown", "reason": "", "alternative": "",
        }
        right = after_map.get(key) or {
            "key": key, "level": "unknown", "reason": "", "alternative": "",
        }
        if left.get("level") == right.get("level"):
            continue
        changes.append({
            "key": key,
            "from_level": left.get("level"),
            "to_level": right.get("level"),
            "reason": right.get("reason") or "",
            "alternative": right.get("alternative") or "",
        })
    return changes


def attach_matrix_to_snapshot(
    snapshot: RuntimeCapabilitySnapshot,
    report: CapabilityProbeReport | None,
) -> RuntimeCapabilitySnapshot:
    """把矩阵写入快照（同 revision / stale）。"""
    matrix = build_matrix_from_probe(
        report,
        revision=snapshot.revision,
        stale=snapshot.stale,
        snapshot=snapshot,
        diagnostics=list(snapshot.diagnostics),
    )
    return snapshot.model_copy(update={"matrix": matrix})


__all__ = [
    "InteractionCapabilityKey",
    "InteractionCapabilityLevel",
    "InteractionCapabilityMatrix",
    "InteractionCapabilityRow",
    "attach_matrix_to_snapshot",
    "build_interaction_matrix",
    "build_matrix_from_probe",
    "matrix_delta",
]
