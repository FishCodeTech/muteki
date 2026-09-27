#!/usr/bin/env python3
"""Append-only recovery for legacy Flag submissions rejected by the old gate.

The old provenance implementation could reject a real Flag even when the value
was already present in a Worker-owned ``tool.result`` record.  This command
repairs only that narrow, auditable shape.  It never edits historic JSONL or
SQLite rows:

* the graph receives a new ``flag_submission`` + terminal decision +
  ``flag_found`` through ``SQLiteSharedGraph.resolve_flag_submission``;
* the session log receives new projection events, so a future SessionStore
  replay shows the recovered result.

The two stores cannot share one transaction.  A deterministic reconciliation id
makes the operation recoverable: re-running after a crash completes a missing
projection without duplicating graph or JSONL events.

By default this command is a dry run.  ``--apply`` is deliberately restricted
to a run whose latest lifecycle event is ``run.finished``.
"""

from __future__ import annotations

import argparse
from collections import defaultdict
from collections.abc import Iterable, Mapping
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import sys
import time
from typing import Any


# Running ``python cmd/reconcile_legacy_flags.py`` makes cmd/ the first import
# location.  Add the repository root explicitly so this remains a standalone
# command instead of relying on a caller's PYTHONPATH.
_REPO_ROOT = Path(__file__).resolve().parents[1]
if str(_REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(_REPO_ROOT))

from muteki.core.path_ids import decode_run_id, encode_run_id  # noqa: E402
from muteki.models.solve_graph import Challenge  # noqa: E402
from muteki.solver.gate import FlagFormatError, flag_verdict  # noqa: E402
from muteki.swarm.shared_graph import SQLiteSharedGraph  # noqa: E402


LEGACY_PROTOCOL = "legacy-event-reconciliation-v1"
RECONCILER_ACTOR = "legacy-reconciler"
_LIFECYCLE_TYPES = frozenset({"run.preparing", "run.started", "run.reopened", "run.finished"})
# Synthetic recovery is intentionally narrower than a historic rejected-submit
# recovery: only a named brace token, in two independent Workers' non-board tool
# output, may compensate for an old build that never created a submission row.
_SYNTHETIC_BRACE_CANDIDATE = re.compile(
    r"(?<![A-Za-z0-9_])(?P<candidate>[A-Za-z][A-Za-z0-9_]{0,31}"
    r"\{[^{}\r\n]{1,200}\})(?![A-Za-z0-9_])"
)


def _object(value: object) -> dict[str, Any]:
    return dict(value) if isinstance(value, Mapping) else {}


def _payload(event: Mapping[str, Any]) -> dict[str, Any]:
    return _object(event.get("payload"))


