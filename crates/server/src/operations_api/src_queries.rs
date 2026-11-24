//! Source SRC QueryAPI routing for Task-scoped reads.
use super::*;

pub(super) fn supports(kind: &str) -> bool {
    matches!(kind, "src.task.list" | "src.task.get" | "src.run.list"
        | "src.record.list" | "src.record.get"
        | "src.exchange.read" | "src.exchange.query" | "src.exchange.draft.read"
        | "src.exchange.compare" | "src.exchange.diff" | "src.exchange.curl_export" | "src.exchange.export"
        | "src.traffic.search" | "src.traffic.read_bytes"
        | "src.diagnostics.wire" | "src.diagnostics.mcp_circuits"
        | "src.proxy_bridge.status" | "src.proxy_bridge.ca_cert" | "src.proxypool.status"
        | "src.mutator.list" | "src.intel.list"
        | "src.workspace.tables" | "src.workspace.sample" | "src.notify.channel.list"
        | "src.report.get" | "src.policy.current"
        | "src.research.records" | "src.research.record.get" | "src.research.plan.current"
        | "src.research.skill.read" | "src.research.openapi.read"
        | "src.research.openapi.compare" | "src.research.javascript.read"
        | "src.research.graphql_grpc.auth_matrix"
        | "src.research.backlog" | "src.research.history"
        | "src.research.precondition.summary"
        | "src.coverage.audit" | "src.coordination.status" | "src.asset.detail" | "src.asset.published"
        | "src.asset.query" | "src.asset.saved_query.read" | "src.asset.full_text"
        | "src.asset.pivot" | "src.asset.export" | "src.asset.fold_routes"
        | "src.connector.manifests" | "src.connector.manifest" | "src.connector.config"
        | "src.connector.status" | "src.connector.batch.list" | "src.connector.batch.get"
        | "src.ingestion.preview" | "src.monitor.preflight" | "src.intel.status"
        | "src.credential.list"
        | "src.workbench.evidence.verify" | "src.workbench.todo.list"
        | "src.workbench.history" | "src.workbench.report.drafts"
        | "src.workbench.report.diff" | "src.workbench.suggestions"
        | "src.execution.capabilities" | "src.execution.receipt"
        | "src.artifact.read" | "src.asset.version.list"
        | "src.browser.context" | "src.browser.generation" | "src.browser.image.read"
        | "src.browser.service" | "src.feedback.diff" | "src.feedback.history"
        | "src.feedback.platform_status" | "src.finding.claim.get" | "src.finding.claims"
        | "src.finding.deduplications" | "src.finding.reviews" | "src.finding.status"
        | "src.identity.compare" | "src.identity.get" | "src.identity.verifications" | "src.identity.login_recipe.get"
        | "src.identity.session.status" | "src.identity.contexts"
        | "src.inspection.requests" | "src.knowledge.history" | "src.knowledge.read"
        | "src.knowledge.search" | "src.knowledge.source.read"
        | "src.monitor.changes" | "src.monitor.get" | "src.monitor.list"
        | "src.monitor.notifications" | "src.monitor.scheduler_health"
        | "src.oast.events" | "src.object.history" | "src.policy.preview"
        | "src.report.list" | "src.report.templates" | "src.retest.recover"
        | "src.stream.export" | "src.workbench.asset.bulk"
        | "src.workbench.asset.links" | "src.workbench.asset.tasklinks"
        | "src.workbench.duplicate.verdicts" | "src.workbench.duplicates"
        | "src.workbench.finding.merges" | "src.workbench.finding.meta"
        | "src.workbench.slice.get" | "src.workbench.snapshot"
        | "src.project.memory.get")
}

fn authorized_caller(
    state: &AppState,
    thread_id: &str,
) -> Result<crate::runs::src_api::AuthorizedSrcCaller, Failure> {
    match crate::runs::src_api::resolve_authorized_src_caller(state, thread_id) {
        Ok(Some(caller)) => Ok(caller),
        Ok(None) => Err(fail(
            StatusCode::FORBIDDEN,
            "task_binding_invalid",
            "Thread is not bound to exactly one SRC Task",
        )),
        Err(error) => Err((
            error.status,
            Json(json!({"error":{"code":error.code,"message":error.message,"detail":error.detail}})),
        )),
    }
}

fn check_ref_error(
    caller: &crate::runs::src_api::AuthorizedSrcCaller,
    kind: &str,
    id: &str,
) -> Result<(), Failure> {
    crate::runs::src_api::check_delegated_ref_access(caller, kind, id).map_err(|error| {
        (
            error.status,
            Json(json!({"error":{"code":error.code,"message":error.message,"detail":error.detail}})),
        )
    })
}

