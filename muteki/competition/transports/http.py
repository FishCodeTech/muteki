"""比赛平台 REST 传输（任务书 10.3 / 设计 8.3 第 1 顺位，COMP-02）。

基于项目现有依赖 ``httpx``（pyproject 主依赖）。职责边界：

- timeout、Retry-After 解析（秒数与 HTTP-date 两种形态）、分页、
  ETag/条件请求与统一 typed 错误分类（``classify_status`` 默认映射；
  错误响应体的安全摘录随 ``error.detail["response_body"]`` 携带，
  CTFd 这类 200/403 混合语义由具体 Adapter 在解析响应体后改写）。
- 支持 cookie 会话（GZCTF 的 ASP.NET Identity cookie）：调用方可传入
  ``httpx.Cookies`` 或使用 ``login_form`` 这类 Adapter 侧流程后复用
  同一 client 的 cookie jar。
- 凭据由 Adapter 在调用前经 ``PlatformSecretStore.resolve`` 短暂物化并
  以 ``headers`` 传入；本模块不持久化任何凭据，错误消息不含请求头。

测试可注入 ``httpx.MockTransport``（构造参数 ``transport``），无需真实
网络。
"""

from __future__ import annotations

import email.utils
import json
from collections.abc import AsyncIterator, Awaitable, Callable
from datetime import datetime, timezone
from typing import Any, Optional

import httpx
from pydantic import Field

from muteki.platform.contracts.base import ContractModel
from muteki.competition.platforms.base import (
    PlatformTimeoutError,
    PlatformTransientError,
    PlatformTransportError,
    classify_status,
)

#: 连接级保守默认超时（秒）：connect / read / write / pool。
DEFAULT_TIMEOUT = httpx.Timeout(30.0, connect=10.0)


class RestResponse(ContractModel):
    """一次 REST 调用的安全结果；``headers`` 只保留安全的响应头子集。"""

    status_code: int = 0
    etag: str = ""
    not_modified: bool = False            # 304：条件请求命中缓存
    json_body: Any = None
    text: str = ""
    retry_after_seconds: Optional[float] = None
    response_headers: dict[str, str] = Field(default_factory=dict)


#: 允许进入 RestResponse / 日志的响应头（限速、缓存与诊断相关，不含 Set-Cookie）。
_SAFE_RESPONSE_HEADERS = frozenset({
    "etag", "retry-after", "x-ratelimit-limit", "x-ratelimit-remaining",
    "x-ratelimit-reset", "content-type", "link", "cache-control",
})

#: 错误响应体摘录上限（字节）。平台错误载荷是安全元数据（状态码、错误
#: kind、message），供具体 Adapter 做二次分类：CTFd 的 200/403 混合语义
#: （``data.status``）与 rCTF 的 ``kind`` 字符串分发都需要读取错误体。
#: 请求头与凭据永远不会出现在响应体里，因此可安全放入 error.detail。
_ERROR_BODY_LIMIT = 2048


def _error_body_detail(response: httpx.Response) -> Any:
    """从错误响应提取安全摘录（JSON 对象或截断文本），供 typed 错误携带。"""
    content_type = response.headers.get("content-type", "")
    if "json" in content_type:
        try:
            return response.json()
        except json.JSONDecodeError:
            pass
    return response.text[:_ERROR_BODY_LIMIT]


def parse_retry_after(value: Optional[str]) -> Optional[float]:
    """解析 Retry-After 头：秒数或 HTTP-date（RFC 9110 §10.2.3）。"""
    if not value:
        return None
    value = value.strip()
    try:
        return max(0.0, float(value))
    except ValueError:
        pass
    try:
        parsed = email.utils.parsedate_to_datetime(value)
    except (TypeError, ValueError):
        return None
    if parsed is None:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return max(0.0, (parsed - datetime.now(timezone.utc)).total_seconds())


