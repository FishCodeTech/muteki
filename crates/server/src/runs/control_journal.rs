//! Confirmed effects are journaled before their Run projection is published.
use super::*;
use rusqlite::OptionalExtension;

pub(super) fn init(conn: &Connection) -> Result<(), String> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS control_results(command_id TEXT PRIMARY KEY,patch_json TEXT NOT NULL,response_json TEXT NOT NULL,confirmed_at REAL NOT NULL); CREATE TABLE IF NOT EXISTS control_publications(command_id TEXT PRIMARY KEY,phase TEXT NOT NULL,conflicts_json TEXT NOT NULL,checked_at REAL NOT NULL)").map_err(|e|e.to_string())
}
fn publish(state: &AppState, run_id: &str, patch: &Value) -> Result<Vec<String>, String> {
    for _ in 0..16 {
        let before = state
            .store
            .get("runs", run_id)
            .map_err(|e| e.to_string())?
            .ok_or("Run disappeared while settling control")?;
        let mut after = before.clone();
        let mut conflicts = Vec::new();
        for change in patch.as_array().ok_or("Control patch is malformed")? {
            if change["fence"] == true {
                continue;
            }
            let key = change["key"].as_str().ok_or("Control patch key missing")?;
            let old = if change["had_before"] == true {
                Some(&change["before"])
            } else {
                None
            };
            let next = if change["remove"] == true {
                None
            } else {
                Some(&change["after"])
            };
            if before.get(key) == next {
                continue;
            }
            if key == "updated" {
                if let (Some(current), Some(confirmed)) =
                    (before[key].as_f64(), next.and_then(Value::as_f64))
                {
                    after[key] = json!(current.max(confirmed));
                    continue;
                }
            }
            if before.get(key) != old {
                conflicts.push(key.to_owned());
                continue;
            }
            if let Some(value) = next {
                after[key] = value.clone();
            } else {
                after
                    .as_object_mut()
                    .ok_or("Run is not an object")?
                    .remove(key);
            }
        }
        if before == after {
            return Ok(conflicts);
        }
        if state
            .store
            .put_many_if(
                &[("runs", run_id, Some(&before))],
                &[("runs", run_id, &after)],
            )
            .map_err(|e| e.to_string())?
        {
            return Ok(conflicts);
        }
    }
    Err("Run changed repeatedly during control settlement".into())
}
fn legacy_execution_fence(
    state: &AppState,
    run: &str,
    conn: &Connection,
    command: &str,
) -> Result<Option<u64>, String> {
    let created: f64 = conn
        .query_row(
            "SELECT created_at FROM control_commands WHERE command_id=?1",
            [command],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if let Some(intent) = state
        .store
        .get("runner_stop_intents", command)
        .map_err(|e| e.to_string())?
        && intent["run_id"] == run
    {
        if let Some(generation) = intent["generation"].as_u64() {
            return Ok(Some(generation));
        }
    }
    if let Some(generation) = state
        .store
        .list("run_freeze_effects")
        .map_err(|e| e.to_string())?
        .iter()
        .find(|row| row["run_id"] == run && row["command_id"] == command)
        .and_then(|row| row["generation"].as_u64())
    {
        return Ok(Some(generation));
    }
    let mut anchor_seen = false;
    let mut before_anchor_gen: Option<u64> = None;
    let mut best_by_ts: Option<(f64, Option<u64>, u64)> = None;
    for event in persisted_event_iter(state, run)? {
        let seq = event["seq"].as_u64();
        let event_type = event["event_type"].as_str();
        if !anchor_seen
            && event_type == Some("control.command")
            && event["payload"]["command_id"] == command
            && seq.is_some()
        {
            anchor_seen = true;
            if let Some(generation) = before_anchor_gen {
                return Ok(Some(generation));
            }
        }
        if matches!(
            event_type,
            Some("run.started" | "run.preparing" | "run.reopened")
        ) {
            let generation = event["payload"]["execution_generation"].as_u64();
            if !anchor_seen && seq.is_some() {
                before_anchor_gen = generation;
            }
            if let (Some(at), Some(generation)) =
                (event["ts"].as_f64().filter(|at| *at <= created), generation)
            {
                let replace = best_by_ts.as_ref().is_none_or(|(prev_at, prev_seq, _)| {
                    at.partial_cmp(prev_at)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| seq.cmp(prev_seq))
                        != std::cmp::Ordering::Less
                });
                if replace {
                    best_by_ts = Some((at, seq, generation));
                }
            }
        }
    }
    Ok(best_by_ts.map(|(_, _, generation)| generation))
}
fn complete(
    state: &AppState,
    run_id: &str,
    conn: &Connection,
    command_id: &str,
    patch: &Value,
    response: &Value,
) -> Result<(), String> {
    let current = state
        .store
        .get("runs", run_id)
        .map_err(|e| e.to_string())?
        .ok_or("Run missing during effect settlement")?;
    let mut migrated = patch.clone();
    if !migrated
        .as_array()
        .into_iter()
        .flatten()
        .any(|change| change["fence"] == true)
    {
        if let Some(generation) = legacy_execution_fence(state, run_id, conn, command_id)? {
            migrated.as_array_mut().ok_or("Control patch is malformed")?.push(json!({"fence":true,"execution_generation":generation,"source":"legacy_saved_authority"}));
            let original: String = conn
                .query_row(
                    "SELECT patch_json FROM control_results WHERE command_id=?1",
                    [command_id],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if serde_json::from_str::<Value>(&original).map_err(|e| e.to_string())? != *patch {
                return Err("Control projection changed during legacy migration".into());
            }
            if conn.execute("UPDATE control_results SET patch_json=?1 WHERE command_id=?2 AND patch_json=?3",params![migrated.to_string(),command_id,original]).map_err(|e|e.to_string())?!=1{return Err("Control projection changed during legacy migration".into());}
        }
    }
    let patch = &migrated;
    let execution = patch
        .as_array()
        .into_iter()
        .flatten()
        .find(|change| change["fence"] == true)
        .and_then(|change| change["execution_generation"].as_u64());
    let superseded = current["control_generation"].as_u64().unwrap_or(0)
        > response["generation"].as_u64().unwrap_or(u64::MAX)
        || execution
            .is_some_and(|generation| current["execution_generation"].as_u64() != Some(generation));
    let conflicts = if superseded {
        Vec::new()
    } else if execution.is_none() {
        vec!["execution_generation_unverified".to_owned()]
    } else {
        publish(state, run_id, patch)?
    };
    let mut settled = response.clone();
    settled["effect_recorded"] = json!(true);
    settled["effect_confirmed"] = json!(
        matches!(
            response["status"].as_str(),
            Some("applied" | "effect_observed")
        ) || response["process_exit_confirmed"] == true
    );
    let phase = if superseded {
        "superseded"
    } else if conflicts.is_empty() {
        "published"
    } else {
        "projection_conflict"
    };
    settled["projection"] = json!({"state":phase,"conflicting_fields":conflicts});
    let status = if phase == "projection_conflict" {
        settled["status"] = json!("partial");
        settled["ok"] = json!(false);
        settled["code"] = json!("run.control.projection_pending");
        settled["detail"] = json!(format!(
            "Control effect was recorded; Run projection still needs reconciliation for: {}",
            conflicts.join(", ")
        ));
        "projection_conflict"
    } else if phase == "superseded" {
        settled["detail"] = json!(
            "A later Run execution or control superseded this projection; the original effect receipt is preserved"
        );
        "superseded"
    } else {
        response["status"].as_str().unwrap_or("unknown")
    };
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute("INSERT INTO control_publications(command_id,phase,conflicts_json,checked_at) VALUES(?1,?2,?3,?4) ON CONFLICT(command_id) DO UPDATE SET phase=excluded.phase,conflicts_json=excluded.conflicts_json,checked_at=excluded.checked_at",params![command_id,phase,json!(conflicts).to_string(),now_secs()]).map_err(|e|e.to_string())?;
    let changed = tx
        .execute(
            "UPDATE control_commands SET status=?1,response_json=?2 WHERE command_id=?3",
            params![status, settled.to_string(), command_id],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("Control admission record disappeared".into());
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}
fn projection_patch(before: &Value, after: &Value) -> Value {
    let keys = before
        .as_object()
        .into_iter()
        .flat_map(|o| o.keys())
        .chain(after.as_object().into_iter().flat_map(|o| o.keys()))
        .collect::<std::collections::BTreeSet<_>>();
    let mut patch=keys.into_iter().filter(|key|before.get(*key)!=after.get(*key)).map(|key|json!({"key":key,"before":before.get(key),"had_before":before.get(key).is_some(),"after":after.get(key),"remove":after.get(key).is_none()})).collect::<Vec<_>>();
    patch.push(json!({"fence":true,"execution_generation":before["execution_generation"]}));
    Value::Array(patch)
}
pub(super) fn finish(
    state: &AppState,
    run_id: &str,
    conn: &Connection,
    command_id: &str,
    before: &Value,
    after: &Value,
    response: &Value,
) -> Result<(), String> {
    let patch = projection_patch(before, after);
    conn.execute("INSERT INTO control_results(command_id,patch_json,response_json,confirmed_at)VALUES(?1,?2,?3,?4) ON CONFLICT(command_id) DO UPDATE SET patch_json=excluded.patch_json,response_json=excluded.response_json,confirmed_at=excluded.confirmed_at",params![command_id,patch.to_string(),response.to_string(),now_secs()]).map_err(|e|e.to_string())?;
    complete(state, run_id, conn, command_id, &patch, response)
}
/// Advance a previously published UNKNOWN/PARTIAL stop using durable exit evidence.
pub(super) fn confirm_pending_stop(
    state: &AppState,
    run_id: &str,
    conn: &Connection,
    command: &str,
    before: &Value,
    after: &Value,
    response: &Value,
) -> Result<(), String> {
    let (action, status): (String, String) = conn
        .query_row(
            "SELECT action,status FROM control_commands WHERE command_id=?1",
            [command],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| e.to_string())?;
    if !matches!(action.as_str(), "stop" | "complete")
        || !matches!(
            status.as_str(),
            "unknown" | "partial" | "projection_conflict"
        )
    {
        return Err("Control command is not a pending stop".into());
    }
    let patch = projection_patch(before, after);
    let transaction = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    transaction.execute("INSERT INTO control_results(command_id,patch_json,response_json,confirmed_at)VALUES(?1,?2,?3,?4) ON CONFLICT(command_id) DO UPDATE SET patch_json=excluded.patch_json,response_json=excluded.response_json,confirmed_at=excluded.confirmed_at",params![command,patch.to_string(),response.to_string(),now_secs()]).map_err(|e|e.to_string())?;
    transaction
        .execute(
            "UPDATE control_commands SET status='unknown' WHERE command_id=?1",
            [command],
        )
        .map_err(|e| e.to_string())?;
    transaction.commit().map_err(|e| e.to_string())?;
    complete(state, run_id, conn, command, &patch, response)
}
pub(super) fn recover(state: &AppState, run_id: &str, conn: &Connection) -> Result<(), String> {
    let mut stmt=conn.prepare("SELECT r.command_id,r.patch_json,r.response_json FROM control_results r JOIN control_commands c USING(command_id) WHERE c.status IN('unknown','projection_conflict') ORDER BY r.confirmed_at,r.command_id").map_err(|e|e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    for (command, patch, response) in rows {
        let patch = serde_json::from_str(&patch).map_err(|e| e.to_string())?;
        let response = serde_json::from_str(&response).map_err(|e| e.to_string())?;
        complete(state, run_id, conn, &command, &patch, &response)?;
    }
    Ok(())
}
pub(super) fn has_result(conn: &Connection, command_id: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT 1 FROM control_results WHERE command_id=?1",
        [command_id],
        |_| Ok(()),
    )
    .optional()
    .map(|v| v.is_some())
    .map_err(|e| e.to_string())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, ()> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(|_| ())
}

fn query_has_row<P: rusqlite::Params>(
    conn: &Connection,
    tables: &[&str],
    sql: &str,
    params: P,
) -> Result<bool, ()> {
    for table in tables {
        if !table_exists(conn, table)? {
            return Ok(false);
        }
    }
    conn.query_row(sql, params, |_| Ok(()))
        .optional()
        .map(|row| row.is_some())
        .map_err(|_| ())
}

fn has_pending_recovery_work(
    state: &AppState,
    run: &Value,
    id: &str,
    freeze_pending: bool,
) -> bool {
    if freeze_pending || run["stopped_lease_cleanup"]["status"] == "pending" {
        return true;
    }
    let db_path = control_dir(state, id).join("control.db");
    match fs::metadata(&db_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
        Ok(_) => {}
    }
    let Ok(conn) = Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return true;
    };
    if super::execution_guard::protected(run)
        && query_has_row(
            &conn,
            &["control_commands"],
            "SELECT 1 FROM control_commands WHERE action='emergency_stop'
        AND status IN ('partial','unknown') LIMIT 1",
            [],
        ) != Ok(false)
    {
        return true;
    }
    if query_has_row(
        &conn,
        &["control_results", "control_commands"],
        "SELECT 1 FROM control_results r JOIN control_commands c USING(command_id)
         WHERE c.status IN('unknown','projection_conflict') LIMIT 1",
        [],
    ) != Ok(false)
    {
        return true;
    }
    if query_has_row(
        &conn,
        &["context_bindings", "context_resources"],
        "SELECT 1 FROM context_bindings b JOIN context_resources r USING(context_id)
         WHERE r.run_id=?1 AND b.status='reserved' AND b.process_epoch<>?2 LIMIT 1",
        params![id, state.chat.epoch()],
    ) != Ok(false)
    {
        return true;
    }
    if query_has_row(
        &conn,
        &["control_commands", "control_results"],
        "SELECT 1 FROM control_commands
         WHERE action='mark_false' AND status='unknown'
           AND command_id NOT IN(SELECT command_id FROM control_results) LIMIT 1",
        [],
    ) != Ok(false)
    {
        return true;
    }
    match table_exists(&conn, "control_commands") {
        Ok(false) => return false,
        Err(()) => return true,
        Ok(true) => {}
    }
    match conn.prepare(
        "SELECT command_id, response_json FROM control_commands
         WHERE action='mark_false' AND status<>'unknown'",
    ) {
        Err(_) => true,
        Ok(mut stmt) => {
            let Ok(rows) = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            }) else {
                return true;
            };
            let existing = state.runs.existing_handle(id);
            for row in rows {
                let Ok((command, resp_raw)) = row else {
                    return true;
                };
                let Ok(resp) = serde_json::from_str::<Value>(&resp_raw) else {
                    return true;
                };
                if resp["effect"]["resolve_scheduled"] == true {
                    let Some(handle) = existing.as_ref() else {
                        return true;
                    };
                    if !handle
                        .settled_false_positives
                        .lock()
                        .unwrap()
                        .contains(&command)
                    {
                        return true;
                    }
                    let key = format!("false-positive-recovery:{command}");
                    if !matches!(state.store.get("run_control_recoveries", &key), Ok(Some(_))) {
                        return true;
                    }
                }
            }
            false
        }
    }
}

