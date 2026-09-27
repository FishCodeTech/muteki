"""Per-prompt context manifest: which scoped sections were assembled into one
worker's prompt, and which fact/intent/artifact refs those sections carried.

Solve and review Workers do not receive the full board file or raw graph path.
The manifest is the machine-readable record of the role-scoped context the model
received, used for audit, prompt-size accounting, and the deck's context view.
Persisted per prompt as ``.muteki_context_manifest.json`` in the worker cwd and
emitted as a ``context_manifest`` blackboard delta (see cli_board_context).
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass(frozen=True)
class ContextManifest:
    role: str
    worker_id: str
    intent_id: str
    fact_seqs: tuple[int, ...] = ()
    intent_ids: tuple[str, ...] = ()
    artifact_ids: tuple[str, ...] = ()
    sections: tuple[str, ...] = ()
    section_chars: dict[str, int] = field(default_factory=dict)
    total_chars: int = 0
    omissions: tuple[dict[str, Any], ...] = ()
    full_board_included: bool = False
    user_fold_fallback: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "role": self.role,
            "worker_id": self.worker_id,
            "intent_id": self.intent_id,
            "fact_seqs": list(self.fact_seqs),
            "intent_ids": list(self.intent_ids),
            "artifact_ids": list(self.artifact_ids),
            "sections": list(self.sections),
            "section_chars": dict(self.section_chars),
            "total_chars": self.total_chars,
            "omissions": [dict(item) for item in self.omissions],
            "full_board_included": self.full_board_included,
            "user_fold_fallback": self.user_fold_fallback,
        }

    @classmethod
    def build(cls, *, role: str, worker_id: str, intent_id: str,
              sections: list[tuple[str, str, dict]],
              omissions: list[dict[str, Any]] | None = None,
              full_board_included: bool = False,
              user_fold_fallback: bool | None = None) -> "ContextManifest":
        """Assemble a manifest from ``(name, text, meta)`` section tuples.
        Empty-text sections are dropped (the model never saw them); each
        section's meta may carry ``fact_seqs`` / ``intent_ids`` /
        ``artifact_ids`` lists, unioned here in section order. Char accounting
        is computed from the FINAL joined text (sections joined with "\\n",
        exactly as the prompt builders hand it to the template)."""
        kept = [(str(name), str(text), meta or {})
                for name, text, meta in (sections or [])
                if text and str(text).strip()]
        fact_seqs: list[int] = []
        intent_ids: list[str] = []
        artifact_ids: list[str] = []
        folded = bool(user_fold_fallback)
        for _name, _text, meta in kept:
            if meta.get("user_fold_fallback"):
                folded = True
            for raw in meta.get("fact_seqs") or []:
                try:
                    seq = int(raw)
                except (TypeError, ValueError):
                    continue
                if seq > 0 and seq not in fact_seqs:
                    fact_seqs.append(seq)
            for raw in meta.get("intent_ids") or []:
                value = str(raw or "").strip()
                if value and value not in intent_ids:
                    intent_ids.append(value)
            for raw in meta.get("artifact_ids") or []:
                value = str(raw or "").strip()
                if value and value not in artifact_ids:
                    artifact_ids.append(value)
        return cls(
            role=str(role or ""),
            worker_id=str(worker_id or ""),
            intent_id=str(intent_id or ""),
            fact_seqs=tuple(fact_seqs),
            intent_ids=tuple(intent_ids),
            artifact_ids=tuple(artifact_ids),
            sections=tuple(name for name, _text, _meta in kept),
            section_chars={name: len(text) for name, text, _meta in kept},
            total_chars=len("\n".join(text for _name, text, _meta in kept)),
            omissions=tuple(dict(item) for item in (omissions or [])),
            full_board_included=bool(full_board_included),
            user_fold_fallback=folded,
        )
