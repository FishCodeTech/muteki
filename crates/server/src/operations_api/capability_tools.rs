//! Catalog tools use the same typed, authorized entry points as the product UI.
use super::*;

pub(super) async fn invoke(state: &Arc<AppState>, token: &str, binding: &Value, name: &str, arguments: &Value) -> Result<Value,Failure> {
    let spec = crate::capabilities::source_tool(name)
        .ok_or_else(||fail(StatusCode::BAD_REQUEST,"capability.tool_unknown","Unknown catalog tool"))?;
    let actor = json!({"kind":"agent","id":binding["principal_id"],"binding_id":binding["binding_id"],
        "binding_version":binding["binding_version"],"thread_id":binding["thread_id"]});
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION,HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_|fail(StatusCode::UNAUTHORIZED,"capability.token_invalid","Invalid gateway token"))?);
    let arg = spec["aggregate_arg"].as_str().unwrap_or("");
    let kind = spec["aggregate_type"].as_str().unwrap_or("");
    let id = if spec["aggregate_from_caller_thread"] == true { binding["thread_id"].as_str().unwrap_or("") }
        else { arguments[arg].as_str().unwrap_or("").trim() };
    let target = spec["target_kind"].as_str().unwrap_or("");
    if matches!(target,"read_events" | "wait") && id.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST,"capability.argument_required",format!("{name} requires arguments.{arg}")));
    }
    if binding["mode"] == "src" {
        let owner = binding["thread_id"].as_str().unwrap_or("");
        let task = crate::runs::src_api::task_for_thread(state,owner).map_err(|(status,body)|(status,Json(body)))?;
        if let Some(run_id) = arguments["run_id"].as_str().filter(|value| !value.is_empty()) {
            let task_id = task["task_id"].as_str().unwrap_or("");
            let runs = crate::runs::src_api::query_projection(state,task_id,"src.run.list",&json!({}))
                .map_err(|(status,body)|(status,Json(body)))?;
            if !runs.as_array().into_iter().flatten().any(|row| row["run_id"] == run_id && row["destroyed_at"].is_null()) {
                return Err(fail(StatusCode::FORBIDDEN,"task_binding_invalid","Run does not belong to the calling SRC Task"));
            }
        }
    }
    let response = match target {
        "command" => {
            let mut payload = spec["payload_defaults"].as_object().cloned().unwrap_or_default();
            payload.extend(arguments.as_object().into_iter().flatten()
                .filter(|(key,_)| !matches!(key.as_str(),"command_id" | "idempotency_key" | "expected_version"))
                .map(|(key,value)|(key.clone(),value.clone())));
            let mut envelope = json!({"command_type":spec["command_type"],"aggregate_type":kind,"aggregate_id":id,"actor":actor,"payload":payload});
            for field in ["command_id","idempotency_key","expected_version"] {
                if let Some(value) = arguments.get(field) { envelope[field] = value.clone(); }
            }
            command(State(state.clone()),headers,Some(Extension(crate::access_api::GatewayCandidate)),Some(Json(envelope))).await?
        },
        "query" => {
            let envelope = json!({"query_type":spec["query_type"],"aggregate_type":kind,"aggregate_id":id,"actor":actor,"params":arguments});
            query(State(state.clone()),headers,Some(Extension(crate::access_api::GatewayCandidate)),Some(Json(envelope))).await?
        },
        "read_events" | "wait" => {
            let mut body = arguments.clone();
            body["aggregate_type"] = json!(kind);
            body["aggregate_id"] = json!(id);
            body["actor"] = actor;
            body.as_object_mut().unwrap().remove(arg);
            let result = if target == "wait" {
                platform_events::wait(State(state.clone()),headers,Some(Extension(crate::access_api::GatewayCandidate)),Some(Json(body))).await?
            } else {
                platform_events::read(State(state.clone()),headers,Some(Extension(crate::access_api::GatewayCandidate)),Some(Json(body))).await?
            };
            return Ok(json!({"result":result.0}));
        },
        "receipt" => {
            let command_id = arguments["command_id"].as_str().unwrap_or("").trim();
            let receipt = state.store.command_receipt(command_id).map_err(storage)?
                .ok_or_else(||fail(StatusCode::NOT_FOUND,"receipt.not_found","Command receipt was not found"))?;
            return Ok(json!({"receipt":receipt}));
        },
        _ => return Err(fail(StatusCode::BAD_REQUEST,"capability.target_invalid","Unsupported catalog target")),
    };
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(),usize::MAX).await.map_err(storage)?;
    let body: Value = serde_json::from_slice(&bytes).map_err(storage)?;
    if !status.is_success() {
        if body["error"].is_object() { return Err((status,Json(body))); }
        if body["receipt"]["error"].is_object() { return Err((status,Json(json!({"error":body["receipt"]["error"]})))); }
        return Err(fail(status,"capability.dispatch_failed",body["detail"].as_str().unwrap_or("Catalog operation failed")));
    }
    Ok(if target == "command" {json!({"receipt":body["receipt"]})}else{json!({"result":body["result"]["result"]})})
}
