"""能力 Binding 的 Bearer 认证令牌编解码（CAP-02，设计 17.5）。

MCP / HTTP-JSONRPC / Agent Plugin 等协议入口用同一个令牌形态把 HTTP
``Authorization: Bearer`` 头映射为 ``BindingContext``：

- 令牌是无状态的自包含 JSON（base64url），携带 binding / grant / thread /
  session / principal / audience / mode 与 grant 的 credential reference；
- 入口只解码、不验权：真正的授权判定（Grant 存在、未撤销、未过期、
  audience / thread / session 匹配）仍由 AgentCapabilityGateway 对
  PlatformStore 完成。令牌内嵌 credential reference 的随机段提供 bearer
  不可猜测性，伪造上下文字段无法通过 Gateway 校验；
- 本模块不保存任何令牌状态： revocation / expiry 以 Store 中的 Grant 为准，
  因此 Binding 实例重启不影响授权撤销状态（CAP-02 验收口径）。

对应核验结论（docs/research/third_party_verification.md §MCP-5）：内部
Runtime 接入采用静态 bearer token 属合规子集；令牌不得放 query string。
"""

from __future__ import annotations

import base64
import binascii
import json
from typing import Any, Mapping, Optional

from muteki.platform.contracts.capabilities import (
    BindingContext,
    CapabilityBinding,
    CapabilityGrant,
    ThreadMode,
)
from muteki.platform.contracts.objects import AgentSession

#: 令牌前缀（版本化，便于将来轮换格式）。
TOKEN_PREFIX = "mtk1."

#: Grant credential reference 的引用前缀。与 CAP-01
#: ``muteki.platform.capability_bindings.CREDENTIAL_REF_PREFIX`` 同值；
#: 这里保持本地常量，避免 Binding 包传递引入 store 层模块。
_GRANT_REF_PREFIX = "secret://capability-grant/"

_REQUIRED_FIELDS = (
    "binding_id", "grant_id", "thread_id", "principal_id", "mode", "cred",
)


class BearerTokenError(ValueError):
    """令牌解码 / 结构校验失败。code 为稳定机器码。"""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


def issue_bearer_token(
    binding: CapabilityBinding,
    grant: CapabilityGrant,
    session: Optional[AgentSession] = None,
) -> str:
    """按 Binding + Grant（+ 可选 AgentSession）签发无状态 bearer token。

    签发方（平台接线 / Adapter）持有 binding 与 grant 契约对象；本函数不做
    任何持久化。grant.credential_ref 必须是 ``secret://capability-grant/``
    引用且路径段与 grant_id 一致，否则拒绝签发（防止把外部凭据本体塞进
    令牌）。
    """
    cred = str(grant.credential_ref or "")
    if not cred.startswith(_GRANT_REF_PREFIX):
        raise BearerTokenError(
            "capability.token.credential_not_reference",
            "grant credential_ref must be a secret://capability-grant/ reference")
    if _grant_id_from_ref(cred) != grant.grant_id:
        raise BearerTokenError(
            "capability.token.credential_mismatch",
            "grant credential_ref does not reference this grant_id")
    payload = {
        "v": 1,
        "binding_id": binding.binding_id,
        "binding_version": binding.binding_version,
        "grant_id": grant.grant_id,
        "thread_id": binding.thread_id,
        "agent_session_id": (
            session.agent_session_id if session is not None
            else grant.agent_session_id),
        "principal_id": binding.principal_id,
        "audience": grant.audience,
        "mode": binding.mode.value if isinstance(binding.mode, ThreadMode)
        else str(binding.mode),
        "cred": cred,
    }
    raw = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    return TOKEN_PREFIX + base64.urlsafe_b64encode(raw).decode("ascii").rstrip("=")


def parse_bearer_token(token: str) -> BindingContext:
    """把 bearer token 解码为 BindingContext；结构非法时抛 BearerTokenError。

    只做格式与自洽校验（credential reference 与 grant_id 匹配）；授权判定
    由 Gateway 完成。
    """
    text = (token or "").strip()
    if not text.startswith(TOKEN_PREFIX):
        raise BearerTokenError(
            "capability.token.malformed", "bearer token must start with 'mtk1.'")
    encoded = text[len(TOKEN_PREFIX):]
    try:
        raw = base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4))
        payload: dict[str, Any] = json.loads(raw.decode("utf-8"))
    except (binascii.Error, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise BearerTokenError(
            "capability.token.malformed", f"bearer token is not valid mtk1: {exc}")
    if not isinstance(payload, dict) or payload.get("v") != 1:
        raise BearerTokenError(
            "capability.token.malformed", "bearer token payload version unsupported")
    missing = [k for k in _REQUIRED_FIELDS if not payload.get(k)]
    if missing:
        raise BearerTokenError(
            "capability.token.malformed",
            f"bearer token payload missing fields: {', '.join(missing)}")
    if _grant_id_from_ref(str(payload["cred"])) != str(payload["grant_id"]):
        raise BearerTokenError(
            "capability.token.credential_mismatch",
            "bearer token credential does not reference its grant_id")
    return BindingContext(
        binding_id=str(payload["binding_id"]),
        binding_version=int(payload.get("binding_version") or 1),
        grant_id=str(payload["grant_id"]),
        thread_id=str(payload["thread_id"]),
        agent_session_id=(str(payload["agent_session_id"])
                          if payload.get("agent_session_id") else None),
        principal_id=str(payload["principal_id"]),
        audience=str(payload.get("audience") or ""),
        mode=ThreadMode(str(payload["mode"])),
    )


def extract_bearer_token(headers: Mapping[str, str]) -> Optional[str]:
    """从请求头取 bearer token（大小写不敏感）；缺失或形态非法返回 None。"""
    value = ""
    for key, item in headers.items():
        if key.lower() == "authorization":
            value = item.strip()
            break
    if not value:
        return None
    scheme, _, token = value.partition(" ")
    if scheme.lower() != "bearer" or not token.strip():
        return None
    return token.strip()


def _grant_id_from_ref(credential_ref: str) -> str:
    """从 ``secret://capability-grant/<grant_id>/<random>`` 取 grant_id。"""
    rest = credential_ref[len(_GRANT_REF_PREFIX):] if credential_ref.startswith(
        _GRANT_REF_PREFIX) else ""
    return rest.split("/", 1)[0] if rest else ""


__all__ = [
    "BearerTokenError",
    "TOKEN_PREFIX",
    "extract_bearer_token",
    "issue_bearer_token",
    "parse_bearer_token",
]
