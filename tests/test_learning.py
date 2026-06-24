"""Self-learning flywheel (§16): distill a solve -> template -> store.

(The recall/retrieve half of the flywheel was retired with the code-driven
executor; what remains is distillation + the template store.)
"""

from pathlib import Path

from muteki.learning.distill import Template, TemplateStore, distill, distill_and_store
from muteki.models.solve_graph import Challenge, HypothesisStatus, SolveGraph


def _solved_graph() -> SolveGraph:
    ch = Challenge(id="c1", name="weak jwt login", category="web", points=100,
                   description="bypass the login and forge an admin JWT token",
                   flag_format=r"flag\{[^}]+\}")
    g = SolveGraph(challenge=ch)
    h = g.add_hypothesis("JWT signed with a weak HS256 secret", "short secret in cookie")
    g.set_status(h.id, HypothesisStatus.CONFIRMED)
    g.add_evidence("run_python", "login bypassed with admin' OR '1'='1")
    g.add_evidence("run_python", "JWT secret brute-forced: Sn1f")
    g.add_evidence("run_python", "forged admin token, got flag{w3b_t00ls}")
    g.flag = "flag{w3b_t00ls}"
    return g


def test_distill_builds_template_without_leaking_flag() -> None:
    g = _solved_graph()
    tpl = distill(g, winner="pro-hi")
    assert tpl.category == "web"
    assert tpl.name == "weak jwt login"
    # the confirmed hypothesis becomes a step
    assert any("weak HS256" in s for s in tpl.steps)
    # the flag VALUE must be redacted from the recipe (generalizable, not answer key)
    blob = tpl.to_yaml()
    assert "flag{w3b_t00ls}" not in blob
    assert "<FLAG>" in blob  # the leaking evidence line was sanitized
    assert "pro-hi" in tpl.source


def test_store_roundtrip(tmp_path: Path) -> None:
    store = TemplateStore(root=tmp_path / "kb")
    g = _solved_graph()
    distill_and_store(g, store, winner="pro-lo")
    loaded = store.load_all()
    assert len(loaded) == 1
    assert loaded[0].name == "weak jwt login"
    assert loaded[0].keywords  # has a fingerprint
