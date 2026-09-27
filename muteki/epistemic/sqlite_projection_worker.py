"""Worker and attempt projection mutation handlers."""

from __future__ import annotations

import json
import time
from typing import Any

from muteki.epistemic.contracts import canonical_digest, canonical_json_bytes
from muteki.epistemic.sqlite_types import (
    IntegrityError,
    ProjectionMutation,
    _strict_nonnegative_int_map,
)


def _apply_worker_projection_mutation(
    self,
    mutation: ProjectionMutation,
    p: dict[str, Any],
    *,
    enforce_live_guards: bool,
) -> bool:
    kind = mutation.kind
    if kind == "attempt_admit":
        _apply_attempt_admit(self, mutation, p, enforce_live_guards=enforce_live_guards)
        return True
    if kind == "attempt_launch":
        _apply_attempt_launch(
            self, mutation, p, enforce_live_guards=enforce_live_guards
        )
        return True
    if kind == "attempt_io_guard":
        _apply_attempt_io_guard(
            self, mutation, p, enforce_live_guards=enforce_live_guards
        )
        return True
    if kind in {"orphan_reconcile_guard", "worker_terminal_guard"}:
        _apply_worker_terminal_guard(self, mutation, p)
        return True
    return False


def _apply_attempt_admit(
    self,
    mutation: ProjectionMutation,
    p: dict[str, Any],
    *,
    enforce_live_guards: bool,
) -> None:
    if mutation.kind == "attempt_admit":
        current_state = self._state()
        current_scope_digest = canonical_digest(
            {
                "execution_generation": current_state.execution_generation,
                "run_fence_epoch": current_state.run_fence_epoch,
                "run_id": current_state.run_id,
            }
        )
        if enforce_live_guards and (
            current_state.kernel_health.value != "ready"
            or current_state.run_execution.value != "running"
            or current_state.search_mode.value != "active"
            or p.get("scope_digest") != current_scope_digest
        ):
            raise IntegrityError(
                "attempt admission is outside the current active execution scope"
            )
        if self._conn.execute(
            "SELECT 1 FROM runtime_attempts WHERE fingerprint=?",
            (p["fingerprint"],),
        ).fetchone():
            raise IntegrityError("attempt fingerprint is already covered")
        branch = self._conn.execute(
            "SELECT state,depends_on_json,max_attempts,attempt_count FROM runtime_branches "
            "WHERE branch_id=?",
            (p["branch_id"],),
        ).fetchone()
        if branch is None or branch[0] != "open" or int(branch[3]) >= int(branch[2]):
            raise IntegrityError(
                "branch is closed/suspended or attempt bound is exhausted"
            )
        for dependency in json.loads(branch[1]):
            row = self._conn.execute(
                "SELECT state FROM runtime_branches WHERE branch_id=?",
                (dependency,),
            ).fetchone()
            if row is None or row[0] != "resolved":
                raise IntegrityError("attempt dependency is not resolved")
        for conflict_key in p.get("conflict_keys", []):
            if self._conn.execute(
                "SELECT 1 FROM effect_conflict_holds WHERE conflict_key=?",
                (conflict_key,),
            ).fetchone():
                raise IntegrityError("effect conflict is already held")

        requested = _strict_nonnegative_int_map(
            p["requested_budget"], name="requested budget"
        )
        account_id = str(p["account_id"])
        ancestry: list[tuple[str, dict[str, int]]] = []
        seen: set[str] = set()
        current = account_id
        while current:
            if current in seen:
                raise IntegrityError("budget ancestry cycle")
            seen.add(current)
            row = self._conn.execute(
                "SELECT parent_id,limits_json,settled_json,held_json,debt "
                "FROM budget_accounts WHERE account_id=?",
                (current,),
            ).fetchone()
            if row is None or int(row[4]):
                raise IntegrityError("budget account missing or in debt")
            limits = self._json_map(row[1])
            settled = self._json_map(row[2])
            held = self._json_map(row[3])
            if set(requested) != set(limits):
                raise IntegrityError(
                    "requested budget must cover every enforced dimension"
                )
            if set(settled) != set(limits) or set(held) != set(limits):
                raise IntegrityError("budget account projection axes diverged")
            for dimension, amount in requested.items():
                if settled[dimension] + held[dimension] + amount > limits[dimension]:
                    raise IntegrityError("budget admission would oversell an ancestor")
            ancestry.append((current, held))
            current = str(row[0] or "")

        self._conn.execute(
            "INSERT INTO runtime_attempts(attempt_id,branch_id,permit_id,scope_digest,"
            "lease_id,lease_epoch,worker_generation,fingerprint,effect_class,state) "
            "VALUES(?,?,?,?,?,?,?,?,?,'reserved')",
            (
                p["attempt_id"],
                p["branch_id"],
                p["permit_id"],
                p["scope_digest"],
                p["lease_id"],
                int(p["lease_epoch"]),
                int(p["worker_generation"]),
                p["fingerprint"],
                p["effect_class"],
            ),
        )
        for ancestor_id, held in ancestry:
            next_held = {key: held[key] + requested.get(key, 0) for key in held}
            self._conn.execute(
                "UPDATE budget_accounts SET held_json=? WHERE account_id=?",
                (canonical_json_bytes(next_held).decode(), ancestor_id),
            )
            reservation_id = f"{p['permit_id']}:{ancestor_id}"
            self._conn.execute(
                "INSERT INTO budget_reservations(reservation_id,account_id,attempt_id,"
                "dimensions_json,state) VALUES(?,?,?,?,'active')",
                (
                    reservation_id,
                    ancestor_id,
                    p["attempt_id"],
                    canonical_json_bytes(requested).decode(),
                ),
            )
        self._conn.execute(
            "UPDATE runtime_branches SET attempt_count=attempt_count+1 WHERE branch_id=?",
            (p["branch_id"],),
        )
        for conflict_key in p.get("conflict_keys", []):
            self._conn.execute(
                "INSERT INTO effect_conflict_holds(conflict_key,operation_id,state) "
                "VALUES(?,?,'active')",
                (conflict_key, p["attempt_id"]),
            )
        return