def _as_int(value: object, default: int = 0) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def _as_float(value: object, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def _read_jsonl(path: Path) -> tuple[list[dict[str, Any]], int]:
    """Read valid object rows, retaining a count for malformed/torn history."""
    events: list[dict[str, Any]] = []
    invalid_rows = 0
    try:
        with path.open("r", encoding="utf-8") as handle:
            for line in handle:
                text = line.strip()
                if not text:
                    continue
                try:
                    parsed = json.loads(text)
                except json.JSONDecodeError:
                    invalid_rows += 1
                    continue
                if isinstance(parsed, Mapping):
                    events.append(dict(parsed))
                else:
                    invalid_rows += 1
    except OSError:
        raise
    return events, invalid_rows


def _latest_challenge(events: Iterable[Mapping[str, Any]]) -> dict[str, Any]:
    """Return the latest valid ``run.preparing`` challenge payload.

    A resumed run may have multiple preparation records.  Its last preparation
    is the contract that governed its final lifecycle, and ``Challenge`` below
    applies the shared flag-format normalizer to it.
    """
    challenge: dict[str, Any] = {}
    for event in events:
        if event.get("event_type") != "run.preparing":
            continue
        candidate = _object(_payload(event).get("challenge"))
        if candidate:
            challenge = candidate
    return challenge


def _terminal_state(events: Iterable[Mapping[str, Any]]) -> tuple[bool, str]:
    """Return whether the current persisted lifecycle is terminal.

    ``control.command`` rows commonly follow ``run.finished``; only lifecycle
    records affect this decision.  A later preparing/started/reopened row means
    the run is active or being resumed and is explicitly outside this command.
    """
    last = ""
    for event in events:
        event_type = str(event.get("event_type") or "")
        if event_type in _LIFECYCLE_TYPES:
            last = event_type
    return last == "run.finished", last or "missing_lifecycle_event"


def _generations(events: Iterable[Mapping[str, Any]]) -> tuple[int, int]:
    """Keep the most recently persisted execution/control generations."""
    execution_generation = 0
    control_generation = 0
    for event in events:
        payload = _payload(event)
        if "execution_generation" in payload:
            execution_generation = _as_int(
                payload.get("execution_generation"), execution_generation)
        if "control_generation" in payload:
            control_generation = _as_int(
                payload.get("control_generation"), control_generation)
    return execution_generation, control_generation


def _max_seq(events: Iterable[Mapping[str, Any]]) -> int:
    return max((_as_int(event.get("seq")) for event in events), default=0)


def _flag_values(payload: Mapping[str, Any]) -> list[str]:
    raw = payload.get("flags")
    if raw is None:
        raw = payload.get("flag")
    values = raw if isinstance(raw, list) else [raw]
    return [str(value).strip() for value in values if str(value or "").strip()]


def _published_session_flags(events: Iterable[Mapping[str, Any]]) -> list[str]:
    """Fold the public JSONL Flag projections exactly enough for reconciliation."""
    flags: list[str] = []
    invalidated: set[str] = set()

    def add(values: Iterable[str]) -> None:
        for value in values:
            if value and value not in invalidated and value not in flags:
                flags.append(value)

    for event in events:
        event_type = str(event.get("event_type") or "")
        payload = _payload(event)
        if event_type == "run.finished":
            add(_flag_values(payload))
        elif event_type == "flag.accepted":
            add(_flag_values(payload))
        elif event_type == "insight.event" and payload.get("kind") == "FlagFound":
            add(_flag_values(payload))
        elif event_type == "blackboard.delta":
            kind = str(payload.get("kind") or "")
            if kind == "flag_invalidated":
                for value in _flag_values(payload):
                    invalidated.add(value)
                    flags[:] = [existing for existing in flags if existing != value]
            elif kind == "flag_found":
                add(_flag_values(payload))
        elif event_type == "run.reopened":
            value = str(payload.get("flag") or "").strip()
            if value:
                invalidated.add(value)
                flags[:] = [existing for existing in flags if existing != value]
    return flags


def _legacy_projection_ids(events: Iterable[Mapping[str, Any]]) -> set[str]:
    identifiers: set[str] = set()
    for event in events:
        projection_id = str(_payload(event).get("legacy_projection_id") or "")
        if projection_id:
            identifiers.add(projection_id)
    return identifiers


def _graph_path(sessions_root: Path, run_id: str) -> Path:
    return (
        sessions_root
        / encode_run_id(run_id)
        / "workspace"
        / "graph"
        / "shared_graph.db"
    )


def _read_graph_events(db_path: Path) -> list[dict[str, Any]]:
    """Read graph events in SQLite's append order without opening the DB writable."""
    uri = f"{db_path.resolve().as_uri()}?mode=ro"
    # A live Web process can be finishing a short SQLite checkpoint while this
    # append-only repair is inspecting history.  A read-only open may then fail
    # transiently on some filesystems even though the DB is sound.  Retry that
    # narrow transport shape; persistent corruption/permission errors still
    # surface to the caller after the bounded attempts.
    rows: list[tuple[Any, ...]] | None = None
    for attempt in range(3):
        connection: sqlite3.Connection | None = None
        try:
            connection = sqlite3.connect(uri, uri=True)
            rows = connection.execute(
                "SELECT seq, ts, actor, kind, payload, dedupe_key "
                "FROM events ORDER BY seq"
            ).fetchall()
            break
        except sqlite3.OperationalError:
            if attempt == 2:
                raise
            time.sleep(0.05 * (attempt + 1))
        finally:
            if connection is not None:
                connection.close()
    if rows is None:  # pragma: no cover - retry loop either returns or raises
        raise sqlite3.OperationalError("graph database could not be opened")

    events: list[dict[str, Any]] = []
    for seq, timestamp, actor, kind, payload_raw, dedupe_key in rows:
        try:
            parsed = json.loads(str(payload_raw or "{}"))
        except (TypeError, ValueError, json.JSONDecodeError):
            parsed = {}
        events.append({
            "seq": _as_int(seq),
            "ts": _as_float(timestamp),
            "actor": str(actor or ""),
            "kind": str(kind or ""),
            "payload": _object(parsed),
            "dedupe_key": str(dedupe_key or ""),
        })
    return events


def _graph_submission_index(
    graph_events: Iterable[Mapping[str, Any]],
) -> tuple[dict[str, dict[str, Any]], dict[str, dict[str, Any]], set[str]]:
    submissions: dict[str, dict[str, Any]] = {}
    decisions: dict[str, dict[str, Any]] = {}
    found_flags: set[str] = set()
    for event in graph_events:
        kind = str(event.get("kind") or "")
        payload = _object(event.get("payload"))
        if kind == "flag_submission":
            submission_id = str(payload.get("submission_id") or "")
            if submission_id:
                submissions[submission_id] = dict(event)
        elif kind == "flag_submission_decision":
            submission_id = str(payload.get("submission_id") or "")
            if submission_id:
                decisions[submission_id] = dict(event)
        elif kind == "flag_found":
            value = str(payload.get("flag") or "").strip()
            if value:
                found_flags.add(value)
    return submissions, decisions, found_flags


def _tool_evidence(
    events: Iterable[Mapping[str, Any]], *, solver_id: str, candidate: str,
    not_after_ts: float,
) -> list[dict[str, Any]]:
    """Return only exact Worker-owned archived actual-output records.

    Chat, thought, fact and command-start records are intentionally excluded.
    ``condensed`` is the durable actual tool-output field used by the original
    session recorder, including truncated records whose visible contents still
    contain the candidate.  The output must precede the old graph submission;
    an after-the-fact worker echo cannot rehabilitate a rejected request.
    """
    matches: list[dict[str, Any]] = []
    for event in events:
        if event.get("event_type") != "tool.result":
            continue
        if str(event.get("solver_id") or "") != solver_id:
            continue
        if _as_float(event.get("ts")) > not_after_ts:
            continue
        result = _object(_payload(event).get("result"))
        condensed = result.get("condensed")
        if (
            not isinstance(condensed, str)
            or candidate not in condensed
            or _is_board_echo(condensed)
            or _is_submission_echo(condensed)
            or _is_cached_flag_echo(condensed, candidate)
        ):
            continue
        matches.append({
            "seq": _as_int(event.get("seq")),
            "ts": _as_float(event.get("ts")),
            "condensed": condensed,
            "truncated": bool(_payload(event).get("truncated")),
        })
    return matches


def _is_board_echo(condensed: str) -> bool:
    """Reject a Blackboard dump masquerading as a command result.

    Workers often retrieve the shared board through a real tool.  Its result is
    technically a ``tool.result`` too, but a Flag printed there is an echo of a
    prior claim rather than new execution evidence.  The normal submitted path
    already has a bounded candidate; the no-submission path needs this extra
    exclusion before it is allowed to synthesize one.
    """
    text = condensed.casefold()
    return any(marker in text for marker in (
        "muteki-team-board",
        "# review-arbiter state",
        "## challenge brief",
        "---deadends---",
        "---facts---",
    ))


def _is_submission_echo(condensed: str) -> bool:
    """Reject a tool result that only reports a Flag-submission attempt.

    Historic Worker adapters often surface ``SUBMITTED fs-...; awaiting
    provenance validation`` after the blackboard command itself.  The candidate
    may be repeated in that output, but it is a self-echo after submission, not
    the original command result required for provenance repair.
    """
    text = condensed.casefold()
    return (
        "submitted fs-" in text
        or "awaiting provenance validation" in text
        or "flag submission accepted" in text
        or "flag submission rejected" in text
    )


def _is_cached_flag_echo(condensed: str, candidate: str) -> bool:
    """Reject a Flag carried inside the target's explicit cache-comment field.

    Some historic exploit attempts replayed a Worker-written cache value as
    ``<!-- cached: flag{...} -->``.  The string is server output, but it has no
    independent value after the Worker has placed a known candidate into that
    cache.  A later response that returns the Flag outside that field remains
    usable; a response containing a populated cache field is conservatively
    excluded from historical repair.
    """
    return bool(re.search(
        r"<!--\s*cached:\s*" + re.escape(candidate) + r"\s*-->",
        condensed,
        flags=re.IGNORECASE,
    ))


def _output_text_views(condensed: str) -> list[str]:
    """Return literal and structured text views of one archived tool result.

    Several CLI adapters persist an MCP/ACP JSON envelope as ``condensed``.
    Searching only the serialized bytes sees ``\\nflag{...}`` and can treat the
    final ``n`` as part of a token prefix.  JSON string leaves restore the actual
    command output without inventing any data; the original literal remains a
    view too for non-JSON adapters.
    """
    views = [condensed]
    try:
        parsed = json.loads(condensed)
    except (TypeError, ValueError, json.JSONDecodeError):
        parsed = None

    def visit(value: object) -> None:
        if isinstance(value, str):
            views.append(value)
        elif isinstance(value, Mapping):
            for child in value.values():
                visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)

    if parsed is not None:
        visit(parsed)
    # Preserve insertion order because output scans can be large and a string
    # can appear both in the envelope and in a parsed leaf.
    return list(dict.fromkeys(view for view in views if view))


