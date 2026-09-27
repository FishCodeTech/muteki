"""Runtime and lifecycle projection mutation handlers."""

from __future__ import annotations

import json
from typing import Any

from muteki.epistemic.contracts import canonical_digest, canonical_json_bytes
from muteki.epistemic.sqlite_types import IntegrityError, ProjectionMutation, _is_sha256


def _apply_runtime_projection_mutation(
    self,
    mutation: ProjectionMutation,
    p: dict[str, Any],
    *,
    enforce_live_guards: bool,
) -> bool:
    kind = mutation.kind
    if (
        (kind.startswith("c6_eval_v2_") and kind.endswith("_bind_guard"))
        or (kind.startswith("c6_eval_") and kind.endswith("_bind_guard"))
        or kind in {"c6_eval_outcome_guard", "c6_eval_outcome_unknown_guard"}
    ):
        _apply_evaluation_guard(self, mutation, p)
        return True
    if kind in {
        "branch_create",
        "execution_start_guard",
        "execution_stop_guard",
        "execution_drain_guard",
    }:
        _apply_execution_guard(
            self, mutation, p, enforce_live_guards=enforce_live_guards
        )
        return True
    if kind in {
        "goal_commit_guard",
        "projection_verify_guard",
        "s4e_closure_guard",
        "canary_commit_guard",
    }:
        _apply_completion_guard(
            self, mutation, p, enforce_live_guards=enforce_live_guards
        )
        return True
    if kind == "branch_state":
        _apply_branch_state(self, mutation, p)
        return True
    return False


def _apply_evaluation_guard(
    self, mutation: ProjectionMutation, p: dict[str, Any]
) -> None:
    if mutation.kind.startswith("c6_eval_v2_") and mutation.kind.endswith(
        "_bind_guard"
    ):
        self._validate_c6_eval_v2_binding_mutation(mutation.kind, p)
        return
    if mutation.kind.startswith("c6_eval_") and mutation.kind.endswith("_bind_guard"):
        self._validate_c6_eval_binding_mutation(mutation.kind, p)
        return
    if mutation.kind in {
        "c6_eval_outcome_guard",
        "c6_eval_outcome_unknown_guard",
    }:
        self._validate_c6_eval_outcome_mutation(mutation.kind, p)
        return


def _apply_execution_guard(
    self,
    mutation: ProjectionMutation,
    p: dict[str, Any],
    *,
    enforce_live_guards: bool,
) -> None:
    if mutation.kind == "branch_create":
        self._conn.execute(
            "INSERT INTO runtime_branches(branch_id,state,depends_on_json,max_attempts) "
            "VALUES(?,?,?,?)",
            (
                p["branch_id"],
                "open",
                canonical_json_bytes(p.get("depends_on", [])).decode(),
                int(p["max_attempts"]),
            ),
        )
        return
    if mutation.kind == "execution_start_guard":
        current = self._state()
        if enforce_live_guards and current.run_execution.value not in {
            "new",
            "stopped",
        }:
            raise IntegrityError("execution start owner is not quiescent")
        owners = self.lifecycle_owner_summary()
        if enforce_live_guards and any(owners.values()):
            raise IntegrityError("execution start has unresolved runtime owners")
        if (
            type(p.get("execution_generation")) is not int
            or type(p.get("run_fence_epoch")) is not int
            or p["execution_generation"] != current.execution_generation + 1
            or p["run_fence_epoch"] < current.run_fence_epoch + 1
        ):
            raise IntegrityError("execution start generation/fence is stale")
        return
    if mutation.kind == "execution_stop_guard":
        current = self._state()
        scope_digest = canonical_digest(
            {
                "execution_generation": current.execution_generation,
                "run_fence_epoch": current.run_fence_epoch,
                "run_id": current.run_id,
            }
        )
        if enforce_live_guards and current.run_execution.value != "running":
            raise IntegrityError("execution stop owner is not running")
        if p != {"scope_digest": scope_digest}:
            raise IntegrityError("execution stop scope is stale or malformed")
        return
    if mutation.kind == "execution_drain_guard":
        current = self._state()
        scope_digest = canonical_digest(
            {
                "execution_generation": current.execution_generation,
                "run_fence_epoch": current.run_fence_epoch,
                "run_id": current.run_id,
            }
        )
        if enforce_live_guards and current.run_execution.value not in {
            "quiescing",
            "reopen_required",
        }:
            raise IntegrityError("execution drain owner is not quiescing")
        if p != {"scope_digest": scope_digest}:
            raise IntegrityError("execution drain scope is stale or malformed")
        if enforce_live_guards and any(self.lifecycle_owner_summary().values()):
            raise IntegrityError("execution drain has unresolved runtime owners")
        return


