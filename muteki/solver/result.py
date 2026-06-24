"""Unified Result + artifact store (tiered, on-demand retrieval — §6.3).

Tactical code returns a condensed `Result` (conclusion + key evidence + next
hint). The full raw output is written to disk as an artifact; the Result only
carries its `artifact_id`. The model later calls `peek(artifact_id, ...)` to
page/search the raw text. This is the antidote to context pollution: a single
disassembly or memory dump can be tens of thousands of tokens.
"""

from __future__ import annotations

import json
import re
import uuid
from pathlib import Path
from typing import Any, Optional

from pydantic import BaseModel, Field


class ArtifactStore:
    """Disk-backed raw-output store. One file per artifact id."""

    def __init__(self, root: str | Path = "artifacts") -> None:
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True)

    def put(self, content: str | bytes, *, suffix: str = ".txt") -> str:
        aid = uuid.uuid4().hex[:12]
        path = self.root / f"{aid}{suffix}"
        if isinstance(content, bytes):
            path.write_bytes(content)
        else:
            path.write_text(content, encoding="utf-8", errors="replace")
        return aid

    def _find(self, artifact_id: str) -> Optional[Path]:
        matches = list(self.root.glob(f"{artifact_id}*"))
        return matches[0] if matches else None

    def read_text(self, artifact_id: str) -> Optional[str]:
        p = self._find(artifact_id)
        if p is None:
            return None
        return p.read_text(encoding="utf-8", errors="replace")

    def size(self, artifact_id: str) -> int:
        p = self._find(artifact_id)
        return p.stat().st_size if p else 0


class PeekResult(BaseModel):
    artifact_id: str
    found: bool
    total_lines: int = 0
    shown_lines: int = 0
    start: int = 0
    content: str = ""
    matched: bool = False


def peek(
    store: ArtifactStore,
    artifact_id: str,
    *,
    query: Optional[str] = None,
    lines: int = 80,
    start: int = 0,
) -> PeekResult:
    """Page/search a stored raw artifact.

    - query given: return up to `lines` lines around the first regex match.
    - no query: return `lines` lines starting at `start`.
    """
    text = store.read_text(artifact_id)
    if text is None:
        return PeekResult(artifact_id=artifact_id, found=False)
    all_lines = text.splitlines()
    total = len(all_lines)

    if query:
        pat = re.compile(query, re.IGNORECASE)
        hit = next((i for i, ln in enumerate(all_lines) if pat.search(ln)), None)
        if hit is None:
            return PeekResult(
                artifact_id=artifact_id, found=True, total_lines=total, matched=False
            )
        lo = max(0, hit - lines // 2)
        hi = min(total, lo + lines)
        chunk = all_lines[lo:hi]
        return PeekResult(
            artifact_id=artifact_id,
            found=True,
            total_lines=total,
            shown_lines=len(chunk),
            start=lo,
            content="\n".join(chunk),
            matched=True,
        )

    lo = max(0, start)
    hi = min(total, lo + lines)
    chunk = all_lines[lo:hi]
    return PeekResult(
        artifact_id=artifact_id,
        found=True,
        total_lines=total,
        shown_lines=len(chunk),
        start=lo,
        content="\n".join(chunk),
    )


class Result(BaseModel):
    """The condensed return from a tactical code block.

    `flag` is the single most important field. `provenance` ties a claimed flag
    back to real tool output — the flag-forgery defense (§11.2) refuses any flag
    that can't be traced here.
    """

    flag: Optional[str] = None
    success: bool = False
    evidence: str = ""  # one-line objective conclusion
    next_hint: Optional[str] = None  # what to try if not solved
    data: dict[str, Any] = Field(default_factory=dict)  # structured extras
    artifact_id: Optional[str] = None  # full raw output, peekable
    error: Optional[str] = None
    provenance: Optional[str] = None  # how the flag was obtained (real output)
    # §6.1 hypothesis-driven search, declared as a SIDE-OUTPUT of the same code
    # block (no separate planning phase). The solver folds these into the
    # SolveGraph so hypotheses, dead-ends, and swarm pruning become first-class.
    hypotheses: list[dict[str, Any]] = Field(default_factory=list)
    status_updates: list[dict[str, Any]] = Field(default_factory=list)

    @classmethod
    def output(
        cls,
        flag: Optional[str] = None,
        evidence: str = "",
        next_hint: Optional[str] = None,
        artifact_id: Optional[str] = None,
        provenance: Optional[str] = None,
        hypotheses: Optional[list[dict[str, Any]]] = None,
        status_updates: Optional[list[dict[str, Any]]] = None,
        **data: Any,
    ) -> "Result":
        return cls(
            flag=flag,
            success=bool(flag),
            evidence=evidence,
            next_hint=next_hint,
            artifact_id=artifact_id,
            provenance=provenance,
            hypotheses=hypotheses or [],
            status_updates=status_updates or [],
            data=data,
        )

    def for_model(self) -> str:
        """Compact text the LLM consumes (never the raw artifact)."""
        parts: list[str] = []
        if self.error:
            parts.append(f"ERROR: {self.error}")
        if self.flag:
            parts.append(f"FLAG={self.flag}")
        if self.evidence:
            parts.append(f"evidence: {self.evidence}")
        if self.data:
            parts.append(f"data: {json.dumps(self.data, default=str)[:1000]}")
        if self.artifact_id:
            parts.append(
                f"[full output saved as artifact {self.artifact_id}; "
                f"peek(artifact_id='{self.artifact_id}', query=..., lines=...) for raw]"
            )
        if self.next_hint:
            parts.append(f"next: {self.next_hint}")
        return "\n".join(parts) if parts else "(no result)"
