//! Run commands backed by the existing Run lifecycle and control journal.
//! Command identity is compared as stored JSON without a digest.
use super::*;
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;

const RUN_KEYS: &str = "run_command_keys";
const BOUND: &str = "bound_runs";
const BOUND_IDS: &str = "bound_run_ids";
const EFFECTS: &str = "platform_effect_receipts";
const OUTBOX: &str = "platform_outbox_live";
const INPUTS: &str = "platform_outbox_inputs";

fn effect_lock(state: &AppState, outbox_id: &str) -> Result<Option<File>, Failure> {
    let root = state_root(state)?.join("control/platform/run-effects");
    std::fs::create_dir_all(&root).map_err(storage)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(root.join(format!("{outbox_id}.lock"))).map_err(storage)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(storage(error)),
    }
}

pub(super) fn supports(kind: &str) -> bool {
    matches!(
        kind,
        "run.create"
            | "run.start"
            | "run.resolve"
            | "run.control"
            | "run.pause"
            | "run.resume"
            | "run.stop"
            | "run.freeze"
            | "run.thaw"
            | "run.complete"
            | "run.hint"
            | "run.answer_decision"
            | "run.add_context"
            | "run.spawn_worker"
            | "run.cancel_worker"
            | "run.operator_directive"
            | "run.rail.update"
            | "run.rail.delete"
            | "run.folder.create"
            | "run.folder.update"
            | "run.folder.delete"
            | "run.archive"
            | "run.purge"
            | "run.open"
            | "run.artifact.attach"
    )
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("").trim()
}
fn target(envelope: &Value) -> String {
    let id = field(&envelope["payload"], "run_id");
    if id.is_empty() {
        field(envelope, "aggregate_id").to_owned()
    } else {
        id.to_owned()
    }
}
fn replay(stored: &Value) -> Response {
    let mut receipt = stored["response"]["receipt"].clone();
    receipt["deduplicated"] = json!(true);
    let status = StatusCode::from_u16(stored["status"].as_u64().unwrap_or(200) as u16)
        .unwrap_or(StatusCode::OK);
    (status, Json(json!({"receipt":receipt}))).into_response()
}
fn receipt(
    command_id: &str,
    run_id: &str,
    state: &str,
    output: Value,
    effect_id: Option<&str>,
    error: Value,
) -> Value {
    json!({"schema_version":1,"command_id":command_id,"receipt_id":new_id("rcpt"),
        "state":state,"run_id":if run_id.is_empty(){Value::Null}else{json!(run_id)},
        "aggregate":{"type":"run","id":run_id},"event_cursor":null,
        "deduplicated":false,"submitted_at":now(),"completed_at":null,
        "next":["receipt","snapshot","events","wait"],"output":output,
        "effect_ids":effect_id.map(|id|json!([id])).unwrap_or_else(||json!([])),"error":error})
}
fn effect_records(envelope: &Value, effect_id: &str, outbox_id: &str) -> (Value, Value) {
    let command_id = field(envelope, "command_id");
    let aggregate_id = field(envelope, "aggregate_id");
    let kind = field(envelope, "command_type");
    let effect = json!({"effect_id":effect_id,"command_id":command_id,
        "acceptance_receipt_id":"","domain":"run","state":"running",
        "aggregate":{"type":"run","id":aggregate_id},"correlation_id":command_id,
        "outbox_id":outbox_id,"destination":"run.gateway","idempotency_key":command_id,
        "attempts":1,"max_attempts":8,"retry_after":null,"object_links":{},
        "output":{},"error":null,"created_at":now(),"updated_at":now(),"completed_at":null});
    let outbox = json!({"outbox_id":outbox_id,"command_id":command_id,"effect_id":effect_id,
        "correlation_id":command_id,"idempotency_key":command_id,"event_id":null,
        "aggregate_type":"run","aggregate_id":aggregate_id,"event_type":kind,
        "destination":"run.gateway","status":"processing","attempts":1,
        "next_attempt_at":null,"payload":{"command_type":kind},
        "created_at":now(),"updated_at":now(),"delivered_at":null,"last_error":null});
    (effect, outbox)
}
fn event(
    state: &AppState,
    run_id: &str,
    envelope: &Value,
    phase: &str,
    action: Option<&str>,
) {
    if run_id.is_empty() || state.store.get("runs", run_id).ok().flatten().is_none() {
        return;
    }
    let kind = field(envelope, "command_type");
    let command_id = field(envelope, "command_id");
    let event_type = if kind == "run.create" {
        "run.created"
    } else if kind.starts_with("run.rail.")
        || kind.starts_with("run.folder.")
        || matches!(
            kind,
            "run.archive" | "run.purge" | "run.open" | "run.artifact.attach"
        )
    {
        if phase == "accepted" {
            "run.operation.accepted"
        } else if phase == "completed" {
            "run.operation.completed"
        } else {
            "run.operation.failed"
        }
    } else if phase == "accepted" {
        "run.command.accepted"
    } else if phase == "completed" {
        "run.command.completed"
    } else {
        "run.command.failed"
    };
    let causation_id = field(&envelope["payload"], "causation_id");
    let correlation_id = field(&envelope["payload"], "correlation_id");
    let actor_id = field(&envelope["actor"], "id");
    let metadata = json!({
        "producer":"builtin.single-security-task",
        "actor_id":if actor_id.is_empty(){"system"}else{actor_id},
        "command_id":command_id,
        "causation_id":if causation_id.is_empty(){command_id}else{causation_id},
        "correlation_id":if correlation_id.is_empty(){command_id}else{correlation_id},
        "idempotency_key":envelope["idempotency_key"],
    });
    crate::runs::emit_event_with_metadata(
        state,
        run_id,
        None,
        event_type,
        json!({"command_type":kind,"command_id":command_id,"run_id":run_id,
            "action":action,"state":phase}),
        Some(&metadata),
    );
}

