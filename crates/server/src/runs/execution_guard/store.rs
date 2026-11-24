use super::cronus::GuardEvaluation;
use crate::AppState;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

pub(super) fn init(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS guard_state(
            generation INTEGER PRIMARY KEY, fence_epoch INTEGER NOT NULL DEFAULT 0,
            egress_state TEXT NOT NULL CHECK(egress_state IN ('open','held','revoked')), stop_id TEXT);
        CREATE TABLE IF NOT EXISTS guard_actions(
            generation INTEGER NOT NULL, call_id TEXT NOT NULL,
            request_json TEXT NOT NULL, evaluation_json TEXT NOT NULL,
            decision TEXT NOT NULL CHECK(decision IN ('allow','ask','deny')),
            cronus_ms REAL, cronus_error INTEGER NOT NULL DEFAULT 0,
            receipt_state TEXT NOT NULL CHECK(receipt_state IN ('not_sent','sent','outcome_unknown','completed')),
            approval_id TEXT UNIQUE, approval_state TEXT CHECK(approval_state IN
                ('pending','allow_once','deny_once','consumed','revoked')),
            note TEXT, PRIMARY KEY(generation,call_id));
        CREATE TABLE IF NOT EXISTS guard_pauses(
            generation INTEGER NOT NULL, worker_id TEXT NOT NULL,
            wait_count INTEGER NOT NULL DEFAULT 0,
            started_at REAL, accumulated_secs REAL NOT NULL DEFAULT 0.0,
            PRIMARY KEY(generation,worker_id));
    ",
    )
}

pub(in crate::runs) fn ensure_open(db: &Connection, generation: u64) -> rusqlite::Result<()> {
    init(db)?;
    db.execute(
        "INSERT OR IGNORE INTO guard_state(generation, fence_epoch, egress_state) VALUES(?1, 0, 'open')",
        [generation],
    )?;
    Ok(())
}

pub(in crate::runs) fn state_tuple(
    db: &Connection,
    generation: u64,
) -> rusqlite::Result<(String, u64)> {
    init(db)?;
    Ok(db
        .query_row(
            "SELECT egress_state, fence_epoch FROM guard_state WHERE generation=?1",
            [generation],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?)),
        )
        .optional()?
        .unwrap_or_else(|| ("open".to_owned(), 0)))
}

pub(in crate::runs) fn has_denied_fingerprint(
    db: &Connection,
    generation: u64,
    fingerprint: &str,
) -> rusqlite::Result<bool> {
    init(db)?;
    db.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM guard_actions
            WHERE generation=?1 AND decision='deny'
              AND json_extract(request_json, '$.fingerprint')=?2
        )",
        params![generation, fingerprint],
        |row| row.get(0),
    )
}

