"""Node-summarizer: zh gist for fact/intent nodes (deepseek-flash, stored once)."""

from __future__ import annotations

import asyncio
import os
import tempfile

import pytest

from muteki.core.events import EventType, node_summarized_payload
from muteki.models.solve_graph import Challenge
from muteki.solver.summarizer import (
    _clean, fallback_summary, summarize_node, translate_need,
)
from muteki.swarm.shared_graph import SQLiteSharedGraph


def _graph() -> SQLiteSharedGraph:
    ch = Challenge(id="t1", name="t", category="web", points=0, description="")
    db = os.path.join(tempfile.mkdtemp(), "g.db")
    return SQLiteSharedGraph(db, ch)


def test_fallback_strips_worker_tag_and_clips():
    raw = "[claude] The challenge confirms an eval() RCE primitive via forward-ref"
    out = fallback_summary(raw, max_chars=40)
    assert "claude" not in out and len(out) <= 40
    assert out.startswith("The challenge")


def test_clean_rejects_runaway_and_falls_back():
    # runaway cap raised 80→200 (P5: a faithful gist keeping every anchor can be
    # long); only a truly absurd answer (>200) falls back.
    assert _clean("x" * 250, "[w] real fact here") == fallback_summary("[w] real fact here")
    # a 200-char anchor-preserving gist is now ACCEPTED (not runaway).
    assert _clean("y" * 200, "[w] real fact") == "y" * 200
    assert _clean('  "提炼后的一句"  ', "raw") == "提炼后的一句"
    assert _clean("：去掉前缀冒号", "raw") == "去掉前缀冒号"


def test_clean_rejects_mid_sentence_fragments():
    raw = "[cursor] Beta real flag is in HTTP header X-Muteki-Flag, not the body"
    bad = "中输出。Beta 真实 flag 在 HTTP 头 X-Muteki-Flag，非响应体。"
    assert _clean(bad, raw) == fallback_summary(raw)
    assert _clean("Beta 真实 flag 在 HTTP 头 X-Muteki-Flag，非响应体。", raw).startswith("Beta")


def test_payload_shape():
    p = node_summarized_payload("中文提炼", node_kind="fact", fact_seq=7)
    assert p == {"summary": "中文提炼", "node_kind": "fact", "fact_seq": 7, "intent_id": ""}


def test_fact_summary_written_back_to_payload():
    g = _graph()
    fs = g.add_evidence(actor="cli-claude", source="claude",
                        fact="a long fact about :3010 backend smuggling", verified=True)
    assert g.record_fact_summary(fact_seq=fs, summary="真flag在:3010后端") is True
    import json
    row = g._conn.execute("SELECT payload FROM events WHERE seq=?", (fs,)).fetchone()
    assert json.loads(row[0])["summary"] == "真flag在:3010后端"
    # guards
    assert g.record_fact_summary(fact_seq=999999, summary="x") is False
    assert g.record_fact_summary(fact_seq=fs, summary="") is False
    g.close()


def test_intent_summary_written_back_to_column():
    g = _graph()
    g.propose_intent(actor="reason", intent_id="intent:x",
                     goal="Exploit the eval RCE via forward-ref")
    assert g.record_intent_summary(intent_id="intent:x", summary="利用前向引用eval RCE") is True
    r = g._conn.execute("SELECT summary FROM intents WHERE intent_id=?", ("intent:x",)).fetchone()
    assert r[0] == "利用前向引用eval RCE"
    assert g.record_intent_summary(intent_id="nope", summary="x") is False
    g.close()


class _FakeBus:
    def __init__(self):
        self.events = []

    async def emit(self, ev):
        self.events.append(ev)


class _FakeLLM:
    """Stands in for LLMClient.chat — returns a canned zh gist."""
    async def chat(self, **kw):
        class R:
            content = "真flag在:3010后端,需smuggling绕nginx"
        return R()


def test_summarize_node_stores_and_emits():
    g = _graph()
    fs = g.add_evidence(actor="cli-claude", source="claude",
                        fact="a sufficiently long english fact about the backend on port 3010",
                        verified=True)
    bus = _FakeBus()
    out = asyncio.run(summarize_node(
        "a sufficiently long english fact about the backend on port 3010",
        node_kind="fact", fact_seq=fs, shared_graph=g,
        llm=_FakeLLM(), bus=bus, run_id="run-x", challenge_id="t1"))
    assert out == "真flag在:3010后端,需smuggling绕nginx"
    # stored on the graph
    import json
    row = g._conn.execute("SELECT payload FROM events WHERE seq=?", (fs,)).fetchone()
    assert json.loads(row[0])["summary"] == out
    # emitted exactly one NODE_SUMMARIZED carrying the gist + node id
    assert len(bus.events) == 1
    ev = bus.events[0]
    assert ev.event_type is EventType.NODE_SUMMARIZED
    assert ev.payload["summary"] == out
    assert ev.payload["fact_seq"] == fs
    assert ev.payload["node_kind"] == "fact"
    g.close()


def test_summarize_node_empty_text_is_noop():
    bus = _FakeBus()
    out = asyncio.run(summarize_node("", node_kind="fact", fact_seq=1, bus=bus, run_id="r"))
    assert out == ""
    assert bus.events == []


class _FakeLLMZh:
    async def chat(self, **kw):
        class R:
            content = "需要一个公网 VPS 做反弹 shell（我在 NAT 后），目标 10.0.0.5:4444"
        return R()


def test_translate_need_emits_hitl_translated():
    from muteki.solver.summarizer import translate_need
    bus = _FakeBus()
    out = asyncio.run(translate_need(
        "Need a public VPS for a reverse shell (I'm behind NAT), target 10.0.0.5:4444",
        worker="cli-codex-1", llm=_FakeLLMZh(), bus=bus, run_id="run-x", challenge_id="t1"))
    assert "VPS" in out and "10.0.0.5:4444" in out, "keeps the technical anchors"
    assert len(bus.events) == 1
    ev = bus.events[0]
    assert ev.event_type is EventType.HITL_TRANSLATED
    assert ev.payload["worker"] == "cli-codex-1"
    assert ev.payload["need_zh"] == out
    assert ev.payload["need"].startswith("Need a public VPS")


def test_translate_need_empty_is_noop():
    bus = _FakeBus()
    out = asyncio.run(translate_need("", worker="w", bus=bus, run_id="r"))
    assert out == ""
    assert bus.events == []


def test_translate_need_skips_when_zh_equals_raw():
    """If the worker already wrote Chinese (model echoes it), don't emit a no-op."""
    from muteki.solver.summarizer import translate_need

    class _Echo:
        async def chat(self, **kw):
            class R:
                content = "目标 10.0.0.5 连不上"
            return R()
    bus = _FakeBus()
    out = asyncio.run(translate_need("目标 10.0.0.5 连不上", worker="w",
                                     llm=_Echo(), bus=bus, run_id="r"))
    assert out == "目标 10.0.0.5 连不上"
    assert bus.events == [], "no HITL_TRANSLATED when translation == raw"
