"""Unit tests for env-gated mid-flight fruitless interrupt (round 3)."""

from __future__ import annotations

from types import SimpleNamespace

from muteki.swarm.fruitless_interrupt_v1 import (
    CONSTRAINT_PREFIX,
    DEFAULT_ARTIFACT_EXTRA_S,
    DEFAULT_MAX_INTERRUPTS,
    DEFAULT_MAX_REBOOTSTRAPS,
    DEFAULT_SETTLE_S,
    DEFAULT_THRESHOLD_S,
    PACKET_PREFIX,
    REQUIRED_PACKET_MARKERS,
    REQUIRED_PACKET_MARKERS_FORENSICS,
    build_artifact_chain_intent_goal,
    build_discriminating_constraint,
    build_working_packet,
    collect_named_artifacts,
    commit_harvested_facts,
    crypto_hypothesis_eligible,
    enabled,
    explore_interrupt_enabled,
    extract_crypto_clues,
    harvest_artifact_tool_facts,
    harvest_enabled,
    infer_replan_domain,
    max_empty_reason_retries,
    max_interrupts,
    max_reboots,
    packet_has_named_artifact,
    packet_meets_replan_quality,
    reason_failure_kind,
    settle_seconds,
    should_inject_artifact_chain_intent,
    should_interrupt_worker,
    should_rebootstrap_after_reason,
    should_retry_empty_reason,
    should_soft_continue_after_retire_miss,
    threshold_seconds,
    worker_artifact_progress,
)


def test_enabled_reads_env(monkeypatch):
    monkeypatch.delenv("MUTEKI_FRUITLESS_INTERRUPT", raising=False)
    assert enabled() is False
    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT", "1")
    assert enabled() is True


def test_explore_gate_reads_env(monkeypatch):
    monkeypatch.delenv("MUTEKI_FRUITLESS_INTERRUPT_EXPLORE", raising=False)
    assert explore_interrupt_enabled() is False
    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT_EXPLORE", "1")
    assert explore_interrupt_enabled() is True


def test_threshold_and_max_defaults_and_floors(monkeypatch):
    monkeypatch.delenv("MUTEKI_FRUITLESS_INTERRUPT_S", raising=False)
    monkeypatch.delenv("MUTEKI_FRUITLESS_INTERRUPT_MAX", raising=False)
    assert threshold_seconds() == DEFAULT_THRESHOLD_S
    assert max_interrupts() == DEFAULT_MAX_INTERRUPTS

    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT_S", "90")
    assert threshold_seconds() == 90.0
    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT_S", "0")
    assert threshold_seconds() == 30.0

    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT_MAX", "2")
    assert max_interrupts() == 2
    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT_MAX", "0")
    assert max_interrupts() == 1


def test_zero_board_fact_gate_protects_pass_path(monkeypatch):
    """Once any verified fact exists, never mid-flight interrupt."""
    monkeypatch.delenv("MUTEKI_FRUITLESS_INTERRUPT_EXPLORE", raising=False)
    assert should_interrupt_worker(
        running_for_s=200.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="bootstrap",
    )
    # Board has a fact — keep the worker (round-2 life regression fix).
    assert not should_interrupt_worker(
        running_for_s=999.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=1,
        flags_now=0,
        worker_mode="bootstrap",
    )
    assert not should_interrupt_worker(
        running_for_s=999.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=1,
        worker_mode="bootstrap",
    )


def test_explore_requires_explicit_allow(monkeypatch):
    monkeypatch.delenv("MUTEKI_FRUITLESS_INTERRUPT_EXPLORE", raising=False)
    assert not should_interrupt_worker(
        running_for_s=200.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="explore",
    )
    assert should_interrupt_worker(
        running_for_s=200.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="explore",
        allow_explore=True,
    )
    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT_EXPLORE", "1")
    assert should_interrupt_worker(
        running_for_s=200.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="explore",
    )


def test_should_interrupt_threshold_and_review():
    assert not should_interrupt_worker(
        running_for_s=60.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="bootstrap",
    )
    assert not should_interrupt_worker(
        running_for_s=999.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        is_review=True,
        worker_mode="bootstrap",
    )


def test_per_worker_clock_under_zero_board():
    """Fresh sibling under threshold is kept even on a flat zero-fact board."""
    assert should_interrupt_worker(
        running_for_s=200.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="bootstrap",
    )
    assert not should_interrupt_worker(
        running_for_s=10.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="bootstrap",
    )


