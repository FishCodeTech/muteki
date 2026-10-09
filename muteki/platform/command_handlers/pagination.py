"""Signed continuation cursors for ordered, read-only product lists."""
from __future__ import annotations

import base64
import hashlib
import hmac
import json
from typing import Any, Callable

from .base import CommandFailed, HandlerContext, make_error
from .cursor import load_cursor_key
from muteki.platform.contracts.errors import ErrorCategory


def _json(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()


def page_items(items: list[Any], params: dict[str, Any], limit: int, ctx: HandlerContext,
               query_type: str, identity: Callable[[Any], str]) -> tuple[list[Any], dict[str, Any]]:
    """Preserve source ordering and reject changed/missing continuation anchors.

    A new row inserted before the first page cannot shift subsequent pages.
    If an anchor disappears or moves before the page's first row, the caller
    receives a stale-cursor error and can restart the query explicitly.
    """
    binding = ctx.binding
    scope = (f"{binding.binding_id}:{binding.binding_version}" if binding is not None
             else f"{ctx.principal.kind}:{ctx.principal.id}")
    filters = {k: v for k, v in params.items() if k not in {"limit", "cursor"}}
    expected = {"query": query_type, "scope": hashlib.sha256(scope.encode()).hexdigest(),
                "filters": hashlib.sha256(_json(filters)).hexdigest()}
    ids = [str(identity(item)) for item in items]
    if len(set(ids)) != len(ids) or any(not value for value in ids):
        raise CommandFailed(make_error("query.page.identity_invalid", "列表记录缺少唯一 ID",
                                       ErrorCategory.INTERNAL, correlation_id=ctx.correlation_id))

    def invalid(code: str, message: str) -> CommandFailed:
        return CommandFailed(make_error(code, message, ErrorCategory.VALIDATION,
                            correlation_id=ctx.correlation_id,
                            recovery_hint="重新查询时省略 cursor；续读时沿用原筛选条件。"))

    cursor = params.get("cursor")
    key = None
    if cursor or len(items) > limit:
        key = getattr(ctx, "cursor_key", None)
        if key is None:
            key = load_cursor_key(ctx.store.db_path)
    start = 0
    head = ids[0] if ids else ""
    if cursor:
        try:
            prefix, value, signature = str(cursor).split(".")
            if prefix != "qpg1" or len(value) > 8192:
                raise ValueError("invalid cursor format")
            raw = base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))
            if not hmac.compare_digest(signature, hmac.new(key, raw, hashlib.sha256).hexdigest()):
                raise ValueError("invalid signature")
            data = json.loads(raw)
        except (ValueError, TypeError, UnicodeError) as exc:
            raise invalid("query.cursor.invalid", "列表游标格式或签名无效") from exc
        if not isinstance(data, dict) or any(data.get(k) != v for k, v in expected.items()):
            raise invalid("query.cursor.mismatch", "列表游标属于不同的查询、授权范围或筛选条件")
        head = str(data.get("head") or "")
        after = str(data.get("after") or "")
        if head not in ids or after not in ids or ids.index(after) < ids.index(head):
            raise invalid("query.cursor.stale", "列表发生变化，续读位置已失效")
        if data.get("order") != hashlib.sha256(_json(ids[ids.index(head):])).hexdigest():
            raise invalid("query.cursor.stale", "列表记录或顺序发生变化，请重新查询")
        start = ids.index(after) + 1
    selected = items[start:start + limit]
    has_more = start + len(selected) < len(items)
    next_cursor = None
    if has_more:
        raw = _json({**expected, "head": head, "after": str(identity(selected[-1])),
                     "order": hashlib.sha256(_json(ids[ids.index(head):])).hexdigest()})
        value = base64.urlsafe_b64encode(raw).decode().rstrip("=")
        next_cursor = "qpg1." + value + "." + hmac.new(key, raw, hashlib.sha256).hexdigest()
    return selected, {"next_cursor": next_cursor, "has_more": has_more}