class RestTransport:
    """面向单个平台连接的 REST 客户端。

    ``base_url`` 使用 ``muteki.competition.commands.canonicalize_base_url``
    规范化后的值。``default_headers`` 可携带平台认证头（如 CTFd 的
    ``Authorization: Token …``）；这些头只用于请求，不进入任何返回值或
    错误消息。
    """

    def __init__(
        self,
        base_url: str,
        *,
        default_headers: Optional[dict[str, str]] = None,
        cookies: Optional[httpx.Cookies] = None,
        timeout: Optional[httpx.Timeout] = None,
        transport: Optional[httpx.AsyncBaseTransport] = None,
        max_body_bytes: int = 64 * 1024 * 1024,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._max_body_bytes = max_body_bytes
        self._client = httpx.AsyncClient(
            base_url=self._base_url,
            headers=dict(default_headers or {}),
            cookies=cookies,
            timeout=timeout or DEFAULT_TIMEOUT,
            transport=transport,
            follow_redirects=False,
            trust_env=False,
        )

    @property
    def base_url(self) -> str:
        return self._base_url

    @property
    def cookies(self) -> httpx.Cookies:
        """cookie jar（GZCTF 会话续期用）；不要把内容写入日志。"""
        return self._client.cookies

    async def aclose(self) -> None:
        await self._client.aclose()

    async def __aenter__(self) -> "RestTransport":
        return self

    async def __aexit__(self, *exc_info: object) -> None:
        await self.aclose()

    # ------------------------------------------------------------------
    # 基础请求
    # ------------------------------------------------------------------

    async def request(
        self,
        method: str,
        path: str,
        *,
        params: Optional[dict[str, Any]] = None,
        json_body: Any = None,
        headers: Optional[dict[str, str]] = None,
        context: str = "",
    ) -> RestResponse:
        """发送请求并按状态码做默认错误分类。

        2xx/304 返回 ``RestResponse``；其余状态抛 typed 错误
        （``PlatformTransportError`` 子类）。超时与网络错误映射为
        ``PlatformTimeoutError`` / ``PlatformTransientError``。
        """
        context = context or f"{method} {path}"
        try:
            response = await self._client.request(
                method,
                path,
                params=params,
                json=json_body,
                headers=headers,
            )
        except httpx.TimeoutException as exc:
            raise PlatformTimeoutError(f"{context}: timed out") from exc
        except httpx.TransportError as exc:
            raise PlatformTransientError(
                f"{context}: transport error ({type(exc).__name__})"
            ) from exc

        if response.status_code == 304:
            return RestResponse(
                status_code=304,
                not_modified=True,
                response_headers=self._safe_headers(response),
            )
        retry_after = parse_retry_after(response.headers.get("retry-after"))
        if not 200 <= response.status_code < 300:
            error = classify_status(
                response.status_code,
                retry_after_seconds=retry_after,
                context=context,
            )
            # 平台错误载荷供 Adapter 二次分类（CTFd data.status / rCTF kind）。
            error.detail.setdefault("response_body", _error_body_detail(response))
            raise error
        return self._build_response(response, retry_after)

    async def get(self, path: str, **kwargs: Any) -> RestResponse:
        return await self.request("GET", path, **kwargs)

    async def post(self, path: str, **kwargs: Any) -> RestResponse:
        return await self.request("POST", path, **kwargs)

    async def get_conditional(
        self,
        path: str,
        *,
        etag: str = "",
        **kwargs: Any,
    ) -> RestResponse:
        """ETag 条件请求（GZCTF /api/Game 等广泛支持 304）。

        命中缓存时返回 ``not_modified=True`` 的空体响应；否则等同 ``get``。
        """
        headers = dict(kwargs.pop("headers", None) or {})
        if etag:
            headers["If-None-Match"] = etag
        return await self.request("GET", path, headers=headers, **kwargs)

    # ------------------------------------------------------------------
    # 分页与流式下载
    # ------------------------------------------------------------------

    async def paginate(
        self,
        fetch_page: Callable[[Any], Awaitable[RestResponse]],
        extract_items: Callable[[RestResponse], list[Any]],
        next_state: Callable[[RestResponse], Any],
        *,
        initial_state: Any = None,
        max_pages: int = 100,
    ) -> AsyncIterator[Any]:
        """通用分页迭代器。

        三种分页形态由具体 Adapter 以闭包表达，本方法只负责循环与上限：

        - 页码（CTFd ``meta.pagination.next``）：``next_state`` 返回下一页码；
        - offset/limit（rCTF）：state 为偏移量；
        - cursor（含 GZCTF count/skip 换算）：state 为游标字符串。

        ``next_state`` 返回 ``None`` 表示没有下一页。
        """
        state = initial_state
        for _ in range(max_pages):
            response = await fetch_page(state)
            for item in extract_items(response):
                yield item
            state = next_state(response)
            if state is None:
                return
        raise PlatformTransportError(
            f"pagination exceeded {max_pages} pages"
        )

    async def download(self, url: str, *, context: str = "") -> bytes:
        """下载附件字节流（签名 URL 或 /Assets/ 路径），超限拒绝。"""
        context = context or f"GET {url}"
        try:
            async with self._client.stream("GET", url) as response:
                if not 200 <= response.status_code < 300:
                    raise classify_status(
                        response.status_code,
                        retry_after_seconds=parse_retry_after(
                            response.headers.get("retry-after")
                        ),
                        context=context,
                    )
                chunks: list[bytes] = []
                total = 0
                async for chunk in response.aiter_bytes():
                    total += len(chunk)
                    if total > self._max_body_bytes:
                        raise PlatformTransportError(
                            f"{context}: body exceeds {self._max_body_bytes} bytes"
                        )
                    chunks.append(chunk)
                return b"".join(chunks)
        except httpx.TimeoutException as exc:
            raise PlatformTimeoutError(f"{context}: timed out") from exc
        except httpx.TransportError as exc:
            raise PlatformTransientError(
                f"{context}: transport error ({type(exc).__name__})"
            ) from exc

    # ------------------------------------------------------------------
    # 内部
    # ------------------------------------------------------------------

    def _build_response(
        self, response: httpx.Response, retry_after: Optional[float]
    ) -> RestResponse:
        body: Any = None
        text = ""
        content_type = response.headers.get("content-type", "")
        if "json" in content_type:
            try:
                body = response.json()
            except json.JSONDecodeError:
                text = response.text
        else:
            text = response.text
        return RestResponse(
            status_code=response.status_code,
            etag=response.headers.get("etag", ""),
            json_body=body,
            text=text,
            retry_after_seconds=retry_after,
            response_headers=self._safe_headers(response),
        )

    @staticmethod
    def _safe_headers(response: httpx.Response) -> dict[str, str]:
        # 只保留白名单响应头；Set-Cookie 与平台自定义认证头永不外泄。
        return {
            name: value
            for name, value in response.headers.items()
            if name.lower() in _SAFE_RESPONSE_HEADERS
        }


__all__ = [
    "DEFAULT_TIMEOUT",
    "RestResponse",
    "RestTransport",
    "parse_retry_after",
]