def _synthetic_no_submission_rows(
    *,
    graph_events: Iterable[Mapping[str, Any]],
    session_events: Iterable[Mapping[str, Any]],
    challenge: Challenge,
    run_id: str,
    session_flags: set[str],
) -> list[dict[str, Any]]:
    """Strictly recover an old run that never emitted a submission at all.

    This is deliberately *not* a fallback for an incomplete or rejected
    submission.  It is restricted to an absent ``flag_submission`` history and
    requires the same complete Flag in two distinct Worker-owned actual-output
    records.  That covers the known a05 recorder omission while avoiding a broad
    scan of chat, reasoning, fact or board text.
    """
    submissions, decisions, graph_flags = _graph_submission_index(graph_events)
    # A previous invocation can have completed the graph transaction and then
    # been interrupted before it projected the result to JSONL.  In that state
    # the only submission is our deterministic legacy-repair submission.  It
    # must not suppress the narrow no-historic-submission evidence scan: that
    # scan is what reconstructs the source metadata needed to append the
    # missing projection.  Any actual historic Worker submission still blocks
    # this synthetic path exactly as before.
    historic_submissions = [
        submission
        for submission in submissions.values()
        if not _is_legacy_repair_submission(submission)
    ]
    if historic_submissions:
        return []

    evidence_by_candidate: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for event in session_events:
        if event.get("event_type") != "tool.result":
            continue
        solver_id = str(event.get("solver_id") or "")
        if not solver_id:
            continue
        payload = _payload(event)
        result = _object(payload.get("result"))
        condensed = result.get("condensed")
        if (
            not isinstance(condensed, str)
            or _is_board_echo(condensed)
            or _is_submission_echo(condensed)
        ):
            continue
        seen_in_event: set[str] = set()
        for output_text in _output_text_views(condensed):
            for match in _SYNTHETIC_BRACE_CANDIDATE.finditer(output_text):
                candidate = match.group("candidate").strip()
                if candidate in seen_in_event:
                    continue
                if _is_cached_flag_echo(condensed, candidate):
                    continue
                verdict = flag_verdict(
                    candidate,
                    output_text,
                    flag_format=challenge.flag_format,
                    artifacts=None,
                )
                if not verdict.accepted:
                    continue
                seen_in_event.add(candidate)
                evidence_by_candidate[candidate].append({
                    "solver_id": solver_id,
                    "seq": _as_int(event.get("seq")),
                    "ts": _as_float(event.get("ts")),
                    "truncated": bool(payload.get("truncated")),
                    "gate": {
                        "accepted": True,
                        "code": verdict.code,
                        "detail": verdict.detail,
                    },
                })

    rows: list[dict[str, Any]] = []
    for candidate, evidence_rows in evidence_by_candidate.items():
        # Duplicate dispatch attempts under the same solver identity are not
        # independent confirmation.  Require two Worker identities, then record
        # the latest complete witness as the source projection.
        solver_ids = {str(row["solver_id"]) for row in evidence_rows}
        if len(solver_ids) < 2:
            continue
        repair_id = _repair_submission_id(run_id, candidate)
        complete = [row for row in evidence_rows if not row["truncated"]]
        source = max(complete or evidence_rows, key=lambda row: _as_int(row["seq"]))
        row: dict[str, Any] = {
            "flag": candidate,
            "repair_submission_id": repair_id,
            "admission": "synthetic_no_submission_two_worker_tool_results",
            "old_attempts": 0,
            "source_solver_ids": sorted(solver_ids),
            "source_event_seqs": sorted(_as_int(item["seq"]) for item in evidence_rows),
            "solver_id": str(source["solver_id"]),
            "old_submission_id": "",
            "old_submission_seq": 0,
            "old_decision_seq": 0,
            "old_decision_code": "no_historic_submission",
            "intent_id": "",
            "source_event_seq": _as_int(source["seq"]),
            "source_event_truncated": bool(source["truncated"]),
            "gate": source["gate"],
        }
        legacy_submission = submissions.get(repair_id)
        legacy_decision = decisions.get(repair_id)
        legacy_decision_payload = (
            _object(legacy_decision.get("payload")) if legacy_decision else {}
        )
        if legacy_submission is not None:
            if bool(legacy_decision_payload.get("accepted")):
                # ``flag_found`` is already atomically committed by
                # resolve_flag_submission.  The JSONL projection may still be
                # absent, so expose this as resumable rather than suppressing
                # it through ``graph_flags`` below.
                row["status"] = "already_reconciled"
            else:
                row["status"] = "skipped_legacy_repair_rejected"
        elif candidate in session_flags or candidate in graph_flags:
            row["status"] = "skipped_existing_flag_found"
        else:
            row["status"] = "eligible_synthetic_no_submission"
        rows.append(row)
    return rows