fn non_empty_str(v: &Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}

#[allow(dead_code)]
fn check_delegated_query_refs(
    caller: &crate::runs::src_api::AuthorizedSrcCaller,
    kind: &str,
    params: &Value,
) -> Result<(), Failure> {
    if caller.delegation.is_none() {
        return Ok(());
    }
    let non_empty = non_empty_str;
    match kind {
        "src.asset.detail" | "src.asset.version.list" => {
            check_ref_error(caller, "asset", non_empty(&params["asset_id"]).unwrap_or(""))?;
        }
        "src.exchange.read" | "src.exchange.curl_export" => {
            check_ref_error(caller, "exchange", non_empty(&params["exchange_id"]).unwrap_or(""))?;
        }
        "src.exchange.compare" | "src.exchange.diff" => {
            let mut checked = false;
            for key in [
                "exchange_id",
                "left_exchange_id",
                "right_exchange_id",
                "from_exchange_id",
                "to_exchange_id",
                "a_exchange_id",
                "b_exchange_id",
                "base_exchange_id",
                "target_exchange_id",
            ] {
                if let Some(id) = non_empty(&params[key]) {
                    checked = true;
                    check_ref_error(caller, "exchange", id)?;
                }
            }
            if !checked {
                check_ref_error(caller, "exchange", "")?;
            }
        }
        "src.report.get" => {
            check_ref_error(caller, "report", non_empty(&params["report_id"]).unwrap_or(""))?;
        }
        "src.workbench.report.diff" | "src.feedback.diff" => {
            let mut checked = false;
            for key in ["report_id", "from_report_id", "to_report_id", "old_report_id", "new_report_id"] {
                if let Some(id) = non_empty(&params[key]) {
                    checked = true;
                    check_ref_error(caller, "report", id)?;
                }
            }
            if !checked {
                check_ref_error(caller, "report", "")?;
            }
        }
        "src.record.get" | "src.research.record.get" => {
            let raw_kind = non_empty(&params["kind"]).unwrap_or("record");
            let rec_kind = if raw_kind == "workbench_report_draft" {
                "report"
            } else {
                raw_kind
            };
            let rec_id = non_empty(&params["record_id"])
                .or_else(|| non_empty(&params["id"]))
                .unwrap_or("");
            check_ref_error(caller, rec_kind, rec_id)?;
        }
        "src.artifact.read" => {
            let ref_data = &params["ref_data"];
            let art_id = non_empty(&ref_data["artifact_id"])
                .or_else(|| non_empty(&ref_data["sha256"]))
                .or_else(|| non_empty(&params["artifact_id"]))
                .or_else(|| non_empty(&params["sha256"]))
                .unwrap_or("");
            check_ref_error(caller, "artifact", art_id)?;
        }
        "src.finding.claim.get" => {
            let fid = non_empty(&params["finding_id"]);
            let cid = non_empty(&params["claim_id"]);
            match (fid, cid) {
                (Some(f), Some(c)) => {
                    if check_ref_error(caller, "finding", f).is_err() {
                        check_ref_error(caller, "finding", c)?;
                    }
                }
                (Some(f), None) => check_ref_error(caller, "finding", f)?,
                (None, Some(c)) => check_ref_error(caller, "finding", c)?,
                (None, None) => check_ref_error(caller, "finding", "")?,
            }
        }
        "src.workbench.evidence.verify" => {
            let ref_data = &params["ref_data"];
            let mut checked = false;
            for (rk, field) in [
                ("artifact", "artifact_id"),
                ("artifact", "sha256"),
                ("exchange", "exchange_id"),
                ("asset", "asset_id"),
                ("finding", "finding_id"),
                ("report", "report_id"),
            ] {
                if let Some(id) = non_empty(&ref_data[field]) {
                    checked = true;
                    check_ref_error(caller, rk, id)?;
                }
            }
            if let Some(id) = non_empty(&ref_data["record_id"]) {
                checked = true;
                let rk = non_empty(&ref_data["kind"]).unwrap_or("record");
                check_ref_error(caller, rk, id)?;
            }
            if !checked {
                let ev_id = non_empty(&params["evidence_id"])
                    .or_else(|| non_empty(&ref_data["evidence_id"]))
                    .unwrap_or("");
                check_ref_error(caller, "exchange", ev_id)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn run_list_projection(state: &AppState, task_id: &str) -> Result<Value, Failure> {
    let bindings = crate::runs::src_api::query_projection(
        state, task_id, "src.run.list", &json!({}))
        .map_err(|(status, body)| (status, Json(body)))?;
    let mut rows = Vec::new();
    for binding in bindings.as_array().into_iter().flatten() {
        if !binding["destroyed_at"].is_null() {
            continue;
        }
        let run_id = binding["run_id"].as_str().unwrap_or("");
        let snapshot = super::run_queries::snapshot(state, run_id)?;
        if snapshot["task_kind"] != "pentest.target" {
            return Err(fail(StatusCode::FORBIDDEN, "task_binding_invalid",
                "Run belongs to another product"));
        }
        rows.push(json!({"binding":binding,"run":snapshot}));
    }
    Ok(json!(rows))
}

pub(super) fn query(
    state: &Arc<AppState>, headers: &HeaderMap, gateway_candidate: bool,
    envelope: Value,
) -> Result<Response, Failure> {
    let result = execute(state, headers, gateway_candidate, envelope)?;
    Ok(Json(json!({"result":result})).into_response())
}

fn execute(
    state: &Arc<AppState>, headers: &HeaderMap, gateway_candidate: bool,
    mut envelope: Value,
) -> Result<Value, Failure> {
    crate::business::require(state,crate::business::Module::Src).map_err(|e|e.failure())?;
    let kind = envelope["query_type"].as_str().unwrap_or("").to_owned();
    if envelope.get("params").is_none() { envelope["params"] = json!({}); }
    if !envelope["params"].is_object() {
        return Err(fail(StatusCode::BAD_REQUEST, "platform.request.invalid", "params must be an object"));
    }
    let task_list = kind == "src.task.list";
    let aggregate_type = envelope["aggregate_type"].as_str().unwrap_or("task");
    let aggregate_id = envelope["aggregate_id"].as_str().unwrap_or("");
    let requested_task = envelope["params"]["task_id"].as_str().unwrap_or("");
    let (scope_type, scope_id) = if task_list {
        ("task", String::new())
    } else { match aggregate_type {
        "thread" if !aggregate_id.is_empty() => ("thread", aggregate_id.to_owned()),
        "task" if !requested_task.is_empty() || !aggregate_id.is_empty() =>
            ("task", if requested_task.is_empty() { aggregate_id } else { requested_task }.to_owned()),
        _ => return Err(fail(StatusCode::BAD_REQUEST, "platform.request.invalid",
            "SRC query needs a Task or Thread aggregate")),
    }};
    let principal = platform_auth::authorize(state, headers, gateway_candidate,
        &envelope, "query", &kind, scope_type, &scope_id)?;
    let mut task_id = if task_list { String::new() } else if scope_type == "thread" {
        let bound = authorized_caller(state, &scope_id)?;
        let id = bound.task_id.clone();
        if !requested_task.is_empty() && requested_task != id {
            return Err(fail(StatusCode::FORBIDDEN, "task_binding_invalid",
                "requested Task does not belong to the Thread"));
        }
        id
    } else {
        scope_id.clone()
    };
    if principal.starts_with("binding:") {
        let token = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer ")).unwrap_or("").trim();
        let owner = state.capability_gateway.owner_for_token(token)
            .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "capability.token_invalid",
                "Gateway token is unknown or retired"))?;
        let bound = authorized_caller(state, &owner)?;
        if (!task_list && bound.task_id != task_id)
            || (scope_type == "thread" && owner != scope_id)
        {
            return Err(fail(StatusCode::FORBIDDEN, "task_binding_invalid",
                "Thread does not belong to this SRC Task"));
        }
        if task_list { task_id = bound.task_id.clone(); }
        let _ = bound;
    }
    if envelope.get("query_id").is_some_and(|value| !value.is_string() && !value.is_null()) {
        return Err(fail(StatusCode::BAD_REQUEST, "platform.request.invalid",
            "query_id must be a string"));
    }
    if envelope["query_id"].as_str().is_none_or(str::is_empty) {
        envelope["query_id"] = json!(new_id("qry"));
    }
    let agent = principal.starts_with("binding:");
    if kind.starts_with("src.knowledge.") {
        let actor_id = if agent {envelope["actor"]["id"].as_str().unwrap_or("")}
            else {principal.strip_prefix("operator:").unwrap_or("local-user")}.to_owned();
        if envelope["params"].get("principal_id").is_none_or(|v| v.is_null()) {
            envelope["params"]["principal_id"] = json!(actor_id);
        }
    }
    let projection = match kind.as_str() {
        "src.task.list" if agent => json!([crate::runs::src_api::query_projection(
            state, &task_id, "src.task.get", &json!({}))
            .map_err(|(status, body)| (status, Json(body)))?]),
        "src.task.list" => json!(crate::runs::src_api::tasks(state).map_err(|error|
            fail(StatusCode::INTERNAL_SERVER_ERROR, "storage.unavailable", error))?),
        "src.record.list" => crate::runs::src_api::query_records_for_actor(
            state, &task_id, &envelope["params"], agent, &principal)
            .map_err(|(status, body)| (status, Json(body)))?,
        "src.research.records" => crate::runs::src_api::query_research_records_for_actor(
            state, &task_id, &envelope["params"], agent)
            .map_err(|(status, body)| (status, Json(body)))?,
        "src.run.list" => run_list_projection(state, &task_id)?,
        "src.workbench.evidence.verify" =>
            crate::runs::src_api::query_workbench_evidence_projection(
                state, &task_id, &envelope["params"],
                if agent { envelope["actor"]["id"].as_str().unwrap_or("") }
                else { "local-user" },
            ).map_err(|(status, body)| (status, Json(body)))?,
        _ => crate::runs::src_api::query_projection(state, &task_id, &kind, &envelope["params"])
            .map_err(|(status, body)| (status, Json(body)))?,
    };
    Ok(json!({"query_id":envelope["query_id"],
        "query_type":kind,"result":projection,"cursor":null}))
}

pub(super) fn tool_query(
    state: &Arc<AppState>, token: &str, owner: &str, kind: &str, args: &Value,
) -> Result<Value, Value> {
    let caller = authorized_caller(state, owner).map_err(tool_error)?;
    let principal = caller.principal_id.as_str();
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| json!({"code":"capability.token_invalid","message":"Invalid bearer token"}))?);
    let mut actor = json!({"kind":"agent","id":principal,"thread_id":owner});
    if caller.delegation.is_some() {
        actor["subthread_id"] = json!(owner);
    }
    let envelope = json!({"query_type":kind,
        "aggregate_type":if kind == "src.task.list" {"task"} else {"thread"},
        "aggregate_id":if kind == "src.task.list" {""} else {owner},
        "actor":actor,
        "params":args});
    let result = execute(state, &headers, true, envelope).map_err(tool_error)?;
    Ok(result["result"].clone())
}

