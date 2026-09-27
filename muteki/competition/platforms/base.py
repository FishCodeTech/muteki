"""平台 Adapter 共享基础层（任务书 10.3 / 设计 8.2、8.3，COMP-02）。

本层冻结三件事，供 COMP-03/04 的具体平台 Adapter 直接实现：

1. **typed 错误分类**：所有 transport 与 Adapter 抛出的平台侧错误统一为
   ``PlatformTransportError`` 子类，携带 ``PlatformErrorCategory``、
   ``retryable`` 与 ``retry_after_seconds``。SubmissionService 状态机
   （设计 9.4 的 rate_limited / auth_required / unknown / transient）
   直接按 category 分支，不再解析消息文本。错误消息只含状态码、路径
   与引用，绝不含请求头或 secret 值。
2. **能力探测框架**：``CapabilityProbe`` 汇总一次 probe 的观察结果，
   ``build_capabilities`` 生成契约 ``PlatformCapabilities``，限速参数
   标注来源（``platform`` / ``response_header`` / ``connection_default``）；
   无法获得平台参数时使用连接级保守默认并显式标注（设计 8.2 末段）。
3. **官方接口优先选择规则**：``select_transport_kind`` 实现
   REST > 授权浏览器（设计 8.3）；REST 可用时浏览器不会成为默认传输。
"""

from __future__ import annotations

from datetime import datetime
from enum import Enum
from typing import Any, Optional

from pydantic import Field

from muteki.competition.models import RevisionArtifact
from muteki.platform.contracts.base import ContractModel, utcnow
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.modules import PlatformCapabilities


# ---------------------------------------------------------------------------
# typed 错误分类
# ---------------------------------------------------------------------------


class PlatformErrorCategory(str, Enum):
    """平台侧错误的稳定分类；与 SubmissionState 的失败分支一一对应。"""

    TIMEOUT = "timeout"                    # 连接/读超时 → transient_failure
    RATE_LIMITED = "rate_limited"          # 429 / 平台限速 → rate_limited
    AUTH_REQUIRED = "auth_required"        # 认证失效/未授权 → auth_required
    PERMISSION = "permission"              # 已认证但无权限（不可重试）
    NOT_FOUND = "not_found"                # 对象不存在（tombstone 候选）
    UNKNOWN_RESULT = "unknown_result"      # 提交已发出但结果丢失 → unknown
    TRANSIENT = "transient"                # 5xx / 网络抖动 → transient_failure
    INVALID_RESPONSE = "invalid_response"  # 响应形态与契约不符
    PLATFORM = "platform"                  # 其他平台侧错误
    UNAVAILABLE = "unavailable"            # transport 不可用（如未装浏览器）


#: category → 统一错误 envelope 的 ErrorCategory 映射（任务书 6.1）。
_CATEGORY_MAP: dict[PlatformErrorCategory, ErrorCategory] = {
    PlatformErrorCategory.TIMEOUT: ErrorCategory.TIMEOUT,
    PlatformErrorCategory.RATE_LIMITED: ErrorCategory.RATE_LIMIT,
    PlatformErrorCategory.AUTH_REQUIRED: ErrorCategory.PERMISSION,
    PlatformErrorCategory.PERMISSION: ErrorCategory.PERMISSION,
    PlatformErrorCategory.NOT_FOUND: ErrorCategory.NOT_FOUND,
    PlatformErrorCategory.UNKNOWN_RESULT: ErrorCategory.PLATFORM,
    PlatformErrorCategory.TRANSIENT: ErrorCategory.PLATFORM,
    PlatformErrorCategory.INVALID_RESPONSE: ErrorCategory.PLATFORM,
    PlatformErrorCategory.PLATFORM: ErrorCategory.PLATFORM,
    PlatformErrorCategory.UNAVAILABLE: ErrorCategory.RUNTIME,
}