def test_working_packet_is_short_and_actionable():
    # Zero facts → bootstrap packet (file→strings→xxd).
    text = build_working_packet(
        None,
        fact_count=0,
        flag_count=0,
        fruitless_workers=1,
        open_intents=0,
        interrupted_worker="cli-claude",
        interrupted_goal="Solve the whole challenge",
        running_for_s=160.0,
        last_goals=["Solve the whole challenge", "grep for flag"],
        named_artifacts=["stfu", "cipher.bin"],
        crypto_clues=[],
    )
    assert text.startswith(PACKET_PREFIX)
    assert "do NOT paraphrase" in text
    assert "Solve the whole challenge" in text
    assert "SAME worker" in text
    assert "file, then strings, then xxd" in text
    assert "FORBIDDEN" in text
    assert "REQUIRED" in text
    assert "whole-challenge bootstrap" in text
    assert "Named artifacts" in text
    assert "`stfu`" in text
    assert packet_has_named_artifact(text, name="stfu")
    assert packet_meets_replan_quality(text)
    assert len(text) < 1400


def test_working_packet_post_harvest_demands_crypto_hypothesis():
    clues = [
        "[file:stfu] ELF 32-bit LSB executable, Intel 80386",
        "[strings:stfu] Supplied tap values",
        "[file:stfu] const=0x0300000001000200",
    ]
    text = build_working_packet(
        None,
        fact_count=3,
        flag_count=0,
        interrupted_worker="cli-claude",
        interrupted_goal="Run file strings xxd on stfu",
        last_goals=["Run file, strings, and xxd on `stfu`"],
        named_artifacts=["stfu"],
        crypto_clues=clues,
    )
    assert "Crypto clues" in text
    assert "Supplied tap values" in text
    assert "falsifiable" in text
    assert "xor/LFSR" in text or "LFSR" in text
    assert "redoing file→strings→xxd" in text
    assert "file, then strings, then xxd" not in text
    assert packet_meets_replan_quality(text)
    for marker in REQUIRED_PACKET_MARKERS:
        assert marker in text
    constraint = build_discriminating_constraint(
        interrupted_goal="Run file strings xxd",
        fact_count=3,
        named_artifacts=["stfu"],
        crypto_clues=clues,
    )
    assert "falsifiable crypto" in constraint
    assert "NOT another file→strings→xxd" in constraint
    assert "Supplied tap" in constraint or "ELF" in constraint


def test_working_packet_requires_named_artifact_hint():
    """Round-8: packet must surface at least one concrete basename."""
    text = build_working_packet(
        None,
        fact_count=0,
        flag_count=0,
        named_artifacts=["stfu"],
        interrupted_goal="Solve stfu",
        last_goals=["Solve stfu"],
    )
    assert packet_has_named_artifact(text, name="stfu")
    assert "file" in text.lower() and "strings" in text.lower()
    assert "xxd" in text.lower()
    assert "SAME worker" in text
    assert "pwd/ls-cwd" in text
    # Without named artifacts, quality helper rejects empty provision list.
    empty = build_working_packet(
        None,
        fact_count=0,
        flag_count=0,
        named_artifacts=[],
    )
    assert "Named artifacts" in empty
    assert not packet_has_named_artifact(empty)
    assert not packet_meets_replan_quality(empty)


def test_collect_named_artifacts_from_attachments_and_workspace(tmp_path):
    by_name = tmp_path / "inputs" / "by-name"
    by_name.mkdir(parents=True)
    (by_name / "payload.enc").write_text("x", encoding="utf-8")
    names = collect_named_artifacts(
        None,
        attachments=["/bench/stfu", "/bench/challenge.json"],
        workspace_root=tmp_path,
        description='See `notes.txt` and ignore flag.txt',
    )
    assert "stfu" in names
    assert "payload.enc" in names
    assert "notes.txt" in names
    assert "challenge.json" not in names
    assert "flag.txt" not in names
    # Prefer bare blob over README.
    ranked = collect_named_artifacts(
        None,
        attachments=["/x/README.md", "/x/stfu"],
    )
    assert ranked[0] == "stfu"


