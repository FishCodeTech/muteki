//! Composer capability catalog and runtime prewarm for the Next conversation shell.
use crate::{AppState, agent_runtime::descriptors, composer};
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::sync::Arc;
type Failure = (StatusCode, Json<Value>);
type Api = Result<Json<Value>, Failure>;
fn error(status: StatusCode, message: &str) -> Failure {
    (
        status,
        Json(json!({"detail":message,"error":{"message":message}})),
    )
}
fn typed(status: StatusCode, code: &str, message: impl Into<String>) -> Failure {
    let category = match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "validation",
        StatusCode::NOT_FOUND => "not_found",
        _ => "runtime",
    };
    (
        status,
        Json(json!({"error":{"code":code,"category":category,"message":message.into(),"retryable":false}})),
    )
}
fn db(_: rusqlite::Error) -> Failure {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not access metadata",
    )
}
fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().map(str::trim).unwrap_or("")
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/conversation/composer-capabilities",
            post(composer_capabilities),
        )
        .route("/api/conversation/runtime-prewarm", post(prewarm))
}

fn extension_capabilities(s: &AppState, adapter: &str, trigger: &Value) -> Api {
    let key = format!("{adapter}:default");
    let health = s.store.get(crate::runtime_api::HEALTH,&key).map_err(db)?.unwrap_or(Value::Null);
    let capabilities = &health["capabilities"];
    let access_modes = capabilities["access_modes"].as_array().cloned().unwrap_or_default();
    Ok(Json(json!({"engine":adapter,"trigger":trigger,"items":[],"count":0,"partial":false,
        "section_errors":[],"access_modes":access_modes,"access_mode_notes":{},
        "runtime":{"adapter_id":adapter,"revision":1,"stale":false,"diagnostics":[],
            "refresh_status":"fresh","matrix":{"adapter_id":adapter,"instance_id":"default",
                "revision":1,"stale":false,"rows":[],"diagnostics":[]}}})))
}

fn canonical(adapter_id: &str) -> Option<String> {
    descriptors::for_adapter(adapter_id).map(|(_, adapter)| adapter.adapter_id.clone())
}

fn workspace(s: &AppState, workspace_id: &str) -> Result<Option<Value>, Failure> {
    if workspace_id.is_empty() {
        return Ok(None);
    }
    s.store
        .get("workspaces", workspace_id)
        .map_err(db)?
        .map(Some)
        .ok_or_else(|| typed(StatusCode::NOT_FOUND, "conversation.workspace.not_found", "当前项目工作目录不存在"))
}

