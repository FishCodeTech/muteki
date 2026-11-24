//! Durable operator context carried by Run control commands.

use super::{AppState, control_dir, now_secs, open_control_db};
use super::graph::SharedGraph;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::fs;

pub(super) fn init(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS context_resources (
            context_id TEXT PRIMARY KEY,
            run_id TEXT NOT NULL,
            source_command_id TEXT NOT NULL UNIQUE,
            request_json TEXT NOT NULL,
            resource_json TEXT NOT NULL,
            standing INTEGER NOT NULL,
            max_bindings INTEGER,
            bound_count INTEGER NOT NULL DEFAULT 0,
            expires_at REAL,
            expired_at REAL
        );
        CREATE TABLE IF NOT EXISTS context_bindings (
            context_id TEXT NOT NULL,
            worker_id TEXT NOT NULL,
            process_epoch TEXT NOT NULL,
            status TEXT NOT NULL,
            reserved_at REAL NOT NULL,
            bound_at REAL,
            PRIMARY KEY (context_id, worker_id)
        );
        CREATE TABLE IF NOT EXISTS context_events (
            event_id TEXT PRIMARY KEY,
            run_id TEXT NOT NULL,
            context_id TEXT NOT NULL,
            operation TEXT NOT NULL,
            source_command_id TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            created_at REAL NOT NULL
        );",
    ).map_err(|error| error.to_string())
}

fn event(
    conn: &Connection, run_id: &str, context_id: &str, operation: &str,
    command_id: &str, payload: &Value,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO context_events
         (event_id,run_id,context_id,operation,source_command_id,payload_json,created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![format!("ctxe-{}", uuid::Uuid::new_v4().simple()), run_id,
            context_id, operation, command_id, payload.to_string(), now_secs()],
    ).map_err(|error| error.to_string())?;
    Ok(())
}

fn transaction<T>(conn: &Connection, apply: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    conn.execute_batch("BEGIN IMMEDIATE").map_err(|error| error.to_string())?;
    match apply() {
        Ok(value) => {
            conn.execute_batch("COMMIT").map_err(|error| error.to_string())?;
            Ok(value)
        }
        Err(error) => { let _ = conn.execute_batch("ROLLBACK"); Err(error) }
    }
}

pub(super) fn create(
    conn: &Connection, run_id: &str, command_id: &str, target: &str,
    delivery_scope: &str, body: &Value,
) -> Result<Value, String> {
    transaction(conn, || create_inner(conn, run_id, command_id, target,
        delivery_scope, "add_context", body))
}

pub(super) fn create_guidance(
    conn: &Connection, run_id: &str, command_id: &str, target: &str,
    delivery_scope: &str, action: &str, body: &Value,
) -> Result<Option<Value>, String> {
    let url = body["url"].as_str().or_else(|| body["target_url"].as_str()).unwrap_or("");
    let content = body["text"].as_str().or_else(||body["hint"].as_str()).unwrap_or(url).trim();
    if content.is_empty() { return Ok(None); }
    let kind = if action == "redirect" { "endpoint" }
        else if content.starts_with("secret://") { "secret_ref" }
        else if action == "focus" { "objective" } else { "clue" };
    let standing = body["standing"].as_bool().unwrap_or(true);
    let planner_managed = body["planner_managed"].as_bool().unwrap_or(standing);
    let mut raw = json!({"kind":kind,"content":content,
        "standing":standing,"planner_managed":planner_managed,"context_id":body["directive_id"]});
    if !body["max_bindings"].is_null() {
        raw["max_bindings"] = body["max_bindings"].clone();
    }
    if !body["ttl_s"].is_null() {
        raw["ttl_s"] = body["ttl_s"].clone();
    }
    let wrapped = json!({"context":raw});
    transaction(conn, || create_inner(conn,run_id,command_id,target,
        delivery_scope,action,&wrapped)).map(Some)
}

