"""Rendering paths: reason/board/review summaries and their block helpers.

Split out of ``shared_graph.py`` (code-health G1) as a mixin of
``SQLiteSharedGraph``. Every method body is byte-for-byte the original; the mixin
is combined back into ``SQLiteSharedGraph`` so behavior and the public surface are
unchanged. Instance state (``self._conn``, ``self._lock``, ``self.challenge``,
``self._append``, the class-level caps, the ``normalize_*`` helpers, …) is
resolved through the composed class at runtime.
"""

from __future__ import annotations

import json  # noqa: F401
import hashlib  # noqa: F401
import re  # noqa: F401
import time  # noqa: F401
from datetime import datetime, timezone
from difflib import SequenceMatcher  # noqa: F401
from pathlib import Path  # noqa: F401
from typing import Any, Optional  # noqa: F401

from muteki.models.solve_graph import Challenge, Evidence, SolveGraph  # noqa: F401
from muteki.solver.graph_text import GRAPH_TEXT_REPR, encode_graph_text
from muteki.solver.result_codes import is_genuine_giveup  # noqa: F401
from muteki.swarm.graph_defs import (  # noqa: F401
    EV_FACT_ADDED, EV_FACT_OBSERVED, EV_HYP_PROPOSED, EV_HYP_REFUTED, EV_DEAD_END,
    EV_INTENT_PROPOSED, EV_INTENT_CLAIMED, EV_INTENT_CONCLUDED, EV_FLAG_FOUND,
    EV_FLAG_INVALIDATED, EV_FLAG_SUBMISSION, EV_FLAG_SUBMISSION_DECISION,
    EV_FINDING_FOUND, EV_DEAD_END,
    EV_POC_SAVED, EV_POC_CLAIMED, EV_POC_CONCLUDED,
    EV_REVIEW_FINDING, EV_FACT_CHALLENGED, EV_FACT_REVALIDATED,
    EV_ROUTE_SUPPRESSED, EV_ROUTE_REOPENED, EV_BRANCH_SPLIT, EV_BRANCH_RESOLVED,
    EV_COORDINATOR_DIRECTIVE, EV_REVIEW_PROPOSAL, EV_REVIEW_PROPOSAL_DECISION,
    EV_LANE_LOCKED, EV_LANE_RELEASED, EV_INTENT_LANE_DEFERRED, EV_FACT_REJECTED,
    EV_FACT_MERGED, EV_FACT_SUPERSEDED, EV_FACT_PINNED, EV_INTENT_STATE_CHANGED,
    EV_OPERATOR_DIRECTIVE, EV_OPERATOR_DIRECTIVE_STATUS, EV_HITL_CLASSIFIED,
    EV_RESOURCE_LOCKED, EV_RESOURCE_RELEASED, EV_GRAPH_COMPACTED,
    FACT_STATE_UNRESOLVED, FACT_STATE_CHALLENGED, FACT_STATE_REVALIDATED,
    FACT_STATE_REJECTED, FACT_STATE_MERGED, FACT_STATE_SUPERSEDED,
    _FACT_TERMINAL_STATES, _FACT_STATES,
    INTENT_DISPATCH_ACTIVE, INTENT_DISPATCH_RESUME, INTENT_DISPATCH_RETIRED,
    INTENT_DISPATCH_CLOSED, _INTENT_DISPATCH_STATES,
    _SERVICE_DEFAULT_PORTS, _LANE_RISK_CLASSES, _FACT_ENGINE_PREFIX_RE,
    _normalize_fact_identity, _clean_lane_risk, _clean_lane_host, canonicalize_lane,
)