fn event_once(state: &AppState, run_id: &str, envelope: &Value, phase: &str) {
    let command_id = field(envelope, "command_id");
    let path = crate::runs::session_dir(state, run_id).join("events.jsonl");
    let recorded = std::fs::read_to_string(path).ok().is_some_and(|content| {
        content.lines().any(|line| {
            serde_json::from_str::<Value>(line).ok().is_some_and(|row| {
                row["payload"]["command_id"] == command_id
                    && row["payload"]["state"] == phase
            })
        })
    });
    if !recorded {
        event(state, run_id, envelope, phase, None);
    }
}
fn error_from_response(status: StatusCode, body: &Value) -> Value {
    if status.is_success() {
        return Value::Null;
    }
    let code = field(&body["error"], "code");
    let message = body["error"]["message"]
        .as_str()
        .or_else(|| body["detail"].as_str())
        .unwrap_or("Run command failed");
    let category = body["error"]["category"].as_str().unwrap_or(match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "validation",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT => "conflict",
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "permission",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit",
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => "timeout",
        StatusCode::BAD_GATEWAY => "runtime",
        _ => "state",
    });
    json!({"schema_version":1,"code":if code.is_empty(){"run.command.rejected"}else{code},
        "message":message,"category":category,
        "recovery_hint":body["error"]["recovery_hint"].as_str().unwrap_or(""),
        "correlation_id":body["error"]["correlation_id"],
        "retryable":body["error"]["retryable"].as_bool().unwrap_or(false),
        "detail":body["error"]["detail"].as_object().cloned().unwrap_or_default()})
}
async fn response_json(response: Response) -> Result<(StatusCode, Value), Failure> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(storage)?;
    let body: Value = serde_json::from_slice(&bytes).map_err(storage)?;
    Ok((status, body))
}

async fn create(
    state: &Arc<AppState>,
    envelope: &Value,
) -> Result<(StatusCode, Value, String), Failure> {
    let payload = &envelope["payload"];
    if payload["legacy"] == true {
        let run_id = target(envelope);
        let body = if run_id.is_empty() {
            json!({"mode":payload["mode"]})
        } else {
            json!({"run_id":run_id,"mode":payload["mode"]})
        };
        let (status, result) =
            response_json(crate::runs::create_run(State(state.clone()), Some(Json(body))).await)
                .await?;
        let id = field(&result, "run_id").to_owned();
        return Ok((status, result, id));
    }
    let key = field(payload, "binding_key");
    let key = if key.is_empty() {
        field(envelope, "aggregate_id")
    } else {
        key
    };
    if key.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "run.binding_key_required",
            "run.create requires payload.binding_key",
        ));
    }
    let task_id = field(payload, "task_id");
    let revision = payload["task_revision"].as_u64().unwrap_or(1);
    let requested = field(payload, "run_id");
    let bound = state.store.get(BOUND, key).map_err(storage)?;
    let row = if let Some(row) = bound {
        row
    } else {
        if !requested.is_empty() && !requested.starts_with("crun_") {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "run.binding.id_namespace",
                "caller-assigned run_id must start with 'crun_'",
            ));
        }
        let id = if requested.is_empty() {
            format!("crun_{}", &uuid::Uuid::new_v4().simple().to_string()[..16])
        } else {
            requested.to_owned()
        };
        if state.store.get("runs", &id).map_err(storage)?.is_some() {
            return Err(fail(
                StatusCode::CONFLICT,
                "run.binding.id_conflict",
                "run_id already exists",
            ));
        }
        let owner = json!({"binding_key":key,"run_id":id});
        let row = json!({"binding_key":key,"run_id":id,
            "task_id":if task_id.is_empty(){Value::Null}else{json!(task_id)},
            "task_kind":field(payload,"task_kind"),"task_revision":revision,
            "executor_id":payload["executor_id"],"created_at":crate::runs::now_secs()});
        if !state
            .store
            .put_many_if(
                &[
                    (BOUND, key, None),
                    (BOUND_IDS, &id, None),
                    ("runs", &id, None),
                ],
                &[(BOUND, key, &row), (BOUND_IDS, &id, &owner)],
            )
            .map_err(storage)?
        {
            if let Some(saved) = state.store.get(BOUND, key).map_err(storage)? {
                saved
            } else {
                return Err(fail(
                    StatusCode::CONFLICT,
                    "run.binding.id_conflict",
                    "run_id is already bound",
                ));
            }
        } else {
            row
        }
    };
    if !field(&row, "task_id").is_empty()
        && !task_id.is_empty()
        && field(&row, "task_id") != task_id
    {
        return Err(fail(
            StatusCode::CONFLICT,
            "run.binding.task_conflict",
            "binding_key is already bound to another Task",
        ));
    }
    if row["task_revision"].as_u64().unwrap_or(0) != revision {
        return Err(fail(
            StatusCode::CONFLICT,
            "run.binding.revision_conflict",
            "binding_key is already bound to another Task revision",
        ));
    }
    let id = field(&row, "run_id").to_owned();
    let mut run=crate::runs::ensure_run_record(state, &id).map_err(storage)?;
    if row["task_kind"]=="pentest.target" && run["started"]!=true{
        run["mode"]=json!("pentest");state.store.put("runs",&id,&run).map_err(storage)?;
    }
    Ok((
        StatusCode::OK,
        json!({"run_id":id,"binding_key":key,
        "task_id":row["task_id"],"task_kind":row["task_kind"],
        "task_revision":row["task_revision"],"executor_id":row["executor_id"]}),
        id,
    ))
}

