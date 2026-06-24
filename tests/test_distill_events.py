"""P-E: flywheel reads the shared event log → distills the verified evidence chain."""

from __future__ import annotations

from muteki.learning.distill import distill, distill_from_events, Template
from muteki.models.solve_graph import Challenge, SolveGraph
from muteki.swarm.shared_graph import SQLiteSharedGraph


def _solved_shared_graph(tmp_path):
    g = SQLiteSharedGraph.open(
        db_path=tmp_path / "g.db",
        challenge=Challenge(id="c1", name="Lazy Leaks", category="forensics",
                            description="pcap credential leak"),
    )
    g.add_evidence(actor="s1", source="run_python", fact="pcap has an HTTP POST",
                   verified=True, verifier="substring_in_artifact")
    g.add_evidence(actor="s1", source="run_python", fact="credentials admin:hunter2",
                   verified=True, verifier="substring_in_artifact")
    g.add_evidence(actor="s2", source="run_python", fact="speculative unrelated note",
                   verified=False, confidence=0.4, verifier="artifact_readable_unlocated")
    g.flag_found(actor="s1", flag="flag{abc}")
    return g


def test_distill_from_events_uses_verified_chain(tmp_path):
    g = _solved_shared_graph(tmp_path)
    tpl = distill_from_events(g, winner="s1")
    g.close()
    assert tpl.category == "forensics"
    # the evidence chain contains the VERIFIED facts only, in order
    assert "pcap has an HTTP POST" in tpl.evidence_chain
    assert "credentials admin:hunter2" in tpl.evidence_chain
    # the unverified speculative note is NOT in the chain
    assert "speculative unrelated note" not in tpl.evidence_chain


def test_flag_value_excluded_from_chain(tmp_path):
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db",
                               challenge=Challenge(id="c", name="x", category="crypto"))
    g.add_evidence(actor="s1", source="x", fact="decoded to flag{secret}",
                   verified=True, verifier="substring_in_artifact")
    g.flag_found(actor="s1", flag="flag{secret}")
    tpl = distill_from_events(g, winner="s1")
    g.close()
    joined = " ".join(tpl.evidence_chain)
    assert "flag{secret}" not in joined
    assert "<FLAG>" in joined


def test_template_yaml_roundtrip_with_chain():
    t = Template(name="n", category="crypto", keywords=["xor"], steps=["s1"],
                 success_format=r"flag\{.*\}", evidence_chain=["a", "b"])
    t2 = Template.from_yaml(t.to_yaml())
    assert t2.evidence_chain == ["a", "b"]


def test_private_graph_distill_still_works():
    # back-compat: distilling a private SolveGraph (no verified flags) falls back
    # to using all evidence for the chain.
    g = SolveGraph(challenge=Challenge(id="c", name="x", category="misc"))
    g.add_evidence(source="x", fact="some fact")
    tpl = distill(g)
    assert tpl.evidence_chain == ["some fact"]


# ── G: per-flag evidence chains (multi-flag writeup) ─────────────────────────

def test_per_flag_chains_intent_linked(tmp_path):
    g = SQLiteSharedGraph.open(
        db_path=tmp_path / "g.db",
        challenge=Challenge(id="m1", name="multi", category="web", expected_flags=2),
    )
    f1 = g.add_evidence(actor="w1", source="x", fact="login endpoint", verified=True)
    g.propose_intent(actor="reason", intent_id="I1", goal="exploit login", from_fact_seqs=[f1])
    g.claim_intent(worker="w1", intent_id="I1")
    f2 = g.add_evidence(actor="w1", source="x", fact="login bypass -> flag{a}", verified=True)
    g.conclude_intent(actor="w1", intent_id="I1", result="solved", to_fact_seq=f2)
    g.flag_found(actor="w1", flag="flag{a}", intent_id="I1")

    f3 = g.add_evidence(actor="w2", source="x", fact="admin panel", verified=True)
    g.propose_intent(actor="reason", intent_id="I2", goal="exploit admin", from_fact_seqs=[f3])
    g.claim_intent(worker="w2", intent_id="I2")
    f4 = g.add_evidence(actor="w2", source="x", fact="admin RCE -> flag{b}", verified=True)
    g.conclude_intent(actor="w2", intent_id="I2", result="solved", to_fact_seq=f4)
    g.flag_found(actor="w2", flag="flag{b}", intent_id="I2")

    chains = g.per_flag_evidence_chains()
    assert chains["flag{a}"] == ["login endpoint", "login bypass -> flag{a}"]
    assert chains["flag{b}"] == ["admin panel", "admin RCE -> flag{b}"]

    tpl = distill_from_events(g, winner="w1")
    assert set(tpl.flag_evidence_chains.keys()) == {"flag #1", "flag #2"}
    y = tpl.to_yaml()
    assert "flag{a}" not in y and "flag{b}" not in y  # values redacted
    assert "flag #1" in y
    g.close()


def test_per_flag_chains_temporal_fallback(tmp_path):
    g = SQLiteSharedGraph.open(
        db_path=tmp_path / "g.db",
        challenge=Challenge(id="m2", name="multi2", category="web", expected_flags=2),
    )
    g.add_evidence(actor="w", source="x", fact="step a", verified=True)
    g.add_evidence(actor="w", source="x", fact="step b", verified=True)
    g.flag_found(actor="w", flag="flag{one}")  # no intent_id → temporal
    g.add_evidence(actor="w", source="x", fact="step c", verified=True)
    g.flag_found(actor="w", flag="flag{two}")
    chains = g.per_flag_evidence_chains()
    assert chains["flag{one}"] == ["step a", "step b"]
    assert chains["flag{two}"] == ["step a", "step b", "step c"]
    g.close()


def test_single_flag_yaml_byte_identical(tmp_path):
    g = SQLiteSharedGraph.open(
        db_path=tmp_path / "g.db",
        challenge=Challenge(id="s1", name="single", category="web"),
    )
    g.add_evidence(actor="w", source="x", fact="only step", verified=True)
    g.flag_found(actor="w", flag="flag{solo}")
    tpl = distill_from_events(g, winner="w")
    assert tpl.flag_evidence_chains == {}
    assert "flag_evidence_chains" not in tpl.to_yaml()
    g.close()