class _RenderMixin:
    @staticmethod
    def _ctf_yaml_scalar(value: Any) -> str:
        return json.dumps(str(value or ""), ensure_ascii=False)

    @staticmethod
    def _ctf_created_at(value: Any) -> str:
        try:
            return datetime.fromtimestamp(
                float(value), tz=timezone.utc
            ).isoformat(timespec="milliseconds").replace("+00:00", "Z")
        except (TypeError, ValueError, OSError):
            return ""

    def to_ctf_graph_yaml(self) -> str:
        """Render the complete CTF Fact-Goal-Step graph in Cairn's wire shape."""
        with self._lock:
            fact_rows = self._conn.execute(
                "SELECT seq,ts,payload FROM events "
                "WHERE challenge_id=? AND kind=? ORDER BY seq",
                (self.challenge.id, EV_FACT_ADDED),
            ).fetchall()
            step_rows = self._conn.execute(
                "SELECT i.intent_id,i.goal,i.priority,i.requested_priority,"
                "i.status,i.dispatch_state,i.result_detail,i.created_seq,ce.ts,"
                "re.payload,i.expected_observable,i.stop_condition,"
                "i.coverage_key,i.requires_capabilities_json,i.required_pocs_json "
                "FROM intents i "
                "LEFT JOIN events ce ON ce.seq=i.created_seq "
                "LEFT JOIN events re ON re.seq=i.result_seq "
                "WHERE i.challenge_id=? ORDER BY i.created_seq,i.intent_id",
                (self.challenge.id,),
            ).fetchall()
            finding_rows = self._conn.execute(
                "SELECT seq,ts,payload FROM events "
                "WHERE challenge_id=? AND kind=? ORDER BY seq",
                (self.challenge.id, EV_FINDING_FOUND),
            ).fetchall()
            observation_rows = self._conn.execute(
                "SELECT observation_seq,intent_id,text,witness,artifact_id,"
                "admitted_fact_seq,created_at,target_epoch,provenance_json "
                "FROM observations "
                "WHERE challenge_id=? ORDER BY observation_seq",
                (self.challenge.id,),
            ).fetchall()
            dead_end_rows = self._conn.execute(
                "SELECT seq,ts,payload FROM events "
                "WHERE challenge_id=? AND kind=? ORDER BY seq",
                (self.challenge.id, EV_DEAD_END),
            ).fetchall()
            poc_rows = self._conn.execute(
                "SELECT poc_id,name,entry_command,status,artifact_id "
                "FROM pocs WHERE challenge_id=? ORDER BY created_seq",
                (self.challenge.id,),
            ).fetchall()

        facts: list[dict[str, Any]] = []
        fact_ids: dict[int, str] = {}
        next_fact_index = 0
        for seq, ts, raw_payload in fact_rows:
            try:
                payload = json.loads(raw_payload or "{}")
            except (TypeError, ValueError, json.JSONDecodeError):
                payload = {}
            text = str(payload.get("fact") or "")
            if not text.strip():
                continue
            source = str(payload.get("source") or "")
            if source == "origin":
                fact_id = "fact_origin"
            else:
                next_fact_index += 1
                fact_id = f"fact_{next_fact_index:03d}"
            fact_ids[int(seq)] = fact_id
            title, separator, content = text.partition("\n")
            facts.append({
                "seq": int(seq),
                "id": fact_id,
                "text": text,
                "title": title,
                "content": content if separator else "",
                "created_at": self._ctf_created_at(ts),
                "target_epoch": str(
                    (payload.get("evidence_provenance") or {}).get("target_epoch") or ""
                ),
            })

        intent_ids = {str(row[0]) for row in step_rows}
        sources = self._intent_sources_map(intent_ids, include_retired=True)
        products = self._intent_products_map(intent_ids, include_retired=True)
        producer_by_fact = {
            int(fact_seq): intent_id
            for intent_id, seqs in products.items()
            for fact_seq in seqs
        }

        observations_by_fact: dict[int, list[str]] = {}
        artifacts_by_fact: dict[int, list[dict[str, Any]]] = {}
        fact_text_by_seq = {
            int(fact["seq"]): str(fact["text"]) for fact in facts
        }
        standalone_observations: list[
            tuple[int, str, str, str, str, float, bool, str, list[dict[str, Any]]]
        ] = []
        for (obs_seq, intent_id, body, witness, artifact_id, admitted_seq,
             created_at, target_epoch, raw_provenance) in observation_rows:
            admitted = int(admitted_seq) if admitted_seq is not None else 0
            obs_id = f"observation_{int(obs_seq)}"
            provenance = json.loads(raw_provenance or "{}")
            refs = [
                dict(ref) for ref in (provenance.get("artifact_refs") or [])
                if isinstance(ref, dict) and str(ref.get("artifact_id") or "")
            ]
            if artifact_id and not any(
                str(ref.get("artifact_id")) == str(artifact_id) for ref in refs
            ):
                refs.append({"artifact_id": str(artifact_id)})
            if admitted in fact_text_by_seq:
                observations_by_fact.setdefault(admitted, []).append(obs_id)
                known = artifacts_by_fact.setdefault(admitted, [])
                seen = {str(item.get("artifact_id")) for item in known}
                for ref in refs:
                    ref_id = str(ref.get("artifact_id"))
                    if ref_id not in seen:
                        known.append(ref)
                        seen.add(ref_id)
            if not admitted or str(body or "") != fact_text_by_seq.get(admitted, ""):
                standalone_observations.append((
                    int(obs_seq), str(intent_id or ""), str(body or ""),
                    str(witness or ""), str(artifact_id or ""), float(created_at or 0),
                    bool(admitted), str(target_epoch or ""), refs,
                ))

        lines = ["facts:" if facts else "facts: []"]
        for fact in facts:
            title, title_encoded = encode_graph_text(fact["title"])
            content, content_encoded = encode_graph_text(fact["content"])
            encoded = title_encoded + content_encoded
            lines.extend([
                f"  - id: {fact['id']}",
                f"    title: {self._ctf_yaml_scalar(title)}",
            ])
            producer = producer_by_fact.get(int(fact["seq"]))
            if producer:
                lines.append(f"    producedBy: {self._ctf_yaml_scalar(producer)}")
            source_observations = observations_by_fact.get(int(fact["seq"]), [])
            if source_observations:
                lines.append("    sourceObservations:")
                lines.extend(f"      - {obs_id}" for obs_id in source_observations)
            source_artifacts = artifacts_by_fact.get(int(fact["seq"]), [])
            if source_artifacts:
                lines.append("    sourceArtifacts:")
                for ref in source_artifacts:
                    lines.append(
                        f"      - id: {self._ctf_yaml_scalar(ref.get('artifact_id'))}"
                    )
                    for key, field in (("sha256", "sha256"), ("command", "command")):
                        if ref.get(field):
                            visible, _ = encode_graph_text(ref[field])
                            lines.append(
                                f"        {key}: {self._ctf_yaml_scalar(visible)}"
                            )
                    if ref.get("size") is not None:
                        lines.append(f"        bytes: {int(ref['size'])}")
            if fact["created_at"]:
                lines.append(f"    createdAt: {fact['created_at']}")
            if fact["target_epoch"]:
                lines.append(
                    f"    targetEpoch: {self._ctf_yaml_scalar(fact['target_epoch'])}"
                )
            if encoded:
                lines.append(f"    representation: {GRAPH_TEXT_REPR}")
                lines.append(f"    encoded_controls: {encoded}")
            if content:
                lines.append(f"    content: {self._ctf_yaml_scalar(content)}")
            else:
                lines.append('    content: ""')

        lines.append("observations:" if standalone_observations else "observations: []")
        for (obs_seq, intent_id, body, witness, artifact_id, created_at,
             admitted, target_epoch, refs) in standalone_observations:
            visible_body, encoded = encode_graph_text(body)
            lines.extend([
                f"  - id: observation_{obs_seq}",
                f"    status: {'admitted' if admitted else 'unadmitted'}",
                f"    text: {self._ctf_yaml_scalar(visible_body)}",
            ])
            if intent_id:
                lines.append(f"    step: {self._ctf_yaml_scalar(intent_id)}")
            if target_epoch:
                lines.append(f"    targetEpoch: {self._ctf_yaml_scalar(target_epoch)}")
            if witness and witness != body:
                visible_witness, _ = encode_graph_text(witness)
                lines.append(f"    witness: {self._ctf_yaml_scalar(visible_witness)}")
            if artifact_id:
                lines.append(f"    artifact: {self._ctf_yaml_scalar(artifact_id)}")
            if refs:
                lines.append("    artifactRefs:")
                lines.extend(
                    f"      - {self._ctf_yaml_scalar(ref.get('artifact_id'))}"
                    for ref in refs
                )
            if encoded:
                lines.append(f"    representation: {GRAPH_TEXT_REPR}")
            created = self._ctf_created_at(created_at)
            if created:
                lines.append(f"    createdAt: {created}")

        lines.append("deadEnds:" if dead_end_rows else "deadEnds: []")
        for dead_seq, dead_ts, raw_payload in dead_end_rows:
            payload = json.loads(raw_payload or "{}")
            lines.extend([
                f"  - id: dead_end_{int(dead_seq)}",
                f"    reason: {self._ctf_yaml_scalar(payload.get('reason'))}",
            ])
            for key, field in (("tested", "tested_scope"), ("observed", "observed_result"),
                               ("step", "intent_id"), ("targetEpoch", "target_epoch")):
                value = str(payload.get(field) or "")
                if value:
                    visible, _ = encode_graph_text(value)
                    lines.append(f"    {key}: {self._ctf_yaml_scalar(visible)}")
            created = self._ctf_created_at(dead_ts)
            if created:
                lines.append(f"    createdAt: {created}")

        lines.append("resources:" if poc_rows else "resources: []")
        for poc_id, name, entry_command, status, artifact_id in poc_rows:
            lines.extend([
                f"  - id: {self._ctf_yaml_scalar(poc_id)}",
                "    kind: poc",
                f"    status: {self._ctf_yaml_scalar(status)}",
                f"    name: {self._ctf_yaml_scalar(name)}",
                f"    entryCommand: {self._ctf_yaml_scalar(entry_command)}",
            ])
            if artifact_id:
                lines.append(f"    artifact: {self._ctf_yaml_scalar(artifact_id)}")

        findings: list[dict[str, Any]] = []
        next_finding_index = 0
        for _seq, ts, raw_payload in finding_rows:
            try:
                payload = json.loads(raw_payload or "{}")
            except (TypeError, ValueError, json.JSONDecodeError):
                payload = {}
            finding_class = str(payload.get("finding_class") or "").strip().lower()
            if finding_class not in {"product", "flag"}:
                continue
            next_finding_index += 1
            from_fact_seq = 0
            try:
                from_fact_seq = int(payload.get("from_fact") or 0)
            except (TypeError, ValueError):
                from_fact_seq = 0
            findings.append({
                "id": f"finding_{next_finding_index:03d}",
                "class": finding_class,
                "title": str(payload.get("title") or "").strip(),
                "from_fact": fact_ids.get(from_fact_seq, ""),
                "from_step": str(payload.get("from_step") or payload.get("intent_id") or ""),
                "created_at": self._ctf_created_at(ts),
            })
        lines.append("findings:" if findings else "findings: []")
        for finding in findings:
            lines.extend([
                f"  - id: {finding['id']}",
                f"    class: {finding['class']}",
                f"    title: {self._ctf_yaml_scalar(finding['title'])}",
            ])
            if finding["from_fact"]:
                lines.append(f"    from_fact: {finding['from_fact']}")
            if finding["from_step"]:
                lines.append(
                    f"    from_step: {self._ctf_yaml_scalar(finding['from_step'])}"
                )
            if finding["created_at"]:
                lines.append(f"    createdAt: {finding['created_at']}")

        created_at = facts[0]["created_at"] if facts else ""
        expected_flags = max(
            1, int(getattr(self.challenge, "expected_flags", 1) or 1)
        )
        lines.extend([
            "goals:",
            "  - id: goal_final",
            "    kind: final",
            "    finding_class: flag",
            f"    expected: {expected_flags}",
            "    criterion: 解决这道 CTF 题目，并提交全部 FLAG，多 FLAG 题目要找到全部 FLAG 并有效提交",
        ])
        if created_at:
            lines.append(f"    createdAt: {created_at}")
        lines.append("steps:" if step_rows else "steps: []")
        for (
            intent_id, action, priority_value, requested_priority, status,
            dispatch_state, result_detail, _created_seq, created_ts,
            raw_result_payload, expected_observable, stop_condition,
            coverage_key, requires_capabilities_json, required_pocs_json,
        ) in step_rows:
            try:
                result_payload = json.loads(raw_result_payload or "{}")
            except (TypeError, ValueError, json.JSONDecodeError):
                result_payload = {}
            requested = str(requested_priority or "").lower()
            priority = requested if requested in {"high", "normal", "low"} else (
                "high" if int(priority_value or 0) > 0
                else "low" if int(priority_value or 0) < 0
                else "normal"
            )
            dropped = (
                str(result_payload.get("result") or "") == "superseded"
                or str(dispatch_state or "") == INTENT_DISPATCH_RETIRED
            )
            state = "dropped" if dropped else (
                "done" if str(status or "") == "done" else "open"
            )
            lines.extend([
                f"  - id: {self._ctf_yaml_scalar(intent_id)}",
                f"    action: {self._ctf_yaml_scalar(action)}",
                "    from:",
            ])
            from_ids = [
                fact_ids[int(seq)]
                for seq in sources.get(str(intent_id), [])
                if int(seq) in fact_ids
            ]
            if from_ids:
                lines.extend(f"      - {fact_id}" for fact_id in from_ids)
            else:
                lines.append("      []")
            lines.extend([
                "    actor: agent",
                f"    priority: {priority}",
                f"    state: {state}",
            ])
            for key, value in (("expectedObservable", expected_observable),
                               ("stopCondition", stop_condition),
                               ("coverageKey", coverage_key)):
                if value:
                    visible, _ = encode_graph_text(value)
                    lines.append(f"    {key}: {self._ctf_yaml_scalar(visible)}")
            capabilities = json.loads(requires_capabilities_json or "[]")
            if capabilities:
                lines.append("    requiresCapabilities:")
                lines.extend(
                    f"      - {self._ctf_yaml_scalar(item)}" for item in capabilities
                )
            required_pocs = json.loads(required_pocs_json or "[]")
            if required_pocs:
                lines.append("    requires:")
                lines.extend(
                    f"      - {self._ctf_yaml_scalar(item)}" for item in required_pocs
                )
            to_ids = [
                fact_ids[int(seq)]
                for seq in products.get(str(intent_id), [])
                if int(seq) in fact_ids
            ]
            if len(to_ids) == 1:
                lines.append(f"    to: {to_ids[0]}")
            elif to_ids:
                lines.append("    to:")
                lines.extend(f"      - {fact_id}" for fact_id in to_ids)
            if state == "done" and str(result_detail or "").strip():
                lines.append(
                    f"    result: {self._ctf_yaml_scalar(result_detail)}"
                )
            timestamp = self._ctf_created_at(created_ts)
            if timestamp:
                lines.append(f"    createdAt: {timestamp}")
        notes: list[str] = []
        for directive in self.operator_directives(active_only=True):
            action = str(directive.get("action") or "").strip().lower()
            if action in {"focus", "redirect", "directive"}:
                continue
            text = str(directive.get("text") or "").strip()
            if not text or text.startswith("secret://"):
                continue
            if text not in notes:
                notes.append(text)
        if notes:
            lines.append("operatorNotes:")
            for text in notes:
                lines.append(f"  - {self._ctf_yaml_scalar(text)}")
        return "\n".join(lines)

    @staticmethod
    def _norm_guidance_text(text: Any) -> str:
        """Normalization for the standing-guidance/operator-directive dedupe:
        strip, collapse whitespace, casefold."""
        return " ".join(str(text or "").split()).casefold()

    def _dedupe_standing_guidance(self, standing_guidance: Optional[list[str]]) -> list[str]:
        """An item already surfaced verbatim as an ACTIVE operator directive must
        not render twice in the reason summary — the directives block wins (it
        carries the action/status bracket), the standing-guidance copy is dropped."""
        items = [str(x) for x in (standing_guidance or [])]
        if not items:
            return items
        directive_texts = {
            self._norm_guidance_text(d.get("text"))
            for d in self.operator_directives(active_only=True)
        }
        directive_texts.discard("")
        if not directive_texts:
            return items
        return [x for x in items
                if self._norm_guidance_text(x) not in directive_texts]

    def _attempted_intents_block(
        self, limit: int = 40, *, after_seq: int = 0,
    ) -> str:
        """Render CONCLUDED intents with each one's conclusion text, so the Reason
        planner sees what was already tried AND what came of it (run-11190: the
        planner kept re-proposing paraphrases of concluded directions because the
        summary never showed them). The result comes from the EV_INTENT_CONCLUDED
        event the row's result_seq points at; superseded/no-result rows render a
        placeholder. Most recent `limit` shown (oldest→newest); earlier ones are
        collapsed into a count line — a goal is one line, so 40 stays cheap."""
        where = "WHERE i.status='done'"
        params: tuple[Any, ...] = ()
        if after_seq > 0:
            where += " AND COALESCE(i.result_seq,0)>?"
            params = (int(after_seq),)
        with self._lock:
            rows = self._conn.execute(
                "SELECT i.goal, e.payload, i.worker_class, i.route_hash, i.branch_id, "
                "i.result_detail FROM intents i "
                "LEFT JOIN events e ON e.seq = i.result_seq "
                f"{where} ORDER BY i.created_seq",
                params,
            ).fetchall()
        if not rows:
            return ""
        omitted = max(0, len(rows) - limit)
        ctf_mode = getattr(self.challenge, "mode", "ctf") == "ctf"
        lines = [
            "\n## Concluded Steps"
            if ctf_mode
            else "\n## Already attempted (concluded intents — do NOT re-propose; "
                 "build on their results)"
        ]
        if omitted:
            lines.append(f"  (… {omitted} earlier attempted intents omitted)")
        for goal, payload, worker_class, route_hash, branch_id, row_detail in rows[-limit:]:
            result = ""
            detail = str(row_detail or "")
            if payload:
                try:
                    p = json.loads(payload) or {}
                    result = str(p.get("result", ""))
                    detail = detail or str(p.get("result_detail", ""))
                except (json.JSONDecodeError, TypeError):
                    result = ""
            tail = result.strip() if result.strip() else "(superseded / no result recorded)"
            if detail.strip():
                tail = f"{tail}: {detail.strip()}"
            meta = []
            if not ctf_mode and worker_class and worker_class != "code":
                meta.append(str(worker_class))
            if not ctf_mode and route_hash:
                meta.append(f"route={route_hash}")
            if not ctf_mode and branch_id:
                meta.append(f"branch={branch_id}")
            suffix = f" ({', '.join(meta)})" if meta else ""
            lines.append(f"- {str(goal)}{suffix} → {tail}")
        return "\n".join(lines)

    def challenged_facts(self) -> list[dict]:
        if getattr(self.challenge, "mode", "ctf") == "ctf":
            return []
        texts = self._fact_text_by_seq()
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq, status, reason, verification_intent_id "
                "FROM fact_reviews WHERE challenge_id=? AND status='challenged' "
                "ORDER BY challenged_seq",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"fact_seq": int(r[0]), "status": r[1], "reason": r[2] or "",
             "verification_intent_id": r[3] or "", "fact": texts.get(int(r[0]), "")}
            for r in rows
        ]

    def revalidated_facts(self) -> list[dict]:
        if getattr(self.challenge, "mode", "ctf") == "ctf":
            return []
        texts = self._fact_text_by_seq()
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq, status, reason FROM fact_reviews "
                "WHERE challenge_id=? AND status='revalidated' ORDER BY revalidated_seq",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"fact_seq": int(r[0]), "status": r[1], "reason": r[2] or "",
             "fact": texts.get(int(r[0]), "")}
            for r in rows
        ]

    def retired_facts(self, *, states: Optional[tuple[str, ...]] = None) -> list[dict]:
        """Facts in a terminal lifecycle state (rejected/merged/superseded) — for the
        review/audit board (kept visible but de-verified)."""
        if getattr(self.challenge, "mode", "ctf") == "ctf":
            return []
        if not self._table_exists("fact_states"):
            return []
        want = states or (FACT_STATE_REJECTED, FACT_STATE_MERGED, FACT_STATE_SUPERSEDED)
        texts = self._fact_text_by_seq(include_retired=True)
        q = ",".join("?" for _ in want)
        with self._lock:
            rows = self._conn.execute(
                f"SELECT fact_seq, state, reason, merged_seq FROM fact_states "
                f"WHERE challenge_id=? AND state IN ({q}) ORDER BY updated_seq",
                (self.challenge.id, *want),
            ).fetchall()
        merges: dict[int, int] = {}
        if self._table_exists("fact_merges"):
            with self._lock:
                mrows = self._conn.execute(
                    "SELECT from_fact_seq, to_fact_seq FROM fact_merges WHERE challenge_id=?",
                    (self.challenge.id,),
                ).fetchall()
            merges = {int(m[0]): int(m[1]) for m in mrows}
        return [
            {"fact_seq": int(r[0]), "state": r[1], "reason": r[2] or "",
             "fact": texts.get(int(r[0]), ""),
             "merged_into": merges.get(int(r[0]))}
            for r in rows
        ]

    def suppressed_routes(self) -> list[dict]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT route_hash, label, reason, until_policy, suppressed_seq "
                "FROM routes WHERE challenge_id=? AND status='suppressed' "
                "ORDER BY suppressed_seq",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"route_hash": r[0], "label": r[1], "reason": r[2] or "",
             "until": r[3] or "new_evidence", "suppressed_seq": r[4]}
            for r in rows
        ]

    def is_route_suppressed(self, route_hash: str) -> bool:
        route = self.normalize_route_hash(route_hash)
        with self._lock:
            row = self._conn.execute(
                "SELECT status FROM routes WHERE challenge_id=? AND route_hash=?",
                (self.challenge.id, route),
            ).fetchone()
        return bool(row and row[0] == "suppressed")

    def branches(self) -> list[dict]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT branch_id, parent_id, title, assumption, prove_or_disprove, "
                "status, source_intent, from_facts_json, expected_observable, "
                "stop_condition, coverage_key, route_hash, lane_key, risk_class, "
                "resource_key "
                "FROM branches WHERE challenge_id=? ORDER BY created_seq, branch_id",
                (self.challenge.id,),
            ).fetchall()
        out: list[dict] = []
        for r in rows:
            try:
                from_facts = [int(seq) for seq in json.loads(r[7] or "[]")]
            except (TypeError, ValueError, json.JSONDecodeError):
                from_facts = []
            out.append({
                "branch_id": r[0], "parent_id": r[1] or "", "title": r[2] or "",
                "assumption": r[3] or "", "prove_or_disprove": r[4] or "",
                "status": r[5] or "open", "source_intent": r[6] or "",
                "from_facts": from_facts, "expected_observable": r[8] or "",
                "stop_condition": r[9] or "", "coverage_key": r[10] or "",
                "route_hash": r[11] or "",
                "lane_key": r[12] or "", "risk_class": r[13] or "",
                "resource_key": r[14] or "",
            })
        return out

    def coordinator_directives(self) -> list[dict]:
        out: list[dict] = []
        for e in self.events():
            if e.get("kind") == EV_COORDINATOR_DIRECTIVE:
                p = dict(e.get("payload") or {})
                p["seq"] = e.get("seq")
                p["actor"] = e.get("actor")
                out.append(p)
        return out

    def latest_unconsumed_directive_seq(self, *, after_seq: int = 0,
                                        action: str = "") -> Optional[dict]:
        directives = [
            d for d in self.coordinator_directives()
            if int(d.get("seq") or 0) > int(after_seq or 0)
            and (not action or d.get("action") == action)
        ]
        return directives[-1] if directives else None

    def genuine_failures_for_route(self, route_hash: str) -> int:
        route = self.normalize_route_hash(route_hash)
        with self._lock:
            rows = self._conn.execute(
                "SELECT e.payload FROM intents i "
                "LEFT JOIN events e ON e.seq = i.result_seq "
                "WHERE i.challenge_id=? AND i.route_hash=? AND i.status='done'",
                (self.challenge.id, route),
            ).fetchall()
        count = 0
        for (payload,) in rows:
            result = ""
            if payload:
                try:
                    result = str((json.loads(payload) or {}).get("result", "")).lower()
                except (json.JSONDecodeError, TypeError):
                    result = ""
            if not result:
                continue
            if any(skip in result for skip in (
                "timeout", "timed out", "cancelled", "canceled", "steered",
                "oom", "killed", "route_suppressed", "superseded",
                "lane_deferred", "lane_blocked", "closed_by_solve",
            )):
                continue
            if any(tok in result for tok in (
                "dead", "failed", "no flag", "no verified flag", "gave up",
                "exhausted", "not exploitable",
            )):
                count += 1
        return count

    def _review_state_block(self) -> str:
        parts: list[str] = []
        challenged = self.challenged_facts()
        if challenged:
            parts.append("\n## Challenged facts (DO NOT treat as verified until revalidated)")
            for f in challenged:
                parts.append(
                    f"- [#{f['fact_seq']}] {f['fact']} :: {f['reason']} "
                    f"(verify via {f['verification_intent_id']})")
        revalidated = self.revalidated_facts()
        if revalidated:
            parts.append("\n## Revalidated facts")
            for f in revalidated:
                parts.append(f"- [#{f['fact_seq']}] {f['fact']} :: {f['reason']}")
        retired = self.retired_facts()
        if retired:
            parts.append("\n## Retired facts (rejected/merged/superseded — do NOT use as evidence)")
            for f in retired:
                tag = f['state']
                if f['state'] == FACT_STATE_MERGED and f.get('merged_into'):
                    tag = f"merged→#{f['merged_into']}"
                parts.append(f"- [#{f['fact_seq']}] ({tag}) {f['fact']} :: {f['reason']}")
        suppressed = self.suppressed_routes()
        if suppressed:
            parts.append("\n## Suppressed routes (ordinary workers must not retry)")
            for r in suppressed:
                parts.append(
                    f"- {r['route_hash']} ({r['label']}): {r['reason']} "
                    f"until={r['until']}")
        branches = self.branches()
        if branches:
            parts.append("\n## Open branches (do not mix incompatible assumptions)")
            for b in branches:
                sources = ",".join(f"#{seq}" for seq in b["from_facts"]) or "none"
                parts.append(
                    f"- {b['branch_id']} [{b['status']}]: {b['assumption']} "
                    f"| source_intent={b['source_intent'] or 'none'} "
                    f"from_facts={sources} route={b['route_hash'] or 'unspecified'} "
                    f"coverage={b['coverage_key'] or 'unspecified'} "
                    f"lane={b['lane_key'] or 'none'} "
                    f"risk={b['risk_class'] or 'none'} "
                    f"resource={b['resource_key'] or 'none'} "
                    f"expected={b['expected_observable'] or b['prove_or_disprove']} "
                    f"stop={b['stop_condition'] or 'unspecified'}")
        directives = self.coordinator_directives()
        if directives:
            parts.append("\n## Review directives")
            for d in directives:
                parts.append(
                    f"- #{d.get('seq')} {d.get('action')}[{d.get('priority','normal')}]: "
                    f"{str(d.get('directive',''))}")
        return "\n".join(parts)

    def _poc_block(self, *, limit: int = 30) -> str:
        rows = self.pocs(inheritable_only=False)
        if not rows:
            return ""
        visible = [p for p in rows if p.get("status") != "quarantined"]
        if not visible:
            return "\n## Shared PoCs\n- all saved PoCs are quarantined; do not inherit them"
        # Only the PoCs the linker actually mounts get the "./inherited/<poc_id>/"
        # promise (#10). A 'wip' PoC under a live lease, or a 'spent' one, is NOT
        # linked into any worker cwd, so advertising that path for it points at a
        # folder that doesn't exist. Split: inheritable (path promised) vs historical
        # (metadata only, no path). The inheritable set is exactly pocs(inheritable
        # _only=True) so the board and the linker never disagree.
        inheritable_ids = {p["poc_id"] for p in self.pocs(inheritable_only=True)}

        def _render(items: list[dict]) -> list[str]:
            out: list[str] = []
            for p in items:
                iid = f" intent={p['intent_id']}" if p.get("intent_id") else ""
                note = f" — {str(p.get('note') or '')}" if p.get("note") else ""
                out.append(f"- {p['poc_id']} ({p['status']}){iid}: "
                           f"{p['entry_command']}{note}")
            return out

        inheritable = [p for p in visible if p["poc_id"] in inheritable_ids]
        historical = [p for p in visible if p["poc_id"] not in inheritable_ids]
        lines: list[str] = []
        if inheritable:
            lines.append("\n## Inheritable PoCs (run/copy under ./inherited/<poc_id>/)")
            omitted = max(0, len(inheritable) - limit)
            if omitted:
                lines.append(f"  (... {omitted} older inheritable PoCs omitted)")
            lines.extend(_render(inheritable[-limit:]))
        if historical:
            # in-use (wip, currently leased) or spent — listed for context, but NOT
            # mounted; don't tell a worker to run them from ./inherited/.
            lines.append("\n## Historical PoCs (in-use or spent; metadata only, not mounted)")
            omitted = max(0, len(historical) - limit)
            if omitted:
                lines.append(f"  (... {omitted} older historical PoCs omitted)")
            lines.extend(_render(historical[-limit:]))
        return "\n".join(lines)

    def _standing_guidance_block(self, standing_guidance: Optional[list[str]]) -> str:
        items = [
            str(x).strip()
            for x in (standing_guidance or [])
            if str(x).strip()
        ]
        if not items:
            return ""
        lines = ["\n## Operator standing guidance (highest priority; guidance, not evidence)"]
        for item in items:
            # Round-7: fruitless-interrupt / chain-completion packets carry
            # REQUIRED/FORBIDDEN replan constraints; keep them intact.
            lines.append(f"- {item}")
        return "\n".join(lines)

    def _operator_directives_block(self) -> str:
        """B: active operator directives the planner MUST prioritize (highest
        priority; guidance, not proven evidence)."""
        directives = self.operator_directives(active_only=True)
        if not directives:
            return ""
        lines = ["\n## Operator directives (MUST prioritize — guidance, not evidence)"]
        for d in directives:
            text = str(d.get("text") or "")
            # Round-7 fruitless-interrupt MUST constraint needs full wording.
            lines.append(f"- [{d['action']}/{d['status']}] {text}")
        return "\n".join(lines)

    def _forbidden_zones_block(self) -> str:
        """D/E: the exclusive lanes + held resource locks the planner must route
        AROUND (don't propose intents that collide with an active lock)."""
        parts: list[str] = []
        lanes = self.active_lanes()
        locks = self.active_resource_locks()
        if not lanes and not locks:
            return ""
        parts.append("\n## Forbidden zones (locked — do NOT propose conflicting work)")
        for lane in lanes:
            parts.append(f"- lane {lane['lane_key']} [{lane['owner_worker']}]")
        for rl in locks:
            parts.append(f"- resource {rl['resource_key']} (scope={rl['scope']}) "
                         f"[{rl['owner_worker']}]")
        return "\n".join(parts)

    def _dead_ends_context_block(self, rows: list[dict], title: str = "") -> str:
        """Compact scoped dead-end section for a projected worker view: one line
        per row — reason plus short scope tags (intent/route/coverage/epoch) so
        the worker can tell why each row landed in ITS context."""
        if not rows:
            return ""
        lines = [title or "\n## Dead ends relevant to this step"]
        for r in rows:
            tags = []
            if r.get("intent_id"):
                tags.append(f"intent={r['intent_id']}")
            if r.get("route_hash"):
                tags.append(f"route={r['route_hash']}")
            if r.get("coverage_key"):
                tags.append(f"coverage={r['coverage_key']}")
            if r.get("target_epoch"):
                tags.append(f"epoch={r['target_epoch']}")
            suffix = f" ({', '.join(tags)})" if tags else ""
            detail = str(r.get("reason") or "")
            if r.get("tested_scope"):
                detail += f" | tested: {r['tested_scope']}"
            if r.get("observed_result"):
                detail += f" | observed: {r['observed_result']}"
            lines.append(f"- {detail}{suffix}")
        return "\n".join(lines)

    def dead_ends_context_block(self, *, intent_id: str = "", coverage_key: str = "",
                                route_hash: str = "", target_epoch: str = "",
                                limit: int = 10**9, title: str = "",
                                epoch_wide: bool = False) -> str:
        """Scoped dead-end section for a worker's projected view: rows selected
        by dead_ends_for_context rendered via _dead_ends_context_block. Empty
        when nothing intersects the given scope. ``epoch_wide`` passes through
        to the row selection (bootstrap: the whole epoch's ruled-out ground)."""
        rows = self.dead_ends_for_context(
            intent_id=intent_id, coverage_key=coverage_key,
            route_hash=route_hash, target_epoch=target_epoch, limit=limit,
            epoch_wide=epoch_wide)
        return self._dead_ends_context_block(rows, title=title)

    def _search_gaps_block(self, *, limit: int = 16) -> str:
        """Coverage/route keys whose intents have ALL concluded (nothing open or
        claimed right now) — a planner hint to re-open a direction deliberately
        or leave it closed, instead of silently re-proposing a paraphrase."""
        if not self._column_exists("intents", "coverage_key"):
            return ""
        with self._lock:
            rows = self._conn.execute(
                "SELECT i.coverage_key, i.route_hash, i.status, i.dispatch_state, "
                "EXISTS(SELECT 1 FROM intent_products p "
                "       WHERE p.intent_id=i.intent_id) "
                "FROM intents i WHERE i.challenge_id=? "
                "AND i.dispatch_state!='retired' "
                "AND worker_class NOT IN ('verifier','review')",
                (self.challenge.id,),
            ).fetchall()
        buckets: dict[tuple[str, str], dict] = {}
        for cov, route, status, dispatch, has_product in rows:
            for kind, key in (("coverage", str(cov or "").strip()),
                              ("route", str(route or "").strip())):
                if not key:
                    continue
                b = buckets.setdefault((kind, key),
                                       {"open": 0, "done": 0, "productive": 0})
                if status in ("open", "claimed") and dispatch == "active":
                    b["open"] += 1
                elif status == "done":
                    b["done"] += 1
                    if has_product:
                        b["productive"] += 1
        gaps = [(kind, key, b) for (kind, key), b in buckets.items()
                if not b["open"] and b["done"]]
        if not gaps:
            return ""
        gaps.sort(key=lambda g: (g[0], g[1]))
        omitted = max(0, len(gaps) - limit)
        lines = ["\n## Search gaps (coverage with no open intent)"]
        if omitted:
            lines.append(f"  (... {omitted} further gap keys omitted)")
        for kind, key, b in gaps[:limit]:
            if b["productive"]:
                note = f"{b['productive']}/{b['done']} concluded produced a fact"
            else:
                note = f"{b['done']} concluded, none produced a fact"
            lines.append(f"- {kind}={key}: {note}")
        return "\n".join(lines)

    def _review_findings_block(self, *, limit: int = 10**9) -> str:
        """Newest review-arbiter findings for the Reason planner: the arbiter no
        longer plans (no intent-creating marker), so its diagnostics land here —
        kind/severity/summary plus the recommended_actions the next plan should
        fold in. Flag-shaped literals are scrubbed like the review projection."""
        rows: list[dict] = []
        for e in self.events():
            if e.get("kind") != EV_REVIEW_FINDING:
                continue
            p = e.get("payload")
            if isinstance(p, dict):
                rows.append(p)
        if not rows:
            return ""
        lines = ["\n## Review findings (diagnostics from the arbiter — fold into planning)"]
        for p in rows[-limit:]:
            kind = str(p.get("kind") or "no_action")
            severity = str(p.get("severity") or "info")
            summary = str(p.get("summary") or "")
            actions = [
                str(a)
                for a in p.get("recommended_actions", []) if a
            ]
            tail = f" → {'; '.join(actions)}" if actions else ""
            lines.append(f"- [{severity}] {kind}: {summary}{tail}")
        return "\n".join(lines)

    def to_reason_summary(
        self, standing_guidance: Optional[list[str]] = None, *,
        include_lineage: bool = False, compact_summary: str = "",
        compact_cutoff_seq: int = 0,
    ) -> str:
        """The PLANNER's board view: the uncapped [#seq]-labelled summary (all
        facts AND all dead-ends — the P1.5 un-blinding lifted only the evidence
        cap; dead-ends stayed clipped to the last 8, so long-run planners forgot
        old dead directions) plus the two intent sections the snapshot can't
        carry: in-flight (open/claimed) and attempted-with-results. REASON_SYSTEM
        references both section titles in its no-re-proposal rule.

        Phase 4: now also carries active operator directives (B, must-prioritize)
        and forbidden zones (D/E, locked lanes/resources to route around). Retired
        facts (rejected/merged/superseded) are already dropped by snapshot(); only
        Phase 4: now also carries active operator directives (B, must-prioritize)
        and forbidden zones (D/E, locked lanes/resources to route around). Retired
        facts (rejected/merged/superseded) are already dropped by snapshot(); only
        dispatch_state='active' intents appear in the open-intents block.

        Graph-VIEW pass: the active-intent lineage section is folded into the
        open-intents parenthetical by default (include_lineage=True restores the
        standalone section); standing guidance already shown verbatim as an
        active operator directive renders only in the directives block; and a
        Search-gaps section lists coverage/route keys with no open intent."""
        cutoff = max(0, int(compact_cutoff_seq))
        relevant = self._reason_relevant_fact_seqs(after_seq=cutoff)
        guidance = self._dedupe_standing_guidance(standing_guidance)
        checkpoint = (
            "# Historical Reason checkpoint\n" + str(compact_summary).strip()
            if str(compact_summary or "").strip() else ""
        )
        if getattr(self.challenge, "mode", "ctf") == "ctf":
            return self.to_ctf_graph_yaml()
        parts = [checkpoint,
                 self._summary_for_fact_seqs(
                     relevant, max_dead_ends=10**9, after_seq=cutoff),
                 self._captured_flags_block(),   # defect-9: already-solved directions
                 self._captured_findings_block(),
                 self._standing_guidance_block(guidance),
                 self._operator_directives_block(),
                 self._forbidden_zones_block(),
                 self._review_state_block(),
                 self._review_findings_block(),
                 self._active_intent_lineage_block() if include_lineage else "",
                 self._open_intents_block(limit=10**9, with_lineage=not include_lineage),
                 self._search_gaps_block(limit=10**9),
                 self._poc_block(limit=10**9),
                 self._attempted_intents_block(after_seq=cutoff),
                 self.access_context_block(),
                 self._reason_frontier_block()]
        return "\n".join(p for p in parts if p and p.strip())

    def _reason_frontier_block(
        self, *, verified_limit: int = 8, candidate_limit: int = 8,
    ) -> str:
        """Repeat the small, current evidence frontier at the end of Reason input.

        The full graph remains authoritative above.  This trailing view prevents
        a newly unlocked capability from being buried under a long attempted-work
        section, while keeping both the verification state and exact fact ids.
        """
        verified_n = max(0, verified_limit)
        verified = (
            self._latest_verified_fact_seqs()[-verified_n:]
            if verified_n else []
        )
        texts = self._fact_text_by_seq()
        if not verified:
            return ""
        lines = [
            "\n## Immediate planning frontier (use this first for the next step)"
        ]
        if getattr(self.challenge, "mode", "ctf") == "ctf":
            try:
                expected = max(1, int(self.challenge.expected_flags or 1))
            except (TypeError, ValueError):
                expected = 1
            captured = len(self.snapshot().flags)
            lines.append(f"- Objective progress: {captured}/{expected} flags captured")
        for seq in reversed(verified):
            fact = str(texts.get(int(seq), "") or "").strip()
            if fact:
                lines.append(f"- verified [#{int(seq)}] {fact[:700]}")
        return "\n".join(lines)

    def _captured_flags_block(self) -> str:
        """defect-9: the flags the run already holds. Surfaced to the planner so it
        does NOT re-propose intents aiming at an already-captured flag (the ezrop-ROP
        re-do: a worker re-running a direction that already yielded flag1). Empty when
        no flags yet — single/zero-flag runs are byte-identical."""
        flags = self.snapshot().flags
        if not flags:
            return ""
        lines = "\n".join(f"  - {f}" for f in flags)
        if getattr(self.challenge, "mode", "ctf") == "ctf":
            return f"\n## Captured flags\n{lines}\n"
        return ("\n## Flags already captured (do NOT propose any intent to re-recover "
                "these — those directions are DONE):\n"
                f"{lines}\n")

    def _captured_findings_block(self) -> str:
        findings = self.snapshot().findings
        if not findings:
            return ""
        lines = []
        for f in findings:
            lines.append(
                f"  - {f.get('finding_class','')} {f.get('resource_id','')} "
                f"({f.get('identity_a','')} / {f.get('identity_b','')})"
            )
        body = "\n".join(lines)
        return (
            "\n## Gated findings already accepted (do NOT re-propose these; "
            "acceptance is the evidence predicate, not a verbal claim):\n"
            f"{body}\n"
        )

    def _credential_block(self, creds: "Optional[list[dict]]" = None) -> str:
        """The canonical credential / unlock-chain section (also used standalone as
        the inline prompt digest). Empty string when no creds qualify."""
        creds = self.canonical_credentials() if creds is None else creds
        if not creds:
            return ""
        chain = " → ".join(f"{c['entity']}:{c['value']}" for c in creds)
        return ("\n## Recovered credentials / unlock chain "
                "(heuristically derived — verify before trusting)\n"
                f"{chain}\n")

    def _brief_block(self) -> str:
        """The FULL, untruncated challenge brief for the board file. SolveGraph's
        to_summary caps the description at 300 chars — but the brief is exactly where
        target/connection blocks live (e.g. an `SSH Access` host/port/creds section,
        run-10070), so capping it forces workers to dig the target out of session
        files. The file has no budget, so carry the whole thing here.

        Target/attachments come from the prompt builder (not the graph), so we render
        what the SolveGraph snapshot has: the challenge description verbatim."""
        c = self.challenge
        desc = (getattr(c, "description", "") or "").strip()
        if not desc:
            return ""
        return ("\n## Challenge brief (full — read for target/connection details)\n"
                f"{desc}\n")

    def to_board_markdown(self) -> str:
        """The FULL board rendered for the workdir file (no truncation): the
        canonical credential chain on top, the FULL challenge brief (target/SSH
        block lives here), then the untruncated [#seq]-labelled fact summary, then
        open intents. The credential section is also the inline prompt digest
        (rendered alone via _credential_block).

        Uses to_summary(max_evidence=10**9, max_dead_ends=10**9) so stage-1 [-16]
        is defeated, ALL dead-ends are shown (a worker re-walking a long-ruled-out
        path is the same waste the planner suffers — see to_reason_summary), and
        the [#seq] labels (the stable fact ids that Reason cites via `from`)
        are preserved; the caller drops the stage-2 [:2000] clip by using this
        method instead of the inline path."""
        creds = self.canonical_credentials()
        parts = [self._credential_block(creds), self._brief_block(),
                 self.to_summary(max_evidence=10**9, max_dead_ends=10**9),
                 self._review_state_block(),
                 self._open_intents_block(),
                 self._poc_block(),
                 # P4: in-progress activities a teammate is doing right now (avoid
                 # redoing a nmap/brute already underway).
                 self._activity_locks_block(),
                 self._lane_locks_block(),
                 self._resource_locks_block(),
                 # P1-A: also show CONCLUDED directions (+ results) to WORKERS, not
                 # just the Reason planner. Without this the board was asymmetric
                 # (to_reason_summary had it, to_board_markdown didn't), so a new
                 # worker re-walked directions already attempted-and-concluded — the
                 # "重走老路" report. A goal is one line; the file has no budget.
                 self._attempted_intents_block()]
        return "\n".join(p for p in parts if p and p.strip())

    def to_review_summary(self) -> str:
        """Review-Arbiter's full audit view. It intentionally includes more than
        Reason's compact planner view: raw event tails, all fact classes, route
        state, branch state, intent lifecycle, PoCs, flags, and operator/review
        directives. It is still derived from append-only events/materialized views."""
        parts = [
            "# Review-Arbiter audit board",
            self._brief_block(),
            self.to_summary(max_evidence=10**9, max_dead_ends=10**9),
            self._captured_flags_block(),
            self._captured_findings_block(),
            self._review_state_block(),
            self._open_intents_block(),
            self._poc_block(limit=80),
            self._activity_locks_block(),
            self._lane_locks_block(),
            self._resource_locks_block(),
            self._attempted_intents_block(limit=120),
            "\n## Recent raw events",
        ]
        for e in self.events()[-80:]:
            payload = dict(e.get("payload") or {})
            preview = json.dumps(payload, ensure_ascii=False, default=str)[:500]
            parts.append(
                f"- #{e.get('seq')} {e.get('kind')} actor={e.get('actor')} "
                f"verified={e.get('verified')} {preview}")
        return "\n".join(p for p in parts if p and str(p).strip())

    def to_review_projection(self, *, fact_seqs: Optional[list[int]] = None,
                             since_seq: int = 0, directive: str = "",
                             limit_facts: int = 10**9) -> str:
        """The review worker's SCOPED board: only the facts under review — each
        with its observation rows (fact_observed events) and a one-line evidence
        provenance summary — the intents that produced them, the standing review
        state, and the dead ends intersecting that scope. Replaces the full
        audit dump (to_review_summary) for the review prompt: NO raw-event tail,
        NO full brief, NO locks/activity tables. Deterministic and bounded;
        payload fields are preserved verbatim."""
        states = self._fact_state_map()
        wanted: list[int] = []
        seen: set[int] = set()
        for raw in fact_seqs or []:
            try:
                seq = int(raw)
            except (TypeError, ValueError):
                continue
            if seq > 0 and seq not in seen:
                seen.add(seq)
                wanted.append(seq)
        params: list[Any] = [EV_FACT_ADDED]
        if wanted:
            where = "kind=? AND seq IN (" + ",".join("?" for _ in wanted) + ")"
            params.extend(wanted)
        else:
            where = "kind=? AND seq > ?"
            params.append(int(since_seq or 0))
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, ts, actor, payload, artifact_id, verified, confidence "
                f"FROM events WHERE {where} ORDER BY seq DESC LIMIT ?",
                (*params, int(limit_facts)),
            ).fetchall()
        scope = [int(r[0]) for r in rows]
        obs_by_fact: dict[int, list[dict]] = {}
        if scope:
            q = ",".join("?" for _ in scope)
            with self._lock:
                ors = self._conn.execute(
                    "SELECT seq, ts, actor, json_extract(payload,'$.fact_seq') "
                    "FROM events WHERE kind=? "
                    f"AND json_extract(payload,'$.fact_seq') IN ({q}) ORDER BY seq",
                    (EV_FACT_OBSERVED, *scope),
                ).fetchall()
            for oseq, ots, oactor, fseq in ors:
                if fseq is None:
                    continue
                obs_by_fact.setdefault(int(fseq), []).append(
                    {"seq": int(oseq), "ts": ots, "actor": str(oactor or "")})
        header = "# Review scope"
        trigger = str(directive or "").strip()
        if trigger:
            header += f" — trigger: {trigger}"
        parts: list[str] = [header]
        fact_lines = ["\n## Facts under review"]
        producer_ids: set[str] = set()
        parsed: list[tuple[int, str, dict, Any, bool, Any]] = []
        for seq, ts, actor, payload, aid, verified, conf in rows:
            try:
                p = json.loads(payload) or {}
            except (json.JSONDecodeError, TypeError):
                p = {}
            parsed.append((int(seq), str(actor or ""), p, aid, bool(verified), conf))
            pid = str(p.get("intent_id") or "").strip()
            if pid:
                producer_ids.add(pid)
        for seq, actor, p, aid, verified, conf in parsed:
            st = states.get(seq, {})
            eff = st.get("verified_effective")
            is_verified = verified if eff is None else bool(eff)
            if st.get("state") == FACT_STATE_CHALLENGED:
                is_verified = False
            verdict = "verified" if is_verified else "candidate"
            lifecycle = str(st.get("state") or FACT_STATE_UNRESOLVED)
            if lifecycle != FACT_STATE_UNRESOLVED:
                verdict += f"/{lifecycle}"
            eff_conf = st.get("confidence_effective")
            conf_val = float(conf or 0) if eff_conf is None else float(eff_conf)
            fact_lines.append(
                f"- [#{seq}] ({verdict}, confidence={conf_val:.2f}) "
                f"{str(p.get('fact') or '')}")
            prov = p.get("evidence_provenance")
            prov = prov if isinstance(prov, dict) else {}
            prov_bits = {
                "worker": prov.get("worker_id") or p.get("source_solver") or actor,
                "intent": prov.get("intent_id") or p.get("intent_id") or "",
                "epoch": prov.get("target_epoch") or p.get("target_epoch") or "",
                "artifact": prov.get("artifact_id") or aid or "",
                "tool_event": prov.get("tool_event_id") or "",
            }
            prov_txt = " ".join(
                f"{k}={v}"
                for k, v in prov_bits.items() if str(v or "").strip())
            if prov_txt:
                fact_lines.append(f"  provenance: {prov_txt}")
            obs = obs_by_fact.get(seq, [])
            for o in obs:
                fact_lines.append(
                    f"  - obs [#{o['seq']}] by {o['actor']} "
                    f"at {float(o['ts'] or 0):.0f}")
        if not parsed:
            fact_lines.append("- (no facts in scope)")
        parts.append("\n".join(fact_lines))
        if scope:
            q = ",".join("?" for _ in scope)
            with self._lock:
                prs = self._conn.execute(
                    "SELECT DISTINCT intent_id FROM intent_products "
                    f"WHERE fact_seq IN ({q})",
                    tuple(scope),
                ).fetchall()
            producer_ids.update(str(r[0]) for r in prs if r and r[0])
        related_bindings: list[dict] = []
        if producer_ids:
            cov_sel = ("i.coverage_key" if self._column_exists("intents", "coverage_key")
                       else "NULL")
            qi = ",".join("?" for _ in producer_ids)
            with self._lock:
                irs = self._conn.execute(
                    "SELECT i.intent_id, i.goal, i.status, i.route_hash, "
                    f"{cov_sel}, e.payload FROM intents i "
                    "LEFT JOIN events e ON e.seq = i.result_seq "
                    f"WHERE i.intent_id IN ({qi}) ORDER BY i.created_seq",
                    tuple(sorted(producer_ids)),
                ).fetchall()
            related_lines = ["\n## Related intents"]
            for iid, goal, status, route, cov, payload in irs:
                result = ""
                if payload:
                    try:
                        result = str((json.loads(payload) or {}).get("result") or "")
                    except (json.JSONDecodeError, TypeError):
                        result = ""
                meta = []
                if route:
                    meta.append(f"route={route}")
                if cov:
                    meta.append(f"coverage={cov}")
                suffix = f" ({', '.join(meta)})" if meta else ""
                tail = f" → {result}" if result else ""
                related_lines.append(
                    f"- {iid} [{status}]: {str(goal)}{suffix}{tail}")
                related_bindings.append({
                    "intent_id": str(iid), "route_hash": str(route or ""),
                    "coverage_key": str(cov or "")})
            parts.append("\n".join(related_lines))
        review_state = self._review_state_block()
        if review_state and review_state.strip():
            parts.append(review_state)
        dead_rows: list[dict] = []
        seen_dead: set[int] = set()
        for b in related_bindings:
            for r in self.dead_ends_for_context(
                    intent_id=b["intent_id"], coverage_key=b["coverage_key"],
                    route_hash=b["route_hash"], limit=24):
                if r["seq"] in seen_dead:
                    continue
                seen_dead.add(r["seq"])
                dead_rows.append(r)
        dead_rows.sort(key=lambda r: -int(r["seq"]))
        dead_block = self._dead_ends_context_block(
            dead_rows, title="\n## Relevant dead ends")
        if dead_block:
            parts.append(dead_block)
        return "\n".join(p for p in parts if p and str(p).strip())