def test_discriminating_constraint_forbids_bootstrap_paraphrase():
    text = build_discriminating_constraint(
        interrupted_goal="Solve the whole challenge from scratch",
        fact_count=0,
        named_artifacts=["stfu"],
    )
    assert text.startswith(CONSTRAINT_PREFIX)
    assert "MUST" in text
    assert "whole-challenge bootstrap" in text
    assert "Solve the whole challenge" in text
    assert "`stfu`" in text
    assert "pwd/ls-cwd" in text
    assert "file→strings→xxd" in text or ("strings" in text and "xxd" in text)


def test_packet_meets_replan_quality_rejects_weak_text():
    weak = (
        f"{PACKET_PREFIX} interrupted=w after=10s verified_facts=0. "
        "Propose something new."
    )
    assert not packet_meets_replan_quality(weak)
    assert not packet_meets_replan_quality("not a packet")


def test_empty_reason_retry_and_chain_inject(monkeypatch):
    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT", "1")
    assert max_empty_reason_retries() >= 1
    assert should_retry_empty_reason(
        pending_recovery=True,
        reason_proposed=0,
        retry_count=0,
    )
    assert not should_retry_empty_reason(
        pending_recovery=True,
        reason_proposed=0,
        retry_count=1,
        max_retries=1,
    )
    assert not should_retry_empty_reason(
        pending_recovery=True,
        reason_proposed=2,
        retry_count=0,
    )
    assert should_inject_artifact_chain_intent(
        pending_recovery=True,
        reason_proposed=0,
        named_artifacts=["stfu"],
        already_injected=False,
    )
    assert not should_inject_artifact_chain_intent(
        pending_recovery=True,
        reason_proposed=0,
        named_artifacts=["stfu"],
        already_injected=True,
    )
    goal = build_artifact_chain_intent_goal(["stfu"])
    assert "`stfu`" in goal
    assert "file" in goal and "strings" in goal and "xxd" in goal.lower()


def test_artifact_progress_defers_hard_cap():
    """Round-9: tooling Named artifact gets hard-cap + extra before interrupt."""
    base = dict(
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="explore",
        allow_explore=True,
        ordinary_worker_count=1,
        seconds_since_last_tool=10.0,
        tool_stall_s=60.0,
        sole_extra_s=120.0,
        hard_cap_s=300.0,
        artifact_extra_s=180.0,
    )
    # No artifact progress → hard-cap at 300.
    assert should_interrupt_worker(
        running_for_s=301.0, artifact_progress=False, **base
    )
    # Artifact progress → deferred past 300, still under 480.
    assert not should_interrupt_worker(
        running_for_s=350.0, artifact_progress=True, **base
    )
    # Extended hard-cap still cuts forever burns.
    assert should_interrupt_worker(
        running_for_s=481.0, artifact_progress=True, **base
    )


def test_worker_artifact_progress_requires_tool_check_not_goal_name():
    # Goal mentioning stfu alone must NOT count (would shield every bootstrap).
    solver = SimpleNamespace(
        intent_goal="Solve stfu [crypto]",
        _raw_tool_commands=["ls -la", "pwd"],
    )
    assert not worker_artifact_progress(solver, ["stfu"])
    solver2 = SimpleNamespace(
        intent_goal="Solve stfu [crypto]",
        _raw_tool_commands=["file ./stfu", "strings stfu | head"],
    )
    assert worker_artifact_progress(solver2, ["stfu"])
    assert DEFAULT_ARTIFACT_EXTRA_S == 180.0


def test_harvest_artifact_tool_facts_from_file_strings_xxd(monkeypatch):
    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT", "1")
    assert harvest_enabled() is True
    solver = SimpleNamespace(
        _raw_tool_commands=[
            "ls -la",
            "file ./stfu",
            "strings ./stfu | head -20",
            "xxd ./stfu | head -5",
        ],
        _raw_tool_outputs=[
            "total 8",
            "stfu: ELF 32-bit LSB executable, Intel 80386, dynamically linked",
            "/lib/ld-linux.so.2\nlibc.so.6\nfopen\nprintf\nSupplied tap values\nUsage: stfu <FILE>",
            "00000000: 7f45 4c46 0101 0100 0000 0000 0000 0000  .ELF............",
        ],
    )
    rows = harvest_artifact_tool_facts(solver, ["stfu"], limit=6)
    checks = {r["check"] for r in rows}
    assert "file" in checks and "strings" in checks and "xxd" in checks
    assert all("`stfu`" in r["fact"] for r in rows)
    assert all(r["witness"] for r in rows)
    # Round-11: high-signal snippets attached.
    assert any(r.get("signals") for r in rows)
    clues = extract_crypto_clues(rows, None, limit=6)
    assert clues
    assert any("ELF" in c or "Supplied" in c or "Usage" in c for c in clues)

    committed: list[dict] = []

    class _Graph:
        def add_evidence(self, **kwargs):
            committed.append(kwargs)
            return len(committed)

    seqs = commit_harvested_facts(_Graph(), actor="cli-claude", rows=rows)
    assert len(seqs) == len(rows)
    assert all(c["verified"] is True for c in committed)
    assert all(c["source"] == "fruitless_interrupt_harvest" for c in committed)

    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT_HARVEST", "0")
    assert harvest_enabled() is False
    assert harvest_artifact_tool_facts(solver, ["stfu"]) == []