def _is_legacy_repair_submission(submission: Mapping[str, Any]) -> bool:
    """Return whether a graph submission was written by this reconciler.

    The protocol label alone is deliberately insufficient: a user Worker could
    theoretically have used the same label.  Only this command's actor and
    deterministic submission-id namespace may be ignored when deciding whether
    a run had an original submission history.
    """
    payload = _object(submission.get("payload"))
    return (
        str(submission.get("actor") or "") == RECONCILER_ACTOR
        and str(payload.get("protocol") or "") == LEGACY_PROTOCOL
        and str(payload.get("submission_id") or "").startswith("legacy-flag-")
    )


def _repair_submission_id(run_id: str, candidate: str) -> str:
    # Candidate-level (not old submission-level) identity collapses duplicate
    # Worker submissions of the same Flag into one append-only repair.
    digest = hashlib.sha256(
        f"{LEGACY_PROTOCOL}\0{run_id}\0{candidate}".encode("utf-8")
    ).hexdigest()[:24]
    return f"legacy-flag-{digest}"


def _candidate_rows(
    *,
    graph_events: Iterable[Mapping[str, Any]],
    session_events: Iterable[Mapping[str, Any]],
    challenge: Challenge,
    run_id: str,
    session_flags: set[str],
) -> list[dict[str, Any]]:
    """Find candidates satisfying every legacy-recovery evidence condition."""
    submissions, decisions, graph_flags = _graph_submission_index(graph_events)
    rejected_by_flag: dict[str, list[dict[str, Any]]] = defaultdict(list)

    for submission_id, submission in submissions.items():
        payload = _object(submission.get("payload"))
        decision = decisions.get(submission_id)
        decision_payload = _object(decision.get("payload")) if decision else {}
        # A recovery is only allowed from an explicitly rejected old request.  A
        # pending or accepted request has different recovery semantics and is left
        # untouched by this narrowly scoped tool.
        if not decision or bool(decision_payload.get("accepted")):
            continue
        protocol = str(payload.get("protocol") or "")
        if protocol == LEGACY_PROTOCOL:
            continue
        candidate = str(payload.get("flag") or "").strip()
        actor = str(submission.get("actor") or "")
        if not candidate or not actor:
            continue
        rejected_by_flag[candidate].append({
            "submission_id": submission_id,
            "submission": submission,
            "decision": decision,
        })

    rows: list[dict[str, Any]] = []
    for candidate, old_attempts in rejected_by_flag.items():
        # Graph order supplies a deterministic causal preference: take the first
        # rejected attempt that has its own prior actual output.  Echo/cache
        # filters above remove the common false early candidates.
        old_attempts.sort(key=lambda item: _as_int(item["submission"].get("seq")))
        repair_id = _repair_submission_id(run_id, candidate)
        legacy_submission = submissions.get(repair_id)
        legacy_decision = decisions.get(repair_id)
        legacy_decision_payload = (
            _object(legacy_decision.get("payload")) if legacy_decision else {}
        )

        selected: dict[str, Any] | None = None
        evidence: dict[str, Any] | None = None
        verdict_code = ""
        verdict_detail = ""
        for attempt in old_attempts:
            source_submission = attempt["submission"]
            source_actor = str(source_submission.get("actor") or "")
            evidence_matches = _tool_evidence(
                session_events,
                solver_id=source_actor,
                candidate=candidate,
                not_after_ts=_as_float(source_submission.get("ts")),
            )
            if not evidence_matches:
                continue
            # Multiple direct command results before one submission are ordered by
            # session sequence; the first is the earliest causal observation,
            # avoiding a later echo of the same value.
            selected_evidence = min(evidence_matches, key=lambda item: item["seq"])
            verdict = flag_verdict(
                candidate,
                selected_evidence["condensed"],
                flag_format=challenge.flag_format,
                artifacts=None,
            )
            verdict_code = verdict.code
            verdict_detail = verdict.detail
            if not verdict.accepted:
                continue
            selected = attempt
            evidence = selected_evidence
            break

        row: dict[str, Any] = {
            "flag": candidate,
            "repair_submission_id": repair_id,
            "old_attempts": len(old_attempts),
            "status": "skipped_no_matching_tool_output",
        }
        if selected is not None and evidence is not None:
            old_submission = selected["submission"]
            old_decision = selected["decision"]
            old_submission_payload = _object(old_submission.get("payload"))
            old_decision_payload = _object(old_decision.get("payload"))
            row.update({
                "solver_id": str(old_submission.get("actor") or ""),
                "old_submission_id": str(old_submission_payload.get("submission_id") or ""),
                "old_submission_seq": _as_int(old_submission.get("seq")),
                "old_decision_seq": _as_int(old_decision.get("seq")),
                "old_decision_code": str(old_decision_payload.get("code") or ""),
                "intent_id": str(old_submission_payload.get("intent_id") or ""),
                "source_event_seq": evidence["seq"],
                "source_event_truncated": evidence["truncated"],
                "gate": {"accepted": True, "code": verdict_code, "detail": verdict_detail},
            })
            # A previous invocation may have committed the graph transaction but
            # crashed before JSONL projections.  Treat its accepted stable repair
            # as resumable, rather than letting its own flag_found suppress the
            # recovery of the display projection.
            if legacy_submission is not None:
                if bool(legacy_decision_payload.get("accepted")):
                    row["status"] = "already_reconciled"
                else:
                    row["status"] = "skipped_legacy_repair_rejected"
            elif candidate in session_flags or candidate in graph_flags:
                row["status"] = "skipped_existing_flag_found"
            else:
                row["status"] = "eligible"
        elif verdict_code:
            row.update({
                "status": "skipped_gate_rejected",
                "gate": {"accepted": False, "code": verdict_code, "detail": verdict_detail},
            })
        rows.append(row)
    return rows