pub async fn recover_all(state: &Arc<AppState>) -> Result<(), String> {
    recover_module(state, None).await
}
pub(crate) async fn recover_module(
    state: &Arc<AppState>,
    module: Option<crate::business::Module>,
) -> Result<(), String> {
    let freeze_pending: HashSet<String> = state
        .store
        .list("run_freeze_effects")
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|row| !matches!(row["phase"].as_str(), Some("settled" | "superseded")))
        .filter_map(|row| row["run_id"].as_str().map(str::to_owned))
        .collect();
    for run in state
        .store
        .list("runs")
        .map_err(|error| error.to_string())?
    {
        if module.is_some_and(|module| crate::business::Module::for_run(&run) != module)
            || module.is_none() && !crate::business::run_available(state, &run)
        {
            continue;
        }
        let Some(id) = run["id"]
            .as_str()
            .or_else(|| run["run_id"].as_str())
            .map(str::to_owned)
        else {
            continue;
        };
        let has_freeze = freeze_pending.contains(&id);
        if let Err(error) = recover_run(state, &run, &id, has_freeze).await {
            tracing::warn!(run_id=id,%error,"Run control recovery remains pending");
        }
    }
    if module.is_none() {
        super::runner_stop_recovery::settle(state).await?;
        terminal::reconcile(state)?;
    }
    Ok(())
}
async fn recover_run(
    state: &Arc<AppState>,
    run: &Value,
    id: &str,
    freeze_pending: bool,
) -> Result<(), String> {
    let check_state = state.clone();
    let check_run = run.clone();
    let check_id = id.to_owned();
    let needs_recovery = tokio::task::spawn_blocking(move || {
        has_pending_recovery_work(&check_state, &check_run, &check_id, freeze_pending)
    })
    .await
    .map_err(|error| error.to_string())?;
    if !needs_recovery {
        return Ok(());
    }
    let handle = {
        let s = state.clone();
        let run_id = id.to_owned();
        tokio::task::spawn_blocking(move || s.runs.handle(&s, &run_id))
            .await
            .map_err(|error| error.to_string())?
    };
    let _guard = handle.delete_lock.lock().await;
    let claimed = {
        let s = state.clone();
        let run_id = id.to_owned();
        tokio::task::spawn_blocking(move || -> Result<bool, String> {
            if !s.runs.claim_execution_owner(&s, &run_id)? {
                return Ok(false);
            }
            let conn = open_control_db(&s, &run_id)?;
            recover(&s, &run_id, &conn)?;
            Ok(true)
        })
        .await
        .map_err(|error| error.to_string())??
    };
    if !claimed {
        return Ok(());
    }
    let live = state
        .store
        .get("runs", id)
        .map_err(|e| e.to_string())?
        .ok_or("Run disappeared during recovery")?;
    if super::execution_guard::recover_stop(state, id, &live).await? {
        return Ok(());
    }
    if freeze_pending && !super::runner_stop_recovery::pending(state, &live)? {
        super::control_freeze::recover_pending(state, id).await?;
    }
    let s = state.clone();
    let run_id = id.to_owned();
    let task_handle = handle.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let handle = task_handle;
        let conn = open_control_db(&s, &run_id)?;
        super::control_freeze::recover_stopped(&s, &run_id)?;
        let pending = {
            let mut rows = conn
                .prepare(
                    "SELECT command_id,payload_json,generation FROM control_commands
                     WHERE action='mark_false' AND status='unknown'
                       AND command_id NOT IN(SELECT command_id FROM control_results)
                     ORDER BY created_at,command_id",
                )
                .map_err(|e| e.to_string())?;
            rows.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
        };
        for (command, raw, generation) in pending {
            let body: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
            let flag = body["flag"]
                .as_str()
                .or_else(|| body["text"].as_str())
                .unwrap_or("")
                .trim();
            if flag.is_empty() {
                continue;
            }
            let before = s
                .store
                .get("runs", &run_id)
                .map_err(|e| e.to_string())?
                .ok_or("False-positive Run missing")?;
            let reopened = graph::SharedGraph::open(&s, &run_id)?.reopen_after_false_positive(
                "system",
                flag,
                "marked_false",
            )?;
            let mut after = before.clone();
            let flags = after["flags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|value| value.as_str() != Some(flag))
                .cloned()
                .collect::<Vec<_>>();
            after["flag"] = flags.first().cloned().unwrap_or(Value::Null);
            after["flags"] = json!(flags);
            after["solved"] = json!(false);
            after["control_generation"] = json!(
                after["control_generation"]
                    .as_i64()
                    .unwrap_or(0)
                    .max(generation)
            );
            let response = json!({
                "ok": true,
                "command_id": command,
                "status": "applied",
                "generation": generation,
                "effect": {
                    "kind": "flag_invalidated",
                    "recovery": "full_coordinator",
                    "reopened": reopened,
                    "resolve_scheduled": true
                }
            });
            finish(&s, &run_id, &conn, &command, &before, &after, &response)?;
        }
        let mut rows = conn
            .prepare(
                "SELECT command_id,response_json FROM control_commands
                 WHERE action='mark_false' AND status<>'unknown'",
            )
            .map_err(|error| error.to_string())?;
        let records = rows
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        for (command, response) in records {
            let response: Value =
                serde_json::from_str(&response).map_err(|error| error.to_string())?;
            if response["effect"]["resolve_scheduled"] == true {
                let raw = conn
                    .query_row(
                        "SELECT payload_json FROM control_commands WHERE command_id=?1",
                        [&command],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(|e| e.to_string())?;
                let body: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
                let flag = body["flag"]
                    .as_str()
                    .or_else(|| body["text"].as_str())
                    .unwrap_or("")
                    .trim();
                handle.rejected_flags.lock().unwrap().insert(flag.into());
                let already_settled = handle
                    .settled_false_positives
                    .lock()
                    .unwrap()
                    .contains(&command);
                if !already_settled {
                    publish_false_positive(&s, &run_id, &command, flag, "system")?;
                }
                super::control_recovery::enqueue_recovery(&s, &run_id, &command)?;
                if !already_settled {
                    handle
                        .settled_false_positives
                        .lock()
                        .unwrap()
                        .insert(command);
                }
            }
        }
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?
}

pub(super) fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            if !state.admission.draining() {
                if let Err(error) = recover_all(&state).await {
                    tracing::warn!(%error,"Control projection recovery remains pending");
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

pub(super) fn publish_false_positive(
    state: &AppState,
    run: &str,
    command: &str,
    flag: &str,
    actor: &str,
) -> Result<(), String> {
    if persisted_event_iter(state, run)?.any(|event| {
        event["event_type"] == "blackboard.delta"
            && event["payload"]["kind"] == "flag_invalidated"
            && event["payload"]["command_id"] == command
    }) {
        return Ok(());
    }
    let event = emit_event(
        state,
        run,
        None,
        "blackboard.delta",
        json!({"kind":"flag_invalidated","flag":flag,"actor":actor,"command_id":command}),
    );
    if event["persisted"] == false {
        return Err("False-positive event persistence failed".into());
    }
    Ok(())
}