def test_rebootstrap_after_reason_timeout_not_idle(monkeypatch):
    """Round-4: ConnectTimeout / empty after interrupt → rebootstrap, not idle."""
    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT", "1")
    # Exact life death shape: interrupt recovery pending, Reason EXCEPTION, empty.
    assert should_rebootstrap_after_reason(
        pending_recovery=True,
        tasks_empty=True,
        open_intents=0,
        reason_proposed=0,
        planner_failure_kind="planner_exception",
        rebootstrap_count=0,
    )
    assert should_rebootstrap_after_reason(
        pending_recovery=True,
        tasks_empty=True,
        open_intents=0,
        reason_proposed=0,
        planner_failure_kind="ConnectTimeout",
        rebootstrap_count=0,
    )
    # Without pending interrupt recovery, do not hijack normal dry-planner pause.
    assert not should_rebootstrap_after_reason(
        pending_recovery=False,
        tasks_empty=True,
        open_intents=0,
        reason_proposed=0,
        planner_failure_kind="planner_exception",
    )
    # Queue still has work — no rebootstrap.
    assert not should_rebootstrap_after_reason(
        pending_recovery=True,
        tasks_empty=True,
        open_intents=2,
        reason_proposed=0,
        planner_failure_kind="planner_exception",
    )
    assert not should_rebootstrap_after_reason(
        pending_recovery=True,
        tasks_empty=False,
        open_intents=0,
        reason_proposed=0,
        planner_failure_kind="planner_exception",
    )


def test_rebootstrap_respects_max_and_env_off(monkeypatch):
    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT", "1")
    monkeypatch.delenv("MUTEKI_FRUITLESS_INTERRUPT_REBOOTSTRAP_MAX", raising=False)
    assert max_reboots() == DEFAULT_MAX_REBOOTSTRAPS
    assert not should_rebootstrap_after_reason(
        pending_recovery=True,
        tasks_empty=True,
        open_intents=0,
        reason_proposed=0,
        planner_failure_kind="planner_exception",
        rebootstrap_count=DEFAULT_MAX_REBOOTSTRAPS,
    )
    monkeypatch.delenv("MUTEKI_FRUITLESS_INTERRUPT", raising=False)
    assert not should_rebootstrap_after_reason(
        pending_recovery=True,
        tasks_empty=True,
        open_intents=0,
        reason_proposed=0,
        planner_failure_kind="planner_exception",
        interrupt_enabled=False,
    )


def test_reason_failure_kind_normalizes_enum_like():
    class _Kind:
        value = "planner_exception"

    class _Fail:
        kind = _Kind()

    assert reason_failure_kind(_Fail()) == "planner_exception"
    assert reason_failure_kind(None) == ""
    assert reason_failure_kind("ConnectTimeout") == "connecttimeout"


def test_sole_worker_recent_tools_deferred_until_hard_cap():
    """Round-5: sole worker still tooling must not die at base threshold."""
    # Past 150s threshold but tools hot → keep (life-shaped).
    assert not should_interrupt_worker(
        running_for_s=160.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="bootstrap",
        ordinary_worker_count=1,
        seconds_since_last_tool=5.0,
        tool_stall_s=60.0,
        sole_extra_s=120.0,
        hard_cap_s=300.0,
    )
    # Still tooling past threshold+sole_extra but under hard_cap → keep.
    assert not should_interrupt_worker(
        running_for_s=280.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="bootstrap",
        ordinary_worker_count=1,
        seconds_since_last_tool=5.0,
        tool_stall_s=60.0,
        sole_extra_s=120.0,
        hard_cap_s=300.0,
    )
    # Hard cap: forever-tooling burn must still be interruptible.
    assert should_interrupt_worker(
        running_for_s=300.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="bootstrap",
        ordinary_worker_count=1,
        seconds_since_last_tool=5.0,
        tool_stall_s=60.0,
        sole_extra_s=120.0,
        hard_cap_s=300.0,
    )