fn forward(payload: &Value) -> Value {
    let mut body = payload.clone();
    if let Some(map) = body.as_object_mut() {
        for key in [
            "run_id",
            "binding_key",
            "task_id",
            "task_kind",
            "task_revision",
            "executor_id",
            "correlation_id",
            "causation_id",
        ] {
            map.remove(key);
        }
    }
    body
}

fn control_actor(actor: &Value) -> String {
    let kind = field(actor, "kind");
    if matches!(kind, "operator" | "system") {
        return "operator".to_owned();
    }
    let id = field(actor, "id");
    let label = if id.is_empty() {
        if kind.is_empty() { "operator".to_owned() } else { kind.to_owned() }
    } else {
        format!("{kind}:{id}")
    };
    label.chars().take(256).collect()
}

fn control_action(kind: &str, payload: &Value) -> Result<String, Failure> {
    let action = if kind == "run.control" {
        field(payload, "control_action")
    } else {
        kind.strip_prefix("run.").unwrap_or("")
    };
    if action.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "run.control.action_invalid",
            "run.control requires payload.control_action",
        ));
    }
    if kind == "run.control"
        && !matches!(
            action,
            "ask"
                | "hint"
                | "focus"
                | "redirect"
                | "directive"
                | "correction"
                | "pause"
                | "freeze"
                | "resume"
                | "thaw"
                | "stop"
                | "complete"
                | "graceful_drain"
                | "force_cancel"
                | "dismiss"
                | "dismiss_help"
                | "clear_standing"
                | "reset_guidance"
                | "mark_false"
                | "submit"
                | "writeup"
                | "answer_decision"
                | "add_context"
                | "expire_context"
                | "spawn_worker"
                | "cancel_worker"
        )
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "run.control.action_invalid",
            format!("unsupported control action {action:?}"),
        ));
    }
    Ok(if action == "operator_directive" {
        "directive".to_owned()
    } else {
        action.to_owned()
    })
}

