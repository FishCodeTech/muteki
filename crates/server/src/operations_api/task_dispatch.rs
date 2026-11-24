//! Shared Task aggregate, separate from the SRC Task workbench.
use super::*;

const TASKS: &str = "platform_tasks";
const EVENTS: &str = "platform_task_events";
const KEYS: &str = "task_command_keys";

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("").trim()
}

fn replay(stored: &Value) -> Response {
    let mut receipt = stored["response"]["receipt"].clone();
    receipt["deduplicated"] = json!(true);
    Json(json!({"receipt":receipt})).into_response()
}

pub(super) async fn create(
    state: Arc<AppState>,
    headers: &HeaderMap,
    gateway_candidate: bool,
    mut envelope: Value,
) -> Result<Response, Failure> {
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
        .is_some_and(|value| value != "task")
    {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "aggregate_type must be task",
        ));
    }
    let kind = field(&envelope["payload"], "kind").to_owned();
    if kind.is_empty() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "task.kind_required",
            "task.create requires payload.kind (如 ctf.challenge / pentest.target)",
        ));
    }
    let requested = field(&envelope["payload"], "task_id");
    let requested = if requested.is_empty() {
        field(&envelope, "aggregate_id")
    } else {
        requested
    };
    let requested = requested.to_owned();
    envelope["aggregate_type"] = json!("task");
    envelope["aggregate_id"] = json!(requested);
    let principal = platform_auth::authorize(
        &state,
        headers,
        gateway_candidate,
        &envelope,
        "command",
        "task.create",
        "task",
        &requested,
    )?;
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
        if let Some(prior) = state.store.get(KEYS, &key).map_err(storage)? {
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
                "command_id was used with different content",
            ));
        }
        return Ok(replay(&stored));
    }
    if envelope["expected_version"]
        .as_u64()
        .is_some_and(|version| version != 0)
    {
        return Err(fail(
            StatusCode::CONFLICT,
            "command.version_conflict",
            "task.create requires expected_version=0",
        ));
    }
    let id = if requested.is_empty() {
        new_id("task")
    } else {
        requested
    };
    if state.store.get(TASKS, &id).map_err(storage)?.is_some() {
        return Err(fail(
            StatusCode::CONFLICT,
            "task.already_exists",
            format!("task {id:?} already exists"),
        ));
    }
    let created_at = now();
    let input = envelope["payload"]["input"]
        .as_object()
        .map(|_| envelope["payload"]["input"].clone())
        .unwrap_or_else(|| json!({}));
    let revision = envelope["payload"]["revision"]
        .as_i64()
        .filter(|v| *v != 0)
        .unwrap_or(1);
    let task = json!({"task_id":id,
        "thread_id":envelope["payload"]["thread_id"].as_str().map(str::trim).filter(|v|!v.is_empty()),
        "project_id":envelope["payload"]["project_id"].as_str().map(str::trim).filter(|v|!v.is_empty()),
        "kind":kind,"title":envelope["payload"]["title"].as_str().unwrap_or(""),
        "input":input,"revision":revision,"created_at":created_at});
    let actor = field(&envelope["actor"], "id");
    let correlation = field(&envelope["payload"], "correlation_id");
    let event = json!({"event_id":new_id("evt"),"aggregate_type":"task","aggregate_id":id,
        "stream_seq":1,"event_type":"core.task.created","occurred_at":created_at,
        "producer":"builtin.single-security-task",
        "actor_id":if actor.is_empty(){"system"}else{actor},
        "command_id":command_id,"causation_id":command_id,
        "correlation_id":if correlation.is_empty(){command_id.as_str()}else{correlation},
        "idempotency_key":envelope["idempotency_key"],
        "payload":{"task_id":id,"thread_id":task["thread_id"],"kind":task["kind"],
            "title":task["title"],"input":task["input"],"revision":task["revision"]}});
    let receipt = json!({"schema_version":1,"command_id":command_id,"receipt_id":new_id("rcpt"),
        "state":"completed","run_id":null,"aggregate":{"type":"task","id":id},
        "deduplicated":false,"submitted_at":created_at,
        "next":["receipt","snapshot","events","wait"],
        "event_cursor":platform_events::signed(&state,"task",&id,1,&principal)?,
        "effect_ids":[],"output":{},"error":null});
    let stored = json!({"request":envelope,"response":{"receipt":receipt},"status":200});
    let mut conditions = vec![
        (COMMANDS, command_id.as_str(), None),
        (TASKS, id.as_str(), None),
    ];
    let mut records = vec![
        (COMMANDS, command_id.as_str(), &stored),
        (TASKS, id.as_str(), &task),
        (EVENTS, id.as_str(), &event),
    ];
    let keyed = json!({"command_id":command_id,"semantic":semantic});
    if !key.is_empty() {
        conditions.push((KEYS, key.as_str(), None));
        records.push((KEYS, key.as_str(), &keyed));
    }
    if !state
        .store
        .put_many_if(&conditions, &records)
        .map_err(storage)?
    {
        if let Some(stored) = state.store.get(COMMANDS, &command_id).map_err(storage)? {
            if stored["request"] == envelope {
                return Ok(replay(&stored));
            }
        }
        if !key.is_empty() {
            if let Some(prior) = state.store.get(KEYS, &key).map_err(storage)? {
                if prior["semantic"] == semantic {
                    if let Some(stored) = state
                        .store
                        .get(COMMANDS, field(&prior, "command_id"))
                        .map_err(storage)?
                    {
                        return Ok(replay(&stored));
                    }
                }
            }
        }
        return Err(fail(
            StatusCode::CONFLICT,
            "task.already_exists",
            format!("task {id:?} already exists"),
        ));
    }
    Ok(Json(json!({"receipt":receipt})).into_response())
}

