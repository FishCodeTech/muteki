"""Effect and catalog projection mutation handlers."""

from __future__ import annotations

import json
from typing import Any

from muteki.epistemic.contracts import canonical_json_bytes
from muteki.epistemic.sqlite_types import (
    EFFECT_LEGAL_TRANSITIONS,
    IntegrityError,
    ProjectionMutation,
    require_positive_effect_revision,
)


def _apply_effect_catalog_projection_mutation(
    self,
    mutation: ProjectionMutation,
    p: dict[str, Any],
    *,
    enforce_live_guards: bool,
) -> bool:
    kind = mutation.kind
    if kind in {"effect_prepare", "effect_transition", "effect_retry"}:
        _apply_effect_mutation(
            self, mutation, p, enforce_live_guards=enforce_live_guards
        )
        return True
    if kind in {
        "draft_create",
        "draft_attachment",
        "provision_begin",
        "provision_materialized",
        "provision_sealed",
        "provision_failed",
    }:
        _apply_catalog_provision_mutation(self, mutation, p)
        return True
    if kind in {
        "archive_assert_settled",
        "archive_begin",
        "archive_complete",
        "purge_begin",
        "purge_item_absent",
        "purge_item_unknown",
        "purge_complete",
    }:
        _apply_catalog_retention_mutation(self, mutation, p)
        return True
    return False


def _apply_effect_mutation(
    self,
    mutation: ProjectionMutation,
    p: dict[str, Any],
    *,
    enforce_live_guards: bool,
) -> None:
    if mutation.kind == "effect_prepare":
        attempt = self._conn.execute(
            "SELECT effect_class,state FROM runtime_attempts WHERE attempt_id=?",
            (p["attempt_id"],),
        ).fetchone()
        if attempt is None or attempt[1] != "running":
            raise IntegrityError("effect requires a running admitted attempt")
        if attempt[0] != p["effect_class"]:
            raise IntegrityError("effect class does not match admitted permit")
        keys = tuple(str(key) for key in p.get("conflict_keys", []))
        admission_rows = self._conn.execute(
            "SELECT payload_json FROM events WHERE kind='ATTEMPT_ADMITTED'"
        ).fetchall()
        admissions = [
            json.loads(row[0])
            for row in admission_rows
            if json.loads(row[0]).get("attempt_id") == p["attempt_id"]
        ]
        if (
            len(admissions) != 1
            or set(admissions[0].get("conflict_keys", ())) != set(keys)
            or len(keys) != len(set(keys))
        ):
            raise IntegrityError(
                "effect conflict keys do not match the admitted permit"
            )
        for key in keys:
            row = self._conn.execute(
                "SELECT operation_id FROM effect_conflict_holds WHERE conflict_key=?",
                (key,),
            ).fetchone()
            if row is not None and row[0] != p["attempt_id"]:
                raise IntegrityError("effect conflict is held by another operation")
            self._conn.execute(
                "INSERT INTO effect_conflict_holds(conflict_key,operation_id,state) "
                "VALUES(?,?,'active') ON CONFLICT(conflict_key) DO UPDATE SET "
                "operation_id=excluded.operation_id,state='active'",
                (key, p["operation_id"]),
            )
        self._conn.execute(
            "INSERT INTO effect_operations(operation_id,attempt_id,effect_class,"
            "conflict_keys_json,state,current_ordinal) VALUES(?,?,?,?,'prepared',1)",
            (
                p["operation_id"],
                p["attempt_id"],
                p["effect_class"],
                canonical_json_bytes(keys).decode(),
            ),
        )
        self._conn.execute(
            "INSERT INTO effect_attempts(operation_id,ordinal,state) VALUES(?,1,'prepared')",
            (p["operation_id"],),
        )
        return
    if mutation.kind == "effect_transition":
        require_positive_effect_revision(p["revision"])
        operation_id = str(p["operation_id"])
        new_state = str(p["new_state"])
        row = self._conn.execute(
            "SELECT state,current_ordinal,conflict_keys_json,attempt_id "
            "FROM effect_operations "
            "WHERE operation_id=?",
            (operation_id,),
        ).fetchone()
        if row is None:
            raise IntegrityError("effect transition compare-and-set failed")
        owner = self._conn.execute(
            "SELECT state FROM runtime_attempts WHERE attempt_id=?",
            (row[3],),
        ).fetchone()
        if enforce_live_guards and (owner is None or owner[0] != "running"):
            raise IntegrityError("effect transition requires a running attempt owner")
        if enforce_live_guards and row[0] == "unknown":
            raise IntegrityError(
                "UNKNOWN effect requires an independent observer receipt"
            )
        # Enforce the FSM from the actual projected state, independent of CAS.
        if new_state not in EFFECT_LEGAL_TRANSITIONS.get(str(row[0]), frozenset()):
            raise IntegrityError(
                f"illegal effect transition {row[0]!r} -> {new_state!r}"
            )
        if row[0] != p["expected_state"]:
            raise IntegrityError("effect transition compare-and-set failed")
        ordinal = int(row[1])
        self._conn.execute(
            "UPDATE effect_operations SET state=? WHERE operation_id=?",
            (new_state, operation_id),
        )
        self._conn.execute(
            "UPDATE effect_attempts SET state=? WHERE operation_id=? AND ordinal=?",
            (new_state, operation_id, ordinal),
        )
        keys = json.loads(row[2])
        if new_state == "unknown":
            for key in keys:
                self._conn.execute(
                    "UPDATE effect_conflict_holds SET state='unknown' "
                    "WHERE conflict_key=? AND operation_id=?",
                    (key, operation_id),
                )
        elif new_state in {"observed", "confirmed_not_applied"}:
            self._conn.execute(
                "DELETE FROM effect_conflict_holds WHERE operation_id=?",
                (operation_id,),
            )
        return
    if mutation.kind == "effect_retry":
        require_positive_effect_revision(p["revision"])
        if enforce_live_guards:
            raise IntegrityError("effect retry requires a fresh canonical admission")
        operation_id = str(p["operation_id"])
        row = self._conn.execute(
            "SELECT state,current_ordinal,conflict_keys_json FROM effect_operations "
            "WHERE operation_id=?",
            (operation_id,),
        ).fetchone()
        if row is None or row[0] != "confirmed_not_applied":
            raise IntegrityError("only confirmed-not-applied effect may retry")
        ordinal = int(row[1]) + 1
        self._conn.execute(
            "UPDATE effect_operations SET state='prepared',current_ordinal=? "
            "WHERE operation_id=?",
            (ordinal, operation_id),
        )
        self._conn.execute(
            "INSERT INTO effect_attempts(operation_id,ordinal,state) "
            "VALUES(?,?,'prepared')",
            (operation_id, ordinal),
        )
        for key in json.loads(row[2]):
            self._conn.execute(
                "INSERT INTO effect_conflict_holds(conflict_key,operation_id,state) "
                "VALUES(?,?,'active')",
                (key, operation_id),
            )
        return


