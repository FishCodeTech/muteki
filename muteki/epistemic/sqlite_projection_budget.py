"""Budget projection mutation handlers."""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from muteki.epistemic.contracts import canonical_json_bytes
from muteki.epistemic.sqlite_types import (
    IntegrityError,
    ProjectionMutation,
    require_positive_effect_revision,
    _strict_nonnegative_int_map,
    _validate_tagged_usage_payload,
)


def _apply_budget_projection_mutation(
    self,
    mutation: ProjectionMutation,
    p: dict[str, Any],
) -> bool:
    kind = mutation.kind
    if kind == "budget_account_create":
        _apply_budget_account_create(self, mutation, p)
        return True
    if kind == "budget_settle":
        _apply_budget_settle(self, mutation, p)
        return True
    if kind == "budget_pessimistic_settle":
        _apply_budget_pessimistic_settle(self, mutation, p)
        return True
    if kind == "budget_unknown":
        _apply_budget_unknown(self, mutation, p)
        return True
    return False


def _apply_budget_account_create(
    self, mutation: ProjectionMutation, p: dict[str, Any]
) -> None:
    if mutation.kind == "budget_account_create":
        limits = _strict_nonnegative_int_map(p["limits"], name="budget limits")
        zeros = {key: 0 for key in limits}
        self._conn.execute(
            "INSERT INTO budget_accounts(account_id,parent_id,limits_json,settled_json,held_json) "
            "VALUES(?,?,?,?,?)",
            (
                p["account_id"],
                p.get("parent_id") or None,
                canonical_json_bytes(limits).decode(),
                canonical_json_bytes(zeros).decode(),
                canonical_json_bytes(zeros).decode(),
            ),
        )
        return


def _apply_budget_settle(self, mutation: ProjectionMutation, p: dict[str, Any]) -> None:
    if mutation.kind == "budget_settle":
        attempt_id = str(p["attempt_id"])
        require_positive_effect_revision(p["settlement_revision"])
        actual = _strict_nonnegative_int_map(p["actual_usage"], name="actual usage")
        attempt = self._conn.execute(
            "SELECT state FROM runtime_attempts WHERE attempt_id=?",
            (attempt_id,),
        ).fetchone()
        if attempt is None or attempt[0] not in {"reserved", "running"}:
            raise IntegrityError("attempt is not active for settlement")
        self._assert_c6_claims_closed_before_attempt_state_change_locked(
            attempt_id=attempt_id
        )
        rows = self._conn.execute(
            "SELECT reservation_id,account_id,dimensions_json,state FROM budget_reservations "
            "WHERE attempt_id=?",
            (attempt_id,),
        ).fetchall()
        if not rows:
            raise IntegrityError("attempt has no reservations")
        reserved_contract = self._json_map(rows[0][2])
        if any(self._json_map(row[2]) != reserved_contract for row in rows):
            raise IntegrityError("attempt reservation dimensions diverged")
        validated_charge = _validate_tagged_usage_payload(
            p,
            reserved=reserved_contract,
            reservation_ids=tuple(str(row[0]) for row in rows),
            unknown_hold=False,
        )
        if actual != validated_charge:
            raise IntegrityError("actual usage diverges from tagged usage")
        for reservation_id, account_id, dims_json, state in rows:
            if state != "active":
                raise IntegrityError("reservation is not active")
            reserved = self._json_map(dims_json)
            if set(actual) != set(reserved):
                raise IntegrityError(
                    "actual usage must cover the exact reserved dimensions"
                )
            account = self._conn.execute(
                "SELECT limits_json,settled_json,held_json FROM budget_accounts "
                "WHERE account_id=?",
                (account_id,),
            ).fetchone()
            if account is None:
                raise IntegrityError("reservation budget account is missing")
            limits = self._json_map(account[0])
            settled = self._json_map(account[1])
            held = self._json_map(account[2])
            if not (set(limits) == set(settled) == set(held) == set(reserved)):
                raise IntegrityError("budget settlement axes diverged")
            next_held = {key: held[key] - reserved[key] for key in held}
            if any(value < 0 for value in next_held.values()):
                raise IntegrityError("budget held amount would become negative")
            next_settled = {key: settled[key] + actual[key] for key in settled}
            debt = int(any(next_settled[key] > limits[key] for key in limits))
            self._conn.execute(
                "UPDATE budget_accounts SET settled_json=?,held_json=?,debt=? "
                "WHERE account_id=?",
                (
                    canonical_json_bytes(next_settled).decode(),
                    canonical_json_bytes(next_held).decode(),
                    debt,
                    account_id,
                ),
            )
            self._conn.execute(
                "UPDATE budget_reservations SET state='settled' WHERE reservation_id=?",
                (reservation_id,),
            )
        cur = self._conn.execute(
            "UPDATE runtime_attempts SET state='terminal' WHERE attempt_id=? "
            "AND state IN ('reserved','running')",
            (attempt_id,),
        )
        if cur.rowcount != 1:
            raise IntegrityError("attempt settlement compare-and-set failed")
        # Admission-level conflict keys fence concurrent attempts. A terminal,
        # observed settlement releases them; UNKNOWN deliberately keeps its
        # hold in the separate budget_unknown path.
        self._conn.execute(
            "DELETE FROM effect_conflict_holds WHERE operation_id=? AND state='active'",
            (attempt_id,),
        )
        return