async fn dispatch(
    state: &Arc<AppState>,
    kind: &str,
    run_id: &str,
    command_id: &str,
    payload: &Value,
    actor: &Value,
) -> Result<(StatusCode, Value), Failure> {
    let mut body = forward(payload);
    if matches!(kind, "run.start" | "run.resolve") {
        body["command_id"] = json!(command_id);
    }
    let response = match kind {
        "run.start" => {
            crate::runs::start_run(State(state.clone()), Path(run_id.to_owned()), Json(body)).await
        }
        "run.resolve" => {
            crate::runs::resolve_run(
                State(state.clone()),
                Path(run_id.to_owned()),
                Some(Json(body)),
            )
            .await
        }
        "run.rail.delete" => {
            if state.store.get("runs", run_id).map_err(storage)?.is_none() {
                return Ok((StatusCode::OK, json!({"ok":false})));
            }
            crate::runs::delete_run(State(state.clone()), Path(run_id.to_owned())).await
        }
        "run.open" => crate::runs::open_run(State(state.clone()), Path(run_id.to_owned())).await,
        "run.rail.update"
        | "run.folder.create"
        | "run.folder.update"
        | "run.folder.delete"
        | "run.archive"
        | "run.purge"
        | "run.artifact.attach" => {
            return maintenance(state, kind, run_id, &body);
        }
        _ => {
            let action = control_action(kind, payload)?;
            let mut control = body;
            control["action"] = json!(action);
            control["command_id"] = json!(command_id);
            control["actor"] = json!(control_actor(actor));
            if !payload["expected_generation"].is_null() {
                control["expected_generation"] = payload["expected_generation"].clone();
            }
            if let Some(map) = control.as_object_mut() {
                map.remove("control_action");
            }
            crate::runs::control_run_trusted(State(state.clone()), Path(run_id.to_owned()), Json(control))
                .await
        }
    };
    let (status, body) = response_json(response).await?;
    if kind == "run.control" && field(payload, "control_action") == "force_cancel"
        && body["status"] == "rejected" {
        return Ok((StatusCode::CONFLICT, json!({"error":{
            "code":"control.generation_conflict",
            "message":field(&body, "detail")
        }})));
    }
    if kind == "run.control" && field(payload, "control_action") == "graceful_drain"
        && body["status"] == "unknown" {
        return Ok((StatusCode::CONFLICT, json!({"error":{
            "code":"run.effect.outcome_unknown",
            "message":"No live coordinator accepted graceful_drain"
        }})));
    }
    let control_kind = matches!(kind,
        "run.control" | "run.pause" | "run.resume" | "run.stop" | "run.freeze"
        | "run.thaw" | "run.complete" | "run.hint" | "run.answer_decision"
        | "run.add_context" | "run.operator_directive" | "run.spawn_worker"
        | "run.cancel_worker");
    if control_kind && body["ok"] == false {
        let code = field(&body,"code");
        let code = if code.is_empty() {"run.command.rejected"} else {code};
        let conflict = code.contains("conflict");
        let message = body["detail"].as_str().filter(|value|!value.is_empty())
            .or_else(||body["status"].as_str()).unwrap_or("Run control was not applied");
        let category = if conflict {"conflict"}
            else if status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST {
                "validation"
            } else {"state"};
        return Ok((if conflict {StatusCode::CONFLICT} else {StatusCode::UNPROCESSABLE_ENTITY},
            json!({"error":{"code":code,"message":message,
                "category":category}})));
    }
    Ok((status, body))
}

fn maintenance(
    state: &AppState,
    kind: &str,
    run_id: &str,
    payload: &Value,
) -> Result<(StatusCode, Value), Failure> {
    let handle=state.runs.handle(state,run_id);
    let _lifecycle=match handle.delete_lock.try_lock(){Ok(guard)=>guard,Err(_)=>return Ok((StatusCode::CONFLICT,json!({"error":{"code":"run.lifecycle_busy","message":"Run lifecycle operation is in progress"}})))};
    if handle.deleting.load(std::sync::atomic::Ordering::SeqCst){return Ok((StatusCode::CONFLICT,json!({"error":{"code":"run.deleting","message":"Run deletion is in progress"}})))}
    match kind {
        "run.rail.update" => {
            let Some(mut run) = state.store.get("runs", run_id).map_err(storage)? else {
                return Ok((
                    StatusCode::NOT_FOUND,
                    json!({"error":{"code":"run.not_found",
                    "message":format!("unknown run {run_id:?}")}}),
                ));
            };
            if let Some(value) = payload.get("pinned") {
                run["pinned"] = json!(value.as_bool().unwrap_or(false));
                run["pinned_at"] = if run["pinned"] == true {
                    json!(crate::runs::now_secs())
                } else {
                    Value::Null
                };
            }
            if let Some(value) = payload.get("archived") {
                run["archived"] = json!(value.as_bool().unwrap_or(false));
                if run["archived"] == true {
                    run["pinned"] = json!(false);
                    run["pinned_at"] = Value::Null;
                    run["archived_at"] = json!(crate::runs::now_secs());
                } else {
                    run["archived_at"] = Value::Null;
                }
            }
            if let Some(value) = payload.get("name") {
                run["custom_name"] = value
                    .as_str()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null);
            }
            if let Some(value) = payload.get("folder_id") {
                run["folder_id"] = value
                    .as_str()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null);
            }
            if let Some(value) = payload.get("order") {
                run["order"] = value.clone();
            }
            run["updated"] = json!(crate::runs::now_secs());
            state.store.put("runs", run_id, &run).map_err(storage)?;
            let summary =
                super::run_queries::summary(&crate::runs::project_run_summary(state, run));
            Ok((StatusCode::OK, json!({"ok":true,"run":summary})))
        }
        "run.folder.create" => {
            let count = state
                .store
                .list("run_folders")
                .map_err(storage)?
                .iter()
                .filter_map(|row| field(row, "id").strip_prefix('f')?.parse::<usize>().ok())
                .max()
                .unwrap_or(0)
                + 1;
            let id = format!("f{count:03}");
            let name = field(payload, "name");
            let name = if name.is_empty() { "Folder" } else { name };
            let folder = json!({"id":id,"name":name,"order":count});
            state
                .store
                .put("run_folders", &id, &folder)
                .map_err(storage)?;
            Ok((StatusCode::OK, json!({"ok":true,"folder":folder})))
        }
        "run.folder.update" => {
            let Some(mut folder) = state.store.get("run_folders", run_id).map_err(storage)? else {
                return Ok((StatusCode::OK, json!({"ok":false})));
            };
            if let Some(name) = payload.get("name") {
                if let Some(name) = name.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    folder["name"] = json!(name);
                }
            }
            if let Some(order) = payload.get("order") {
                folder["order"] = order.clone();
            }
            state
                .store
                .put("run_folders", run_id, &folder)
                .map_err(storage)?;
            Ok((StatusCode::OK, json!({"ok":true})))
        }
        "run.folder.delete" => {
            let present = state
                .store
                .get("run_folders", run_id)
                .map_err(storage)?
                .is_some();
            if present {
                state.store.delete("run_folders", run_id).map_err(storage)?;
                for mut run in state.store.list("runs").map_err(storage)? {
                    if run["folder_id"] == run_id {
                        let id = field(&run, "run_id").to_owned();
                        run["folder_id"] = Value::Null;
                        state.store.put("runs", &id, &run).map_err(storage)?;
                    }
                }
            }
            Ok((StatusCode::OK, json!({"ok":present})))
        }
        "run.artifact.attach" => Ok((
            StatusCode::OK,
            json!({"ok":true,
            "files":payload["files"].as_array().cloned().unwrap_or_default()}),
        )),
        _ => Ok((
            StatusCode::BAD_REQUEST,
            json!({"error":{"code":"run.operation.unsupported",
            "message":format!("unsupported Run operation {kind:?}")}}),
        )),
    }
}