def _apply_catalog_provision_mutation(
    self, mutation: ProjectionMutation, p: dict[str, Any]
) -> None:
    if mutation.kind == "draft_create":
        self._conn.execute(
            "INSERT INTO catalog_drafts(draft_id,policy_json,state) VALUES(?,?,'open')",
            (p["draft_id"], canonical_json_bytes(p["policy"]).decode()),
        )
        return
    if mutation.kind == "draft_attachment":
        draft = self._conn.execute(
            "SELECT state FROM catalog_drafts WHERE draft_id=?", (p["draft_id"],)
        ).fetchone()
        if draft is None or draft[0] != "open":
            raise IntegrityError("attachment requires an open draft")
        self._conn.execute(
            "INSERT INTO catalog_attachments(attachment_id,draft_id,digest,byte_count) "
            "VALUES(?,?,?,?)",
            (p["attachment_id"], p["draft_id"], p["digest"], int(p["byte_count"])),
        )
        return
    if mutation.kind == "provision_begin":
        draft = self._conn.execute(
            "SELECT state FROM catalog_drafts WHERE draft_id=?", (p["draft_id"],)
        ).fetchone()
        if draft is None or draft[0] != "open":
            raise IntegrityError("provision requires an open draft")
        self._conn.execute(
            "INSERT INTO provision_operations(operation_id,draft_id,allocated_run_id,"
            "target_root,manifest_digest,owner_epoch,state) "
            "VALUES(?,?,?,?,?,?,'run_allocated')",
            (
                p["operation_id"],
                p["draft_id"],
                p["run_id"],
                p["target_root"],
                p["manifest_digest"],
                int(p["owner_epoch"]),
            ),
        )
        self._conn.execute(
            "INSERT INTO catalog_runs(run_id,operation_id,manifest_digest,state) "
            "VALUES(?,?,?,'allocating')",
            (p["run_id"], p["operation_id"], p["manifest_digest"]),
        )
        self._conn.execute(
            "UPDATE catalog_drafts SET state='provisioning' WHERE draft_id=?",
            (p["draft_id"],),
        )
        return
    if mutation.kind == "provision_materialized":
        cur = self._conn.execute(
            "UPDATE provision_operations SET state='run_materialized' "
            "WHERE operation_id=? AND state='run_allocated' AND owner_epoch=?",
            (p["operation_id"], int(p["owner_epoch"])),
        )
        if cur.rowcount != 1:
            raise IntegrityError("provision owner/state fence failed")
        return
    if mutation.kind == "provision_sealed":
        cur = self._conn.execute(
            "UPDATE provision_operations SET state='sealed' "
            "WHERE operation_id=? AND state='run_materialized' AND owner_epoch=?",
            (p["operation_id"], int(p["owner_epoch"])),
        )
        if cur.rowcount != 1:
            raise IntegrityError("provision seal fence failed")
        self._conn.execute(
            "UPDATE catalog_runs SET state='sealed',anchor_digest=? "
            "WHERE run_id=? AND operation_id=? AND state='allocating'",
            (p["anchor_digest"], p["run_id"], p["operation_id"]),
        )
        self._conn.execute(
            "UPDATE catalog_drafts SET state='sealed' WHERE draft_id=("
            "SELECT draft_id FROM provision_operations WHERE operation_id=?)",
            (p["operation_id"],),
        )
        return
    if mutation.kind == "provision_failed":
        self._conn.execute(
            "UPDATE provision_operations SET state='failed_seal' "
            "WHERE operation_id=? AND owner_epoch=? AND state!='sealed'",
            (p["operation_id"], int(p["owner_epoch"])),
        )
        self._conn.execute(
            "UPDATE catalog_runs SET state='failed_seal' WHERE operation_id=?",
            (p["operation_id"],),
        )
        self._conn.execute(
            "UPDATE catalog_drafts SET state='failed' WHERE draft_id=("
            "SELECT draft_id FROM provision_operations WHERE operation_id=?)",
            (p["operation_id"],),
        )
        return