class PlatformTransportError(RuntimeError):
    """平台传输/适配层错误基类。

    ``message`` 只允许出现状态码、方法、路径、``secret://`` 引用等安全
    内容；transport 实现负责在构造消息前剥离请求头与凭据。
    """

    category: PlatformErrorCategory = PlatformErrorCategory.PLATFORM
    retryable: bool = False

    def __init__(
        self,
        message: str,
        *,
        status_code: Optional[int] = None,
        retry_after_seconds: Optional[float] = None,
        detail: Optional[dict[str, Any]] = None,
    ) -> None:
        super().__init__(message)
        self.status_code = status_code
        self.retry_after_seconds = retry_after_seconds
        self.detail = dict(detail or {})

    @property
    def error_category(self) -> ErrorCategory:
        """映射到统一错误 envelope 的分类。"""
        return _CATEGORY_MAP[self.category]


class PlatformTimeoutError(PlatformTransportError):
    category = PlatformErrorCategory.TIMEOUT
    retryable = True


class PlatformRateLimitedError(PlatformTransportError):
    """平台限速；``retry_after_seconds`` 来自 Retry-After / 平台错误载荷。"""

    category = PlatformErrorCategory.RATE_LIMITED
    retryable = True


class PlatformAuthRequiredError(PlatformTransportError):
    """认证缺失或失效：连接应转 ``auth_required``，暂停外部动作。"""

    category = PlatformErrorCategory.AUTH_REQUIRED
    retryable = False


class PlatformPermissionError(PlatformTransportError):
    category = PlatformErrorCategory.PERMISSION
    retryable = False


class PlatformNotFoundError(PlatformTransportError):
    category = PlatformErrorCategory.NOT_FOUND
    retryable = False


class PlatformUnknownResultError(PlatformTransportError):
    """请求可能已被平台受理，但结果未能取回（提交后断流等）。

    对应设计 9.4 的 ``unknown``：禁止直接重复提交，必须由 reconciler
    核对远端状态。
    """

    category = PlatformErrorCategory.UNKNOWN_RESULT
    retryable = False


class PlatformTransientError(PlatformTransportError):
    category = PlatformErrorCategory.TRANSIENT
    retryable = True


class TransportUnavailableError(PlatformTransportError):
    """transport 本身不可用（如 Playwright 未安装）；属明确降级，不崩溃。"""

    category = PlatformErrorCategory.UNAVAILABLE
    retryable = False


def classify_status(
    status_code: int,
    *,
    retry_after_seconds: Optional[float] = None,
    context: str = "",
) -> PlatformTransportError:
    """HTTP 状态码 → typed 错误的默认映射。

    平台特异的 200/403 混合语义（如 CTFd ``data.status``）由具体 Adapter
    在解析响应体后改写，不走这里。
    """
    label = context or "platform request"
    if status_code in (401,):
        return PlatformAuthRequiredError(
            f"{label}: authentication required (HTTP {status_code})",
            status_code=status_code,
        )
    if status_code == 403:
        return PlatformPermissionError(
            f"{label}: permission denied (HTTP {status_code})",
            status_code=status_code,
        )
    if status_code == 404:
        return PlatformNotFoundError(
            f"{label}: not found (HTTP {status_code})", status_code=status_code
        )
    if status_code == 429:
        return PlatformRateLimitedError(
            f"{label}: rate limited (HTTP {status_code})",
            status_code=status_code,
            retry_after_seconds=retry_after_seconds,
        )
    if 500 <= status_code < 600:
        return PlatformTransientError(
            f"{label}: server error (HTTP {status_code})", status_code=status_code
        )
    return PlatformTransportError(
        f"{label}: unexpected status (HTTP {status_code})", status_code=status_code
    )


# ---------------------------------------------------------------------------
# 限速配置（设计 8.2：优先平台参数，缺省用连接级保守配置并标注来源）
# ---------------------------------------------------------------------------


class RateLimitSource(str, Enum):
    PLATFORM = "platform"                  # 平台能力 / 配置显式给出
    RESPONSE_HEADER = "response_header"    # 从响应头 / 错误载荷实测
    CONNECTION_DEFAULT = "connection_default"  # 连接级保守默认