fn create_inner(
    conn: &Connection, run_id: &str, command_id: &str, target: &str,
    delivery_scope: &str, action: &str, body: &Value,
) -> Result<Value, String> {
    let request = json!({"target":target,"action":action,"context":body["context"]});
    if let Some((prior_request, resource)) = conn.query_row(
        "SELECT request_json,resource_json FROM context_resources WHERE source_command_id=?1",
        [command_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).optional().map_err(|error| error.to_string())? {
        if serde_json::from_str::<Value>(&prior_request).ok().as_ref() != Some(&request) {
            return Err("command_id was used with different context content".to_owned());
        }
        return serde_json::from_str(&resource).map_err(|error| error.to_string());
    }
    let raw = match &body["context"] {
        Value::String(content) => json!({"content":content}),
        Value::Object(_) => body["context"].clone(),
        _ => return Err("add_context payload.context must be an object or string".to_owned()),
    };
    let content = raw["content"].as_str().unwrap_or("").trim();
    if content.is_empty() {
        return Err("add_context context.content is required".to_owned());
    }
    let kind = raw["kind"].as_str().unwrap_or("clue");
    if !matches!(kind, "clue" | "constraint" | "endpoint" | "objective" | "secret_ref" | "operator_note") {
        return Err(format!("unknown context kind {kind:?}"));
    }
    if kind == "secret_ref" && !content.starts_with("secret://") {
        return Err("secret_ref context must contain a secret:// reference".to_owned());
    }
    let standing = raw["standing"].as_bool().unwrap_or(false);
    let max_bindings = if standing || raw["planner_managed"]==true {None}else{raw["max_bindings"].as_u64().or(Some(1))};
    if max_bindings == Some(0) {
        return Err("context max_bindings must be positive".to_owned());
    }
    let created_at = now_secs();
    let expires_at = raw["expires_at"].as_f64().or_else(||
        raw["ttl_s"].as_f64().map(|ttl| created_at + ttl.max(0.0)));
    let context_id = raw["context_id"].as_str().map(str::to_owned).unwrap_or_else(||format!("CTX-{}", uuid::Uuid::new_v4().simple()));
    let taint = if kind == "secret_ref" || content.starts_with("secret://") {
        "secret_reference"
    } else { "operator_unverified" };
    let resource = json!({"context_id":context_id,"run_id":run_id,"kind":kind,
        "content":content,"scope":delivery_scope,"taint":taint,"standing":standing,
        "max_bindings":max_bindings,"created_at":created_at,"expires_at":expires_at,
        "metadata":{"source_command_id":command_id,"action":action,
            "command_scope":target}});
    conn.execute(
        "INSERT INTO context_resources
         (context_id,run_id,source_command_id,request_json,resource_json,standing,max_bindings,expires_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        params![context_id,run_id,command_id,request.to_string(),resource.to_string(),
            if standing { 1 } else { 0 },max_bindings.map(|v| v as i64),expires_at],
    ).map_err(|error| error.to_string())?;
    event(conn,run_id,&context_id,"created",command_id,&resource)?;
    Ok(resource)
}

pub(super) fn resource_for_command(
    conn: &Connection, command_id: &str,
) -> Result<Option<Value>, String> {
    let raw: Option<String> = conn.query_row(
        "SELECT resource_json FROM context_resources WHERE source_command_id=?1",
        [command_id], |row| row.get(0),
    ).optional().map_err(|error| error.to_string())?;
    raw.map(|value| serde_json::from_str(&value).map_err(|error| error.to_string()))
        .transpose()
}

pub(super) fn active_continuations(
    state: &AppState, run_id: &str,
) -> Result<Vec<Value>, String> {
    let conn = open_control_db(state,run_id)?;
    let mut stmt = conn.prepare(
        "SELECT resource_json FROM context_resources
         WHERE run_id=?1 AND expired_at IS NULL AND (expires_at IS NULL OR expires_at>?2)
           AND (max_bindings IS NULL OR bound_count<max_bindings)
         ORDER BY rowid",
    ).map_err(|error| error.to_string())?;
    let rows = stmt.query_map(params![run_id,now_secs()], |row| row.get::<_,String>(0))
        .map_err(|error| error.to_string())?;
    Ok(rows.filter_map(|row| row.ok().and_then(|raw|
        serde_json::from_str::<Value>(&raw).ok()))
        .filter(|resource| resource["scope"].as_str()
            .is_some_and(|scope| scope.starts_with("intent:I-control-")))
        .collect())
}

pub(super) fn propose_continuation(graph: &SharedGraph, resource: &Value) -> bool {
    let Some(intent_id) = resource["scope"].as_str()
        .and_then(|scope| scope.strip_prefix("intent:")) else { return false; };
    let command_id = resource["metadata"]["source_command_id"].as_str().unwrap_or("");
    if !intent_id.starts_with("I-control-") || command_id.is_empty() {
        return false;
    }
    if graph.all_intents().iter().any(|intent| intent["intent_id"] == intent_id) {
        return true;
    }
    graph.propose_intent(
        "operator", intent_id,
        &format!("Continue the blocked execution using operator-provided context for control command {command_id}."),
        &json!({"source":"operator_continuation","source_command_id":command_id,
            "action":resource["metadata"]["action"],
            "requested_scope":resource["metadata"]["command_scope"],
            "priority":"high","worker_class":"shell_agent"}),
        None,
    ) > 0
}

pub(super) fn expire(
    conn: &Connection, run_id: &str, command_id: &str, context_id: &str, actor: &str, reason: &str,
) -> Result<bool, String> {
    transaction(conn, || expire_inner(conn,run_id,command_id,context_id,actor,reason))
}

fn expire_inner(
    conn: &Connection, run_id: &str, command_id: &str, context_id: &str, actor: &str, reason: &str,
) -> Result<bool, String> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM context_resources WHERE run_id=?1 AND context_id=?2)",
        params![run_id,context_id], |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    if !exists { return Err(format!("unknown context_id {context_id:?}")); }
    let at = now_secs();
    let changed = conn.execute(
        "UPDATE context_resources SET expired_at=?1
         WHERE run_id=?2 AND context_id=?3 AND expired_at IS NULL",
        params![at,run_id,context_id],
    ).map_err(|error| error.to_string())?;
    if changed != 0 {
        event(conn,run_id,context_id,"expired",command_id,
            &json!({"actor":actor,"reason":reason,"expired_at":at}))?;
    }
    Ok(changed != 0)
}