def _apply_catalog_retention_mutation(
    self, mutation: ProjectionMutation, p: dict[str, Any]
) -> None:
    if mutation.kind == "archive_assert_settled":
        active_attempts = int(
            self._conn.execute(
                "SELECT COUNT(*) FROM runtime_attempts WHERE state IN ('reserved','running','unknown')"
            ).fetchone()[0]
        )
        active_reservations = int(
            self._conn.execute(
                "SELECT COUNT(*) FROM budget_reservations WHERE state IN ('active','unknown')"
            ).fetchone()[0]
        )
        active_effects = int(
            self._conn.execute("SELECT COUNT(*) FROM effect_conflict_holds").fetchone()[
                0
            ]
        )
        if active_attempts or active_reservations or active_effects:
            raise IntegrityError("run has unsettled attempt/effect/budget owners")
        return
    if mutation.kind == "archive_begin":
        run = self._conn.execute(
            "SELECT state FROM catalog_runs WHERE run_id=?", (p["run_id"],)
        ).fetchone()
        if run is None or run[0] != "sealed":
            raise IntegrityError("archive requires a sealed catalog run")
        self._conn.execute(
            "INSERT INTO archive_operations(operation_id,run_id,owner_epoch,state,requested_at_ns) "
            "VALUES(?,?,?,'requested',?)",
            (
                p["operation_id"],
                p["run_id"],
                int(p["owner_epoch"]),
                int(p["requested_at_ns"]),
            ),
        )
        return
    if mutation.kind == "archive_complete":
        cur = self._conn.execute(
            "UPDATE archive_operations SET state='archived',run_receipt_digest=?,"
            "archive_receipt_digest=? WHERE operation_id=? AND run_id=? "
            "AND owner_epoch=? AND state='requested'",
            (
                p["run_receipt_digest"],
                p["archive_receipt_digest"],
                p["operation_id"],
                p["run_id"],
                int(p["owner_epoch"]),
            ),
        )
        if cur.rowcount != 1:
            raise IntegrityError("archive owner/state fence failed")
        cur = self._conn.execute(
            "UPDATE catalog_runs SET state='archived' WHERE run_id=? AND state='sealed'",
            (p["run_id"],),
        )
        if cur.rowcount != 1:
            raise IntegrityError("catalog archive transition failed")
        return
    if mutation.kind == "purge_begin":
        run = self._conn.execute(
            "SELECT state FROM catalog_runs WHERE run_id=?", (p["run_id"],)
        ).fetchone()
        if run is None or run[0] != "archived":
            raise IntegrityError("purge requires an archived catalog run")
        items = tuple(p.get("items") or ())
        if not items:
            raise IntegrityError("purge plan must contain at least one item")
        self._conn.execute(
            "INSERT INTO purge_operations(operation_id,run_id,owner_epoch,state,"
            "plan_digest,plan_receipt_digest,requested_at_ns) "
            "VALUES(?,?,?,'purge_pending',?,?,?)",
            (
                p["operation_id"],
                p["run_id"],
                int(p["owner_epoch"]),
                p["plan_digest"],
                p["plan_receipt_digest"],
                int(p["requested_at_ns"]),
            ),
        )
        for ordinal, item in enumerate(items):
            self._conn.execute(
                "INSERT INTO purge_plan_items(operation_id,ordinal,locator,adapter,state) "
                "VALUES(?,?,?,?,'pending')",
                (p["operation_id"], ordinal, item["locator"], item["adapter"]),
            )
        return
    if mutation.kind == "purge_item_absent":
        cur = self._conn.execute(
            "UPDATE purge_plan_items SET state='absent',action_receipt_digest=?,"
            "absence_receipt_digest=? WHERE operation_id=? AND ordinal=? "
            "AND locator=? AND adapter=? AND state IN ('pending','unknown')",
            (
                p["action_receipt_digest"],
                p["absence_receipt_digest"],
                p["operation_id"],
                int(p["ordinal"]),
                p["locator"],
                p["adapter"],
            ),
        )
        if cur.rowcount != 1:
            raise IntegrityError("purge plan item identity/state fence failed")
        remaining_unknown = int(
            self._conn.execute(
                "SELECT COUNT(*) FROM purge_plan_items WHERE operation_id=? AND state='unknown'",
                (p["operation_id"],),
            ).fetchone()[0]
        )
        if not remaining_unknown:
            self._conn.execute(
                "UPDATE purge_operations SET state='purge_pending' "
                "WHERE operation_id=? AND state='purge_unknown'",
                (p["operation_id"],),
            )
        return
    if mutation.kind == "purge_item_unknown":
        cur = self._conn.execute(
            "UPDATE purge_plan_items SET state='unknown',action_receipt_digest=? "
            "WHERE operation_id=? AND ordinal=? AND locator=? AND adapter=? "
            "AND state='pending'",
            (
                p["action_receipt_digest"],
                p["operation_id"],
                int(p["ordinal"]),
                p["locator"],
                p["adapter"],
            ),
        )
        if cur.rowcount != 1:
            raise IntegrityError("purge unknown item identity/state fence failed")
        self._conn.execute(
            "UPDATE purge_operations SET state='purge_unknown' "
            "WHERE operation_id=? AND state='purge_pending'",
            (p["operation_id"],),
        )
        return
    if mutation.kind == "purge_complete":
        operation = self._conn.execute(
            "SELECT run_id,owner_epoch,plan_digest,state FROM purge_operations "
            "WHERE operation_id=?",
            (p["operation_id"],),
        ).fetchone()
        if (
            operation is None
            or operation[0] != p["run_id"]
            or int(operation[1]) != int(p["owner_epoch"])
            or operation[2] != p["plan_digest"]
            or operation[3] != "purge_pending"
        ):
            raise IntegrityError("purge owner/plan/state fence failed")
        pending = int(
            self._conn.execute(
                "SELECT COUNT(*) FROM purge_plan_items WHERE operation_id=? AND state!='absent'",
                (p["operation_id"],),
            ).fetchone()[0]
        )
        if pending:
            raise IntegrityError("purge cannot complete before every absence readback")
        self._conn.execute(
            "UPDATE purge_operations SET state='purged',absence_receipt_digest=? "
            "WHERE operation_id=?",
            (p["absence_receipt_digest"], p["operation_id"]),
        )
        cur = self._conn.execute(
            "UPDATE catalog_runs SET state='purged' WHERE run_id=? AND state='archived'",
            (p["run_id"],),
        )
        if cur.rowcount != 1:
            raise IntegrityError("catalog purge transition failed")
        self._conn.execute(
            "INSERT INTO catalog_tombstones(run_id,purge_operation_id,plan_digest,"
            "absence_receipt_digest,purged_at_ns) VALUES(?,?,?,?,?)",
            (
                p["run_id"],
                p["operation_id"],
                p["plan_digest"],
                p["absence_receipt_digest"],
                int(p["purged_at_ns"]),
            ),
        )
        return
