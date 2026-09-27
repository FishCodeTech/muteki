"""事件游标：绑定 aggregate/stream 与授权范围的不透明值（任务书 6.2，COMMAND-01）。

格式：``v1.<base64url(payload)>.<base64url(hmac-sha256 截断签名)>``，
payload 为 ``{"at": aggregate_type, "aid": aggregate_id, "seq": n, "scp": 授权范围摘要}``。

- 签名密钥来自 ``MUTEKI_CURSOR_SECRET``，未设置时在 platform.db 旁生成一次
  随机 sidecar 文件（0600），同一数据库重启后游标继续有效。
- 校验时拒绝：签名不符、aggregate 与请求不一致（跨 Competition/Run/Thread
  复用）、调用方授权范围与游标绑定范围不一致。协议 Binding 不得自行解析或
  重新编号游标。
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import secrets
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

#: 游标格式版本前缀；不兼容变更时递增。
_CURSOR_PREFIX = "v1"
#: HMAC 签名截断长度（字节）。
_SIG_BYTES = 24


class CursorError(ValueError):
    """游标无法解析、签名不符或 scope 绑定不匹配。"""


@dataclass(frozen=True)
class CursorInfo:
    """解码后的游标内容。"""

    aggregate_type: str
    aggregate_id: str
    seq: int
    #: 授权范围摘要（principal / binding 标识的 sha256 截断），空串表示未绑定
    scope_digest: str = ""


def scope_digest(scope: str) -> str:
    """授权范围（如 ``operator:local-user`` 或 binding_id）的稳定摘要。"""
    text = str(scope or "").strip()
    if not text:
        return ""
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:16]


def load_cursor_key(db_path: str | Path) -> bytes:
    """加载或生成游标签名密钥。

    优先取环境变量 ``MUTEKI_CURSOR_SECRET``；否则在 ``<db_path>.cursor-key``
    持久化一份随机密钥（首次创建，0600），保证重启后历史游标仍可校验。
    """
    env = os.environ.get("MUTEKI_CURSOR_SECRET", "").strip()
    if env:
        return hashlib.sha256(env.encode("utf-8")).digest()
    key_path = Path(str(db_path) + ".cursor-key")
    if key_path.exists():
        return bytes.fromhex(key_path.read_text(encoding="utf-8").strip())
    key = secrets.token_bytes(32)
    key_path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(str(key_path), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as fh:
        fh.write(key.hex())
    return key


def _b64e(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).decode("ascii").rstrip("=")


def _b64d(text: str) -> bytes:
    pad = "=" * (-len(text) % 4)
    return base64.urlsafe_b64decode(text + pad)


def encode_cursor(
    key: bytes,
    aggregate_type: str,
    aggregate_id: str,
    seq: int,
    *,
    scope: str = "",
) -> str:
    """生成绑定 (aggregate_type, aggregate_id, scope) 的不透明游标。"""
    payload = json.dumps(
        {
            "at": str(aggregate_type),
            "aid": str(aggregate_id),
            "seq": int(seq),
            "scp": scope_digest(scope),
        },
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")
    sig = hmac.new(key, payload, hashlib.sha256).digest()[:_SIG_BYTES]
    return f"{_CURSOR_PREFIX}.{_b64e(payload)}.{_b64e(sig)}"


def decode_cursor(key: bytes, cursor: str) -> CursorInfo:
    """解析并校验签名；任何不符抛 ``CursorError``。"""
    text = str(cursor or "").strip()
    parts = text.split(".")
    if len(parts) != 3 or parts[0] != _CURSOR_PREFIX:
        raise CursorError("malformed event cursor")
    try:
        payload = _b64d(parts[1])
        sig = _b64d(parts[2])
        data = json.loads(payload.decode("utf-8"))
    except Exception as exc:  # base64 / json 解析失败
        raise CursorError("malformed event cursor") from exc
    expected = hmac.new(key, payload, hashlib.sha256).digest()[:_SIG_BYTES]
    if not hmac.compare_digest(sig, expected):
        raise CursorError("event cursor signature mismatch")
    try:
        return CursorInfo(
            aggregate_type=str(data["at"]),
            aggregate_id=str(data["aid"]),
            seq=int(data["seq"]),
            scope_digest=str(data.get("scp") or ""),
        )
    except (KeyError, TypeError, ValueError) as exc:
        raise CursorError("malformed event cursor payload") from exc


def assert_cursor_scope(
    key: bytes,
    cursor: Optional[str],
    aggregate_type: str,
    aggregate_id: str,
    *,
    scope: str = "",
) -> int:
    """校验游标并返回起始 stream_seq（cursor 为空时返回 0）。

    拒绝跨 aggregate（Competition/Run/Thread）复用；调用方给出授权范围
    （``scope``）且游标绑定了范围时，两者摘要必须一致。
    """
    if not cursor:
        return 0
    info = decode_cursor(key, cursor)
    if info.aggregate_type != str(aggregate_type) or info.aggregate_id != str(aggregate_id):
        raise CursorError(
            "event cursor is bound to "
            f"{info.aggregate_type}/{info.aggregate_id}, not "
            f"{aggregate_type}/{aggregate_id}"
        )
    if scope and info.scope_digest and info.scope_digest != scope_digest(scope):
        raise CursorError("event cursor is bound to a different authorization scope")
    return info.seq


__all__ = [
    "CursorError",
    "CursorInfo",
    "assert_cursor_scope",
    "decode_cursor",
    "encode_cursor",
    "load_cursor_key",
    "scope_digest",
]