def _projection_events(
    *,
    run_id: str,
    challenge_id: str,
    candidate: str,
    repair_id: str,
    source_event_seq: int,
    source_submission_id: str,
    flags: list[str],
    expected_flags: int,
    multi_flag: bool,
    execution_generation: int,
    control_generation: int,
    next_seq: int,
) -> tuple[dict[str, Any], dict[str, Any]]:
    """Build the two append-only SessionStore replay projections."""
    now = time.time()
    common = {
        "run_id": run_id,
        "challenge_id": challenge_id,
        "solver_id": None,
    }
    flag_projection_id = f"{repair_id}:flag_found"
    finish_projection_id = f"{repair_id}:run_finished"
    generations = {
        "execution_generation": execution_generation,
        "control_generation": control_generation,
    }
    flag_delta = {
        **common,
        "event_type": "blackboard.delta",
        "seq": next_seq,
        "ts": now,
        "payload": {
            "kind": "flag_found",
            "actor": RECONCILER_ACTOR,
            "flag": candidate,
            "protocol": LEGACY_PROTOCOL,
            "legacy_reconciliation": True,
            "legacy_reconciliation_id": repair_id,
            "legacy_projection_id": flag_projection_id,
            "source_event_seq": source_event_seq,
            "source_submission_id": source_submission_id,
            **generations,
        },
    }
    run_finished = {
        **common,
        "event_type": "run.finished",
        "seq": next_seq + 1,
        "ts": now,
        "payload": {
            "flag": flags[0] if flags else candidate,
            "flags": flags,
            "expected_flags": expected_flags,
            "multi_flag": multi_flag,
            "solved": True,
            "reason": "legacy_flag_reconciliation",
            "protocol": LEGACY_PROTOCOL,
            "legacy_reconciliation": True,
            "legacy_reconciliation_id": repair_id,
            "legacy_projection_id": finish_projection_id,
            "source_event_seq": source_event_seq,
            "source_submission_id": source_submission_id,
            **generations,
        },
    }
    return flag_delta, run_finished