def _apply_attempt_launch(
    self,
    mutation: ProjectionMutation,
    p: dict[str, Any],
    *,
    enforce_live_guards: bool,
) -> None:
    if mutation.kind == "attempt_launch":
        current_state = self._state()
        current_scope_digest = canonical_digest(
            {
                "execution_generation": current_state.execution_generation,
                "run_fence_epoch": current_state.run_fence_epoch,
                "run_id": current_state.run_id,
            }
        )
        if enforce_live_guards and (
            current_state.kernel_health.value != "ready"
            or current_state.run_execution.value != "running"
            or current_state.search_mode.value != "active"
            or p.get("scope_digest") != current_scope_digest
        ):
            raise IntegrityError("launch is outside the current active scope")
        attempt_id = str(p["attempt_id"])
        attempt = self._conn.execute(
            "SELECT permit_id,scope_digest,lease_id,state "
            "FROM runtime_attempts WHERE attempt_id=?",
            (attempt_id,),
        ).fetchone()
        if attempt != (
            p["permit_id"],
            p["scope_digest"],
            p["lease_id"],
            "reserved",
        ):
            raise IntegrityError("launch permit projection compare-and-set failed")
        reservations = self._conn.execute(
            "SELECT reservation_id,state FROM budget_reservations WHERE attempt_id=?",
            (attempt_id,),
        ).fetchall()
        expected_ids = tuple(p["reservation_ids"])
        if (
            not reservations
            or len(reservations) != len(expected_ids)
            or {str(row[0]) for row in reservations} != set(expected_ids)
            or any(row[1] != "active" for row in reservations)
        ):
            raise IntegrityError("launch reservations are not exactly active")
        cur = self._conn.execute(
            "UPDATE runtime_attempts SET state='running' "
            "WHERE attempt_id=? AND state='reserved'",
            (attempt_id,),
        )
        if cur.rowcount != 1:
            raise IntegrityError("launch attempt compare-and-set failed")
        return


