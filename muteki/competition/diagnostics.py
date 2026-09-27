"""平台连接诊断（任务书 10.3 / 设计 16.1「测试连接」，COMP-02）。

``diagnose_connection`` 对一条 PlatformConnection 执行有界 probe，输出
结构化 ``ConnectionDiagnostic``：逐项检查、建议的连接状态
（active / auth_required / disabled）与探测到的能力。诊断结果只含安全
元数据与 ``secret://`` 引用——真实凭据经 PlatformSecretStore 在
Adapter 内部短暂物化，不进入诊断输出。

本模块只读不写：连接状态的落库与事件发射由 COMP-01 的命令路径
（``competition.connection.test``）负责，诊断结果作为其输入。
"""

from __future__ import annotations

import time
from datetime import datetime
from typing import Awaitable, Callable, Optional

from pydantic import Field

from muteki.platform.contracts.base import ContractModel, utcnow
from muteki.platform.contracts.modules import (
    PlatformCapabilities,
    PlatformConnectionRef,
)
from muteki.competition.models import ConnectionStatus
from muteki.competition.platforms.base import (
    PlatformErrorCategory,
    PlatformTransportError,
    TransportUnavailableError,
)

#: probe 的默认总时限（秒）；超时按 TIMEOUT 分类记录。
DEFAULT_PROBE_TIMEOUT_SECONDS = 30.0


class ConnectionCheck(ContractModel):
    """单项检查的结果。"""

    name: str = ""                        # resolve_credential / probe / …
    ok: bool = False
    category: str = ""                    # PlatformErrorCategory.value；空 = 无错误
    message: str = ""
    latency_ms: float = 0.0


class ConnectionDiagnostic(ContractModel):
    """一次连接诊断的完整结果（安全元数据，可进入 API 响应与事件）。"""

    connection_id: str = ""
    platform_kind: str = ""
    ok: bool = False
    # 建议写入 PlatformConnection.status 的值
    suggested_status: str = ConnectionStatus.DISABLED.value
    capabilities: Optional[PlatformCapabilities] = None
    checks: list[ConnectionCheck] = Field(default_factory=list)
    probed_at: datetime = Field(default_factory=utcnow)


#: 错误分类 → 建议连接状态。permission / not_found 等视为配置问题
#: （disabled）；transient / timeout 不改连接状态（保持 active 由上层
#: 决定重试）；automation 被组织者关闭属 permission。
_STATUS_BY_CATEGORY: dict[PlatformErrorCategory, str] = {
    PlatformErrorCategory.AUTH_REQUIRED: ConnectionStatus.AUTH_REQUIRED.value,
    PlatformErrorCategory.PERMISSION: ConnectionStatus.DISABLED.value,
    PlatformErrorCategory.NOT_FOUND: ConnectionStatus.DISABLED.value,
    PlatformErrorCategory.UNAVAILABLE: ConnectionStatus.DISABLED.value,
    PlatformErrorCategory.TIMEOUT: ConnectionStatus.ACTIVE.value,
    PlatformErrorCategory.RATE_LIMITED: ConnectionStatus.ACTIVE.value,
    PlatformErrorCategory.TRANSIENT: ConnectionStatus.ACTIVE.value,
    PlatformErrorCategory.UNKNOWN_RESULT: ConnectionStatus.ACTIVE.value,
    PlatformErrorCategory.INVALID_RESPONSE: ConnectionStatus.ACTIVE.value,
    PlatformErrorCategory.PLATFORM: ConnectionStatus.ACTIVE.value,
}


def suggested_status_for_error(exc: PlatformTransportError) -> str:
    """typed 错误 → 建议连接状态（competition.connection.test 使用）。"""
    return _STATUS_BY_CATEGORY.get(exc.category, ConnectionStatus.ACTIVE.value)


async def diagnose_connection(
    probe: Callable[[PlatformConnectionRef], Awaitable[PlatformCapabilities]],
    connection: PlatformConnectionRef,
    *,
    resolve_credential: Optional[Callable[[], Awaitable[None]]] = None,
) -> ConnectionDiagnostic:
    """执行连接诊断。

    ``probe`` 是 Adapter 的 probe 入口（签名与契约 PlatformAdapter 一致）。
    ``resolve_credential`` 可选：单独验证凭据引用可解析（值立即丢弃）。
    """
    checks: list[ConnectionCheck] = []

    if resolve_credential is not None:
        started = time.monotonic()
        try:
            await resolve_credential()
        except PlatformTransportError as exc:
            checks.append(_check("resolve_credential", started, exc))
            return _diagnostic(connection, checks, None)
        except Exception as exc:  # 存储层错误等：不暴露细节类型以外的信息
            checks.append(
                ConnectionCheck(
                    name="resolve_credential",
                    ok=False,
                    category=PlatformErrorCategory.PLATFORM.value,
                    message=f"credential resolution failed ({type(exc).__name__})",
                    latency_ms=_elapsed_ms(started),
                )
            )
            return _diagnostic(connection, checks, None)
        checks.append(
            ConnectionCheck(
                name="resolve_credential", ok=True,
                latency_ms=_elapsed_ms(started),
            )
        )

    started = time.monotonic()
    try:
        capabilities = await probe(connection)
    except TransportUnavailableError as exc:
        checks.append(_check("probe", started, exc))
        return _diagnostic(connection, checks, None)
    except PlatformTransportError as exc:
        checks.append(_check("probe", started, exc))
        return _diagnostic(connection, checks, None)
    checks.append(
        ConnectionCheck(name="probe", ok=True, latency_ms=_elapsed_ms(started))
    )
    return _diagnostic(connection, checks, capabilities)


def _check(
    name: str, started: float, exc: PlatformTransportError
) -> ConnectionCheck:
    return ConnectionCheck(
        name=name,
        ok=False,
        category=exc.category.value,
        # 异常消息约定为安全内容（状态码/路径/引用），见 base 层注释。
        message=str(exc),
        latency_ms=_elapsed_ms(started),
    )


def _diagnostic(
    connection: PlatformConnectionRef,
    checks: list[ConnectionCheck],
    capabilities: Optional[PlatformCapabilities],
) -> ConnectionDiagnostic:
    ok = bool(checks) and all(check.ok for check in checks)
    suggested = ConnectionStatus.ACTIVE.value
    if not ok:
        failed = next((check for check in checks if not check.ok), None)
        if failed is not None and failed.category:
            try:
                category = PlatformErrorCategory(failed.category)
            except ValueError:
                category = PlatformErrorCategory.PLATFORM
            suggested = _STATUS_BY_CATEGORY.get(
                category, ConnectionStatus.DISABLED.value
            )
    return ConnectionDiagnostic(
        connection_id=connection.connection_id,
        platform_kind=connection.platform_kind,
        ok=ok,
        suggested_status=suggested,
        capabilities=capabilities,
        checks=checks,
    )


def _elapsed_ms(started: float) -> float:
    return round((time.monotonic() - started) * 1000.0, 3)


__all__ = [
    "ConnectionCheck",
    "ConnectionDiagnostic",
    "DEFAULT_PROBE_TIMEOUT_SECONDS",
    "diagnose_connection",
    "suggested_status_for_error",
]