def _apply_budget_pessimistic_settle(
    self, mutation: ProjectionMutation, p: dict[str, Any]
) -> None:
    if mutation.kind == "budget_pessimistic_settle":
        if set(p) != {
            "attempt_id",
            "charge_basis",
            "charged_usage",
            "reservation_ids",
            "settlement_revision",
            "usage_report",
            "usage_report_digest",
        }:
            raise IntegrityError(
                "pessimistic settlement payload shape is not versioned"
            )
        attempt_id = str(p["attempt_id"])
        require_positive_effect_revision(p["settlement_revision"])
        if p["charge_basis"] != "unobserved_reservation_ceiling":
            raise IntegrityError("pessimistic settlement basis is false")
        charged = _strict_nonnegative_int_map(
            p["charged_usage"], name="pessimistically charged usage"
        )
        attempt = self._conn.execute(
            "SELECT state FROM runtime_attempts WHERE attempt_id=?",
            (attempt_id,),
        ).fetchone()
        if attempt is None or attempt[0] not in {"reserved", "running"}:
            raise IntegrityError("attempt is not active for pessimistic settlement")
        self._assert_c6_claims_closed_before_attempt_state_change_locked(
            attempt_id=attempt_id
        )
        rows = self._conn.execute(
            "SELECT reservation_id,account_id,dimensions_json,state "
            "FROM budget_reservations WHERE attempt_id=?",
            (attempt_id,),
        ).fetchall()
        if not rows or any(row[3] != "active" for row in rows):
            raise IntegrityError(
                "pessimistic settlement reservations are not all active"
            )
        reserved_contract = self._json_map(rows[0][2])
        if any(self._json_map(row[2]) != reserved_contract for row in rows):
            raise IntegrityError("attempt reservation dimensions diverged")
        report = p.get("usage_report")
        measurements = (
            report.get("measurements") if isinstance(report, Mapping) else None
        )
        if (
            type(measurements) not in {list, tuple}
            or not measurements
            or any(
                not isinstance(item, Mapping) or item.get("status") != "unknown"
                for item in measurements
            )
        ):
            raise IntegrityError(
                "pessimistic settlement must label every usage axis UNKNOWN"
            )
        validated_charge = _validate_tagged_usage_payload(
            p,
            reserved=reserved_contract,
            reservation_ids=tuple(str(row[0]) for row in rows),
            unknown_hold=True,
            charge_key="charged_usage",
        )
        if charged != validated_charge or charged != reserved_contract:
            raise IntegrityError(
                "pessimistic settlement must charge the full reservation ceiling"
            )
        for reservation_id, account_id, dims_json, _state in rows:
            reserved = self._json_map(dims_json)
            account = self._conn.execute(
                "SELECT limits_json,settled_json,held_json "
                "FROM budget_accounts WHERE account_id=?",
                (account_id,),
            ).fetchone()
            if account is None:
                raise IntegrityError("pessimistic settlement budget account is missing")
            limits = self._json_map(account[0])
            settled = self._json_map(account[1])
            held = self._json_map(account[2])
            if not (set(limits) == set(settled) == set(held) == set(reserved)):
                raise IntegrityError("pessimistic settlement budget axes diverged")
            next_held = {axis: held[axis] - reserved[axis] for axis in held}
            if any(value < 0 for value in next_held.values()):
                raise IntegrityError("budget held amount would become negative")
            next_settled = {axis: settled[axis] + charged[axis] for axis in settled}
            debt = int(any(next_settled[axis] > limits[axis] for axis in limits))
            self._conn.execute(
                "UPDATE budget_accounts SET settled_json=?,held_json=?,debt=? "
                "WHERE account_id=?",
                (
                    canonical_json_bytes(next_settled).decode(),
                    canonical_json_bytes(next_held).decode(),
                    debt,
                    account_id,
                ),
            )
            self._conn.execute(
                "UPDATE budget_reservations SET state='settled' WHERE reservation_id=?",
                (reservation_id,),
            )
        cur = self._conn.execute(
            "UPDATE runtime_attempts SET state='terminal' WHERE attempt_id=? "
            "AND state IN ('reserved','running')",
            (attempt_id,),
        )
        if cur.rowcount != 1:
            raise IntegrityError(
                "pessimistic settlement attempt compare-and-set failed"
            )
        self._conn.execute(
            "DELETE FROM effect_conflict_holds WHERE operation_id=? AND state='active'",
            (attempt_id,),
        )
        return


