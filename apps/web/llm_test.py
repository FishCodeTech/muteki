"""Test-connectivity for the planner/titler LLM endpoint (DESIGN §2.4 補強C-1).

Tests the values the operator is EDITING (passed in the request body), not the
saved config — so a freshly-typed base_url/model is what gets tested.

Key correctness rule (reviewer P3): judge `ok` by API success, NOT by non-empty
content. The configured models are reasoning models (muteki/core/llm.py header):
tokens go to `reasoning_content` first, so a small cap can return empty `content`
on a perfectly healthy endpoint. We use the client's default cap and treat "chat
returned without raising" as success.

The API key is NOT taken from the request — it stays in .env
(MUTEKI_DEEPSEEK_API_KEY). base_url empty → default DeepSeek endpoint.
"""

from __future__ import annotations

from typing import Any, Optional


async def test_llm_endpoint(
    *,
    which: str,
    base_url: Optional[str] = None,
    model: Optional[str] = None,
) -> dict[str, Any]:
    """Make one minimal chat against the (edited) endpoint. Never raises."""
    from muteki.core.llm import LLMClient

    which = (which or "").strip() or "planner"
    base_url = (base_url or "").strip()
    model = (model or "").strip()
    if not model:
        return {"ok": False, "detail": "model 不能为空", "model": ""}

    client = LLMClient(base_url=base_url) if base_url else LLMClient()
    try:
        # default cap (generous) so a reasoning model's content isn't starved.
        resp = await client.chat(
            model=model,
            messages=[{"role": "user", "content": "ping"}],
            temperature=0.0,
            stream=False,
        )
        # ok = the call SUCCEEDED. content may be empty on a reasoning model and
        # that's still a healthy endpoint (reviewer P3) — do NOT require content.
        fr = getattr(resp, "finish_reason", "") or ""
        if fr == "error":
            return {"ok": False, "detail": "endpoint 返回 error finish_reason", "model": model}
        return {"ok": True, "detail": "端点可达，凭据有效", "model": model}
    except Exception as exc:  # noqa: BLE001
        return {"ok": False, "detail": str(exc)[:200], "model": model}
    finally:
        try:
            await client.aclose()
        except Exception:  # noqa: BLE001
            pass