def _apply_completion_guard(
    self,
    mutation: ProjectionMutation,
    p: dict[str, Any],
    *,
    enforce_live_guards: bool,
) -> None:
    if mutation.kind == "goal_commit_guard":
        current = self._state()
        if enforce_live_guards and current.run_execution.value != "running":
            raise IntegrityError("goal completion owner is not running")
        if set(p) != {"gate_receipts"} or type(p["gate_receipts"]) not in {
            list,
            tuple,
        }:
            raise IntegrityError("goal completion gate receipts are malformed")
        bindings = tuple(p["gate_receipts"])
        if not bindings:
            raise IntegrityError("goal completion requires a gate receipt")
        normalized: list[tuple[str, str]] = []
        for item in bindings:
            if (
                type(item) not in {list, tuple}
                or len(item) != 2
                or not _is_sha256(item[0])
                or not _is_sha256(item[1])
            ):
                raise IntegrityError("goal completion gate receipt is malformed")
            normalized.append((item[0], item[1]))
        if (
            normalized != sorted(normalized)
            or len(set(normalized)) != len(normalized)
            or len({item[0] for item in normalized}) != len(normalized)
        ):
            raise IntegrityError("goal completion gate receipts are not canonical")
        for flag_digest, receipt_digest in normalized:
            rows = self._conn.execute(
                "SELECT e.payload_json FROM events e JOIN commands c "
                "ON c.command_id=e.command_id "
                "WHERE e.kind='FLAG_ACCEPTED' AND c.receipt_digest=?",
                (receipt_digest,),
            ).fetchall()
            matching = [
                json.loads(row[0])
                for row in rows
                if json.loads(row[0]).get("flag_digest") == flag_digest
            ]
            if len(matching) != 1:
                raise IntegrityError("goal completion gate receipt does not resolve")
            accepted = matching[0]
            admissions = self._conn.execute(
                "SELECT payload_json FROM events WHERE kind='ATTEMPT_ADMITTED'"
            ).fetchall()
            admitted_attempts = [
                json.loads(row[0]).get("attempt_id")
                for row in admissions
                if json.loads(row[0]).get("attempt_digest")
                == accepted.get("attempt_digest")
            ]
            if len(admitted_attempts) != 1:
                raise IntegrityError("goal completion attempt does not resolve")
            progress_rows = self._conn.execute(
                "SELECT payload_json FROM events WHERE kind='PROGRESS_RECORDED'"
            ).fetchall()
            progress_matches = [
                json.loads(row[0])
                for row in progress_rows
                if (
                    json.loads(row[0]).get("kind") == "goal_unit"
                    and json.loads(row[0]).get("basis_digest") == receipt_digest
                    and json.loads(row[0]).get("goal_unit") == flag_digest
                    and json.loads(row[0]).get("attempt_id") == admitted_attempts[0]
                )
            ]
            if len(progress_matches) != 1:
                raise IntegrityError(
                    "goal completion lacks an exact verified progress occurrence"
                )
        return
    if mutation.kind == "projection_verify_guard":
        current = self._state()
        scope_digest = canonical_digest(
            {
                "execution_generation": current.execution_generation,
                "run_fence_epoch": current.run_fence_epoch,
                "run_id": current.run_id,
            }
        )
        if set(p) != {"after", "before", "equivalent", "scope_digest"}:
            raise IntegrityError("projection verification payload is malformed")
        current_digest = self.runtime_projection_digest()
        if (
            p["equivalent"] is not True
            or not _is_sha256(p["before"])
            or p["before"] != p["after"]
            or p["after"] != current_digest
            or p["scope_digest"] != scope_digest
        ):
            raise IntegrityError("projection verification is not equivalent")
        return
    if mutation.kind == "s4e_closure_guard":
        current = self._state()
        scope_digest = canonical_digest(
            {
                "execution_generation": current.execution_generation,
                "run_fence_epoch": current.run_fence_epoch,
                "run_id": current.run_id,
            }
        )
        if set(p) != {
            "all_clean",
            "components",
            "invariants",
            "schema",
            "scope_digest",
            "solved",
        }:
            raise IntegrityError("S4-E closure payload is malformed")
        if type(p["all_clean"]) is not bool or type(p["solved"]) is not bool:
            raise IntegrityError("S4-E closure booleans are malformed")
        if p["scope_digest"] != scope_digest:
            raise IntegrityError("S4-E closure scope is stale")
        required_components = {
            "canonical_permit",
            "capture_manifest",
            "gate_input",
            "orphan_summary",
            "s4e_schema",
            "usage_closure",
        }
        components = p["components"]
        if (
            type(components) is not dict
            or set(components) != required_components
            or any(not _is_sha256(value) for value in components.values())
        ):
            raise IntegrityError("S4-E closure components are malformed")
        invariants = p["invariants"]
        required_invariants = {
            "capture_pairs",
            "effects_close",
            "gate_closes",
            "orphan_free",
            "usage_closes",
        }
        if (
            type(invariants) is not dict
            or set(invariants) != required_invariants
            or any(type(value) is not bool for value in invariants.values())
            or p["all_clean"] is not all(invariants.values())
            or (p["solved"] and not p["all_clean"])
        ):
            raise IntegrityError("S4-E closure invariants are inconsistent")
        schema = p["schema"]
        if (
            type(schema) is not dict
            or schema.get("name") != "muteki-s4e-closure"
            or schema.get("version") != 1
            or tuple(schema.get("required_components") or ())
            != (
                "canonical_permit",
                "capture_manifest",
                "gate_input",
                "orphan_summary",
                "usage_closure",
            )
            or components["s4e_schema"] != canonical_digest(schema)
        ):
            raise IntegrityError("S4-E closure schema is malformed")
        return
    if mutation.kind == "canary_commit_guard":
        if set(p) != {"canary_digest", "level", "receipt_chain", "run_id"}:
            raise IntegrityError("canary admission payload is malformed")
        chain = p["receipt_chain"]
        if (
            not _is_sha256(p["canary_digest"])
            or p["level"] != "live_local"
            or type(p["run_id"]) is not str
            or not p["run_id"]
            or type(chain) is not dict
            or not chain
            or any(
                type(name) is not str
                or not name
                or name != name.strip()
                or not _is_sha256(digest)
                for name, digest in chain.items()
            )
        ):
            raise IntegrityError("canary admission contract is malformed")
        run = self._conn.execute(
            "SELECT state FROM catalog_runs WHERE run_id=?", (p["run_id"],)
        ).fetchone()
        if enforce_live_guards and (run is None or run[0] != "sealed"):
            raise IntegrityError("canary admission has no sealed catalog run")
        return


def _apply_branch_state(self, mutation: ProjectionMutation, p: dict[str, Any]) -> None:
    if mutation.kind == "branch_state":
        cur = self._conn.execute(
            "UPDATE runtime_branches SET state=? WHERE branch_id=? AND state=?",
            (p["new_state"], p["branch_id"], p["expected_state"]),
        )
        if cur.rowcount != 1:
            raise IntegrityError("branch state compare-and-set failed")
        return