pub(super) async fn handle(
    state: Arc<AppState>,
    headers: &HeaderMap,
    gateway_candidate: bool,
    mut envelope: Value,
) -> Result<Response, Failure> {
    let kind = field(&envelope, "command_type").to_owned();
    if envelope.get("payload").is_none() {
        envelope["payload"] = json!({});
    }
    if !envelope["payload"].is_object() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "payload must be an object",
        ));
    }
    if envelope["aggregate_type"]
        .as_str()
        .is_some_and(|value| value != "run")
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "aggregate_type must be run",
        ));
    }
    let mut run_id = target(&envelope);
    if kind != "run.create" && run_id.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "run.id_required",
            format!("{kind} requires a run_id"),
        ));
    }
    envelope["aggregate_type"] = json!("run");
    if kind != "run.create" && !run_id.is_empty() {
        envelope["aggregate_id"] = json!(run_id);
    }
    let principal = platform_auth::authorize(
        &state,
        headers,
        gateway_candidate,
        &envelope,
        "command",
        &kind,
        "run",
        &run_id,
    )?;
    if kind != "run.create"
        && !kind.starts_with("run.folder.")
        && !matches!(
            kind.as_str(),
            "run.rail.update"
                | "run.rail.delete"
                | "run.archive"
                | "run.purge"
                | "run.open"
                | "run.artifact.attach"
        )
        && !principal.starts_with("operator:")
    {
        let bearer = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .unwrap_or("");
        let mut thread_id = state
            .capability_gateway
            .owner_for_token(bearer)
            .or_else(|| envelope["actor"]["thread_id"].as_str().map(str::to_owned))
            .unwrap_or_default();
        if thread_id.is_empty() {
            let actor = &envelope["actor"];
            let binding_id = field(actor, "binding_id");
            let requested_version = actor["binding_version"].as_u64();
            let mut bindings = state.store.list(crate::capabilities::BINDINGS).map_err(storage)?
                .into_iter().flat_map(|row| row["versions"].as_array().cloned().unwrap_or_default())
                .filter(|row| row["binding_id"] == binding_id).collect::<Vec<_>>();
            bindings.sort_by_key(|row| row["binding_version"].as_u64().unwrap_or(0));
            thread_id = bindings.into_iter().rev()
                .find(|row| requested_version.is_none_or(|version| row["binding_version"] == version))
                .and_then(|row| row["thread_id"].as_str().map(str::to_owned))
                .unwrap_or_default();
        }
        if let Some(task) = state
            .store
            .list("src_tasks")
            .map_err(storage)?
            .into_iter()
            .find(|task| task["binding"]["thread_id"] == thread_id && !thread_id.is_empty())
        {
            if state
                .store
                .get("runs", &run_id)
                .map_err(storage)?
                .is_none_or(|run| run["src_task_id"] != task["task_id"])
            {
                return Err(fail(
                    StatusCode::FORBIDDEN,
                    "run.not_owner",
                    format!("run {run_id:?} is not bound to this SRC Task"),
                ));
            }
        }
    }
    if envelope["command_id"].as_str().is_none_or(str::is_empty) {
        envelope["command_id"] = json!(new_id("cmd"));
    }
    let command_id = field(&envelope, "command_id").to_owned();
    let key = field(&envelope, "idempotency_key").to_owned();
    let mut semantic = envelope.clone();
    if let Some(object) = semantic.as_object_mut() {
        object.remove("command_id");
        object.remove("idempotency_key");
    }
    let module=crate::business::operation_module(&state,envelope["command_type"].as_str().unwrap_or(""),&envelope).map_err(|e|e.failure())?;
    let _lock=crate::business::dispatch_lock(&state,module).lock().await;
    crate::business::require_operation(&state,envelope["command_type"].as_str().unwrap_or(""),&envelope).map_err(|e|e.failure())?;
    if !key.is_empty() {
        if let Some(prior) = state.store.get(RUN_KEYS, &key).map_err(storage)? {
            if prior["semantic"] != semantic {
                return Err(fail(
                    StatusCode::CONFLICT,
                    "command.idempotency_conflict",
                    "idempotency_key was used with different content",
                ));
            }
            if let Some(stored) = state
                .store
                .get(COMMANDS, field(&prior, "command_id"))
                .map_err(storage)?
            {
                return Ok(replay(&stored));
            }
        }
    }
    if let Some(stored) = state.store.get(COMMANDS, &command_id).map_err(storage)? {
        if stored["request"] != envelope {
            return Err(fail(
                StatusCode::CONFLICT,
                "command.idempotency_conflict",
                "command_id was already used with different content",
            ));
        }
        return Ok(replay(&stored));
    }
    let needs_existing_run = !matches!(
        kind.as_str(),
        "run.create"
            | "run.rail.delete"
            | "run.folder.create"
            | "run.folder.update"
            | "run.folder.delete"
            | "run.archive"
            | "run.purge"
            | "run.open"
            | "run.artifact.attach"
    );
    if needs_existing_run && state.store.get("runs", &run_id).map_err(storage)?.is_none() {
        return Err(fail(
            StatusCode::NOT_FOUND,
            "run.not_found",
            format!("unknown run {run_id:?}"),
        ));
    }
    if let Some(expected) = envelope["expected_version"].as_u64() {
        let current = aggregate_version(&state, "run", &run_id)?;
        if expected != current {
            return Err(fail(
                StatusCode::CONFLICT,
                "command.expected_version_conflict",
                format!("Expected version {expected}, current version {current}"),
            ));
        }
    }
    let effect_id = (kind != "run.create").then(|| new_id("effect"));
    let outbox_id = effect_id.as_ref().map(|_| new_id("outbox"));
    let _effect_guard = if let Some(outbox_id) = outbox_id.as_deref() {
        Some(effect_lock(&state, outbox_id)?.ok_or_else(|| {
            fail(StatusCode::CONFLICT, "run.effect_busy", "Run effect is already executing")
        })?)
    } else {
        None
    };
    let accepted = receipt(
        &command_id,
        &run_id,
        "accepted",
        json!({}),
        effect_id.as_deref(),
        Value::Null,
    );
    let stored = json!({"request":envelope,"response":{"receipt":accepted},"status":200});
    let claim =
        json!({"command_id":command_id,"request":envelope,"state":"accepted","created_at":now()});
    let keyed = json!({"command_id":command_id,"semantic":semantic,"created_at":now()});
    let (effect, outbox) =
        if let (Some(eid), Some(oid)) = (effect_id.as_deref(), outbox_id.as_deref()) {
            let (effect, outbox) = effect_records(&envelope, eid, oid);
            (Some(effect), Some(outbox))
        } else {
            (None, None)
        };
    let mut conditions = vec![(COMMANDS, command_id.as_str(), None)];
    let mut records = vec![
        (COMMANDS, command_id.as_str(), &stored),
        (COMMAND_CLAIMS, command_id.as_str(), &claim),
    ];
    if !key.is_empty() {
        conditions.push((RUN_KEYS, key.as_str(), None));
        records.push((RUN_KEYS, key.as_str(), &keyed));
    }
    if let (Some(eid), Some(oid), Some(effect), Some(outbox)) = (
        effect_id.as_deref(),
        outbox_id.as_deref(),
        effect.as_ref(),
        outbox.as_ref(),
    ) {
        records.push((EFFECTS, eid, effect));
        records.push((OUTBOX, oid, outbox));
        records.push((INPUTS, oid, &envelope));
    }
    if !state
        .store
        .put_many_if(&conditions, &records)
        .map_err(storage)?
    {
        if let Some(prior) = state.store.get(COMMANDS, &command_id).map_err(storage)? {
            if prior["request"] == envelope {
                return Ok(replay(&prior));
            }
        }
        if !key.is_empty() {
            if let Some(prior_key) = state.store.get(RUN_KEYS, &key).map_err(storage)? {
                if prior_key["semantic"] == semantic {
                    if let Some(prior) = state
                        .store
                        .get(COMMANDS, field(&prior_key, "command_id"))
                        .map_err(storage)?
                    {
                        return Ok(replay(&prior));
                    }
                }
            }
        }
        return Err(fail(
            StatusCode::CONFLICT,
            "command.idempotency_conflict",
            "command identity is already in use",
        ));
    }
    if kind != "run.create" {
        event(&state, &run_id, &envelope, "accepted", None);
    }
    let outcome = if kind == "run.create" {
        create(&state, &envelope)
            .await
            .map(|(status, body, id)| (status, body, Some(id)))
    } else {
        dispatch(&state, &kind, &run_id, &command_id, &envelope["payload"], &envelope["actor"])
            .await
            .map(|(status, body)| (status, body, None))
    };
    let (status, body, created_id) = match outcome {
        Ok(value) => value,
        Err((status, Json(body))) => (status, body, None),
    };
    if let Some(created_id) = created_id {
        run_id = created_id;
    }
    let unknown = body["error"]["code"] == "run.effect.outcome_unknown";
    let phase = if unknown {
        "outcome_unknown"
    } else if status.is_success() {
        "completed"
    } else {
        "failed"
    };
    if kind == "run.create" {
        if status.is_success() {
            event(&state, &run_id, &envelope, "accepted", None);
        }
    } else {
        event(&state, &run_id, &envelope, phase, None);
    }
    let error = error_from_response(status, &body);
    let output = if status.is_success() { body } else { json!({}) };
    let mut final_receipt = receipt(
        &command_id,
        &run_id,
        if unknown {
            "outcome_unknown"
        } else if kind == "run.create" && status.is_success() {
            "accepted"
        } else if status == StatusCode::CONFLICT {
            "conflict"
        } else {
            phase
        },
        output,
        effect_id.as_deref(),
        error.clone(),
    );
    final_receipt["receipt_id"] = accepted["receipt_id"].clone();
    if kind.starts_with("run.folder.") {
        final_receipt["run_id"] = Value::Null;
    }
    final_receipt["submitted_at"] = accepted["submitted_at"].clone();
    if kind != "run.create" || !status.is_success() {
        final_receipt["completed_at"] = json!(now());
    }
    let final_stored =
        json!({"request":envelope,"response":{"receipt":final_receipt},"status":status.as_u16()});
    let final_claim =
        json!({"command_id":command_id,"request":envelope,"state":phase,"updated_at":now()});
    let mut updates = vec![
        (COMMANDS, command_id.as_str(), &final_stored),
        (COMMAND_CLAIMS, command_id.as_str(), &final_claim),
    ];
    let mut final_effect = effect.unwrap_or(Value::Null);
    let mut final_outbox = outbox.unwrap_or(Value::Null);
    if let (Some(eid), Some(oid)) = (effect_id.as_deref(), outbox_id.as_deref()) {
        final_effect["state"] = json!(phase);
        final_effect["attempts"] = json!(1);
        final_effect["acceptance_receipt_id"] = final_receipt["receipt_id"].clone();
        final_effect["output"] = json!({"http_status":status.as_u16()});
        final_effect["error"] = error;
        final_effect["updated_at"] = json!(now());
        final_effect["completed_at"] = json!(now());
        final_outbox["status"] = json!(if unknown {
            "outcome_unknown"
        } else if status.is_success() {
            "delivered"
        } else {
            "failed"
        });
        final_outbox["attempts"] = json!(1);
        final_outbox["last_error"] = if unknown {
            json!("run.effect.outcome_unknown")
        } else if status.is_success() {
            Value::Null
        } else {
            json!("run.command.rejected")
        };
        final_outbox["delivered_at"] = if status.is_success() {
            json!(now())
        } else {
            Value::Null
        };
        final_outbox["updated_at"] = json!(now());
        updates.push((EFFECTS, eid, &final_effect));
        updates.push((OUTBOX, oid, &final_outbox));
    }
    state.store.put_many(&updates).map_err(storage)?;
    Ok((status, Json(json!({"receipt":final_receipt}))).into_response())
}

