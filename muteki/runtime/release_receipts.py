"""Fail-closed loading of Protocol 2 release receipts.

The live-local canary records the baseline and fault-suite receipt digests that
were current when it was admitted.  A later process may reuse those digests only
when the checkout still has the exact same code identity.  This module binds the
local receipt document to:

* the current Git HEAD;
* the complete binary tracked diff from HEAD; and
* every untracked implementation/test file in the release scope.

Shell-exported values keep precedence.  A missing, malformed, stale, or
unverifiable document is a no-op, so the Protocol 2 adapter remains fail-closed.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Mapping, MutableMapping


BASELINE_ENV = "MUTEKI_PROTOCOL2_BASELINE_RECEIPT"
FAULT_SUITE_ENV = "MUTEKI_PROTOCOL2_FAULT_SUITE_RECEIPT"
DEFAULT_RECEIPT_PATH = Path("docs/_local/protocol2_release_receipts.json")
SCHEMA = "protocol2-release-receipts-v2"
FOCUSED_AREAS = (
    "epistemic", "catalog", "lifecycle", "admission", "supervisor",
    "network", "progress", "web-driver",
)

# Generated/runtime material is deliberately outside this set.  New source or
# tests cannot silently escape the receipt merely because they are untracked.
_UNTRACKED_ROOTS = ("apps", "cmd", "muteki", "scripts", "skills", "tests")


@dataclass(frozen=True)
class WorktreeIdentity:
    head: str
    tracked_diff_sha256: str
    untracked_manifest_sha256: str
    worktree_digest: str


@dataclass(frozen=True)
class ReceiptLoadResult:
    loaded: bool
    reason: str
    identity: WorktreeIdentity | None = None


class ReceiptIdentityError(RuntimeError):
    """The checkout identity cannot be computed without ambiguity."""


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _canonical(value: object) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, sort_keys=True, separators=(",", ":"),
    ).encode("utf-8")


def _git(root: Path, *args: str) -> bytes:
    try:
        completed = subprocess.run(
            ["git", "-C", str(root), *args],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except OSError as exc:
        raise ReceiptIdentityError("git is unavailable") from exc
    if completed.returncode != 0:
        detail = completed.stderr.decode("utf-8", errors="replace").strip()
        raise ReceiptIdentityError(detail[:160] or "git identity command failed")
    return completed.stdout


def _safe_untracked_path(root: Path, raw: bytes) -> tuple[str, Path]:
    try:
        relative = raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ReceiptIdentityError("untracked path is not UTF-8") from exc
    posix = PurePosixPath(relative)
    if posix.is_absolute() or ".." in posix.parts or not posix.parts:
        raise ReceiptIdentityError("unsafe untracked path")
    path = root.joinpath(*posix.parts)
    if path.is_symlink() or not path.is_file():
        raise ReceiptIdentityError(f"untracked release input is not a regular file: {relative}")
    try:
        path.resolve(strict=True).relative_to(root.resolve(strict=True))
    except (OSError, ValueError) as exc:
        raise ReceiptIdentityError(f"untracked release input escaped root: {relative}") from exc
    return relative, path


def compute_worktree_identity(root: Path) -> WorktreeIdentity:
    """Return the deterministic code identity used by the release gate."""
    root = Path(root)
    head = _git(root, "rev-parse", "HEAD").decode("ascii", errors="strict").strip()
    if len(head) != 40:
        raise ReceiptIdentityError("unexpected Git HEAD identity")
    tracked = _sha256(_git(root, "diff", "--binary", "HEAD"))
    raw_paths = _git(
        root, "ls-files", "--others", "--exclude-standard", "-z", "--",
        *_UNTRACKED_ROOTS,
    )
    records: list[dict[str, object]] = []
    for raw in sorted(item for item in raw_paths.split(b"\0") if item):
        relative, path = _safe_untracked_path(root, raw)
        payload = path.read_bytes()
        records.append({
            "path": relative,
            "bytes": len(payload),
            "sha256": _sha256(payload),
        })
    untracked = _sha256(_canonical(records))
    worktree = _sha256(_canonical({
        "head": head,
        "tracked_diff_sha256": tracked,
        "untracked_manifest_sha256": untracked,
    }))
    return WorktreeIdentity(head, tracked, untracked, worktree)


def _digest(value: object) -> str:
    return str(value or "").strip().lower()


def _is_digest(value: str) -> bool:
    return len(value) == 64 and all(char in "0123456789abcdef" for char in value)


def derive_release_receipts(identity: WorktreeIdentity) -> tuple[str, str]:
    """Derive the only valid receipts for a green test of this worktree."""
    baseline = _sha256(_canonical({
        "command": "./init.sh",
        "kind": "baseline",
        "result": "green",
        "worktree_digest": identity.worktree_digest,
    }))
    fault_suite = _sha256(_canonical({
        "focused_areas": FOCUSED_AREAS,
        "kind": "fault_suite",
        "result": "green",
        "worktree_digest": identity.worktree_digest,
    }))
    return baseline, fault_suite


def verify_receipt_document(
    document: Mapping[str, object], identity: WorktreeIdentity,
) -> tuple[str, str]:
    """Validate a receipt document and return (baseline, fault-suite)."""
    if document.get("schema") != SCHEMA:
        raise ReceiptIdentityError("unsupported release receipt schema")
    expected = {
        "head": identity.head,
        "tracked_diff_sha256": identity.tracked_diff_sha256,
        "untracked_manifest_sha256": identity.untracked_manifest_sha256,
        "worktree_digest": identity.worktree_digest,
    }
    for key, value in expected.items():
        if _digest(document.get(key)) != value:
            raise ReceiptIdentityError(f"release receipt does not match current {key}")
    baseline = _digest(document.get("baseline_receipt"))
    fault_suite = _digest(document.get("fault_suite_receipt"))
    if not _is_digest(baseline) or not _is_digest(fault_suite):
        raise ReceiptIdentityError("release receipt digest is malformed")
    verification = document.get("verification")
    if not isinstance(verification, Mapping):
        raise ReceiptIdentityError("release verification record is absent")
    if verification.get("command") != "./init.sh":
        raise ReceiptIdentityError("release verification command is not ./init.sh")
    if verification.get("exit_code") != 0 or verification.get("result") != "green":
        raise ReceiptIdentityError("release verification is not green")
    if tuple(verification.get("focused_areas") or ()) != FOCUSED_AREAS:
        raise ReceiptIdentityError("release fault-suite coverage is incomplete")
    expected_baseline, expected_fault_suite = derive_release_receipts(identity)
    if baseline != expected_baseline or fault_suite != expected_fault_suite:
        raise ReceiptIdentityError("release digest derivation does not match worktree")
    return baseline, fault_suite


def load_verified_release_receipts(
    *,
    root: Path,
    receipt_path: Path | None = None,
    environ: MutableMapping[str, str] | None = None,
) -> ReceiptLoadResult:
    """Load worktree-bound receipts without overriding explicit environment.

    This function intentionally never raises at an application entrypoint.  Its
    caller can expose ``reason`` diagnostically; absence leaves the existing
    Protocol 2 gate closed.
    """
    root = Path(root)
    path = Path(receipt_path) if receipt_path is not None else root / DEFAULT_RECEIPT_PATH
    target = os.environ if environ is None else environ
    if not path.is_file():
        return ReceiptLoadResult(False, "release receipt document is absent")
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(document, Mapping):
            raise ReceiptIdentityError("release receipt document is not an object")
        identity = compute_worktree_identity(root)
        baseline, fault_suite = verify_receipt_document(document, identity)
    except (OSError, UnicodeError, json.JSONDecodeError, ReceiptIdentityError) as exc:
        return ReceiptLoadResult(False, str(exc)[:200])
    # Preserve shell/.env authority, but surface a conflict immediately rather
    # than reporting that the verified file was loaded successfully.
    for name, expected in (
        (BASELINE_ENV, baseline), (FAULT_SUITE_ENV, fault_suite),
    ):
        explicit = str(target.get(name) or "").strip()
        if explicit and explicit != expected:
            return ReceiptLoadResult(
                False, f"explicit {name} conflicts with verified release receipt",
                identity,
            )
    if not str(target.get(BASELINE_ENV) or "").strip():
        target[BASELINE_ENV] = baseline
    if not str(target.get(FAULT_SUITE_ENV) or "").strip():
        target[FAULT_SUITE_ENV] = fault_suite
    return ReceiptLoadResult(
        True, "verified worktree-bound release receipts loaded", identity,
    )