def _append_json_rows(path: Path, rows: Iterable[Mapping[str, Any]]) -> None:
    """Append complete JSONL rows and flush them before releasing the file."""
    rendered = [json.dumps(dict(row), ensure_ascii=False, separators=(",", ":")) for row in rows]
    if not rendered:
        return
    with path.open("ab+") as handle:
        try:
            import fcntl  # type: ignore[import-not-found]
        except ImportError:  # pragma: no cover - Windows fallback
            fcntl = None
        if fcntl is not None:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        try:
            handle.seek(0, os.SEEK_END)
            size = handle.tell()
            needs_separator = False
            if size:
                handle.seek(-1, os.SEEK_END)
                needs_separator = handle.read(1) not in {b"\n", b"\r"}
            handle.seek(0, os.SEEK_END)
            if needs_separator:
                handle.write(b"\n")
            for line in rendered:
                handle.write(line.encode("utf-8") + b"\n")
            handle.flush()
            os.fsync(handle.fileno())
        finally:
            if fcntl is not None:
                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def _append_projections(
    *,
    session_path: Path,
    run_id: str,
    challenge: Challenge,
    candidate_row: Mapping[str, Any],
) -> dict[str, Any]:
    """Append missing session projections, idempotently, after a graph repair.

    This function rereads immediately before appending so an interrupted earlier
    run can be resumed without duplicate projections.
    """
    events, _invalid = _read_jsonl(session_path)
    terminal, lifecycle = _terminal_state(events)
    if not terminal:
        return {"appended": False, "reason": f"run_not_terminal:{lifecycle}"}

    repair_id = str(candidate_row["repair_submission_id"])
    existing_projection_ids = _legacy_projection_ids(events)
    flag_projection_id = f"{repair_id}:flag_found"
    finish_projection_id = f"{repair_id}:run_finished"
    session_flags = _published_session_flags(events)
    candidate = str(candidate_row["flag"])
    flags = list(session_flags)
    if candidate not in flags:
        flags.append(candidate)
    execution_generation, control_generation = _generations(events)
    flag_delta, run_finished = _projection_events(
        run_id=run_id,
        challenge_id=challenge.id,
        candidate=candidate,
        repair_id=repair_id,
        source_event_seq=_as_int(candidate_row.get("source_event_seq")),
        source_submission_id=str(candidate_row.get("old_submission_id") or ""),
        flags=flags,
        expected_flags=max(1, int(challenge.expected_flags or 1)),
        multi_flag=bool(challenge.multi_flag),
        execution_generation=execution_generation,
        control_generation=control_generation,
        next_seq=_max_seq(events) + 1,
    )
    append_rows: list[dict[str, Any]] = []
    if flag_projection_id not in existing_projection_ids:
        append_rows.append(flag_delta)
    if finish_projection_id not in existing_projection_ids:
        # If the flag projection already exists but the final projection is being
        # resumed, calculate a fresh next sequence after that known row.
        if append_rows:
            run_finished["seq"] = _as_int(flag_delta["seq"]) + 1
        else:
            run_finished["seq"] = _max_seq(events) + 1
        append_rows.append(run_finished)
    _append_json_rows(session_path, append_rows)
    return {
        "appended": bool(append_rows),
        "events": [str(row["event_type"]) for row in append_rows],
        "seq": [_as_int(row["seq"]) for row in append_rows],
        "flags": flags,
    }


