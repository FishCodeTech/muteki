"""Unit tests for env-gated chain-completion force."""

from __future__ import annotations

import os

from muteki.swarm.chain_completion_v1 import (
    BRIEF_PREFIX,
    build_progress_brief,
    enabled,
    should_force_reason,
)


def test_enabled_reads_env(monkeypatch):
    monkeypatch.delenv("MUTEKI_CHAIN_COMPLETION", raising=False)
    assert enabled() is False
    monkeypatch.setenv("MUTEKI_CHAIN_COMPLETION", "1")
    assert enabled() is True


def test_should_force_reason_only_on_fruitless_reap(monkeypatch):
    monkeypatch.setenv("MUTEKI_CHAIN_COMPLETION", "1")
    assert should_force_reason(
        just_reaped=True,
        slots_free=True,
        graph_grew=False,
        flag_count=0,
        need_reason_already=False,
        open_intents=0,
    )
    assert not should_force_reason(
        just_reaped=True,
        slots_free=True,
        graph_grew=True,
        flag_count=0,
        need_reason_already=False,
        open_intents=0,
    )
    assert not should_force_reason(
        just_reaped=True,
        slots_free=True,
        graph_grew=False,
        flag_count=1,
        need_reason_already=False,
        open_intents=0,
    )
    assert not should_force_reason(
        just_reaped=True,
        slots_free=True,
        graph_grew=False,
        flag_count=0,
        need_reason_already=True,
        open_intents=0,
    )
    assert not should_force_reason(
        just_reaped=True,
        slots_free=True,
        graph_grew=False,
        flag_count=0,
        need_reason_already=False,
        open_intents=2,
    )


def test_progress_brief_mentions_follow_up():
    text = build_progress_brief(
        fact_count=0,
        flag_count=0,
        fruitless_workers=2,
        open_intents=0,
        last_goals=["Solve stfu"],
    )
    assert text.startswith(BRIEF_PREFIX)
    assert "NEW" in text
    assert "Solve stfu" in text
    assert "do NOT paraphrase" in text
    assert "FORBIDDEN" in text
    assert "REQUIRED" in text
    assert "discriminating experiment" in text
    assert "whole-challenge bootstrap" in text