async fn composer_capabilities(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Api {
    let thread_id = field(&body, "thread_id").to_owned();
    let trigger = field(&body, "trigger").to_owned();
    let mut adapter_id = field(&body, "adapter_id").to_owned();
    let mut workspace_id = field(&body, "workspace_id").to_owned();
    let doc = if thread_id.is_empty() {
        None
    } else {
        let _guard = s.chat.active.lock().await;
        match crate::chat::load(&s, &thread_id) {
            Ok(doc) => Some(doc),
            Err((StatusCode::NOT_FOUND, _)) => {
                return Err(typed(StatusCode::NOT_FOUND, "conversation.thread.not_found", "当前对话不存在"));
            }
            Err(failure) => return Err(failure),
        }
    };
    if let Some(doc) = &doc {
        if adapter_id.is_empty() {
            adapter_id = field(&doc["runtime"], "adapter_id").to_owned();
        }
        workspace_id = field(&doc["thread"], "workspace_id").to_owned();
    }
    let Some(canonical_id) = canonical(&adapter_id) else {
        if crate::extensions::ready_capabilities(&s,"external-agent-adapter")
            .ok().is_some_and(|rows|rows.iter().any(|entry|entry["id"] == adapter_id)) {
            return extension_capabilities(&s,&adapter_id,&body["trigger"]);
        }
        return Err(typed(StatusCode::BAD_REQUEST, "conversation.composer.adapter_unknown", format!("未知 Runtime adapter：{adapter_id:?}")));
    };
    let (engine, _) = descriptors::for_adapter(&canonical_id).expect("canonical adapters resolve");
    let engine = engine.engine().to_owned();
    let ws = workspace(&s, &workspace_id)?;
    let project_id = field(&body, "project_id").to_owned();
    if let Some(ws) = &ws
        && !project_id.is_empty()
        && field(ws, "project_id") != project_id
    {
        return Err(typed(StatusCode::BAD_REQUEST, "conversation.workspace.project_mismatch", "当前项目与工作目录不匹配"));
    }
    let workspace_root = ws.as_ref().map(|ws| field(ws, "root_path").to_owned()).unwrap_or_default();

    let selected = doc.as_ref().map(|doc| field(&doc["runtime"], "adapter_id").to_owned()).unwrap_or_default();
    let same_runtime = doc.is_some() && canonical(&selected).as_deref() == Some(canonical_id.as_str());
    let instance_id = match field(&body, "instance_id") {
        "" if same_runtime => Some(field(&doc.as_ref().unwrap()["runtime"], "instance_id")).filter(|i| !i.is_empty()).unwrap_or("default").to_owned(),
        "" => "default".to_owned(),
        explicit => explicit.to_owned(),
    };
    if instance_id != "default" && crate::runtime_api::instance(&s.store, &canonical_id, &instance_id).is_none() {
        return Err(typed(StatusCode::NOT_FOUND, "conversation.runtime.instance_not_found", format!("Runtime 实例不存在：{canonical_id}:{instance_id}")));
    }

    let mut diagnostics: Vec<String> = Vec::new();
    let epoch = s.chat.epoch().to_owned();
    let mut commands: Vec<Value> = Vec::new();
    let mut snapshot_state: Option<(bool, u64)> = None;
    let mut matrix = Value::Null;
    if let Some(doc) = &doc {
        if same_runtime {
            let session = &doc["native_session"];
            let session_matches = canonical(field(session, "adapter_id")).as_deref() == Some(canonical_id.as_str());
            let connected = session_matches && session["open"] == true && session["epoch"] == epoch.as_str();
            let revision = session["capability_revision"].as_u64().unwrap_or(0);
            if session_matches && let Some(rows) = session["commands"].as_array() {
                commands = rows.clone();
                snapshot_state = Some((!connected, revision));
            }
            if !connected {
                if crate::native_chat::refresh_capabilities(&s, &thread_id, doc) {
                    diagnostics.push("Runtime 动态能力正在后台刷新；当前先显示已确认的插件与 Skill".into());
                } else if let Some(error) = s.native.refresh_state(&thread_id).and_then(|row| row.last_error) {
                    diagnostics.push(error);
                } else if snapshot_state.is_none() {
                    diagnostics.push("会话能力目录尚未加载".into());
                }
            }
            if let Some((connection, _)) = crate::native_chat::connection(&epoch, doc) {
                matrix = connection["matrix"].clone();
            }
        } else {
            diagnostics.push("当前选择与活动 Session 不同；发送后会建立新 Session，在此之前不展示未经该 Runtime 确认的命令".into());
        }
    }
    if matrix.is_null()
        && let Some((connection, _)) = crate::native_chat::connection(&epoch, &json!({"runtime":{"adapter_id":canonical_id,"instance_id":instance_id}}))
    {
        matrix = connection["matrix"].clone();
    }

    let state = s.clone();
    let request_engine = engine.clone();
    let request_trigger = trigger.clone();
    let query = field(&body, "query").to_owned();
    let current = thread_id.clone();
    let adapter_for_snapshot = canonical_id.clone();
    let native_capabilities = doc.as_ref().map(|doc|doc["native_session"]["capabilities"].clone()).unwrap_or(Value::Null);
    let (items, section_errors) = tokio::task::spawn_blocking(move || {
        let mut errors = Vec::new();
        let snapshot = snapshot_state.map(|(stale, revision)| composer::RuntimeSnapshot {
            adapter_id: adapter_for_snapshot.clone(),
            commands: &commands,
            native_capabilities: &native_capabilities,
            stale,
            revision,
        });
        let request = composer::CatalogRequest {
            engine: &request_engine,
            workspace_root: &workspace_root,
            trigger: &request_trigger,
            query: &query,
            project_id: &project_id,
            current_thread_id: &current,
            snapshot,
        };
        let items = composer::catalog(&state, &request, &mut errors);
        (items, errors)
    })
    .await
    .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "Composer catalog task failed"))?;

    let refresh = if thread_id.is_empty() { None } else { s.native.refresh_state(&thread_id) };
    let refresh_status = match (&refresh, snapshot_state) {
        (Some(row), _) if row.in_flight => "refreshing",
        (Some(row), _) if row.last_error.is_some() => "failed",
        (_, None) => "missing",
        (_, Some((true, _))) => "stale",
        _ => "fresh",
    };
    let access = composer::access_modes(&canonical_id);
    let count = items.len();
    Ok(Json(json!({
        "engine": engine,
        "trigger": trigger,
        "access_modes": access["access_modes"],
        "access_mode_notes": access["access_mode_notes"],
        "items": items,
        "count": count,
        "partial": !section_errors.is_empty(),
        "section_errors": section_errors,
        "runtime": {
            "adapter_id": canonical_id,
            "revision": snapshot_state.map_or(0, |(_, revision)| revision),
            "stale": snapshot_state.is_none_or(|(stale, _)| stale),
            "diagnostics": diagnostics,
            "matrix": matrix,
            "refresh_status": refresh_status,
            "last_error": refresh.as_ref().and_then(|row| row.last_error.clone()),
            "refresh_attempts": refresh.as_ref().map_or(0, |row| row.attempts),
            "retry_after_seconds": refresh.as_ref().map_or(0, |row| row.retry_after()),
        },
    })))
}