def _apply_budget_unknown(
    self, mutation: ProjectionMutation, p: dict[str, Any]
) -> None:
    if mutation.kind == "budget_unknown":
        attempt_id = str(p["attempt_id"])
        require_positive_effect_revision(p["revision"])
        attempt = self._conn.execute(
            "SELECT state FROM runtime_attempts WHERE attempt_id=?",
            (attempt_id,),
        ).fetchone()
        if attempt is None or attempt[0] not in {"reserved", "running"}:
            raise IntegrityError("attempt is not active for UNKNOWN usage")
        self._assert_c6_claims_closed_before_attempt_state_change_locked(
            attempt_id=attempt_id
        )
        rows = self._conn.execute(
            "SELECT reservation_id,account_id,dimensions_json,state "
            "FROM budget_reservations WHERE attempt_id=?",
            (attempt_id,),
        ).fetchall()
        reservation_count = len(rows)
        active_count = sum(row[3] == "active" for row in rows)
        if reservation_count == 0 or active_count != reservation_count:
            raise IntegrityError("attempt reservations are not all active")
        reserved_contract = self._json_map(rows[0][2])
        if any(self._json_map(row[2]) != reserved_contract for row in rows):
            raise IntegrityError("attempt reservation dimensions diverged")
        held_charge = _validate_tagged_usage_payload(
            p,
            reserved=reserved_contract,
            reservation_ids=tuple(str(row[0]) for row in rows),
            unknown_hold=True,
        )
        for _reservation_id, account_id, dimensions_json, _state in rows:
            reserved = self._json_map(dimensions_json)
            account = self._conn.execute(
                "SELECT limits_json,settled_json,held_json FROM budget_accounts "
                "WHERE account_id=?",
                (account_id,),
            ).fetchone()
            if account is None:
                raise IntegrityError("UNKNOWN reservation account is missing")
            limits = self._json_map(account[0])
            settled = self._json_map(account[1])
            held = self._json_map(account[2])
            if not (set(limits) == set(settled) == set(held) == set(reserved)):
                raise IntegrityError("UNKNOWN budget axes diverged")
            next_held = {
                axis: held[axis] + held_charge[axis] - reserved[axis] for axis in held
            }
            debt = int(
                any(settled[axis] + next_held[axis] > limits[axis] for axis in limits)
            )
            self._conn.execute(
                "UPDATE budget_accounts SET held_json=?,debt=? WHERE account_id=?",
                (canonical_json_bytes(next_held).decode(), debt, account_id),
            )
        self._conn.execute(
            "UPDATE budget_reservations SET dimensions_json=? "
            "WHERE attempt_id=? AND state='active'",
            (canonical_json_bytes(held_charge).decode(), attempt_id),
        )
        cur = self._conn.execute(
            "UPDATE budget_reservations SET state='unknown' "
            "WHERE attempt_id=? AND state='active'",
            (attempt_id,),
        )
        if cur.rowcount != reservation_count:
            raise IntegrityError("UNKNOWN reservation compare-and-set failed")
        cur = self._conn.execute(
            "UPDATE runtime_attempts SET state='unknown' WHERE attempt_id=? "
            "AND state IN ('reserved','running')",
            (attempt_id,),
        )
        if cur.rowcount != 1:
            raise IntegrityError("UNKNOWN attempt compare-and-set failed")
        return