class RateLimitSpec(ContractModel):
    """提交/请求的限速参数；``source`` 标注数值来源。"""

    max_attempts: Optional[int] = None     # 窗口内最大次数；None 表示未知
    window_seconds: Optional[float] = None
    cooldown_seconds: float = 30.0         # 两次提交间的最小间隔
    source: str = RateLimitSource.CONNECTION_DEFAULT.value


#: 无法从平台获得限速参数时的连接级保守默认（设计 8.2 末段）。
CONSERVATIVE_RATE_LIMIT = RateLimitSpec(
    max_attempts=None,
    window_seconds=None,
    cooldown_seconds=30.0,
    source=RateLimitSource.CONNECTION_DEFAULT.value,
)


# ---------------------------------------------------------------------------
# 能力探测框架（设计 8.2）
# ---------------------------------------------------------------------------


class CapabilityProbe(ContractModel):
    """一次连接 probe 的结构化观察结果。

    具体平台 Adapter 在 probe 中填充本模型，再经 ``build_capabilities``
    生成契约 ``PlatformCapabilities`` 存到 ``PlatformConnection.capabilities``。
    所有字段只允许安全元数据；凭据一律是 ``secret://`` 引用。
    """

    platform_kind: str = ""
    probed_at: datetime = Field(default_factory=utcnow)
    # 认证方式，例如 ["token_header"] / ["bearer"] / ["cookie_session"] /
    # ["browser_session"]；以及 token 权限级别等安全元数据
    auth_methods: list[str] = Field(default_factory=list)
    auth_detail: dict[str, Any] = Field(default_factory=dict)
    # 同步方式：full / cursor / etag / webhook
    sync_modes: list[str] = Field(default_factory=list)
    attachments: bool = False
    hints: bool = False
    prerequisites: bool = False
    multi_flag: bool = False
    # 动态实例
    dynamic_instances: bool = False
    instance_ttl_seconds: Optional[int] = None
    instance_renewable: bool = False
    instance_stoppable: bool = False
    max_instances: Optional[int] = None
    # 提交
    submit: bool = False
    submit_async: bool = False              # 异步判定（GZCTF 形态）
    max_submit_attempts: Optional[int] = None
    rate_limit: RateLimitSpec = Field(
        default_factory=lambda: CONSERVATIVE_RATE_LIMIT.model_copy()
    )
    scoreboard: bool = False
    # 平台/比赛是否允许自动化；False 时调度必须阻止自动调用（设计 8.3）
    automation_allowed: bool = True
    automation_note: str = ""
    # 平台版本 / schema 快照等附加证据
    evidence: dict[str, Any] = Field(default_factory=dict)

    def build_capabilities(self) -> PlatformCapabilities:
        """生成契约模型；未知字段进 ``detail``，保持 schema 稳定。"""
        return PlatformCapabilities(
            platform_kind=self.platform_kind,
            sync=bool(self.sync_modes),
            artifacts=self.attachments,
            dynamic_instances=self.dynamic_instances,
            submit=self.submit,
            scoreboard=self.scoreboard,
            detail={
                "probed_at": self.probed_at.isoformat(),
                "auth_methods": list(self.auth_methods),
                "auth_detail": dict(self.auth_detail),
                "sync_modes": list(self.sync_modes),
                "hints": self.hints,
                "prerequisites": self.prerequisites,
                "multi_flag": self.multi_flag,
                "instance_ttl_seconds": self.instance_ttl_seconds,
                "instance_renewable": self.instance_renewable,
                "instance_stoppable": self.instance_stoppable,
                "max_instances": self.max_instances,
                "submit_async": self.submit_async,
                "max_submit_attempts": self.max_submit_attempts,
                "rate_limit": self.rate_limit.model_dump(mode="json"),
                "automation_allowed": self.automation_allowed,
                "automation_note": self.automation_note,
                "evidence": dict(self.evidence),
            },
        )


