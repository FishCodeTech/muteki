"""Summarize one fact/intent node into a one-line zh gist for the graph + board.

The "事实图" / "知识黑板" nodes render worker output verbatim — long English
sentences like "[claude] The challenge name's joke ('...one or two RCEs later')
confirms the intended bug — the forward-ref eval() is a genuine RCE primitive..."
which a 184px sticky can only show the head of. We ask deepseek-v4-flash (cheap,
fast) for a short Chinese gist that — top priority — keeps the technical anchors
WHOLE (port / path / payload fragment / CVE / flag value / credential), STORE it
(so we never re-ask), and emit NODE_SUMMARIZED so the deck swaps the truncated raw
text for the gist. The raw text stays available in a <details> disclosure on the
card. The gist is a pure FRONT-END label — it never feeds workers/planner (they
read ev.fact verbatim), so its only failure mode is showing a half-anchor, which
fallback_summary now prevents.

Same shape as apps/web/titler.py: fire-and-forget, never raises (a flaky summary
must never disturb a solve), degrades to a clipped head of the raw text.
"""

from __future__ import annotations

import re
from typing import Any, Optional

from muteki.core.event_bus import EventBus
from muteki.core.events import (
    Event, EventType, hitl_translated_payload, node_summarized_payload,
)
from muteki.core.llm import LLMClient

SUMMARY_MODEL = "deepseek-v4-flash"
# Translation of a worker's hand-raise; same cheap/fast tier as the node gist. The
# caller passes the configured model (titler → planner → this default) so a user who
# reconfigures their LLM profiles gets translations on the same engine.
TRANSLATE_MODEL = "deepseek-v4-flash"

_TRANSLATE_SYSTEM = (
    "你把 CTF 解题器向操作者「举手求助」的一句话翻译成中文。给你一句（通常是英文的）"
    "求助原文，回复一句通顺的中文，让操作者一眼看懂它缺什么 / 环境哪里不可用。最高优先级："
    "完整保留所有技术标识 —— IP、端口、URL、路径、凭证、工具名、错误码、CVE，这些绝不能"
    "省略或改写。简洁直接（约 80 字以内，含完整标识可超出），不要寒暄、不要引号、不要前缀，"
    "只回复翻译后的那一句。"
)

_SYSTEM = (
    "你给 CTF 解题过程中的一条「事实」或「意图」做中文提炼。给你一句（通常是英文的）"
    "原文，回复一句简短的中文，抓住技术要点。最高优先级：完整保留关键标识 —— 端口、"
    "路径、HTTP 方法、payload 片段、CVE 编号、flag 完整值/格式、凭证、IP、工具名，"
    "这些绝不能截断或省略，宁可句子长一点也要保留全。在保留这些的前提下尽量精炼"
    "（约 60 字以内，含完整标识可适当超出）。不要寒暄、不要引号、不要前缀（如「总结："
    "」），只回复提炼后的那一句。"
)

# Technical anchors that must never be cut mid-token in a fallback truncation:
# a flag, a credential, a host:port, a CVE id. The gist is purely a front-end label
# (raw text stays in the card's <details>), so the only harm a fallback can do is
# show a HALF flag/cred — this preserves them whole.
_ANCHOR_RE = re.compile(
    r"(?:flag\w*\{[^}]*\}|bl_[0-9a-f]{8,}|"
    r"\b\d{1,3}(?:\.\d{1,3}){3}(?::\d{2,5})?\b|:\d{2,5}\b|"
    r"CVE-\d{4}-\d+|[A-Za-z0-9_.\\/-]+:[^\s]{4,})",
    re.IGNORECASE)
_FRAGMENT_RE = re.compile(
    r"^[。，、；：！？.,;:!?…）》」』\])\s]"
    r"|^[\u4e00-\u9fff]{1,4}[。.][A-Za-z0-9]"
)

_GIST_MAX = 120  # generous front-end label cap (was 40 — too short for a flag UUID)


def fallback_summary(text: str, max_chars: int = _GIST_MAX) -> str:
    """A clipped head of the raw text — the always-available degraded gist when the
    flash summarizer fails. Strips a leading `[worker]` tag, collapses whitespace.

    NEVER cuts a technical anchor (flag / credential / host:port / CVE) in half: if
    the head up to max_chars would split an anchor, the truncation is extended to
    the anchor's end (the gist is a label; a half-flag is worse than a slightly
    longer line). Otherwise truncates at the last whitespace boundary, not mid-word.
    """
    t = (text or "").strip()
    if t.startswith("["):
        rb = t.find("]")
        if 0 < rb < 20:
            t = t[rb + 1:].strip()
    t = " ".join(t.split())
    if len(t) <= max_chars:
        return t
    # would the cut land inside an anchor? if so, extend to the anchor's end.
    cut = max_chars
    for m in _ANCHOR_RE.finditer(t):
        if m.start() < cut < m.end():
            cut = m.end()
            break
    head = t[:cut]
    # if we didn't have to extend for an anchor, back off to a word boundary.
    if cut == max_chars:
        sp = head.rfind(" ")
        if sp > max_chars * 0.6:   # only if it doesn't shave too much
            head = head[:sp]
    return head