pub(super) async fn tool_command(
    state: &Arc<AppState>, token: &str, owner: &str, kind: &str, args: &Value,
) -> Result<Value, Value> {
    let caller = authorized_caller(state, owner).map_err(tool_error)?;
    let task_id = caller.task_id.as_str();
    let principal = caller.principal_id.as_str();
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| json!({"code":"capability.token_invalid","message":"Invalid bearer token"}))?);
    let mut payload = args.clone();
    let command_id = payload.as_object_mut()
        .and_then(|object| object.remove("command_id"))
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("cmd-{}", uuid::Uuid::new_v4().simple()));
    let expected_version = payload.as_object_mut()
        .and_then(|object| object.remove("expected_version"));
    let idempotency_key = payload.as_object_mut()
        .and_then(|object| object.remove("idempotency_key"));
    let mut actor = json!({"kind":"agent","id":principal,"thread_id":owner});
    if caller.delegation.is_some() {
        actor["subthread_id"] = json!(owner);
    }
    let mut envelope = json!({"command_id":command_id,"command_type":kind,
        "aggregate_type":"task","aggregate_id":task_id,
        "actor":actor,
        "payload":payload});
    if let Some(value) = expected_version { envelope["expected_version"] = value; }
    if let Some(value) = idempotency_key { envelope["idempotency_key"] = value; }
    let response = super::command(State(state.clone()), headers,
        Some(Extension(crate::access_api::GatewayCandidate)), Some(Json(envelope)))
        .await.map_err(tool_error)?;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await
        .map_err(|error| json!({"code":"capability.response_invalid","message":error.to_string()}))?;
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|error| json!({"code":"capability.response_invalid","message":error.to_string()}))?;
    if !status.is_success() {
        let mut error = body["error"].clone();
        error["status"] = json!(status.as_u16());
        return Err(error);
    }
    Ok(body["receipt"].clone())
}

fn tool_error((status, Json(body)): Failure) -> Value {
    let mut error = body["error"].clone();
    error["status"] = json!(status.as_u16());
    error
}