# ---------------------------------------------------------------------------
# 官方接口优先的传输选择（设计 8.3：REST > 授权浏览器）
# ---------------------------------------------------------------------------

TRANSPORT_REST = "rest"
TRANSPORT_BROWSER = "browser"

#: 优先级数值越小越优先；浏览器永远是最后手段。
_TRANSPORT_PRIORITY: dict[str, int] = {
    TRANSPORT_REST: 0,
    TRANSPORT_BROWSER: 1,
}


def select_transport_kind(
    available: list[str] | tuple[str, ...] | set[str],
    *,
    automation_allowed: bool = True,
) -> str:
    """在平台声明可用的传输中选择默认传输。

    规则（设计 8.3）：

    - 官方 REST > 授权浏览器；
    - 浏览器不能成为已有 REST 接口平台的默认传输——只要 REST 可用，
      本函数就不会返回 ``browser``；
    - 平台或比赛禁止自动化（``automation_allowed=False``）时抛
      ``PlatformPermissionError``，任何传输（含浏览器）都不得绕过该策略。
    """
    if not automation_allowed:
        raise PlatformPermissionError(
            "platform automation is disabled by organizer policy"
        )
    candidates = [k for k in available if k in _TRANSPORT_PRIORITY]
    if not candidates:
        raise TransportUnavailableError(
            f"no usable transport among: {sorted(available)}"
        )
    return min(candidates, key=lambda k: _TRANSPORT_PRIORITY[k])


# ---------------------------------------------------------------------------
# Adapter → SyncService 交接模型（COMP-03 扩展；设计 8.1 sync() 的 upsert 载荷）
# ---------------------------------------------------------------------------


class RemoteChallengeSnapshot(ContractModel):
    """一次平台同步产生的单题快照，由具体 Adapter 产出、SyncService 消费。

    字段与 ``CompetitionChallenge`` / ``ChallengeRevision``（COMP-01）一一对应：
    前半部分是题目级身份与远端状态，``points`` 起是 revision 内容字段。
    ``ChallengeRevision`` 是 ``extra="forbid"`` 的冻结契约，自定义 challenge
    type 的平台特有字段不进契约模型，统一保留在 ``revision_extensions``；
    平台原始对象完整保留在 ``raw_payload``，供审计与后续 schema 演进核对。
    附件的 sha256 在同步时通常未知（选手侧 API 不给哈希），先以空串占位，
    下载落 CAS 后由 SyncService 回填（设计 7.2）。
    """

    external_challenge_id: str = ""
    name: str = ""
    category: str = ""
    remote_state: str = "open"          # open / closed / solved_remote / hidden
    hidden: bool = False                # 未解锁占位题（CTFd type=="hidden" 等）
    # ---- 以下为 revision 内容字段 ----
    points: float = 0.0
    description: str = ""
    target: str = ""                    # 稳定目标描述，绝不含动态实例地址
    flag_format: str = ""
    hints: list[str] = Field(default_factory=list)
    prerequisites: list[str] = Field(default_factory=list)
    multi_flag: bool = False
    expected_flags: int = 1
    artifacts: list[RevisionArtifact] = Field(default_factory=list)
    revision_extensions: dict[str, Any] = Field(default_factory=dict)
    raw_payload: dict[str, Any] = Field(default_factory=dict)


__all__ = [
    "CapabilityProbe",
    "CONSERVATIVE_RATE_LIMIT",
    "PlatformAuthRequiredError",
    "PlatformErrorCategory",
    "PlatformNotFoundError",
    "PlatformPermissionError",
    "PlatformRateLimitedError",
    "PlatformTimeoutError",
    "PlatformTransientError",
    "PlatformTransportError",
    "PlatformUnknownResultError",
    "RateLimitSource",
    "RateLimitSpec",
    "RemoteChallengeSnapshot",
    "TRANSPORT_BROWSER",
    "TRANSPORT_REST",
    "TransportUnavailableError",
    "classify_status",
    "select_transport_kind",
]