def _inspect_run(sessions_root: Path, run_id: str) -> dict[str, Any]:
    safe_run_id = encode_run_id(run_id)
    session_path = sessions_root / f"{safe_run_id}.jsonl"
    report: dict[str, Any] = {
        "run_id": run_id,
        "session_path": str(session_path),
        "apply_eligible": False,
        "candidates": [],
    }
    if not session_path.is_file():
        report.update({"status": "skipped_session_missing"})
        return report

    try:
        session_events, invalid_rows = _read_jsonl(session_path)
    except OSError as exc:
        report.update({"status": "skipped_session_unreadable", "detail": str(exc)})
        return report
    report["invalid_jsonl_rows"] = invalid_rows
    terminal, lifecycle = _terminal_state(session_events)
    report["terminal"] = terminal
    report["lifecycle"] = lifecycle

    challenge_payload = _latest_challenge(session_events)
    if not challenge_payload:
        report.update({"status": "skipped_preparing_challenge_missing"})
        return report
    try:
        challenge = Challenge.model_validate(challenge_payload)
    except (ValueError, FlagFormatError) as exc:
        report.update({"status": "skipped_invalid_challenge_contract", "detail": str(exc)})
        return report
    report["challenge_id"] = challenge.id
    report["normalized_flag_format"] = challenge.flag_format
    report["expected_flags"] = max(1, int(challenge.expected_flags or 1))
    report["challenge_multi_flag"] = bool(challenge.multi_flag)

    db_path = _graph_path(sessions_root, run_id)
    report["graph_path"] = str(db_path)
    if not db_path.is_file():
        report.update({"status": "skipped_graph_missing"})
        return report
    try:
        graph_events = _read_graph_events(db_path)
    except (OSError, sqlite3.Error) as exc:
        report.update({"status": "skipped_graph_unreadable", "detail": str(exc)})
        return report

    rows = _candidate_rows(
        graph_events=graph_events,
        session_events=session_events,
        challenge=challenge,
        run_id=run_id,
        session_flags=set(_published_session_flags(session_events)),
    )
    if not rows:
        rows = _synthetic_no_submission_rows(
            graph_events=graph_events,
            session_events=session_events,
            challenge=challenge,
            run_id=run_id,
            session_flags=set(_published_session_flags(session_events)),
        )
    report["candidates"] = rows
    eligible = [
        row for row in rows
        if row.get("status") in {"eligible", "eligible_synthetic_no_submission"}
    ]
    reconciled = [row for row in rows if row.get("status") == "already_reconciled"]
    if not terminal:
        for row in rows:
            if row.get("status") in {
                "eligible", "eligible_synthetic_no_submission", "already_reconciled",
            }:
                row["status"] = "skipped_run_not_terminal"
        report.update({"status": "skipped_run_not_terminal"})
        return report
    if eligible:
        report.update({"status": "eligible", "apply_eligible": True})
    elif reconciled:
        report.update({"status": "already_reconciled", "apply_eligible": True})
    else:
        report.update({"status": "no_eligible_candidate"})
    return report


