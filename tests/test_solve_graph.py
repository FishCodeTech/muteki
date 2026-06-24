"""Coverage for SolveGraph mutation + the load-bearing to_summary()."""

from muteki.models.solve_graph import (
    Challenge,
    HypothesisStatus,
    SolveGraph,
)


def _graph() -> SolveGraph:
    return SolveGraph(
        challenge=Challenge(
            id="c1", name="baby-web", category="web", points=100,
            description="find the flag", target="http://localhost:8000",
        )
    )


def test_add_evidence_and_hypothesis() -> None:
    g = _graph()
    ev = g.add_evidence("web.http", "server is Flask 2.0", artifact_id="a1")
    assert ev.fact == "server is Flask 2.0"
    h = g.add_hypothesis("SSTI in name param", "Flask + reflected input", priority=0.8)
    assert h.id.startswith("H")
    assert g.get_hypothesis(h.id) is h


def test_status_transition_refute_records_dead_end() -> None:
    g = _graph()
    h = g.add_hypothesis("SQLi in id", "numeric param", priority=0.6)
    g.set_status(h.id, HypothesisStatus.REFUTED, refuted_reason="param is integer-cast server-side")
    assert g.get_hypothesis(h.id).status is HypothesisStatus.REFUTED
    assert "param is integer-cast server-side" in g.dead_ends


def test_active_hypotheses_excludes_resolved() -> None:
    g = _graph()
    a = g.add_hypothesis("A", "r")
    b = g.add_hypothesis("B", "r")
    c = g.add_hypothesis("C", "r")
    g.set_status(a.id, HypothesisStatus.CONFIRMED)
    g.set_status(b.id, HypothesisStatus.REFUTED, refuted_reason="nope")
    active = g.active_hypotheses()
    assert c in active and a not in active and b not in active


def test_to_summary_is_concise_and_includes_key_state() -> None:
    g = _graph()
    g.add_evidence("web.http", "X-Powered-By: Express")
    h1 = g.add_hypothesis("Prototype pollution", "Express + JSON body", priority=0.9)
    h2 = g.add_hypothesis("JWT none alg", "jwt cookie present", priority=0.4)
    g.set_status(h2.id, HypothesisStatus.REFUTED, refuted_reason="alg pinned to HS256")
    g.set_status(h1.id, HypothesisStatus.TESTING)
    s = g.to_summary()
    assert "baby-web" in s
    assert "Express" in s
    assert "Prototype pollution" in s
    assert "alg pinned to HS256" in s  # dead-end listed
    # refuted hypothesis statement should not appear as an active hypothesis line
    assert "JWT none alg" not in s
    # summary stays small
    assert len(s) < 1500


def test_to_summary_shows_flag_when_found() -> None:
    g = _graph()
    g.flag = "flag{got_it}"
    assert "flag{got_it}" in g.to_summary()


def test_roundtrip_serialization() -> None:
    g = _graph()
    g.add_evidence("t", "f")
    g.add_hypothesis("h", "r")
    raw = g.model_dump_json()
    back = SolveGraph.model_validate_json(raw)
    assert back.challenge.id == "c1"
    assert len(back.evidence) == 1
    assert len(back.hypotheses) == 1


def test_to_summary_partitions_verified_vs_candidate() -> None:
    # §8.5 PCSG: once the provenance gate is wired (verifier set), to_summary()
    # splits confirmed facts from unverified candidates. (Migrated from the old
    # test_verifier_gate.py when the standalone verifier module was retired.)
    g = _graph()
    g.add_evidence("x", "solid fact", verified=True, verifier="substring_in_artifact")
    g.add_evidence("x", "shaky fact", verified=False, confidence=0.4,
                   verifier="artifact_readable_unlocated")
    out = g.to_summary()
    assert "## Verified evidence" in out
    assert "solid fact" in out
    assert "Candidates / needs verification" in out
    assert "[UNVERIFIED]" in out
    assert "shaky fact" in out


def test_to_summary_backcompat_when_ungated() -> None:
    # before the gate is wired (no verifier set), behave as the old single block.
    g = _graph()
    g.add_evidence("x", "legacy fact")  # no verifier
    out = g.to_summary()
    assert "## Confirmed evidence" in out
    assert "Candidates" not in out


def test_attachments_surface_in_summary() -> None:
    """Player-facing files must appear in to_summary() so the agent reads them
    (the keystone fix — they were previously invisible)."""
    from muteki.models.solve_graph import Challenge, SolveGraph
    ch = Challenge(id="c", name="codereview", category="web",
                   description="find the bug", attachments=["/x/app.py", "/x/Dockerfile"])
    s = SolveGraph(challenge=ch).to_summary()
    assert "Attached files" in s
    assert "/x/app.py" in s and "/x/Dockerfile" in s


# ── multi-flag data model (Phase 1) ──────────────────────────────────────────

def test_challenge_expected_flags_defaults_to_one() -> None:
    # back-compat: every existing Challenge / old serialized payload must read 1.
    assert Challenge(id="c", name="n", category="web").expected_flags == 1


def test_solvegraph_add_flag_dedups_and_keeps_first_invariant() -> None:
    g = _graph()
    assert g.flag is None and g.flags == []
    assert g.add_flag("flag{a}") is True
    assert g.flag == "flag{a}" and g.flags == ["flag{a}"]
    # dedup: same flag again is a no-op
    assert g.add_flag("flag{a}") is False
    assert g.flags == ["flag{a}"]
    # a second distinct flag appends; `flag` stays the FIRST (invariant)
    assert g.add_flag("flag{b}") is True
    assert g.flags == ["flag{a}", "flag{b}"] and g.flag == "flag{a}"
    # empty/None ignored
    assert g.add_flag("") is False and g.flags == ["flag{a}", "flag{b}"]


def test_solvegraph_roundtrip_carries_flags() -> None:
    g = _graph()
    g.add_flag("flag{a}")
    g.add_flag("flag{b}")
    g2 = SolveGraph.model_validate(g.model_dump())
    assert g2.flags == ["flag{a}", "flag{b}"] and g2.flag == "flag{a}"