async fn prewarm(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Api {
    let adapter_id = field(&body, "adapter_id");
    if crate::extensions::ready_capabilities(&s,"external-agent-adapter")
        .ok().is_some_and(|rows|rows.iter().any(|entry|entry["id"] == adapter_id)) {
        return Ok(Json(json!({"started":false})));
    }
    if canonical(adapter_id).is_none() {
        return Err(typed(StatusCode::BAD_REQUEST, "conversation.runtime.adapter_unknown", format!("未知 Runtime adapter：{adapter_id:?}")));
    }
    let workspace_id = field(&body, "workspace_id");
    let Some(ws) = workspace(&s, workspace_id)? else {
        return Err(typed(StatusCode::NOT_FOUND, "conversation.workspace.not_found", "当前项目工作目录不存在"));
    };
    let instance_id = Some(field(&body, "instance_id")).filter(|i| !i.is_empty()).unwrap_or("default");
    if instance_id != "default" && crate::runtime_api::instance(&s.store, adapter_id, instance_id).is_none() {
        return Err(typed(StatusCode::NOT_FOUND, "conversation.runtime.instance_not_found", format!("Runtime 实例不存在：{adapter_id}:{instance_id}")));
    }
    let runtime = json!({
        "adapter_id": adapter_id, "instance_id": instance_id,
        "credential_id": field(&body, "credential_id"), "model": field(&body, "model"),
        "access_mode": field(&body, "access_mode"),
    });
    let root = std::path::PathBuf::from(field(&ws, "root_path"));
    match crate::native_chat::prewarm(&s, &runtime, root).await {
        Ok(started) => Ok(Json(json!({"started":started}))),
        Err(error) => {
            let status = match error.code {
                "capability.unsupported" | "provider.descriptor_unknown" | "runtime.access_mode_invalid" => StatusCode::BAD_REQUEST,
                code if code.starts_with("credential.") => StatusCode::BAD_REQUEST,
                "runtime.workspace_missing" => StatusCode::NOT_FOUND,
                _ => StatusCode::BAD_GATEWAY,
            };
            Err(typed(status, error.code, error.message))
        }
    }
}