def _clean(raw: str, text: str) -> str:
    """Sanitize the model's answer; fall back to an anchor-safe clipped head if
    unusable. The runaway cap is generous (a faithful gist that keeps every anchor
    can legitimately be long); only truly absurd answers fall back."""
    s = (raw or "").strip().strip("\"'“”‘’").strip()
    s = s.splitlines()[0].strip() if s else ""
    s = s.lstrip("：:").strip()
    # reject empty or runaway answers (model ignored the instruction). 200 is the
    # absurd-cap; a normal anchor-preserving gist stays well under it.
    if not s or len(s) > 200:
        return fallback_summary(text)
    if _FRAGMENT_RE.match(s):
        return fallback_summary(text)
    return s


async def summarize_node(
    text: str,
    *,
    node_kind: str,              # "fact" | "intent"
    fact_seq: int = -1,
    intent_id: str = "",
    shared_graph: Any = None,    # SQLiteSharedGraph — store the gist if given
    llm: Optional[LLMClient] = None,
    bus: Optional[EventBus] = None,
    run_id: Optional[str] = None,
    challenge_id: Optional[str] = None,
) -> str:
    """Compute a zh gist for one node, STORE it, and emit NODE_SUMMARIZED.

    Never raises. The caller runs this detached (asyncio.create_task) so any LLM
    failure degrades to `fallback_summary` and never surfaces as an unhandled
    task. Storing is best-effort and idempotent (record_*_summary patches once)."""
    if not (text or "").strip():
        return ""
    summary = fallback_summary(text)
    owns_llm = llm is None
    try:
        client = llm or LLMClient()
        try:
            resp = await client.chat(
                model=SUMMARY_MODEL,
                messages=[
                    {"role": "system", "content": _SYSTEM},
                    {"role": "user", "content": text[:2000]},
                ],
                temperature=0.3,
                # reasoning model (deepseek-v4-flash): most tokens go to the thinking
                # phase, so 2000 frequently left the ACTUAL one-line answer truncated
                # → flash "failed" → fell back to a hard-clipped head (the high-
                # frequency source of operator-seen "flag2{9f23aa1…" being cut). Give
                # the answer enough room to land after reasoning.
                max_tokens=6000,
                stream=False,
                run_id=run_id,
                challenge_id=challenge_id,
            )
            summary = _clean(resp.content, text)
        finally:
            if owns_llm:
                await client.aclose()
    except Exception:
        summary = summary or fallback_summary(text)

    # store (best-effort, idempotent) — once stored we never re-summarize.
    if shared_graph is not None and summary:
        try:
            if node_kind == "fact" and fact_seq and fact_seq > 0:
                shared_graph.record_fact_summary(fact_seq=fact_seq, summary=summary)
            elif node_kind == "intent" and intent_id:
                shared_graph.record_intent_summary(intent_id=intent_id, summary=summary)
        except Exception:
            pass

    if bus is not None and run_id is not None and summary:
        await bus.emit(
            Event(
                event_type=EventType.NODE_SUMMARIZED,
                run_id=run_id,
                challenge_id=challenge_id,
                payload=node_summarized_payload(
                    summary, node_kind=node_kind,
                    fact_seq=fact_seq, intent_id=intent_id),
            )
        )
    return summary


async def translate_need(
    need: str,
    *,
    worker: str,
    model: Optional[str] = None,
    llm: Optional[LLMClient] = None,
    bus: Optional[EventBus] = None,
    run_id: Optional[str] = None,
    challenge_id: Optional[str] = None,
) -> str:
    """Translate a worker's hand-raise (NEED_INPUT) into zh and emit HITL_TRANSLATED.

    Mirrors summarize_node: fire-and-forget, never raises (a flaky translation must
    never disturb a solve), degrades to the raw text. `model` is the configured
    translation model (titler → planner → TRANSLATE_MODEL); the caller resolves it
    so a user's LLM-profile choice carries through. The deck matches the result to
    the pending card by (worker, need) and swaps the displayed text to zh."""
    raw = (need or "").strip()
    if not raw:
        return ""
    zh = ""
    owns_llm = llm is None
    try:
        client = llm or LLMClient()
        try:
            resp = await client.chat(
                model=model or TRANSLATE_MODEL,
                messages=[
                    {"role": "system", "content": _TRANSLATE_SYSTEM},
                    {"role": "user", "content": raw[:2000]},
                ],
                temperature=0.2,
                max_tokens=6000,   # reasoning model: leave room past the thinking phase
                stream=False,
                run_id=run_id,
                challenge_id=challenge_id,
            )
            zh = _clean(resp.content, raw)
        finally:
            if owns_llm:
                await client.aclose()
    except Exception:
        zh = ""

    # only emit when we got a translation that is actually different from the raw
    # ask (skip a no-op echo, e.g. the worker already wrote in Chinese).
    if zh and zh != raw and bus is not None and run_id is not None:
        await bus.emit(
            Event(
                event_type=EventType.HITL_TRANSLATED,
                run_id=run_id,
                challenge_id=challenge_id,
                payload=hitl_translated_payload(worker, need, zh),
            )
        )
    return zh