pub(super) fn clear_standing(
    state: &AppState, conn: &Connection, run_id: &str,
    command_id: &str, text: &str, actor: &str, reason: &str,
) -> Result<Vec<Value>, String> {
    transaction(conn, || clear_standing_inner(state,conn,run_id,command_id,text,actor,reason))
}

fn clear_standing_inner(
    state: &AppState, conn: &Connection, run_id: &str,
    command_id: &str, text: &str, actor: &str, reason: &str,
) -> Result<Vec<Value>, String> {
    let at = now_secs();
    let mut stmt = conn.prepare(
        "SELECT resource_json FROM context_resources
         WHERE run_id=?1 AND standing=1 AND expired_at IS NULL
         AND (expires_at IS NULL OR expires_at>?2)",
    ).map_err(|error| error.to_string())?;
    let resources: Vec<Value> = stmt.query_map(params![run_id,at], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .filter_map(|row| row.ok().and_then(|raw| serde_json::from_str(&raw).ok()))
        .filter(|resource: &Value| {
            let content = resource["content"].as_str().unwrap_or("");
            exact_context_text_matches(state,run_id,content,text)
        })
        .collect();
    drop(stmt);
    for resource in &resources {
        let id = resource["context_id"].as_str().unwrap_or("");
        expire_inner(conn,run_id,command_id,id,actor,reason)?;
    }
    Ok(resources)
}

fn exact_context_text_matches(
    state: &AppState, run_id: &str, candidate: &str, exact: &str,
) -> bool {
    if exact.is_empty() { return true; }
    let left = if candidate.starts_with("secret://") {
        match resolve_secret(state,run_id,candidate) {
            Ok(value) => value,
            Err(_) => return false,
        }
    } else { candidate.to_owned() };
    let right = if exact.starts_with("secret://") {
        match resolve_secret(state,run_id,exact) {
            Ok(value) => value,
            Err(_) => return false,
        }
    } else { exact.to_owned() };
    left == right
}

fn scope_matches(scope: &str, run_id: &str, worker_id: &str, engine: &str, intent: &str) -> bool {
    scope == "global" || scope == "coordinator" || scope == format!("run:{run_id}")
        || scope == format!("challenge:{run_id}")
        || scope == format!("worker:{worker_id}") || scope == format!("solver:{worker_id}")
        || scope == format!("engine:{engine}") || scope == format!("intent:{intent}")
}

/// Bind matching context while constructing the one Worker prompt that carries it.
pub(super) fn bind_for_worker(
    state: &AppState, run_id: &str, worker_id: &str, engine: &str, intent: &str,
) -> Result<String, String> {
    let conn = open_control_db(state, run_id)?;
    transaction(&conn, || {
        let at = now_secs();
        let epoch = state.chat.epoch();
        let mut stmt = conn.prepare(
            "SELECT context_id,resource_json,max_bindings,bound_count FROM context_resources
             WHERE run_id=?1 AND expired_at IS NULL AND (expires_at IS NULL OR expires_at>?2)
             ORDER BY rowid",
        ).map_err(|error| error.to_string())?;
        let rows: Vec<(String,Value,Option<i64>,i64)> = stmt.query_map(params![run_id,at], |row| {
            Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,row.get::<_, i64>(3)?))
        }).map_err(|error| error.to_string())?
          .filter_map(|row| row.ok().and_then(|(id,raw,max,count)|
              serde_json::from_str(&raw).ok().map(|resource| (id,resource,max,count))))
          .collect();
        drop(stmt);
        let mut lines = Vec::new();
        for (id,resource,max,count) in rows {
            let graph = SharedGraph::open(state,run_id)?;
            if graph.directives().iter().any(|d|d["directive_id"]==id && matches!(d["status"].as_str(),Some("expired"|"consumed"))){continue;}
            let scope = resource["scope"].as_str().unwrap_or("");
            if !scope_matches(scope,run_id,worker_id,engine,intent) { continue; }
            if max.is_some_and(|limit| count >= limit) { continue; }
            let inserted = if max.is_none() {
                // Unlimited standing context may be delivered again to the same
                // seat after a new Worker process or service generation starts.
                conn.execute(
                    "INSERT INTO context_bindings
                     (context_id,worker_id,process_epoch,status,reserved_at)
                     VALUES (?1,?2,?3,'reserved',?4)
                     ON CONFLICT(context_id,worker_id) DO UPDATE SET
                       process_epoch=excluded.process_epoch,status='reserved',
                       reserved_at=excluded.reserved_at,bound_at=NULL
                     WHERE context_bindings.status!='reserved'",
                    params![id,worker_id,epoch,at],
                )
            } else {
                conn.execute(
                    "INSERT OR IGNORE INTO context_bindings
                     (context_id,worker_id,process_epoch,status,reserved_at)
                     VALUES (?1,?2,?3,'reserved',?4)",
                    params![id,worker_id,epoch,at],
                )
            }.map_err(|error| error.to_string())?;
            if inserted == 0 { continue; }
            conn.execute(
                "UPDATE context_resources SET bound_count=bound_count+1 WHERE context_id=?1",
                [&id],
            ).map_err(|error| error.to_string())?;
            event(&conn,run_id,&id,"reserved",resource["metadata"]["source_command_id"].as_str().unwrap_or(""),
                &json!({"worker_id":worker_id,"reserved_at":at}))?;
            if resource["metadata"]["action"]!="add_context" && resource["taint"]!="secret_reference" {continue;}
            lines.push(format!("- [{}] {}",resource["kind"].as_str().unwrap_or("clue"),
                resource["content"].as_str().unwrap_or("")));
        }
        Ok::<_,String>(if lines.is_empty() { String::new() }
            else { format!("\n## Operator Context\n{}\n",lines.join("\n")) })
    })
}

