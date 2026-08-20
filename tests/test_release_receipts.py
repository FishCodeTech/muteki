from __future__ import annotations

import json
import subprocess
from dataclasses import asdict
from pathlib import Path

from muteki.runtime.release_receipts import (
    BASELINE_ENV,
    FAULT_SUITE_ENV,
    SCHEMA,
    WorktreeIdentity,
    compute_worktree_identity,
    derive_release_receipts,
    load_verified_release_receipts,
    verify_receipt_document,
)


def _run(root: Path, *args: str) -> None:
    subprocess.run(["git", "-C", str(root), *args], check=True,
                   capture_output=True)


def _repo(tmp_path: Path) -> Path:
    root = tmp_path / "repo"
    root.mkdir()
    _run(root, "init", "-q")
    _run(root, "config", "user.email", "tests@example.invalid")
    _run(root, "config", "user.name", "Muteki Tests")
    (root / "tracked.txt").write_text("base\n", encoding="utf-8")
    _run(root, "add", "tracked.txt")
    _run(root, "commit", "-qm", "base")
    return root


def _document(identity: WorktreeIdentity) -> dict:
    baseline, fault_suite = derive_release_receipts(identity)
    return {
        "schema": SCHEMA,
        **asdict(identity),
        "baseline_receipt": baseline,
        "fault_suite_receipt": fault_suite,
        "verification": {"command": "./init.sh", "exit_code": 0,
                         "result": "green", "focused_areas": [
                             "epistemic", "catalog", "lifecycle", "admission",
                             "supervisor", "network", "progress", "web-driver",
                         ]},
    }


def test_identity_covers_tracked_diff_and_untracked_release_inputs(tmp_path):
    root = _repo(tmp_path)
    initial = compute_worktree_identity(root)
    (root / "tracked.txt").write_text("changed\n", encoding="utf-8")
    tracked = compute_worktree_identity(root)
    assert tracked.tracked_diff_sha256 != initial.tracked_diff_sha256
    assert tracked.worktree_digest != initial.worktree_digest

    source = root / "muteki" / "new.py"
    source.parent.mkdir()
    source.write_text("VALUE = 1\n", encoding="utf-8")
    untracked = compute_worktree_identity(root)
    assert untracked.untracked_manifest_sha256 != tracked.untracked_manifest_sha256
    assert untracked.worktree_digest != tracked.worktree_digest

    # Operational output is outside the release scope and cannot make a tested
    # checkout stale merely because a browser screenshot was produced.
    output = root / "output" / "page.png"
    output.parent.mkdir()
    output.write_bytes(b"png")
    assert compute_worktree_identity(root) == untracked


def test_verified_document_loads_without_overriding_explicit_env(tmp_path):
    root = _repo(tmp_path)
    identity = compute_worktree_identity(root)
    path = root / "receipt.json"
    path.write_text(json.dumps(_document(identity)), encoding="utf-8")
    env = {BASELINE_ENV: _document(identity)["baseline_receipt"]}
    result = load_verified_release_receipts(
        root=root, receipt_path=path, environ=env)
    assert result.loaded is True
    assert env[BASELINE_ENV] == _document(identity)["baseline_receipt"]
    assert env[FAULT_SUITE_ENV] == _document(identity)["fault_suite_receipt"]


def test_conflicting_explicit_receipt_fails_closed(tmp_path):
    root = _repo(tmp_path)
    identity = compute_worktree_identity(root)
    path = root / "receipt.json"
    path.write_text(json.dumps(_document(identity)), encoding="utf-8")
    env = {BASELINE_ENV: "c" * 64}
    result = load_verified_release_receipts(
        root=root, receipt_path=path, environ=env)
    assert result.loaded is False
    assert "conflicts" in result.reason
    assert FAULT_SUITE_ENV not in env


def test_stale_document_fails_closed_without_loading(tmp_path):
    root = _repo(tmp_path)
    identity = compute_worktree_identity(root)
    path = root / "receipt.json"
    path.write_text(json.dumps(_document(identity)), encoding="utf-8")
    (root / "muteki").mkdir()
    (root / "muteki" / "late.py").write_text("late = True\n", encoding="utf-8")
    env: dict[str, str] = {}
    result = load_verified_release_receipts(
        root=root, receipt_path=path, environ=env)
    assert result.loaded is False
    assert "untracked_manifest_sha256" in result.reason
    assert BASELINE_ENV not in env
    assert FAULT_SUITE_ENV not in env


def test_verification_must_be_green():
    identity = WorktreeIdentity("1" * 40, "2" * 64, "3" * 64, "4" * 64)
    document = _document(identity)
    document["verification"]["exit_code"] = 1
    try:
        verify_receipt_document(document, identity)
    except Exception as exc:
        assert "not green" in str(exc)
    else:
        raise AssertionError("non-green verification was accepted")
