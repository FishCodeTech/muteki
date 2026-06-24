"""Coverage for Result + tiered artifact peek (§6.3)."""

from pathlib import Path

from muteki.solver.peek import ArtifactStore, peek
from muteki.solver.result import Result


def test_result_output_sets_success_from_flag() -> None:
    r = Result.output(flag="flag{x}", evidence="solved via decode")
    assert r.success is True
    assert r.flag == "flag{x}"
    r2 = Result.output(evidence="no luck", next_hint="try base64")
    assert r2.success is False


def test_result_for_model_is_compact_and_mentions_peek() -> None:
    r = Result.output(
        evidence="dumped 5000 lines",
        artifact_id="abc123",
        next_hint="search for flag",
        rows=5000,
    )
    txt = r.for_model()
    assert "evidence: dumped 5000 lines" in txt
    assert "abc123" in txt
    assert "peek(" in txt
    assert "next: search for flag" in txt


def test_artifact_store_put_and_peek_paging(tmp_path: Path) -> None:
    store = ArtifactStore(root=tmp_path)
    big = "\n".join(f"line {i}" for i in range(1000))
    aid = store.put(big)
    # page from start
    p = peek(store, aid, lines=10, start=0)
    assert p.found and p.total_lines == 1000 and p.shown_lines == 10
    assert p.content.splitlines()[0] == "line 0"
    # page deeper
    p2 = peek(store, aid, lines=5, start=500)
    assert p2.content.splitlines()[0] == "line 500"


def test_peek_query_centers_on_match(tmp_path: Path) -> None:
    store = ArtifactStore(root=tmp_path)
    lines = [f"noise {i}" for i in range(200)]
    lines[123] = "the flag is flag{deep_inside}"
    aid = store.put("\n".join(lines))
    p = peek(store, aid, query=r"flag\{", lines=10)
    assert p.matched is True
    assert "flag{deep_inside}" in p.content
    assert p.shown_lines <= 10


def test_peek_missing_artifact(tmp_path: Path) -> None:
    store = ArtifactStore(root=tmp_path)
    p = peek(store, "doesnotexist")
    assert p.found is False


def test_peek_query_no_match(tmp_path: Path) -> None:
    store = ArtifactStore(root=tmp_path)
    aid = store.put("nothing here\nor here")
    p = peek(store, aid, query="flag")
    assert p.found is True and p.matched is False