pub(super) fn recover_prior_epoch(
    conn: &Connection, run_id: &str, process_epoch: &str,
) -> Result<usize, String> {
    transaction(conn, || {
        let mut stmt = conn.prepare(
            "SELECT b.context_id,b.worker_id FROM context_bindings b
             JOIN context_resources r USING (context_id)
             WHERE r.run_id=?1 AND b.status='reserved' AND b.process_epoch<>?2",
        ).map_err(|error| error.to_string())?;
        let rows: Vec<(String,String)> = stmt.query_map(params![run_id,process_epoch], |row|
            Ok((row.get(0)?,row.get(1)?)))
            .map_err(|error| error.to_string())?.filter_map(Result::ok).collect();
        drop(stmt);
        for (context_id,worker_id) in &rows {
            conn.execute(
                "UPDATE context_bindings SET status='delivery_unknown'
                 WHERE context_id=?1 AND worker_id=?2 AND status='reserved'",
                params![context_id,worker_id],
            ).map_err(|error| error.to_string())?;
            event(conn,run_id,context_id,"delivery_unknown","runtime-recovery",
                &json!({"worker_id":worker_id,"recovered_at":now_secs()}))?;
        }
        Ok(rows.len())
    })
}

/// Commit the reservation after the Worker process has actually started.
pub(super) fn commit_for_worker(
    state: &AppState, run_id: &str, worker_id: &str,
) -> Result<bool, String> {
    let conn = open_control_db(state,run_id)?;
    transaction(&conn, || {
        let mut stmt = conn.prepare(
            "SELECT r.context_id,r.source_command_id,r.expired_at,r.expires_at
             FROM context_resources r JOIN context_bindings b USING (context_id)
             WHERE r.run_id=?1 AND b.worker_id=?2 AND b.status='reserved'
               AND b.process_epoch=?3",
        ).map_err(|error| error.to_string())?;
        let rows: Vec<(String,String,Option<f64>,Option<f64>)> = stmt.query_map(
            params![run_id,worker_id,state.chat.epoch()], |row|
            Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).map_err(|error| error.to_string())?.filter_map(Result::ok).collect();
        drop(stmt);
        let at = now_secs();
        let mut all_bound = true;
        for (context_id,command_id,expired_at,expires_at) in rows {
            let expired = expired_at.is_some() || expires_at.is_some_and(|at_expire| at_expire <= at);
            let status = if expired { "delivery_unknown" } else { "bound" };
            if expired { all_bound = false; }
            conn.execute(
                "UPDATE context_bindings SET status=?1,bound_at=?2
                 WHERE context_id=?3 AND worker_id=?4 AND status='reserved'",
                params![status,at,context_id,worker_id],
            ).map_err(|error| error.to_string())?;
            event(&conn,run_id,&context_id,status,&command_id,
                &json!({"worker_id":worker_id,"at":at}))?;
        }
        Ok(all_bound)
    })
}