def test_tool_stall_allows_interrupt_before_hard_cap():
    """Tools stopped → interrupt at base threshold (hung / empty spin)."""
    assert should_interrupt_worker(
        running_for_s=160.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="bootstrap",
        ordinary_worker_count=1,
        seconds_since_last_tool=90.0,
        tool_stall_s=60.0,
        sole_extra_s=120.0,
        hard_cap_s=300.0,
    )


def test_multi_worker_recent_tools_also_deferred():
    assert not should_interrupt_worker(
        running_for_s=200.0,
        threshold_s=150.0,
        facts_at_start=0,
        flags_at_start=0,
        facts_now=0,
        flags_now=0,
        worker_mode="bootstrap",
        ordinary_worker_count=2,
        seconds_since_last_tool=10.0,
        tool_stall_s=60.0,
        sole_extra_s=120.0,
        hard_cap_s=300.0,
    )


def test_soft_continue_after_retire_miss_round6(monkeypatch):
    """Round-5 hang: retire miss must soft-continue for interrupt victims."""
    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT", "1")
    assert should_soft_continue_after_retire_miss(
        was_fruitless_interrupt=True,
    )
    assert not should_soft_continue_after_retire_miss(
        was_fruitless_interrupt=False,
    )
    assert not should_soft_continue_after_retire_miss(
        was_fruitless_interrupt=True,
        interrupt_enabled=False,
    )
    monkeypatch.delenv("MUTEKI_FRUITLESS_INTERRUPT_SETTLE_S", raising=False)
    assert settle_seconds() == DEFAULT_SETTLE_S
    monkeypatch.setenv("MUTEKI_FRUITLESS_INTERRUPT_SETTLE_S", "25")
    assert settle_seconds() == 25.0


def test_retire_timeout_override_accepted():
    """_retire_worker_account timeout kwarg is honored (round-6 settle)."""
    import asyncio
    import inspect
    from types import SimpleNamespace

    from muteki.swarm.swarm import Swarm

    async def _run():
        sw = object.__new__(Swarm)
        sw._worker_runtime_owners = {}
        sw._worker_runtime_reapers = {}
        sw._worker_runtime_incomplete = False
        sw._control_shutdown_incomplete = False
        sw._shutdown_incomplete_causes = set()
        sw._cancel_solver = lambda s: True  # type: ignore[method-assign]
        sw._finish_worker_retirement = (  # type: ignore[method-assign]
            lambda *a, **k: False
        )

        async def _slow_wait(timeout=None):
            await asyncio.sleep(60)
            return False

        solver = SimpleNamespace(
            solver_id="cli-test",
            _muteki_account_retired=False,
            runtime_exit_confirmed=lambda: False,
            wait_runtime_exit=_slow_wait,
            _thread_cancel_cleanup_timeout=staticmethod(lambda: 2.0),
        )
        t0 = asyncio.get_running_loop().time()
        ok = await Swarm._retire_worker_account(
            sw, solver, timeout=0.05)
        elapsed = asyncio.get_running_loop().time() - t0
        assert ok is False
        assert elapsed < 1.0  # honoured override, not the 2s default
        # Cancel dangling reaper so the loop closes cleanly.
        for task in list(sw._worker_runtime_reapers.values()):
            task.cancel()
            try:
                await task
            except (asyncio.CancelledError, Exception):
                pass

    asyncio.run(_run())
    assert "timeout" in inspect.signature(
        Swarm._retire_worker_account).parameters


