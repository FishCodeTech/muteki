"""比赛平台领域模块（COMP-02 起）。

``base`` 提供所有平台 Adapter 共享的 typed 错误分类、能力探测框架与
官方接口优先的传输选择规则；具体平台（CTFd/rCTF/GZCTF/浏览器）由
COMP-03/04 在本层之上实现。
"""

from muteki.competition.platforms.base import (
    CapabilityProbe,
    PlatformAuthRequiredError,
    PlatformErrorCategory,
    PlatformNotFoundError,
    PlatformPermissionError,
    PlatformRateLimitedError,
    PlatformTimeoutError,
    PlatformTransientError,
    PlatformTransportError,
    PlatformUnknownResultError,
    RateLimitSpec,
    TransportUnavailableError,
    select_transport_kind,
)
from muteki.competition.platforms.browser import (
    BrowserSiteProfile,
    GenericBrowserAdapter,
)

__all__ = [
    "BrowserSiteProfile",
    "CapabilityProbe",
    "GenericBrowserAdapter",
    "PlatformAuthRequiredError",
    "PlatformErrorCategory",
    "PlatformNotFoundError",
    "PlatformPermissionError",
    "PlatformRateLimitedError",
    "PlatformTimeoutError",
    "PlatformTransientError",
    "PlatformTransportError",
    "PlatformUnknownResultError",
    "RateLimitSpec",
    "TransportUnavailableError",
    "select_transport_kind",
]