pub(super) async fn recover(state: &Arc<AppState>, observed: &Value) -> Result<bool, Failure> {
    let outbox_id = field(observed, "outbox_id");
    let Some(_guard) = effect_lock(state, outbox_id)? else {
        return Ok(false);
    };
    let Some(mut outbox) = state.store.get(OUTBOX, outbox_id).map_err(storage)? else {
        return Ok(false);
    };
    if !matches!(field(&outbox, "status"), "pending" | "processing") {
        return Ok(false);
    }
    let effect_id = field(&outbox, "effect_id").to_owned();
    let command_id = field(&outbox, "command_id").to_owned();
    let Some(mut effect) = state.store.get(EFFECTS, &effect_id).map_err(storage)? else {
        return Ok(false);
    };
    let Some(mut stored) = state.store.get(COMMANDS, &command_id).map_err(storage)? else {
        return Ok(false);
    };
    if stored["response"]["receipt"]["state"] != "accepted" {
        return Ok(false);
    }
    let envelope = state.store.get(INPUTS, outbox_id).map_err(storage)?
        .ok_or_else(|| fail(StatusCode::INTERNAL_SERVER_ERROR,
            "run.effect.input_missing", "Accepted Run effect has no persisted input"))?;
    let kind = field(&envelope, "command_type").to_owned();
    let run_id = field(&envelope, "aggregate_id").to_owned();
    let control = matches!(kind.as_str(),
        "run.control" | "run.pause" | "run.resume" | "run.stop" | "run.freeze"
        | "run.thaw" | "run.complete" | "run.hint" | "run.answer_decision"
        | "run.add_context" | "run.operator_directive" | "run.spawn_worker"
        | "run.cancel_worker");
    let worker_action = matches!(kind.as_str(), "run.spawn_worker" | "run.cancel_worker")
        || kind == "run.control" && matches!(field(&envelope["payload"], "control_action"),
            "spawn_worker" | "cancel_worker" | "force_cancel");
    let execution = matches!(kind.as_str(), "run.start" | "run.resolve");
    if control || execution {
        match state.runs.claim_execution_owner(state, &run_id) {
            Ok(true) => {},
            Ok(false) => return Ok(false),
            Err(error) => return Err(fail(StatusCode::INTERNAL_SERVER_ERROR,
                "run.execution_owner_unavailable", error)),
        }
    }
    let journaled_worker = worker_action
        && crate::runs::control_command_recorded(state, &run_id, &command_id)
            .map_err(|error| fail(StatusCode::INTERNAL_SERVER_ERROR,
                "run.control.journal_unavailable", error))?;
    if !execution && (!control || (worker_action && !journaled_worker)) {
        let error = json!({"code":"run.effect.outcome_unknown",
            "message":"Run effect may have executed before the service stopped",
            "category":"state","retryable":false});
        stored["response"]["receipt"]["state"] = json!("outcome_unknown");
        stored["response"]["receipt"]["error"] = error.clone();
        stored["status"] = json!(409);
        effect["state"] = json!("outcome_unknown");
        effect["error"] = error;
        effect["updated_at"] = json!(now());
        outbox["status"] = json!("outcome_unknown");
        outbox["last_error"] = json!("run.effect.outcome_unknown");
        outbox["updated_at"] = json!(now());
        state.store.put_many(&[(COMMANDS, &command_id, &stored),
            (EFFECTS, &effect_id, &effect), (OUTBOX, outbox_id, &outbox)]).map_err(storage)?;
        return Ok(true);
    }
    event_once(state, &run_id, &envelope, "accepted");
    let attempts = outbox["attempts"].as_u64().unwrap_or(0) + 1;
    outbox["status"] = json!("processing");
    outbox["attempts"] = json!(attempts);
    outbox["updated_at"] = json!(now());
    effect["state"] = json!("running");
    effect["attempts"] = json!(attempts);
    effect["updated_at"] = json!(now());
    state.store.put_many(&[(OUTBOX, outbox_id, &outbox),
        (EFFECTS, &effect_id, &effect)]).map_err(storage)?;
    let (status, body) = match dispatch(state, &kind, &run_id, &command_id, &envelope["payload"], &envelope["actor"]).await {
        Ok(value) => value,
        Err((status, Json(body))) => (status, body),
    };
    let unknown = body["error"]["code"] == "run.effect.outcome_unknown";
    let phase = if unknown { "outcome_unknown" }
        else if status.is_success() { "completed" } else { "failed" };
    event_once(state, &run_id, &envelope, phase);
    let accepted = &stored["response"]["receipt"];
    let error = error_from_response(status, &body);
    let mut receipt = receipt(&command_id, &run_id,
        if unknown { "outcome_unknown" }
        else if status == StatusCode::CONFLICT { "conflict" } else { phase },
        if status.is_success() { body } else { json!({}) }, Some(&effect_id), error.clone());
    receipt["receipt_id"] = accepted["receipt_id"].clone();
    receipt["submitted_at"] = accepted["submitted_at"].clone();
    receipt["completed_at"] = json!(now());
    stored["response"]["receipt"] = receipt;
    stored["status"] = json!(status.as_u16());
    let claim = json!({"command_id":command_id,"request":envelope,
        "state":phase,"updated_at":now()});
    effect["state"] = json!(phase);
    effect["acceptance_receipt_id"] = stored["response"]["receipt"]["receipt_id"].clone();
    effect["output"] = json!({"http_status":status.as_u16()});
    effect["error"] = error;
    effect["updated_at"] = json!(now());
    effect["completed_at"] = json!(now());
    outbox["status"] = json!(if unknown {"outcome_unknown"}
        else if status.is_success(){"delivered"}else{"failed"});
    outbox["last_error"] = if status.is_success(){Value::Null}
        else if unknown {json!("run.effect.outcome_unknown")}
        else {json!("run.command.rejected")};
    outbox["delivered_at"] = if status.is_success(){json!(now())}else{Value::Null};
    outbox["updated_at"] = json!(now());
    state.store.put_many(&[(COMMANDS, &command_id, &stored),
        (COMMAND_CLAIMS, &command_id, &claim), (EFFECTS, &effect_id, &effect),
        (OUTBOX, outbox_id, &outbox)]).map_err(storage)?;
    Ok(true)
}