def _apply_candidate(
    *, sessions_root: Path, run_id: str, report: dict[str, Any], candidate: dict[str, Any]
) -> dict[str, Any]:
    """Write a new graph decision and then the durable JSONL projections."""
    session_path = Path(str(report["session_path"]))
    session_events, _invalid = _read_jsonl(session_path)
    terminal, lifecycle = _terminal_state(session_events)
    if not terminal:
        return {"applied": False, "reason": f"run_not_terminal:{lifecycle}"}

    challenge_payload = _latest_challenge(session_events)
    challenge = Challenge.model_validate(challenge_payload)
    db_path = _graph_path(sessions_root, run_id)
    graph = SQLiteSharedGraph.open(db_path=db_path, challenge=challenge)
    try:
        resolution = graph.resolve_flag_submission(
            actor=RECONCILER_ACTOR,
            submission_id=str(candidate["repair_submission_id"]),
            flag=str(candidate["flag"]),
            intent_id=str(candidate.get("intent_id") or ""),
            protocol=LEGACY_PROTOCOL,
            accepted=True,
            code="accepted",
            detail=(
                "legacy event reconciliation: rejected submission has matching "
                f"Worker tool.result seq {candidate.get('source_event_seq')}"
            ),
        )
    finally:
        graph.close()
    if not bool(resolution.get("accepted")):
        return {
            "applied": False,
            "reason": "graph_resolution_rejected",
            "resolution": resolution,
        }
    projection = _append_projections(
        session_path=session_path,
        run_id=run_id,
        challenge=challenge,
        candidate_row=candidate,
    )
    return {"applied": True, "resolution": resolution, "projection": projection}


def _run_ids(sessions_root: Path, requested: list[str]) -> list[str]:
    if requested:
        # Preserve caller order but do not inspect a repeated run twice.
        return list(dict.fromkeys(str(run_id) for run_id in requested if str(run_id)))
    ids: list[str] = []
    for path in sorted(sessions_root.glob("*.jsonl")):
        ids.append(decode_run_id(path.stem))
    return ids


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--sessions-root",
        default="sessions",
        help="session JSONL root (default: ./sessions)",
    )
    parser.add_argument(
        "--run-id",
        action="append",
        default=[],
        help="one run id to inspect; repeat for multiple runs (default: all JSONL runs)",
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="append the graph + JSONL recovery projections; default is dry-run",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv)
    sessions_root = Path(args.sessions_root).expanduser().resolve()
    summary: dict[str, Any] = {
        "protocol": LEGACY_PROTOCOL,
        "dry_run": not bool(args.apply),
        "sessions_root": str(sessions_root),
        "runs": [],
    }
    if not sessions_root.is_dir():
        summary["error"] = "sessions_root_missing"
        print(json.dumps(summary, ensure_ascii=False, sort_keys=True))
        return 2

    for run_id in _run_ids(sessions_root, list(args.run_id)):
        try:
            report = _inspect_run(sessions_root, run_id)
        except Exception as exc:  # preserve other runs' audit instead of aborting
            report = {"run_id": run_id, "status": "inspection_error", "detail": str(exc)}
        if args.apply and report.get("apply_eligible"):
            applied: list[dict[str, Any]] = []
            selected_count = 0
            # A single-flag run must not turn two competing old candidates into a
            # collection.  Multi-flag runs may reconcile each independently.
            challenge_multi = bool(report.get("challenge_multi_flag", False))
            for candidate in report.get("candidates", []):
                if candidate.get("status") not in {
                    "eligible", "eligible_synthetic_no_submission", "already_reconciled",
                }:
                    continue
                if selected_count and not challenge_multi:
                    candidate["status"] = "skipped_single_flag_run"
                    continue
                try:
                    outcome = _apply_candidate(
                        sessions_root=sessions_root,
                        run_id=run_id,
                        report=report,
                        candidate=candidate,
                    )
                except Exception as exc:
                    outcome = {"applied": False, "reason": "apply_error", "detail": str(exc)}
                candidate["apply"] = outcome
                applied.append(outcome)
                if outcome.get("applied"):
                    selected_count += 1
                    candidate["status"] = "applied"
            report["apply"] = applied
        summary["runs"].append(report)

    statuses = defaultdict(int)
    candidate_statuses = defaultdict(int)
    for report in summary["runs"]:
        statuses[str(report.get("status") or "unknown")] += 1
        for candidate in report.get("candidates", []):
            candidate_statuses[str(candidate.get("status") or "unknown")] += 1
    summary["counts"] = {
        "runs": len(summary["runs"]),
        "run_status": dict(sorted(statuses.items())),
        "candidate_status": dict(sorted(candidate_statuses.items())),
    }
    print(json.dumps(summary, ensure_ascii=False, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