pub(super) fn query(
    state: &AppState,
    headers: &HeaderMap,
    gateway_candidate: bool,
    mut envelope: Value,
) -> Result<Response, Failure> {
    let kind = field(&envelope, "query_type").to_owned();
    if envelope.get("params").is_none() {
        envelope["params"] = json!({});
    }
    if !envelope["params"].is_object() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "params must be an object",
        ));
    }
    let requested = field(&envelope["params"], "task_id");
    let requested = if requested.is_empty() {
        field(&envelope, "aggregate_id")
    } else {
        requested
    };
    let target = if kind == "task.get" {
        requested.to_owned()
    } else {
        String::new()
    };
    envelope["aggregate_type"] = json!("task");
    envelope["aggregate_id"] = json!(target);
    platform_auth::authorize(
        state,
        headers,
        gateway_candidate,
        &envelope,
        "query",
        &kind,
        "task",
        &target,
    )?;
    if envelope["query_id"].as_str().is_none_or(str::is_empty) {
        envelope["query_id"] = json!(new_id("qry"));
    }
    let result = if kind == "task.get" {
        state
            .store
            .get(TASKS, &target)
            .map_err(storage)?
            .ok_or_else(|| {
                fail(
                    StatusCode::NOT_FOUND,
                    "task.not_found",
                    format!("unknown task {target:?}"),
                )
            })?
    } else {
        list(state, &envelope["params"])?
    };
    let cursor = result.get("next_cursor").cloned().unwrap_or(Value::Null);
    Ok(Json(
        json!({"result":{"query_id":envelope["query_id"],"query_type":kind,
        "result":result,"cursor":cursor}}),
    )
    .into_response())
}

fn list(state: &AppState, params: &Value) -> Result<Value, Failure> {
    let mut tasks = state.store.list(TASKS).map_err(storage)?;
    tasks.sort_by(|a, b| {
        field(b, "created_at")
            .cmp(field(a, "created_at"))
            .then_with(|| field(b, "task_id").cmp(field(a, "task_id")))
    });
    let kind = field(params, "kind");
    let prefix = field(params, "kind_prefix");
    let project = field(params, "project_id");
    let thread = field(params, "thread_id");
    let search = field(params, "query").to_lowercase();
    tasks.retain(|task| {
        (kind.is_empty() || field(task, "kind") == kind)
            && (prefix.is_empty() || field(task, "kind").starts_with(prefix))
            && (project.is_empty() || field(task, "project_id") == project)
            && (thread.is_empty() || field(task, "thread_id") == thread)
            && (search.is_empty()
                || format!(
                    "{} {} {}",
                    field(task, "task_id"),
                    field(task, "kind"),
                    field(task, "title")
                )
                .to_lowercase()
                .contains(&search))
    });
    let total = tasks.len();
    let limit = params["limit"]
        .as_i64()
        .or_else(|| params["limit"].as_str().and_then(|v| v.parse().ok()))
        .filter(|v| *v != 0)
        .unwrap_or(500)
        .clamp(1, 2000) as usize;
    let cursor = field(params, "cursor");
    let start = if cursor.is_empty() {
        0
    } else {
        tasks
            .iter()
            .position(|task| field(task, "task_id") == cursor)
            .map(|index| index + 1)
            .ok_or_else(|| {
                fail(
                    StatusCode::BAD_REQUEST,
                    "query.cursor.stale",
                    "列表发生变化，续读位置已失效",
                )
            })?
    };
    let rows: Vec<Value> = tasks
        .into_iter()
        .skip(start)
        .take(limit)
        .map(|task| {
            json!({
        "task_id":task["task_id"],"thread_id":task["thread_id"],"project_id":task["project_id"],
        "kind":task["kind"],"title":task["title"],"revision":task["revision"],
        "created_at":task["created_at"]})
        })
        .collect();
    let has_more = start + rows.len() < total;
    let next_cursor = if has_more {
        rows.last().and_then(|row| row["task_id"].as_str())
    } else {
        None
    };
    Ok(json!({"tasks":rows,"total":total,"returned":rows.len(),
        "next_cursor":next_cursor,"has_more":has_more}))
}

pub(super) fn event(state: &AppState, task_id: &str) -> Result<Option<Value>, Failure> {
    state.store.get(EVENTS, task_id).map_err(storage)
}