pub(super) fn release_for_worker(
    state: &AppState, run_id: &str, worker_id: &str,
) -> Result<usize, String> {
    let conn = open_control_db(state,run_id)?;
    transaction(&conn, || {
        let mut stmt = conn.prepare(
            "SELECT r.context_id,r.source_command_id FROM context_resources r
             JOIN context_bindings b USING (context_id)
             WHERE r.run_id=?1 AND b.worker_id=?2 AND b.status='reserved'
               AND b.process_epoch=?3",
        ).map_err(|error| error.to_string())?;
        let rows: Vec<(String,String)> = stmt.query_map(
            params![run_id,worker_id,state.chat.epoch()], |row|
            Ok((row.get(0)?,row.get(1)?)),
        ).map_err(|error| error.to_string())?.filter_map(Result::ok).collect();
        drop(stmt);
        for (context_id,command_id) in &rows {
            conn.execute("DELETE FROM context_bindings WHERE context_id=?1 AND worker_id=?2
                AND status='reserved'",params![context_id,worker_id])
                .map_err(|error| error.to_string())?;
            conn.execute("UPDATE context_resources SET bound_count=bound_count-1
                WHERE context_id=?1 AND bound_count>0",[context_id])
                .map_err(|error| error.to_string())?;
            event(&conn,run_id,context_id,"released",command_id,
                &json!({"worker_id":worker_id,"released_at":now_secs()}))?;
        }
        Ok(rows.len())
    })
}

pub(super) fn unknown_for_worker(
    state: &AppState, run_id: &str, worker_id: &str,
) -> Result<usize, String> {
    let conn = open_control_db(state,run_id)?;
    transaction(&conn, || {
        let mut stmt = conn.prepare(
            "SELECT r.context_id,r.source_command_id FROM context_resources r
             JOIN context_bindings b USING (context_id)
             WHERE r.run_id=?1 AND b.worker_id=?2 AND b.status='reserved'
               AND b.process_epoch=?3",
        ).map_err(|error| error.to_string())?;
        let rows: Vec<(String,String)> = stmt.query_map(
            params![run_id,worker_id,state.chat.epoch()], |row|
            Ok((row.get(0)?,row.get(1)?)),
        ).map_err(|error| error.to_string())?.filter_map(Result::ok).collect();
        drop(stmt);
        for (context_id,command_id) in &rows {
            conn.execute("UPDATE context_bindings SET status='delivery_unknown'
                WHERE context_id=?1 AND worker_id=?2 AND status='reserved'",
                params![context_id,worker_id]).map_err(|error| error.to_string())?;
            event(&conn,run_id,context_id,"delivery_unknown",command_id,
                &json!({"worker_id":worker_id,"at":now_secs()}))?;
        }
        Ok(rows.len())
    })
}

fn resolve_secret(state: &AppState, run_id: &str, reference: &str) -> Result<String, String> {
    let id = reference.strip_prefix("secret://")
        .ok_or_else(|| "invalid operator secret reference".to_owned())?;
    if id.len() < 3 || id.len() > 128 || !id.bytes().all(|byte|
        byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
        return Err("invalid operator secret reference".to_owned());
    }
    fs::read_to_string(control_dir(state,run_id).join("secrets").join(format!("{id}.secret")))
        .map_err(|_| format!("operator secret reference is unavailable: {reference}"))
}

/// Replace only the secret references from contexts bound to this Worker.
/// The returned text is transient and must not enter Run events or the control journal.
pub(super) fn materialize_for_worker(
    state: &AppState, run_id: &str, worker_id: &str, safe_prompt: &str,
) -> Result<(String, Vec<String>), String> {
    let conn = open_control_db(state,run_id)?;
    let mut stmt = conn.prepare(
        "SELECT resource_json,expired_at,expires_at FROM context_resources
         JOIN context_bindings USING (context_id)
         WHERE run_id=?1 AND worker_id=?2
           AND context_bindings.status='reserved'
           AND context_bindings.process_epoch=?3",
    ).map_err(|error| error.to_string())?;
    let rows: Vec<(String,Option<f64>,Option<f64>)> = stmt.query_map(
        params![run_id,worker_id,state.chat.epoch()],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).map_err(|error| error.to_string())?.filter_map(Result::ok).collect();
    let mut prompt = safe_prompt.to_owned();
    let mut secrets = Vec::new();
    for (raw,expired_at,expires_at) in rows {
        let resource: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
        if expired_at.is_some() || expires_at.is_some_and(|at| at <= now_secs()) {
            return Err("operator context expired before Worker delivery".to_owned());
        }
        let reference = resource["content"].as_str().unwrap_or("");
        if !reference.starts_with("secret://") { continue; }
        let safe_line = format!("- [{}] {reference}",
            resource["kind"].as_str().unwrap_or("secret_ref"));
        let secret = resolve_secret(state,run_id,reference)?;
        if !prompt.contains(&safe_line) {
            return Err("bound operator secret context is absent from Worker prompt".to_owned());
        }
        prompt = prompt.replacen(&safe_line,
            &format!("- [{}] {secret}",resource["kind"].as_str().unwrap_or("secret_ref")),1);
        secrets.push(secret);
    }
    Ok((prompt,secrets))
}