def test_collect_named_artifacts_filters_hex_and_harvest_noise():
    """Round-13: xxd hex crumbs / harvest prose must not become Named artifacts."""
    class _Graph:
        def __init__(self):
            self._facts = [
                "Tool harvest (xxd) on `stfu`: 00002460: ae00 0000 0e00 0000 08af 0408",
                "Tool harvest (file) on `pcapin_abc.pcap`: path /private/var/folders/x/nyu-ab-tmp "
                "fruitless_interrupt_harvest source fact on Tool",
            ]

        def recent_fact_texts(self, limit=6):  # pragma: no cover - alternate API
            return self._facts[:limit]

    # Shared graph helper used by module is _recent_fact_texts via list_facts/snapshot.
    # Feed via a minimal object that exposes snapshot facts.
    class _G2:
        def snapshot(self):
            return {
                "facts": [
                    {"text": "Tool harvest (xxd) on `stfu`: 00002460: 0e00 08af 13cc"},
                    {
                        "text": (
                            "Tool harvest (file) on "
                            "`pcapin_73c7fb6024b5e6eec22f5a7dcf2f5d82.pcap`: "
                            "pcap capture file"
                        )
                    },
                ]
            }

    names = collect_named_artifacts(
        _G2(),
        attachments=["/bench/stfu"],
    )
    assert "stfu" in names
    assert "pcapin_73c7fb6024b5e6eec22f5a7dcf2f5d82.pcap" in names
    for noise in ("0e00", "08af", "13cc", "fact", "harvest", "Tool", "source", "on"):
        assert noise not in names


def test_domain_gate_crypto_vs_forensics_pcap():
    """Round-13: crypto hypothesis only when eligible; pcap forbids header XOR."""
    assert infer_replan_domain(
        category="crypto",
        named_artifacts=["stfu"],
        crypto_clues=["[file:stfu] ELF 32-bit LSB executable"],
        fact_count=3,
    ) == "crypto"
    assert crypto_hypothesis_eligible(
        category="crypto",
        named_artifacts=["stfu"],
        crypto_clues=["[file:stfu] ELF 32-bit LSB executable"],
        fact_count=3,
    )
    assert infer_replan_domain(
        category="forensics",
        named_artifacts=["pcapin_73c7.pcap"],
        crypto_clues=["[file:pcap] pcap capture file, microsecond ts"],
        fact_count=1,
    ) == "forensics_pcap"
    assert not crypto_hypothesis_eligible(
        category="forensics",
        named_artifacts=["pcapin_73c7.pcap"],
        crypto_clues=["[file:pcap] pcap capture file"],
        fact_count=1,
    )

    pcap = "pcapin_73c7fb6024b5e6eec22f5a7dcf2f5d82.pcap"
    text = build_working_packet(
        None,
        fact_count=1,
        flag_count=0,
        named_artifacts=[pcap],
        crypto_clues=[
            f"[file:{pcap}] pcap capture file, microsecond ts",
            f"[file:{pcap}] /private/var/folders/j4/x/nyu-ab-tmp/work",
        ],
        category="forensics",
        interrupted_goal="Solve pcapin",
        last_goals=["Solve pcapin"],
    )
    assert "Forensics clues" in text
    assert "tshark" in text
    assert "XOR" in text or "xor" in text.lower()
    assert "global header" in text.lower() or "pcap global" in text.lower()
    assert "falsifiable" not in text
    assert "Crypto clues" not in text
    assert "/private/var/folders/" not in text  # path noise filtered
    assert packet_meets_replan_quality(text)
    for marker in REQUIRED_PACKET_MARKERS_FORENSICS:
        assert marker in text
    constraint = build_discriminating_constraint(
        interrupted_goal="Solve pcapin",
        fact_count=1,
        named_artifacts=[pcap],
        crypto_clues=[f"[file:{pcap}] pcap capture file"],
        category="forensics",
        domain="forensics_pcap",
    )
    assert "tshark" in constraint
    assert "XOR" in constraint or "xor" in constraint.lower()
    assert "falsifiable crypto" not in constraint

    crypto_text = build_working_packet(
        None,
        fact_count=3,
        flag_count=0,
        named_artifacts=["stfu"],
        crypto_clues=["[file:stfu] ELF 32-bit LSB executable, Intel 80386"],
        category="crypto",
        interrupted_goal="Run file strings xxd",
        last_goals=["Run file, strings, and xxd on `stfu`"],
    )
    assert "Crypto clues" in crypto_text
    assert "falsifiable" in crypto_text
    assert packet_meets_replan_quality(crypto_text)

    chain = build_artifact_chain_intent_goal([pcap], domain="forensics_pcap")
    assert "tshark" in chain or "tcpdump" in chain
    assert "xxd" not in chain.lower()
