"""Vulnerability-report collection events and queries.

Append-only events on the shared graph. Accepted reports are the pentest
success object. Review proposals stay on the existing review channel.
"""

from __future__ import annotations

import json
from typing import Any, Optional

from muteki.swarm.graph_defs import (
    EV_REPORT_ACCEPTED,
    EV_REPORT_REJECTED,
    EV_REPORT_REPRO_DECISION,
    EV_REPORT_SUBMITTED,
    EV_REPORT_VALUE_DECISION,
)


class _ReportsMixin:
    def report_submitted(
        self, *, actor: str, report: dict,
        intent_id: Optional[str] = None,
    ) -> int:
        payload = dict(report or {})
        report_id = str(payload.get("report_id") or "").strip()
        if intent_id:
            payload["intent_id"] = intent_id
        payload["status"] = "submitted"
        return self._append(
            EV_REPORT_SUBMITTED, actor, payload, verified=False,
            dedupe_key=f"report-submitted::{report_id}")

    def report_rejected(
        self, *, actor: str, report_id: str, code: str, detail: str = "",
        intent_id: Optional[str] = None,
    ) -> int:
        payload = {
            "report_id": str(report_id or ""),
            "code": str(code or ""),
            "detail": str(detail or "")[:800],
        }
        if intent_id:
            payload["intent_id"] = intent_id
        return self._append(
            EV_REPORT_REJECTED, actor, payload, verified=False,
            dedupe_key=f"report-rejected::{report_id}::{code}::{detail[:80]}")

    def report_repro_decision(
        self, *, actor: str, report_id: str, reproduced: bool,
        detail: str = "", witness: str = "", intent_id: Optional[str] = None,
    ) -> int:
        payload = {
            "report_id": str(report_id or ""),
            "reproduced": bool(reproduced),
            "detail": str(detail or "")[:800],
            "witness": str(witness or "")[:400],
            "verifier": actor,
        }
        if intent_id:
            payload["intent_id"] = intent_id
        return self._append(
            EV_REPORT_REPRO_DECISION, actor, payload, verified=bool(reproduced),
            dedupe_key=f"report-repro::{report_id}::{actor}")

    def report_value_decision(
        self, *, actor: str, report_id: str, accepted: bool,
        code: str, detail: str = "",
    ) -> int:
        payload = {
            "report_id": str(report_id or ""),
            "accepted": bool(accepted),
            "code": str(code or ""),
            "detail": str(detail or "")[:800],
        }
        return self._append(
            EV_REPORT_VALUE_DECISION, actor, payload, verified=bool(accepted),
            dedupe_key=f"report-value::{report_id}")

    def report_accepted(
        self, *, actor: str, report: dict,
    ) -> int:
        payload = dict(report or {})
        report_id = str(payload.get("report_id") or "").strip()
        payload["status"] = "accepted"
        return self._append(
            EV_REPORT_ACCEPTED, actor, payload, verified=True,
            dedupe_key=f"report-accepted::{report_id}")

    def _report_event_rows(self, kind: str) -> list[dict[str, Any]]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, actor, payload FROM events "
                "WHERE challenge_id=? AND kind=? ORDER BY seq",
                (self.challenge.id, kind),
            ).fetchall()
        out: list[dict[str, Any]] = []
        for seq, actor, raw in rows:
            try:
                payload = json.loads(raw) if isinstance(raw, str) else dict(raw or {})
            except (TypeError, json.JSONDecodeError):
                payload = {}
            if not isinstance(payload, dict):
                payload = {}
            payload["_seq"] = int(seq)
            payload["_actor"] = str(actor or "")
            out.append(payload)
        return out

    def report_record(self, report_id: str) -> dict[str, Any] | None:
        wanted = str(report_id or "").strip()
        if not wanted:
            return None
        for row in self._report_event_rows(EV_REPORT_SUBMITTED):
            if str(row.get("report_id") or "") == wanted:
                return dict(row)
        return None

    def report_states(self) -> dict[str, dict[str, Any]]:
        """report_id → latest folded status + submitted payload."""
        states: dict[str, dict[str, Any]] = {}
        for row in self._report_event_rows(EV_REPORT_SUBMITTED):
            rid = str(row.get("report_id") or "").strip()
            if not rid:
                continue
            states[rid] = {
                "status": "submitted",
                "report": dict(row),
                "report_id": rid,
            }
        for row in self._report_event_rows(EV_REPORT_REPRO_DECISION):
            rid = str(row.get("report_id") or "").strip()
            if rid not in states:
                continue
            if row.get("reproduced"):
                states[rid]["status"] = "reproduced"
            else:
                states[rid]["status"] = "repro_failed"
            states[rid]["repro"] = dict(row)
        for row in self._report_event_rows(EV_REPORT_VALUE_DECISION):
            rid = str(row.get("report_id") or "").strip()
            if rid not in states:
                continue
            if row.get("accepted"):
                states[rid]["status"] = "value_accepted"
            else:
                states[rid]["status"] = "value_rejected"
            states[rid]["value"] = dict(row)
        for row in self._report_event_rows(EV_REPORT_ACCEPTED):
            rid = str(row.get("report_id") or "").strip()
            if not rid:
                continue
            entry = states.get(rid) or {"report_id": rid, "report": dict(row)}
            entry["status"] = "accepted"
            entry["accepted"] = dict(row)
            states[rid] = entry
        return states

    def pending_report_repros(self) -> list[dict[str, Any]]:
        fail_counts: dict[str, int] = {}
        for row in self._report_event_rows(EV_REPORT_REPRO_DECISION):
            if row.get("reproduced"):
                continue
            rid = str(row.get("report_id") or "").strip()
            if rid:
                fail_counts[rid] = fail_counts.get(rid, 0) + 1
        out: list[dict[str, Any]] = []
        for item in self.report_states().values():
            status = item.get("status")
            report = dict(item.get("report") or {})
            rid = str(item.get("report_id") or "")
            if status == "submitted":
                out.append(report)
            elif status == "repro_failed" and fail_counts.get(rid, 0) < 3:
                out.append(report)
        return out

    def pending_report_value_judges(self) -> list[dict[str, Any]]:
        return [
            dict(item["report"])
            for item in self.report_states().values()
            if item.get("status") == "reproduced"
        ]

    def accepted_reports(self) -> list[dict[str, Any]]:
        out: list[dict[str, Any]] = []
        for item in self.report_states().values():
            if item.get("status") != "accepted":
                continue
            row = dict(item.get("accepted") or item.get("report") or {})
            out.append(row)
        return out
