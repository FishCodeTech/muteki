"""Event-backed sub-goals and fresh CTF visit projections."""

from __future__ import annotations

import json
import re

from muteki.swarm.graph_defs import (
    EV_FACT_ADDED,
    EV_INTENT_STATE_CHANGED,
    EV_SUB_GOAL_ADDED,
    EV_SUB_GOAL_SATISFIED,
    EV_VISIT_WAVE_RESET,
    FACT_STATE_SUPERSEDED,
)


class _PlanningMixin:
    def _latest_visit_wave_locked(self) -> dict:
        row = self._conn.execute(
            "SELECT seq,payload FROM events WHERE challenge_id=? AND kind=? "
            "ORDER BY seq DESC LIMIT 1",
            (self.challenge.id, EV_VISIT_WAVE_RESET),
        ).fetchone()
        return {**json.loads(row[1]), "seq": int(row[0])} if row else {}

    def latest_visit_wave(self) -> dict:
        with self._lock:
            return self._latest_visit_wave_locked()

    def _current_visit_wave_cutoff_locked(self) -> int:
        return int(self._latest_visit_wave_locked().get("seq") or 0)

    def current_visit_wave_cutoff(self) -> int:
        with self._lock:
            return self._current_visit_wave_cutoff_locked()

    def _sub_goals_locked(self) -> list[dict]:
        rows = self._conn.execute(
            "SELECT kind,ts,payload FROM events WHERE challenge_id=? AND seq>? "
            "AND kind IN (?,?) ORDER BY seq",
            (self.challenge.id, self._current_visit_wave_cutoff_locked(),
             EV_SUB_GOAL_ADDED, EV_SUB_GOAL_SATISFIED),
        ).fetchall()
        goals: dict[str, dict] = {}
        fmt_ts = getattr(self, "_ctf_created_at", None)
        for kind, ts, raw in rows:
            payload = json.loads(raw)
            goal_id = payload["goal_id"]
            iso_ts = fmt_ts(ts) if callable(fmt_ts) else ""
            if kind == EV_SUB_GOAL_ADDED:
                goals[goal_id] = {
                    "goal_id": goal_id, "criterion": payload["criterion"],
                    "state": "open", "reason": "", "evidence_fact_seqs": [],
                    "created_at": iso_ts, "satisfied_at": "",
                }
            elif goal_id in goals:
                goals[goal_id].update(
                    state="satisfied", reason=payload["reason"],
                    evidence_fact_seqs=payload["evidence_fact_seqs"],
                    satisfied_at=iso_ts,
                )
        active_seqs = self._active_fact_seq_set_locked()
        for goal in goals.values():
            if goal["state"] == "satisfied":
                ev = [int(s) for s in (goal.get("evidence_fact_seqs") or [])]
                if not ev or not set(ev).issubset(active_seqs):
                    goal["state"] = "open"
        return list(goals.values())

    def sub_goals(self) -> list[dict]:
        with self._lock:
            return self._sub_goals_locked()

    def add_sub_goal(self, *, actor: str, criterion: str, goal_id: str = "") -> str:
        criterion = str(criterion).strip()
        if not criterion:
            raise ValueError("Sub-goal criterion must not be empty")
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                reserved = {
                    str(json.loads(row[0])["goal_id"])
                    for row in self._conn.execute(
                        "SELECT payload FROM events WHERE challenge_id=? AND kind=?",
                        (self.challenge.id, EV_SUB_GOAL_ADDED),
                    ).fetchall()
                }
                if not goal_id:
                    index = len(reserved) + 1
                    while f"goal_{index:03d}" in reserved:
                        index += 1
                    goal_id = f"goal_{index:03d}"
                if not re.fullmatch(r"goal_[0-9]{3,}", goal_id):
                    raise ValueError("Sub-goal ID must be goal_NNN")
                if goal_id in reserved:
                    matching = next(
                        (g for g in self._sub_goals_locked() if g["goal_id"] == goal_id), None,
                    )
                    if matching is None or matching["criterion"] != criterion:
                        raise ValueError(f"Sub-goal ID already exists: {goal_id}")
                    self._conn.rollback()
                    return goal_id
                seq = self._append_locked(
                    EV_SUB_GOAL_ADDED, actor,
                    {"goal_id": goal_id, "criterion": criterion},
                    dedupe_key=f"sub-goal::{self.challenge.id}::{goal_id}",
                )
                if seq <= 0:
                    raise RuntimeError("Sub-goal append failed")
                self._conn.commit()
                return goal_id
            except BaseException:
                self._conn.rollback()
                raise

    def satisfy_sub_goal(
        self, *, actor: str, goal_id: str, reason: str, evidence_fact_seqs: list[int],
    ) -> int:
        evidence = list(dict.fromkeys(int(seq) for seq in evidence_fact_seqs))
        if not evidence or not str(reason).strip():
            raise ValueError("Sub-goal satisfaction needs active evidence and a reason")
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                goal = next(
                    (g for g in self._sub_goals_locked() if g["goal_id"] == goal_id), None,
                )
                if goal is None:
                    raise ValueError(f"Unknown sub-goal: {goal_id}")
                if not set(evidence).issubset(self._active_fact_seq_set_locked()):
                    raise ValueError("Sub-goal evidence references an inactive Fact")
                if goal["state"] == "satisfied":
                    self._conn.rollback()
                    return 0
                seq = self._append_locked(
                    EV_SUB_GOAL_SATISFIED, actor,
                    {"goal_id": goal_id, "reason": str(reason).strip(),
                     "evidence_fact_seqs": evidence},
                    dedupe_key=None,
                )
                if seq <= 0:
                    raise RuntimeError("Sub-goal satisfaction append failed")
                self._conn.commit()
                return seq
            except BaseException:
                self._conn.rollback()
                raise

    def reset_for_new_visit_wave(
        self, *, digest_path: str, prior_summary: str, generation: int | None = None,
    ) -> dict:
        """Archive the planning frontier, retaining every event and artifact."""
        if (getattr(self.challenge, "mode", "ctf") != "ctf"
                or (generation is not None and int(generation) < 2)):
            raise ValueError("A fresh visit wave requires a CTF revisit generation")
        if not str(digest_path).strip():
            raise ValueError("A fresh visit wave requires a saved history digest")
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                prior = self._latest_visit_wave_locked()
                generation = (
                    int(generation) if generation is not None
                    else max(2, int(prior.get("generation") or 1) + 1)
                )
                if int(prior.get("generation") or 0) >= generation:
                    self._conn.rollback()
                    return {**prior, "reset_seq": prior["seq"], "reset": False}
                facts = [
                    int(seq) for seq, raw in self._conn.execute(
                        "SELECT seq,payload FROM events WHERE challenge_id=? AND kind=?",
                        (self.challenge.id, EV_FACT_ADDED),
                    ).fetchall()
                    if json.loads(raw).get("source") != "origin"
                ]
                intents = self._conn.execute(
                    "SELECT intent_id,status,result_detail FROM intents WHERE challenge_id=? "
                    "AND created_seq>?",
                    (self.challenge.id, int(prior.get("seq") or 0)),
                ).fetchall()
                payload = {
                    "generation": generation, "digest_path": str(digest_path),
                    "prior_summary": str(prior_summary), "fact_seqs": facts,
                    "intent_ids": [row[0] for row in intents],
                }
                reset_seq = self._append_locked(
                    EV_VISIT_WAVE_RESET, "coordinator", payload,
                    dedupe_key=f"visit-wave::{self.challenge.id}::{generation}",
                )
                if reset_seq <= 0:
                    raise RuntimeError("Visit wave reset append failed")
                reason = f"Archived before Visit #{generation}; see {digest_path}"
                for fact_seq in facts:
                    self._conn.execute(
                        "INSERT INTO fact_states "
                        "(fact_seq,challenge_id,state,retired_seq,reason,updated_seq) "
                        "VALUES (?,?,?,?,?,?) ON CONFLICT(fact_seq) DO UPDATE SET "
                        "retired_seq=COALESCE(fact_states.retired_seq,excluded.retired_seq)",
                        (fact_seq, self.challenge.id, FACT_STATE_SUPERSEDED,
                         reset_seq, reason, reset_seq),
                    )
                for intent_id, status, detail in intents:
                    event_seq = self._append_locked(
                        EV_INTENT_STATE_CHANGED, "coordinator",
                        {"intent_id": intent_id, "status": "done",
                         "dispatch_state": "retired", "reason": reason},
                    )
                    self._conn.execute(
                        "UPDATE intents SET status='done',dispatch_state='retired',"
                        "lease_until=NULL,close_reason=?,result_detail=?,"
                        "result_seq=CASE WHEN status='done' THEN result_seq ELSE ? END "
                        "WHERE challenge_id=? AND intent_id=?",
                        (reason, detail if status == "done" or detail else reason,
                         event_seq, self.challenge.id, intent_id),
                    )
                title = f"前序波次(Visit #{generation - 1})探索复盘摘要与产物索引"
                content = str(prior_summary).strip()
                content += f"\n完整历史、已确认事实、能力与入口、有界排除及产物索引: {digest_path}"
                fact_seq = self._add_evidence_locked(
                    actor="coordinator", source="prior_visit", fact=f"{title}\n{content}",
                    verified=True, provenance={"generation": generation, "digest_path": digest_path},
                )
                if fact_seq <= 0:
                    raise RuntimeError("Visit wave digest Fact append failed")
                self._conn.commit()
                return {**payload, "seq": reset_seq, "reset_seq": reset_seq,
                        "fact_seq": fact_seq, "reset": True}
            except BaseException:
                self._conn.rollback()
                raise
