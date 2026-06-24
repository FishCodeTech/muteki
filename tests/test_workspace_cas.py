import json
from pathlib import Path

from muteki.solver.workspace import (
    cleanup_worker_scratch,
    ensure_workspace,
    link_shared_into_worker,
    materialize_input,
    materialize_shared_artifact,
)


def test_workspace_inputs_are_content_addressed_and_manifested(tmp_path):
    workspace = tmp_path / "workspace"
    a = tmp_path / "a.txt"
    b = tmp_path / "b.txt"
    a.write_text("same bytes")
    b.write_text("same bytes")

    ma = materialize_input(workspace, a, name="a.txt")
    mb = materialize_input(workspace, b, name="b.txt")

    assert ma["sha256"] == mb["sha256"]
    assert ma["object"] == mb["object"]
    assert ma["object"].read_text() == "same bytes"
    assert (workspace / "inputs" / "by-name" / "a.txt").resolve() == ma["object"]
    assert (workspace / "inputs" / "by-name" / "b.txt").resolve() == ma["object"]

    manifest = json.loads((workspace / "manifest.json").read_text())
    assert manifest["artifact_truth"] == "shared_graph.events"
    assert sorted(i["name"] for i in manifest["inputs"]) == ["a.txt", "b.txt"]


def test_shared_artifact_links_use_multisegment_relative_targets(tmp_path):
    workspace = tmp_path / "workspace"
    worker = workspace / "workers" / "cli-claude-1"
    worker.mkdir(parents=True)
    payload = tmp_path / "payload.py"
    payload.write_text("print('hello')")

    art = materialize_shared_artifact(workspace, payload, name="payload.py", kind="poc")
    link = link_shared_into_worker(workspace, worker, "payload.py", art["sha256"])

    assert link.is_symlink()
    assert not link.readlink().is_absolute()
    assert "shared/objects" in link.readlink().as_posix()
    assert link.resolve() == art["object"]
    index_rows = (workspace / "shared" / "index.jsonl").read_text().splitlines()
    assert len(index_rows) == 1
    assert json.loads(index_rows[0])["kind"] == "poc"


def test_cleanup_worker_scratch_preserves_winner_only(tmp_path):
    workspace = ensure_workspace(tmp_path / "workspace")
    workers = workspace / "workers"
    for name in ("cli-claude-1", "cli-codex-2", "_homes"):
        (workers / name).mkdir(parents=True)
    (workspace / "shared" / "objects" / "aa" / "bb").mkdir(parents=True)
    (workspace / "graph" / "shared_graph.db").write_text("db")

    removed = cleanup_worker_scratch(workers, keep=["cli-claude-1"])

    assert workers / "cli-codex-2" in removed
    assert (workers / "cli-claude-1").exists()
    assert (workers / "_homes").exists()
    assert (workspace / "shared").exists()
    assert (workspace / "graph" / "shared_graph.db").exists()