pub(in crate::runs) fn record_evaluation(
    db: &Connection,
    generation: u64,
    call_id: &str,
    request_view: &Value,
    eval: &GuardEvaluation,
) -> Result<(String, Option<String>, bool, bool), String> {
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    ensure_open(&tx, generation).map_err(|e| e.to_string())?;
    let current_egress: String = tx
        .query_row(
            "SELECT egress_state FROM guard_state WHERE generation=?1",
            [generation],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if current_egress == "revoked" {
        return Ok(("emergency_stop".into(), None, false, false));
    }
    if current_egress == "held" {
        return Ok(("held_retry".into(), None, false, false));
    }
    if let Some((existing_decision, existing_approval)) = tx
        .query_row(
            "SELECT decision, approval_id FROM guard_actions WHERE generation=?1 AND call_id=?2",
            params![generation, call_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?
    {
        tx.commit().map_err(|e| e.to_string())?;
        return Ok((existing_decision, existing_approval, false, false));
    }

    let (approval_id, approval_state, receipt_state) = match eval.decision.as_str() {
        "ask" => (
            Some(format!("ga-{}", Uuid::new_v4().simple())),
            Some("pending"),
            "not_sent",
        ),
        "deny" => (None, None, "not_sent"),
        _ => (None, None, "not_sent"),
    };
    let cronus_error = i64::from(!eval.available);
    tx.execute(
        "INSERT INTO guard_actions(
            generation, call_id, request_json, evaluation_json, decision, cronus_ms, cronus_error,
            receipt_state, approval_id, approval_state, note
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            generation,
            call_id,
            request_view.to_string(),
            eval.to_stored_json().to_string(),
            eval.decision,
            eval.latency_ms,
            cronus_error,
            receipt_state,
            approval_id,
            approval_state,
            eval.reason,
        ],
    )
    .map_err(|e| e.to_string())?;
    let mut moved_to_held = false;
    if eval.decision == "ask" {
        moved_to_held = tx
            .execute(
                "UPDATE guard_state SET egress_state='held' WHERE generation=?1 AND egress_state='open'",
                [generation],
            )
            .map_err(|e| e.to_string())?
            == 1;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok((eval.decision.clone(), approval_id, moved_to_held, true))
}

pub(in crate::runs) fn poll_decision(
    db: &Connection,
    generation: u64,
    approval_id: &str,
) -> Result<Option<(String, Option<String>)>, String> {
    init(db).map_err(|e| e.to_string())?;
    let (egress, _) = state_tuple(db, generation).map_err(|e| e.to_string())?;
    if egress == "revoked" {
        return Ok(Some((
            "emergency_stop".to_owned(),
            Some("Run egress authorization revoked by emergency stop".to_owned()),
        )));
    }
    let row: Option<(Option<String>, Option<String>)> = db
        .query_row(
            "SELECT approval_state, note FROM guard_actions WHERE generation=?1 AND approval_id=?2",
            params![generation, approval_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    match row {
        Some((Some(state), note)) if state == "allow_once" => {
            Ok(Some(("allow_once".to_owned(), note)))
        }
        Some((Some(state), note)) if state == "deny_once" => {
            Ok(Some(("deny_once".to_owned(), note)))
        }
        Some((Some(state), note)) if state == "revoked" => {
            Ok(Some(("emergency_stop".to_owned(), note)))
        }
        Some((Some(state), _)) if state == "pending" => Ok(None),
        _ => Ok(Some((
            "deny_once".to_owned(),
            Some("Approval record no longer pending".to_owned()),
        ))),
    }
}

pub(in crate::runs) fn consume_for_dispatch(
    db: &Connection,
    generation: u64,
    call_id: &str,
    approval_id: Option<&str>,
    allow: bool,
) -> Result<bool, String> {
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    init(&tx).map_err(|e| e.to_string())?;
    let revoked_now: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM guard_state WHERE generation=?1 AND egress_state='revoked')",
            [generation],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if allow && revoked_now {
        return Ok(false);
    }
    let updated = match (approval_id, allow) {
        (Some(aid), true) => tx
            .execute(
                "UPDATE guard_actions
                 SET approval_state='consumed', receipt_state='sent'
                 WHERE generation=?1 AND call_id=?2 AND approval_id=?3
                   AND approval_state='allow_once' AND receipt_state='not_sent'
                   AND EXISTS(SELECT 1 FROM guard_state WHERE generation=?1 AND egress_state<>'revoked')",
                params![generation, call_id, aid],
            )
            .map_err(|e| e.to_string())?,
        (Some(aid), false) => tx
            .execute(
                "UPDATE guard_actions
                 SET approval_state='consumed'
                 WHERE generation=?1 AND call_id=?2 AND approval_id=?3
                   AND approval_state='deny_once' AND receipt_state='not_sent'",
                params![generation, call_id, aid],
            )
            .map_err(|e| e.to_string())?,
        (None, true) => tx
            .execute(
                "UPDATE guard_actions
                 SET receipt_state='sent'
                 WHERE generation=?1 AND call_id=?2 AND decision='allow' AND receipt_state='not_sent'
                   AND EXISTS(SELECT 1 FROM guard_state WHERE generation=?1 AND egress_state='open')",
                params![generation, call_id],
            )
            .map_err(|e| e.to_string())?,
        (None, false) => 1,
    };
    if updated != 1 {
        return Ok(false);
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(true)
}

pub(in crate::runs) fn settle_call(
    db: &Connection,
    generation: u64,
    call_id: &str,
    summary: &str,
) -> Result<Option<Value>, String> {
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    init(&tx).map_err(|e| e.to_string())?;
    let changed = tx
        .execute(
            "UPDATE guard_actions SET receipt_state='completed',
                note=CASE WHEN note IS NULL OR note='' THEN ?3 ELSE note END
             WHERE generation=?1 AND call_id=?2 AND receipt_state='sent'",
            params![generation, call_id, summary],
        )
        .map_err(|e| e.to_string())?;
    let view = if changed == 1 {
        tx.query_row(
            "SELECT call_id, approval_id, decision, receipt_state, approval_state, note, request_json, evaluation_json
             FROM guard_actions WHERE generation=?1 AND call_id=?2",
            params![generation, call_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())?
        .map(|row| build_action_view(row, false))
        .transpose()?
    } else {
        None
    };
    tx.commit().map_err(|e| e.to_string())?;
    Ok(view)
}

pub(in crate::runs) fn begin_wait_pause(
    db: &Connection,
    generation: u64,
    worker_id: &str,
) -> Result<(), String> {
    let now = super::super::now_secs();
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    init(&tx).map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO guard_pauses(generation, worker_id, wait_count, started_at, accumulated_secs)
         VALUES(?1, ?2, 1, ?3, 0.0)
         ON CONFLICT(generation, worker_id) DO UPDATE SET
            wait_count = wait_count + 1,
            started_at = CASE WHEN wait_count = 0 THEN ?3 ELSE started_at END",
        params![generation, worker_id, now],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

pub(in crate::runs) fn end_wait_pause(
    db: &Connection,
    generation: u64,
    worker_id: &str,
) -> Result<(), String> {
    let now = super::super::now_secs();
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    init(&tx).map_err(|e| e.to_string())?;
    tx.execute(
        "UPDATE guard_pauses SET
            accumulated_secs = accumulated_secs + CASE WHEN wait_count = 1 AND started_at IS NOT NULL THEN MAX(0.0, ?3 - started_at) ELSE 0.0 END,
            started_at = CASE WHEN wait_count <= 1 THEN NULL ELSE started_at END,
            wait_count = MAX(0, wait_count - 1)
         WHERE generation=?1 AND worker_id=?2",
        params![generation, worker_id, now],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

pub(in crate::runs) fn pause_seconds(
    db: &Connection,
    generation: u64,
    worker_id: &str,
) -> rusqlite::Result<(f64, bool)> {
    init(db)?;
    let row: Option<(i64, Option<f64>, f64)> = db
        .query_row(
            "SELECT wait_count, started_at, accumulated_secs FROM guard_pauses WHERE generation=?1 AND worker_id=?2",
            params![generation, worker_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((count, started, acc)) = row else {
        return Ok((0.0, false));
    };
    let paused = count > 0;
    let extra = if paused {
        started
            .map(|at| (super::super::now_secs() - at).max(0.0))
            .unwrap_or(0.0)
    } else {
        0.0
    };
    Ok((acc + extra, paused))
}

pub(in crate::runs) fn revoke(
    db: &Connection,
    generation: u64,
    command: &str,
) -> rusqlite::Result<(Vec<String>, Vec<String>)> {
    init(db)?;
    db.execute(
        "INSERT INTO guard_state(generation,fence_epoch,egress_state,stop_id) VALUES(?1,1,'revoked',?2)
        ON CONFLICT(generation) DO UPDATE SET fence_epoch=fence_epoch+1,egress_state='revoked',stop_id=excluded.stop_id",
        params![generation, command],
    )?;
    let now = super::super::now_secs();
    db.execute(
        "UPDATE guard_pauses SET
            accumulated_secs = accumulated_secs + CASE WHEN wait_count > 0 AND started_at IS NOT NULL THEN MAX(0.0, ?2 - started_at) ELSE 0.0 END,
            wait_count = 0, started_at = NULL
         WHERE generation=?1",
        params![generation, now],
    )?;
    let changed = |sql| -> rusqlite::Result<Vec<String>> {
        db.prepare(sql)?
            .query_map([generation], |row| row.get(0))?
            .collect()
    };
    let approvals = changed(
        "UPDATE guard_actions SET approval_state='revoked'
        WHERE generation=?1 AND approval_state IN ('pending','allow_once') RETURNING approval_id",
    )?;
    let unsettled = changed(
        "UPDATE guard_actions SET receipt_state='outcome_unknown'
        WHERE generation=?1 AND receipt_state='sent' RETURNING call_id",
    )?;
    Ok((approvals, unsettled))
}

pub(in crate::runs) fn revoked(db: &Connection, generation: u64) -> rusqlite::Result<bool> {
    init(db)?;
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM guard_state WHERE generation=?1 AND egress_state='revoked')",
        [generation],
        |r| r.get(0),
    )
}

type ActionRow = (
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
);

fn build_action_view(row: ActionRow, is_pending: bool) -> Result<Value, String> {
    let (call, approval, decision, receipt_state, approval_state, note, raw_req, raw_eval) = row;
    let request: Value = serde_json::from_str(&raw_req).map_err(|e| e.to_string())?;
    let eval: Value = serde_json::from_str(&raw_eval).map_err(|e| e.to_string())?;
    let status = if is_pending {
        approval_state.clone().unwrap_or_else(|| "pending".into())
    } else {
        receipt_state.clone()
    };
    let cronus = if eval.get("available").is_some() {
        eval.clone()
    } else {
        Value::Null
    };
    Ok(json!({
        "tool_call_id": call,
        "receipt_id": call,
        "approval_id": approval,
        "status": status,
        "delivery_status": receipt_state,
        "decision": decision,
        "tool_name": request["tool_name"],
        "action_summary": request["action_summary"].as_str().unwrap_or_else(|| request["tool_name"].as_str().unwrap_or("tool")),
        "command": request["command"],
        "http_summary": request["http_summary"],
        "script_summary": request["script_summary"],
        "args_redacted": request["args_redacted"],
        "worker_id": request["worker_id"],
        "worker_role": request["worker_role"],
        "reason": eval["reason"].as_str().or(note.as_deref()),
        "risk_level": eval["risk_level"],
        "note": note,
        "cronus": cronus,
        "created_at_ms": request["created_at_ms"],
    }))
}

pub(in crate::runs) fn metrics(db: &Connection, generation: u64) -> Result<Value, String> {
    db.query_row(
        "SELECT count(*),count(*) FILTER(WHERE decision='allow'),
        count(*) FILTER(WHERE decision='ask'),count(*) FILTER(WHERE decision='deny'),
        count(*) FILTER(WHERE cronus_error<>0),coalesce(avg(cronus_ms),0) FROM guard_actions WHERE generation=?1",
        [generation],
        |r| {
            Ok(json!({
                "total_evaluated": r.get::<_, u64>(0)?,
                "allowed": r.get::<_, u64>(1)?,
                "escalated": r.get::<_, u64>(2)?,
                "denied": r.get::<_, u64>(3)?,
                "cronus_errors": r.get::<_, u64>(4)?,
                "avg_cronus_ms": r.get::<_, f64>(5)?
            }))
        },
    )
    .map_err(|e| e.to_string())
}

pub(super) fn snapshot(db: &Connection, generation: u64) -> Result<Value, String> {
    init(db).map_err(|e| e.to_string())?;
    let tx = db.unchecked_transaction().map_err(|e| e.to_string())?;
    let (egress, stop): (String, Option<String>) = tx
        .query_row(
            "SELECT egress_state,stop_id FROM guard_state WHERE generation=?1",
            [generation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| ("open".to_owned(), None));
    let read = |sql: &str, is_pending: bool| -> Result<Vec<Value>, String> {
        let mut stmt = tx.prepare(sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([generation], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        rows.map(|r| build_action_view(r.map_err(|e| e.to_string())?, is_pending))
            .collect()
    };
    let pending = read(
        "SELECT call_id, approval_id, decision, receipt_state, approval_state, note, request_json, evaluation_json
         FROM guard_actions WHERE generation=?1 AND approval_state='pending' ORDER BY rowid",
        true,
    )?;
    let receipts = read(
        "SELECT call_id, approval_id, decision, receipt_state, approval_state, note, request_json, evaluation_json
         FROM guard_actions WHERE generation=?1 ORDER BY rowid DESC LIMIT 100",
        false,
    )?;
    let stats = metrics(&tx, generation)?;
    let receipt = stop.map(|id| stop_receipt(&tx, &id)).transpose()?;
    Ok(json!({
        "enabled": true,
        "mode": "protected",
        "egress_state": egress,
        "pending_approvals": pending,
        "recent_receipts": receipts,
        "metrics": stats,
        "emergency_stop_receipt": receipt
    }))
}

pub(in crate::runs) fn stop_receipt(db: &Connection, id: &str) -> Result<Value, String> {
    let raw: String = db
        .query_row(
            "SELECT response_json FROM control_commands WHERE command_id=?1 AND action='emergency_stop'",
            [id],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    let mut receipt: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    receipt["status"] = json!(match receipt["status"].as_str() {
        Some("applied" | "effect_observed") if receipt["process_exit_confirmed"] == true =>
            "APPLIED",
        Some("partial" | "projection_conflict") => "PARTIAL",
        _ => "UNKNOWN",
    });
    Ok(receipt)
}

pub(in crate::runs) fn exit_evidence(
    state: &AppState,
    run: &str,
    generation: u64,
) -> Result<(bool, bool, bool), String> {
    let mut confirmed = 0;
    let mut observed = 0;
    let mut run_stopped = false;
    for collection in ["runner_runs", "runner_controller_runs", "runner_commands"] {
        for row in state.store.list(collection).map_err(|e| e.to_string())? {
            if (row["run_id"] != run && row["owner_run_id"] != run)
                || row["generation"] != generation
            {
                continue;
            }
            observed += 1;
            let stopped = if collection == "runner_commands" {
                crate::runner_execution::facts(&row).stopped
            } else {
                row["state"] == "stopped"
            };
            if stopped {
                confirmed += 1;
                run_stopped |= collection != "runner_commands" && row["run_id"] == run;
            }
        }
    }
    Ok((
        run_stopped && confirmed == observed,
        confirmed > 0,
        observed > 0,
    ))
}
