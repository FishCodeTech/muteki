"""Writeup distillation (§16) — turn a successful solve into a reusable template.

After a solve, the trace (the solve-graph: confirmed evidence + the winning
hypotheses + the flag) is distilled into a compact YAML template recording:

  - trigger: category + keyword fingerprint (what kind of challenge this is)
  - steps:   the decision chain that worked, as short `code_hint`s
  - success: how we knew we'd won (flag format)
  - source:  provenance (challenge id, winning solver)

These are recalled later (retrieve.py) as a "challenges like this are usually
solved like this" PRIOR injected into the solver's hypotheses — NOT copied
verbatim (the design is explicit: prior, not answer key). We deliberately do NOT
store the flag value or target-specific secrets; templates must generalize.

This is the lowest-priority long-term flywheel (§16), so it's intentionally
lightweight: plain YAML files in a directory, keyword fingerprints, no vector DB
yet (retrieve.py documents where that upgrade slots in).
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

import yaml

from muteki.models.solve_graph import HypothesisStatus, SolveGraph

# words too generic to be a useful fingerprint
_STOP = {
    "the", "a", "an", "to", "of", "and", "or", "is", "it", "in", "on", "at",
    "this", "that", "find", "flag", "challenge", "you", "your", "with", "for",
    "via", "get", "post", "http", "https", "www", "com", "are", "be", "by",
}


def _keywords(text: str, k: int = 12) -> list[str]:
    toks = re.findall(r"[a-zA-Z][a-zA-Z0-9_]{2,}", text.lower())
    seen: list[str] = []
    for t in toks:
        if t in _STOP or t in seen:
            continue
        seen.append(t)
        if len(seen) >= k:
            break
    return seen


@dataclass
class Template:
    """A distilled solve recipe. `trigger` decides recall; `steps` is the prior."""

    name: str
    category: str
    keywords: list[str]
    steps: list[str]  # short code_hints / decision points that worked
    success_format: str
    source: str = ""
    notes: str = ""
    # P-E: the EVIDENCE CHAIN — the ordered VERIFIED-fact trail distilled from the
    # event log (not just "hypothesis A → B"). Recall injects this so the next
    # similar challenge knows "the evidence chain looked like pcap→port→pw→flag".
    evidence_chain: list[str] = field(default_factory=list)
    # G: per-flag evidence chains for a MULTI-FLAG challenge — each captured flag
    # gets its own proof path (intent-linked, with a temporal fallback). Empty for a
    # single-flag run (the flat evidence_chain is authoritative there).
    flag_evidence_chains: dict[str, list[str]] = field(default_factory=dict)

    def to_yaml(self) -> str:
        body = {
            "name": self.name,
            "category": self.category,
            "keywords": self.keywords,
            "steps": self.steps,
            "evidence_chain": self.evidence_chain,
            "success_format": self.success_format,
            "source": self.source,
            "notes": self.notes,
        }
        # only emit per-flag chains for a genuine multi-flag writeup (keeps the
        # single-flag YAML byte-identical to before).
        if self.flag_evidence_chains:
            body["flag_evidence_chains"] = self.flag_evidence_chains
        return yaml.safe_dump(body, sort_keys=False, allow_unicode=True)

    @classmethod
    def from_yaml(cls, text: str) -> "Template":
        d = yaml.safe_load(text) or {}
        return cls(
            name=d.get("name", ""),
            category=d.get("category", "misc"),
            keywords=list(d.get("keywords", [])),
            steps=list(d.get("steps", [])),
            success_format=d.get("success_format", r"flag\{.*?\}"),
            source=d.get("source", ""),
            notes=d.get("notes", ""),
            evidence_chain=list(d.get("evidence_chain", [])),
            flag_evidence_chains={
                str(k): list(v) for k, v in (d.get("flag_evidence_chains") or {}).items()
            },
        )


def distill(graph: SolveGraph, *, winner: Optional[str] = None,
            per_flag_chains: Optional[dict[str, list[str]]] = None) -> Template:
    """Build a generalizable Template from a solved SolveGraph.

    Steps come from confirmed hypotheses (the lines of attack that worked) plus
    the ordered confirmed evidence facts (the breadcrumb trail), lightly
    sanitized. The flag value is intentionally excluded.

    G: when `per_flag_chains` is supplied (multi-flag, from the event log), each
    flag's own proof path is recorded under a SANITIZED label (flag #1, #2, …) so
    the template never stores a real flag value yet keeps the paths separable."""
    c = graph.challenge
    kws = _keywords(f"{c.name} {c.description}")
    # category signal words bias the fingerprint
    steps: list[str] = []
    confirmed = [h for h in graph.hypotheses if h.status is HypothesisStatus.CONFIRMED]
    for h in confirmed:
        steps.append(h.statement.strip())
    # add the evidence trail (objective facts), minus anything containing a flag.
    # multi-flag: sanitize EVERY collected flag, not just the first — otherwise a
    # not-yet-redacted flag leaks into the reusable knowledge template.
    all_flags = [f for f in (graph.flags or ([graph.flag] if graph.flag else [])) if f]

    def _sanitize(fact: str) -> str:
        for fl in all_flags:
            if fl in fact:
                fact = fact.replace(fl, "<FLAG>")
        return fact

    for ev in graph.evidence:
        fact = _sanitize(ev.fact.strip())
        if fact and fact not in steps:
            steps.append(fact)
    # keep it tight
    steps = steps[:12]
    # P-E: the evidence chain = the ordered VERIFIED fact trail (the proof path),
    # flag-sanitized. Falls back to all evidence if none are marked verified
    # (e.g. a private-graph distill that never went through the gate).
    verified_ev = [ev for ev in graph.evidence if getattr(ev, "verified", False)]
    chain_src = verified_ev if verified_ev else list(graph.evidence)
    chain: list[str] = []
    for ev in chain_src:
        fact = _sanitize(ev.fact.strip())
        if fact and fact not in chain:
            chain.append(fact)
    # G: per-flag chains — only meaningful for a genuine multi-flag solve (≥2 flags).
    # Sanitize each chain's fact texts AND relabel the flag key to "flag #N" so a
    # real flag value never lands in the reusable template.
    flag_chains: dict[str, list[str]] = {}
    if per_flag_chains and len(all_flags) >= 2:
        # preserve discovery order: index flags by their position in all_flags
        order = {fl: i for i, fl in enumerate(all_flags)}
        for fl, fchain in sorted(per_flag_chains.items(),
                                 key=lambda kv: order.get(kv[0], 1_000)):
            label = f"flag #{order.get(fl, len(flag_chains)) + 1}"
            sanitized = []
            for fact in fchain:
                s = _sanitize((fact or "").strip())
                if s and s not in sanitized:
                    sanitized.append(s)
            if sanitized:
                flag_chains[label] = sanitized[:12]
    return Template(
        name=c.name or c.id,
        category=c.category,
        keywords=kws,
        steps=steps,
        success_format=c.flag_format,
        source=f"{c.id}" + (f" (solver {winner})" if winner else ""),
        evidence_chain=chain[:12],
        flag_evidence_chains=flag_chains,
    )


def distill_from_events(shared_graph, *, winner: Optional[str] = None) -> Template:
    """P-E: distill from the shared graph's event log (C's payoff).

    Instead of reverse-engineering from a post-overwrite snapshot, read this
    solve's full event sequence — the evidence chain is right there, in order,
    with verified/confidence intact. Materializes to a SolveGraph then distills,
    so the evidence_chain is the VERIFIED proof path.

    G: also pulls per-flag evidence chains from the event log (intent-linked, with a
    temporal fallback) so a multi-flag solve records each flag's own proof path."""
    snap = shared_graph.snapshot()
    per_flag: Optional[dict[str, list[str]]] = None
    if hasattr(shared_graph, "per_flag_evidence_chains"):
        try:
            per_flag = shared_graph.per_flag_evidence_chains()
        except Exception:
            per_flag = None
    return distill(snap, winner=winner, per_flag_chains=per_flag)


class TemplateStore:
    """Directory of YAML templates (the 'knowledge base' of §16/§18)."""

    def __init__(self, root: str | Path = "knowledge") -> None:
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True)

    def save(self, tpl: Template) -> Path:
        safe = re.sub(r"[^a-zA-Z0-9_-]+", "_", tpl.name) or "template"
        # de-dup by name: one template per challenge name (latest wins)
        path = self.root / f"{tpl.category}__{safe}.yaml"
        path.write_text(tpl.to_yaml(), encoding="utf-8")
        return path

    def load_all(self) -> list[Template]:
        out: list[Template] = []
        for p in sorted(self.root.glob("*.yaml")):
            try:
                out.append(Template.from_yaml(p.read_text(encoding="utf-8")))
            except (yaml.YAMLError, OSError):
                continue
        return out


def distill_and_store(
    graph: SolveGraph, store: TemplateStore, *, winner: Optional[str] = None
) -> Template:
    tpl = distill(graph, winner=winner)
    store.save(tpl)
    return tpl