def _apply_attempt_io_guard(
    self,
    mutation: ProjectionMutation,
    p: dict[str, Any],
    *,
    enforce_live_guards: bool,
) -> None:
    if mutation.kind == "attempt_io_guard":
        action = p.get("action")
        if action not in {
            "candidate",
            "capture",
            "cognitive_capture",
            "gate",
            "c6_launch",
        }:
            raise IntegrityError("attempt I/O guard action is invalid")
        if action in {"candidate", "gate"} and (
            self._is_c6_v2_observer_attempt_locked(p.get("attempt_id"))
        ):
            raise IntegrityError(
                "C6 evaluation v2 observer cannot reach candidate/gate authority"
            )
        current_state = self._state()
        current_scope_digest = canonical_digest(
            {
                "execution_generation": current_state.execution_generation,
                "run_fence_epoch": current_state.run_fence_epoch,
                "run_id": current_state.run_id,
            }
        )
        if enforce_live_guards and (
            current_state.kernel_health.value != "ready"
            or current_state.run_execution.value != "running"
            or current_state.search_mode.value != "active"
            or p.get("scope_digest") != current_scope_digest
        ):
            raise IntegrityError("attempt I/O is outside the current active scope")
        attempt = self._conn.execute(
            "SELECT permit_id,scope_digest,lease_id,state "
            "FROM runtime_attempts WHERE attempt_id=?",
            (p["attempt_id"],),
        ).fetchone()
        if attempt != (p["permit_id"], p["scope_digest"], p["lease_id"], "running"):
            raise IntegrityError("attempt I/O owner is not active")
        reservations = self._conn.execute(
            "SELECT state FROM budget_reservations WHERE attempt_id=?",
            (p["attempt_id"],),
        ).fetchall()
        if not reservations or any(row[0] != "active" for row in reservations):
            raise IntegrityError("attempt I/O reservations are not active")
        launches = self._conn.execute(
            "SELECT event_digest,payload_json FROM events "
            "WHERE kind='WORKER_LAUNCH_PREPARED'"
        ).fetchall()
        matching_launches = [
            (str(row[0]), json.loads(row[1]))
            for row in launches
            if json.loads(row[1]).get("permit_id") == p["permit_id"]
        ]
        if len(matching_launches) != 1 or any(
            matching_launches[0][1].get(name) != p.get(name)
            for name in (
                "attempt_digest",
                "lease_digest",
                "permit_digest",
                "scope_digest",
            )
        ):
            raise IntegrityError("attempt I/O launch lineage is invalid")
        if action == "c6_launch":
            expires_at_ns = p.get("expires_at_ns")
            if type(expires_at_ns) is not int or expires_at_ns < 0:
                raise IntegrityError("C6 host launch expiry is malformed")
            if matching_launches[0][0] != p.get("worker_launch_event_digest"):
                raise IntegrityError(
                    "C6 host launch guard does not bind the exact worker launch"
                )
            admissions = self._conn.execute(
                "SELECT payload_json FROM events WHERE kind='ATTEMPT_ADMITTED'"
            ).fetchall()
            matching_admissions = [
                json.loads(row[0])
                for row in admissions
                if json.loads(row[0]).get("permit_id") == p["permit_id"]
            ]
            if (
                len(matching_admissions) != 1
                or matching_admissions[0].get("expires_at_ns") != expires_at_ns
                or matching_admissions[0].get("permit_digest") != p.get("permit_digest")
            ):
                raise IntegrityError("C6 host launch expiry diverges from admission")
            if enforce_live_guards and time.time_ns() >= expires_at_ns:
                raise IntegrityError("C6 host launch permit is expired")
        terminal_rows = self._conn.execute(
            "SELECT payload_json FROM events "
            "WHERE kind IN ('WORKER_TERMINAL','WORKER_UNKNOWN')"
        ).fetchall()
        if any(
            json.loads(row[0]).get("permit_id") == p["permit_id"]
            for row in terminal_rows
        ):
            raise IntegrityError("attempt I/O occurs after worker terminal")
        if action in {"capture", "cognitive_capture"}:
            manifest_rows = self._conn.execute(
                "SELECT payload_json FROM events "
                "WHERE kind='CAPTURE_MANIFEST_ADVANCED' ORDER BY seq"
            ).fetchall()
            manifests = [
                json.loads(row[0])
                for row in manifest_rows
                if json.loads(row[0]).get("permit_digest") == p["permit_digest"]
            ]
            previous = ""
            for ordinal, manifest in enumerate(manifests):
                if (
                    manifest.get("ordinal") != ordinal
                    or manifest.get("previous_manifest_digest") != previous
                ):
                    raise IntegrityError("capture manifest chain is discontinuous")
                try:
                    body = {
                        name: manifest[name]
                        for name in (
                            "attempt_digest",
                            "byte_count",
                            "candidate_id",
                            "capture_id",
                            "flag_digest",
                            "flag_format_digest",
                            "lease_digest",
                            "ordinal",
                            "permit_digest",
                            "policy_digest",
                            "previous_manifest_digest",
                            "raw_digest",
                            "stream",
                            "terminal",
                        )
                    }
                except KeyError as exc:
                    raise IntegrityError("capture manifest is incomplete") from exc
                previous = canonical_digest(body)
                if manifest.get("manifest_digest") != previous:
                    raise IntegrityError("capture manifest digest mismatch")
            if not manifests or manifests[-1].get("manifest_digest") != p.get(
                "manifest_digest"
            ):
                raise IntegrityError("capture append was not the manifest head")
        elif action == "gate":
            capture = self._conn.execute(
                "SELECT payload_json FROM events WHERE event_digest=? "
                "AND kind='CAPTURE_CHUNK_SEALED'",
                (p["capture_event_digest"],),
            ).fetchone()
            if capture is None:
                raise IntegrityError("gate capture event is missing")
            capture_payload = json.loads(capture[0])
            if any(
                capture_payload.get(name) != p.get(name)
                for name in (
                    "attempt_digest",
                    "candidate_id",
                    "flag_digest",
                    "flag_format_digest",
                    "lease_digest",
                    "manifest_digest",
                    "permit_digest",
                    "policy_digest",
                    "raw_digest",
                )
            ):
                raise IntegrityError("gate capture lineage is invalid")
        return


def _apply_worker_terminal_guard(
    self, mutation: ProjectionMutation, p: dict[str, Any]
) -> None:
    if mutation.kind == "orphan_reconcile_guard":
        attempt = self._conn.execute(
            "SELECT permit_id,scope_digest,lease_id,state "
            "FROM runtime_attempts WHERE attempt_id=?",
            (p["attempt_id"],),
        ).fetchone()
        if attempt != (p["permit_id"], p["scope_digest"], p["lease_id"], "running"):
            raise IntegrityError("orphan owner is no longer in flight")
        launch = self._conn.execute(
            "SELECT event_digest,payload_json FROM events "
            "WHERE kind='WORKER_LAUNCH_PREPARED' AND event_digest=?",
            (p["launch_event_digest"],),
        ).fetchone()
        if launch is None:
            raise IntegrityError("orphan launch receipt is missing")
        launch_payload = json.loads(launch[1])
        if any(
            launch_payload.get(name) != p.get(name)
            for name in (
                "attempt_digest",
                "attempt_id",
                "lease_digest",
                "lease_id",
                "permit_digest",
                "permit_id",
                "scope_digest",
            )
        ):
            raise IntegrityError("orphan launch lineage diverged")
        self._assert_c6_claims_closed_before_worker_terminal_locked(
            permit_id=p["permit_id"],
            worker_terminal_event_id=p["worker_unknown_event_id"],
        )
        terminals = self._conn.execute(
            "SELECT event_id,payload_json FROM events "
            "WHERE kind IN ('WORKER_TERMINAL','WORKER_UNKNOWN')"
        ).fetchall()
        matching_terminals = [
            (str(row[0]), json.loads(row[1]))
            for row in terminals
            if json.loads(row[1]).get("permit_id") == p["permit_id"]
        ]
        if (
            len(matching_terminals) != 1
            or matching_terminals[0][0] != p["worker_unknown_event_id"]
        ):
            raise IntegrityError("orphan terminal compare-and-append failed")
        budget_terminals = self._conn.execute(
            "SELECT event_id,payload_json FROM events "
            "WHERE kind IN ('BUDGET_PESSIMISTICALLY_SETTLED',"
            "'BUDGET_SETTLED','BUDGET_USAGE_UNKNOWN')"
        ).fetchall()
        matching_budget = [
            (str(row[0]), json.loads(row[1]))
            for row in budget_terminals
            if json.loads(row[1]).get("attempt_id") == p["attempt_id"]
        ]
        if (
            len(matching_budget) != 1
            or matching_budget[0][0] != p["budget_unknown_event_id"]
        ):
            raise IntegrityError("orphan budget compare-and-append failed")
        return
    if mutation.kind == "worker_terminal_guard":
        attempt = self._conn.execute(
            "SELECT permit_id,scope_digest,lease_id,state "
            "FROM runtime_attempts WHERE attempt_id=?",
            (p["attempt_id"],),
        ).fetchone()
        if (
            attempt is None
            or tuple(attempt[:3]) != (p["permit_id"], p["scope_digest"], p["lease_id"])
            or attempt[3] not in {"running", "terminal", "unknown"}
        ):
            raise IntegrityError("worker terminal has no closed attempt owner")
        admission = self._conn.execute(
            "SELECT event_digest,payload_json FROM events "
            "WHERE kind='ATTEMPT_ADMITTED' AND event_digest=?",
            (p["admission_event_digest"],),
        ).fetchone()
        launch = self._conn.execute(
            "SELECT event_digest,payload_json FROM events "
            "WHERE kind='WORKER_LAUNCH_PREPARED' AND event_digest=?",
            (p["launch_event_digest"],),
        ).fetchone()
        if admission is None or launch is None:
            raise IntegrityError("worker terminal lineage is missing")
        admission_payload = json.loads(admission[1])
        launch_payload = json.loads(launch[1])
        for source in (admission_payload, launch_payload):
            if any(
                source.get(name) != p.get(name)
                for name in (
                    "attempt_digest",
                    "attempt_id",
                    "lease_digest",
                    "lease_id",
                    "permit_digest",
                    "permit_id",
                    "scope_digest",
                )
                if name in source
            ):
                raise IntegrityError("worker terminal lineage diverged")
        self._assert_c6_claims_closed_before_worker_terminal_locked(
            permit_id=p["permit_id"],
            worker_terminal_event_id=p["terminal_event_id"],
        )
        terminal_rows = self._conn.execute(
            "SELECT event_id,payload_json FROM events "
            "WHERE kind IN ('WORKER_TERMINAL','WORKER_UNKNOWN')"
        ).fetchall()
        matching = [
            (str(row[0]), json.loads(row[1]))
            for row in terminal_rows
            if json.loads(row[1]).get("permit_id") == p["permit_id"]
        ]
        if len(matching) != 1 or matching[0][0] != p["terminal_event_id"]:
            raise IntegrityError("worker terminal compare-and-append failed")
        return
