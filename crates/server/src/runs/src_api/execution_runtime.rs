//! Durable execution admission shared by the capability gateway, native hooks and transparent inspection.
use super::*;

fn transaction<T>(s: &AppState, work: impl FnOnce() -> Api<T>) -> Api<T> {
    s.store.document_scope(work).map_err(db)?
}
pub(super) fn receipt(s: &AppState, id: &str, invocation: &str) -> Api<Value> {
    Ok(rec(s, id, "execution_receipt", invocation)?["payload"].clone())
}
fn map_forbidden_or_conflict(error: Error, as_conflict: bool) -> Error {
    if as_conflict && error.code == "task_binding_invalid" {
        Error::new(StatusCode::CONFLICT, "authorization_stale", error.message)
    } else {
        error
    }
}

pub(crate) fn check_delegated_scope(
    _s: &AppState,
    caller: &AuthorizedSrcCaller,
    operation: &str,
    _tool_name: Option<&str>,
    _payload: &Value,
    allow_readonly_workbench: bool,
) -> Api<()> {
    let Some(delegation) = caller.delegation.as_ref() else {
        return Ok(());
    };
    let profile = delegation["profile"]
        .as_str()
        .unwrap_or("readonly_analysis");
    if profile == "readonly_analysis" {
        if !allow_readonly_workbench
            || !matches!(
                operation,
                "src.workbench.suggestion.propose" | "src.workbench.todo.save"
            )
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.delegation_readonly",
                "只读分析子线程无权执行该 SRC 操作",
            ));
        }
        return Ok(());
    }
    if profile == "scoped_execution" {
        return Ok(());
    }
    Err(Error::new(
        StatusCode::FORBIDDEN,
        "src.delegation_readonly",
        "未知委派权限档位，默认拒绝写操作",
    ))
}

fn live_run_generation(s: &AppState, run_id: &str) -> Api<Option<u64>> {
    if run_id.is_empty() {
        return Ok(None);
    }
    Ok(s.store
        .get("runs", run_id)
        .map_err(db)?
        .and_then(|run| run["execution_generation"].as_u64()))
}

pub(super) fn admit(s: &AppState, id: &str, request: &Value, ttl_seconds: i64) -> Api<Value> {
    execution_policy::validate(request)?;
    transaction(s, || {
        let invocation = require(request, "invocation_id")?;
        let req_tid = request["thread_id"].as_str().unwrap_or("");
        let caller = resolve_authorized_src_caller(s, req_tid)?.ok_or_else(|| {
            Error::new(
                StatusCode::FORBIDDEN,
                "src.task_binding_invalid",
                "execution Task binding is stale",
            )
        })?;
        if request["task_id"] != id
            || caller.task_id != id
            || caller.task["lifecycle"]["state"] != "active"
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.task_binding_invalid",
                "execution Task binding is stale",
            ));
        }
        check_delegated_scope(
            s,
            &caller,
            request["action"].as_str().unwrap_or(""),
            request["tool_name"].as_str(),
            &request["arguments"],
            false,
        )?;
        if let Ok(prior) = rec(s, id, "execution_request", invocation) {
            if prior["payload"] != *request {
                return Err(Error::conflict(
                    "invocation already bound to another execution request",
                ));
            }
            return receipt(s, id, invocation);
        }
        let p = policy(s, id)?;
        if request["policy_version"] != p["version"] {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "authorization_stale",
                "execution policy changed",
            ));
        }
        let mut decision = execution_policy::evaluate(&p, request)?;
        execution_policy::apply_grant(s, id, &p, request, &mut decision)?;
        let unresolved = decision["status"] == "unresolved";
        let effect = decision["effect"].as_str().unwrap_or("deny");
        let state = match (unresolved, effect) {
            (false, "allow") => "ready",
            (false, "ask") => "waiting_approval",
            _ => "denied",
        };
        let decision_id = format!("sdec-{}", uuid::Uuid::new_v4().simple());
        let approval_id = if state == "waiting_approval" {
            format!("sapproval-{}", uuid::Uuid::new_v4().simple())
        } else {
            String::new()
        };
        decision["decision_id"] = json!(decision_id);
        decision["invocation_id"] = json!(invocation);
        decision["policy_version"] = p["version"].clone();
        decision["policy_digest"] = p["digest"].clone();
        decision["approval_id"] = json!(approval_id);
        decision["timestamp"] = json!(now());
        let control_epoch = request["control_epoch"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| s.chat.epoch());
        let run_execution_generation = request
            .get("run_execution_generation")
            .and_then(Value::as_u64)
            .or_else(|| {
                request["run_id"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .and_then(|run_id| live_run_generation(s, run_id).ok().flatten())
            });
        save_rec(
            s,
            id,
            "execution_request",
            invocation,
            request.clone(),
            Some(0),
        )?;
        save_rec(
            s,
            id,
            "policy_decision",
            invocation,
            decision.clone(),
            Some(0),
        )?;
        if state == "waiting_approval" {
            let expires =
                chrono::Utc::now() + chrono::Duration::seconds(ttl_seconds.clamp(1, 86400));
            save_rec(
                s,
                id,
                "execution_approval",
                &approval_id,
                json!({
                    "approval_id": approval_id, "invocation_id": invocation, "request_snapshot":request,
                    "input_ref":request["arguments_hash"], "matched_rule_refs":decision["matched_rule_refs"],
                    "policy_version":p["version"], "policy_digest":p["digest"], "status":"pending",
                    "requested_at":now(), "expires_at":expires.to_rfc3339(), "decided_by":"",
                    "decided_at":null, "decision":null, "grant_id":null, "consumed_at":null
                }),
                Some(0),
            )?;
        }
        let value = json!({"invocation_id":invocation,"decision_id":decision_id,"approval_id":approval_id,"dispatch_state":state,"policy_version":p["version"],"policy_digest":p["digest"],"control_epoch":control_epoch,"run_execution_generation":run_execution_generation,"started_at":null,"ended_at":if state=="denied"{json!(now())}else{Value::Null},"external_receipt_ref":"","result_ref":null,"result":null,"error_code":if state=="denied"{decision["reason_code"].clone()}else{json!("")},"outcome_known":state=="denied","history":[{"state":state,"at":now()}]});
        save_rec(
            s,
            id,
            "execution_receipt",
            invocation,
            value.clone(),
            Some(0),
        )?;
        Ok(value)
    })
}
fn transition(
    s: &AppState,
    id: &str,
    invocation: &str,
    mut value: Value,
    state: &str,
) -> Api<Value> {
    let row = rec(s, id, "execution_receipt", invocation)?;
    value["dispatch_state"] = json!(state);
    if !value["history"].is_array() {
        value["history"] = json!([]);
    }
    let error = value["error_code"].clone();
    value["history"]
        .as_array_mut()
        .unwrap()
        .push(json!({"state":state,"at":now(),"error_code":error}));
    Ok(save_rec(
        s,
        id,
        "execution_receipt",
        invocation,
        value,
        row["revision"].as_u64(),
    )?["payload"]
        .clone())
}
fn current_authority_inner(
    s: &AppState,
    id: &str,
    thread: Option<&str>,
    version: &Value,
    digest: &Value,
) -> Api<(Value, Option<AuthorizedSrcCaller>)> {
    let caller = if let Some(tid) = thread {
        let found = resolve_authorized_src_caller(s, tid)
            .map_err(|e| map_forbidden_or_conflict(e, true))?
            .ok_or_else(|| {
                Error::new(
                    StatusCode::CONFLICT,
                    "authorization_stale",
                    "execution Task binding is no longer active",
                )
            })?;
        if found.task_id != id
            || found.task["lifecycle"]["state"] != "active"
            || found.task["binding"].is_null()
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "authorization_stale",
                "execution Task binding is no longer active",
            ));
        }
        Some(found)
    } else {
        let t = task(s, id)?;
        if t["lifecycle"]["state"] != "active" || t["binding"].is_null() {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "authorization_stale",
                "execution Task binding is no longer active",
            ));
        }
        None
    };
    let p = policy(s, id)?;
    if (!version.is_null() && *version != p["version"])
        || (!digest.is_null() && *digest != p["digest"])
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "authorization_stale",
            "execution policy changed before dispatch",
        ));
    }
    if let Some(found) = caller.as_ref()
        && let Some(delegation) = found.delegation.as_ref()
    {
        let profile = delegation["profile"]
            .as_str()
            .unwrap_or("readonly_analysis");
        if profile == "readonly_analysis" {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.delegation_readonly",
                "只读分析子线程无权执行该 SRC 操作",
            ));
        }
    }
    Ok((p, caller))
}
/// Re-read the authorization snapshot inside the same short transaction as dispatch.
/// A policy activation marker fences new work before existing connections are quiesced.
pub(super) fn current_authority(
    s: &AppState,
    id: &str,
    thread: Option<&str>,
    version: &Value,
    digest: &Value,
) -> Api<Value> {
    Ok(current_authority_inner(s, id, thread, version, digest)?.0)
}
pub(super) fn consume_approval(s: &AppState, id: &str, aid: &str, p: &Value) -> Api<Value> {
    let row = rec(s, id, "execution_approval", aid)?;
    let mut approval = row["payload"].clone();
    let expired = approval["expires_at"]
        .as_str()
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .is_none_or(|at| at <= chrono::Utc::now());
    if approval["status"] != "approved"
        || expired
        || !approval["consumed_at"].is_null()
        || approval["policy_version"] != p["version"]
        || (!approval["policy_digest"].is_null() && approval["policy_digest"] != p["digest"])
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "approval_expired",
            "approval is unavailable, consumed or stale",
        ));
    }
    approval["consumed_at"] = json!(now());
    save_rec(
        s,
        id,
        "execution_approval",
        aid,
        approval.clone(),
        row["revision"].as_u64(),
    )?;
    Ok(approval)
}
pub(super) fn dispatch(s: &AppState, id: &str, invocation: &str) -> Api<Value> {
    transaction(s, || {
        let mut value = receipt(s, id, invocation)?;
        if !matches!(
            value["dispatch_state"].as_str(),
            Some("ready" | "waiting_approval")
        ) {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "invocation_replayed",
                "execution has already crossed its dispatch boundary",
            ));
        }
        let request = rec(s, id, "execution_request", invocation)?["payload"].clone();
        let (p, caller) = current_authority_inner(
            s,
            id,
            request["thread_id"].as_str(),
            &request["policy_version"],
            &value["policy_digest"],
        )?;
        if let Some(caller) = caller.as_ref() {
            check_delegated_scope(
                s,
                caller,
                request["action"].as_str().unwrap_or(""),
                request["tool_name"].as_str(),
                &request["arguments"],
                false,
            )?;
        }
        if value["control_epoch"]
            .as_str()
            .is_some_and(|epoch| !epoch.is_empty() && epoch != s.chat.epoch())
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "authorization_stale",
                "Execution belongs to a prior control epoch",
            ));
        }
        if let (Some(run_id), Some(generation)) = (
            request["run_id"].as_str().filter(|id| !id.is_empty()),
            value["run_execution_generation"].as_u64(),
        ) && live_run_generation(s, run_id)? != Some(generation)
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "authorization_stale",
                "Run execution generation changed",
            ));
        }
        if value["dispatch_state"] == "ready" {
            let mut current = execution_policy::evaluate(&p, &request)?;
            execution_policy::apply_grant(s, id, &p, &request, &mut current)?;
            if current["effect"] != "allow" {
                return Err(Error::new(
                    StatusCode::CONFLICT,
                    "authorization_stale",
                    "Execution needs fresh policy admission",
                ));
            }
        }
        if value["dispatch_state"] == "waiting_approval" {
            let aid = require(&value, "approval_id")?;
            let row = rec(s, id, "execution_approval", aid)?;
            let mut approval = row["payload"].clone();
            if approval["invocation_id"] != invocation || approval["request_snapshot"] != request {
                return Err(Error::conflict("approval does not match this execution"));
            }
            let expired = approval["expires_at"]
                .as_str()
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                .is_none_or(|at| at <= chrono::Utc::now());
            if expired
                || matches!(
                    approval["status"].as_str(),
                    Some("denied" | "expired" | "cancelled" | "superseded")
                )
            {
                if expired && approval["status"] == "pending" {
                    approval["status"] = json!("expired");
                    save_rec(
                        s,
                        id,
                        "execution_approval",
                        aid,
                        approval,
                        row["revision"].as_u64(),
                    )?;
                }
                value["error_code"] = json!(if expired {
                    "approval_expired"
                } else {
                    "policy_denied"
                });
                value["ended_at"] = json!(now());
                value["outcome_known"] = json!(true);
                return transition(s, id, invocation, value, "denied");
            }
            if approval["status"] != "approved" {
                return Err(Error::new(
                    StatusCode::CONFLICT,
                    "approval_required",
                    "execution awaits an operator decision",
                ));
            }
            consume_approval(s, id, aid, &p)?;
        }
        value["started_at"] = json!(now());
        transition(s, id, invocation, value, "dispatched")
    })
}
fn settle_inner(
    s: &AppState,
    id: &str,
    invocation: &str,
    result: Value,
    error: Option<&str>,
    unknown: bool,
    allow_prior_epoch: bool,
) -> Api<Value> {
    transaction(s, || {
        let mut value = receipt(s, id, invocation)?;
        if matches!(
            value["dispatch_state"].as_str(),
            Some("completed" | "failed" | "denied" | "outcome_unknown")
        ) {
            return Ok(value);
        }
        if !allow_prior_epoch {
            if let Some(rec_epoch) = value["control_epoch"].as_str().filter(|v| !v.is_empty())
                && rec_epoch != s.chat.epoch()
            {
                return Err(Error::new(
                    StatusCode::CONFLICT,
                    "authorization_stale",
                    "execution receipt belongs to a prior control epoch",
                ));
            }
            if !unknown
                && let Some(rec_gen) = value["run_execution_generation"].as_u64()
                && let Ok(request_row) = rec(s, id, "execution_request", invocation)
                && let Some(run_id) = request_row["payload"]["run_id"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                && live_run_generation(s, run_id)?.is_some_and(|live| live != rec_gen)
            {
                return Err(Error::new(
                    StatusCode::CONFLICT,
                    "authorization_stale",
                    "Run execution_generation changed before completion",
                ));
            }
        }
        let state = if unknown {
            "outcome_unknown"
        } else if error.is_some() {
            "failed"
        } else {
            "completed"
        };
        value["result"] = result;
        value["error_code"] = json!(error.unwrap_or(""));
        value["outcome_known"] = json!(!unknown);
        value["ended_at"] = json!(now());
        transition(s, id, invocation, value, state)
    })
}
pub(super) fn settle(
    s: &AppState,
    id: &str,
    invocation: &str,
    result: Value,
    error: Option<&str>,
    unknown: bool,
) -> Api<Value> {
    settle_inner(s, id, invocation, result, error, unknown, false)
}
pub(super) fn settle_from_recovery(
    s: &AppState,
    id: &str,
    invocation: &str,
    result: Value,
    error: Option<&str>,
    unknown: bool,
) -> Api<Value> {
    settle_inner(s, id, invocation, result, error, unknown, true)
}
pub(super) fn approval(s: &AppState, id: &str, invocation: &str) -> Api<Option<Value>> {
    let receipt = receipt(s, id, invocation)?;
    let Some(aid) = receipt["approval_id"]
        .as_str()
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(rec(s, id, "execution_approval", aid)?))
}

pub(crate) fn task_for_thread(s: &AppState, tid: &str) -> Api<Option<String>> {
    let doc = chat::load(s, tid)
        .map_err(|_| Error::conflict("Conversation policy owner is unavailable"))?;
    if let Some(caller) = resolve_authorized_src_caller(s, tid)? {
        return Ok(Some(caller.task_id));
    }
    if doc["thread"]["mode"] != "src"
        && !doc["src_policy_gates"].is_object()
        && doc["native_session"]["src_task_id"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        return Ok(None);
    }
    Err(Error::conflict(
        "SRC conversation has no active Task binding",
    ))
}
pub(super) fn pending_row(
    s: &AppState,
    id: &str,
    invocation: &str,
    delivery: &str,
) -> Api<Option<Value>> {
    let Some(row) = approval(s, id, invocation)? else {
        return Ok(None);
    };
    let value = &row["payload"];
    if value["status"] != "pending" {
        return Ok(None);
    };
    let request = &value["request_snapshot"];
    Ok(Some(
        json!({"approval_id":value["approval_id"],"approval_kind":if request["resource_kind"]=="http"{"http"}else{"tool"},"title":format!("SRC 执行审批：{}",request["tool_name"].as_str().unwrap_or("")),"tool_name":request["tool_name"],"input":request["arguments"],"status":"pending","requested_at":value["requested_at"],"options":[{"option_id":"src_allow_once","label":"允许一次","kind":"allow_once"},{"option_id":"src_deny","label":"拒绝","kind":"reject_once"}],"scopes":["once","task"],"response_capability":{"answerable":true},"src_execution":{"task_id":id,"invocation_id":invocation,"approval_id":value["approval_id"],"revision":row["revision"],"delivery":delivery}}),
    ))
}
pub(super) fn project_chat(s: &AppState, id: &str, invocation: &str) -> Api<()> {
    let t = task(s, id)?;
    let tid = require(&t["binding"], "thread_id")?;
    let row = pending_row(s, id, invocation, "managed")?;
    let aid = approval(s, id, invocation)?
        .map(|row| {
            row["payload"]["approval_id"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        })
        .unwrap_or_default();
    if aid.is_empty() {
        return Ok(());
    }
    let mut doc = chat::load(s, tid)
        .map_err(|error| Error::conflict(format!("SRC approval chat unavailable: {error:?}")))?;
    if let Some(mut row) = row {
        row["turn_id"] = doc["thread"]["state"]["running_turn_id"].clone();
        if !doc["thread"]["state"]["pending_approvals"].is_object() {
            doc["thread"]["state"]["pending_approvals"] = json!({});
        }
        if doc["thread"]["state"]["pending_approvals"][&aid] == row {
            return Ok(());
        }
        doc["thread"]["state"]["pending_approvals"][&aid] = row.clone();
        chat::emit(s, tid, &mut doc, "core.approval.requested", row);
    } else {
        if let Some(rows) = doc["thread"]["state"]["pending_approvals"].as_object_mut() {
            if rows.remove(&aid).is_none() {
                return Ok(());
            }
        }
        chat::emit(
            s,
            tid,
            &mut doc,
            "core.approval.resolved",
            json!({"approval_id":aid,"status":"resolved"}),
        );
    }
    crate::agent_runtime::translate::refresh_pending(&mut doc);
    chat::save(s, tid, &mut doc)
        .map_err(|error| Error::conflict(format!("SRC approval chat save failed: {error:?}")))
}
pub(super) async fn publish(s: &Arc<AppState>, id: &str, invocation: &str) -> Api<()> {
    {
        let _guard = s.chat.active.lock().await;
        project_chat(s, id, invocation)?;
    }
    if receipt(s, id, invocation)?["dispatch_state"] == "waiting_approval" {
        super::domains::notify_event(s, id, "approval_pending", invocation).await?;
    }
    Ok(())
}
pub(crate) fn is_managed_approval(doc: &Value, payload: &Value) -> bool {
    let aid = payload["approval_id"].as_str().unwrap_or("");
    doc["thread"]["state"]["pending_approvals"][aid]["src_execution"]["delivery"] == "managed"
}
pub(crate) fn decide_from_chat(
    s: &AppState,
    tid: &str,
    doc: &mut Value,
    payload: &Value,
) -> Api<Value> {
    let aid = require(payload, "approval_id")?;
    let row = doc["thread"]["state"]["pending_approvals"][aid].clone();
    let link = &row["src_execution"];
    let id = require(link, "task_id")?;
    if task(s, id)?["binding"]["thread_id"] != tid {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.task_binding_invalid",
            "approval belongs to another thread",
        ));
    }
    let decision = match payload["decision"].as_str().unwrap_or("") {
        "allow" | "approve" | "approve_once" => {
            if payload["scope"] == "task" {
                "approve_task_scope"
            } else {
                "approve_once"
            }
        }
        "approve_task_scope" => "approve_task_scope",
        "deny" | "reject" | "cancel" => "deny",
        _ => return Err(Error::invalid("invalid SRC approval decision")),
    };
    let current = rec(s, id, "execution_approval", require(link, "approval_id")?)?;
    let result = decide_approval(
        s,
        id,
        &json!({"approval_id":link["approval_id"],"base_revision":current["revision"],"decision":decision,"scope":payload.get("grant_scope").cloned().unwrap_or_else(||json!({}))}),
    )?;
    if let Some(rows) = doc["thread"]["state"]["pending_approvals"].as_object_mut() {
        rows.remove(aid);
    }
    crate::agent_runtime::translate::refresh_pending(doc);
    chat::emit(
        s,
        tid,
        doc,
        "core.approval.resolved",
        json!({"approval_id":aid,"decision":decision,"status":"resolved"}),
    );
    Ok(result)
}

fn gateway_error_code(receipt: &Value) -> &'static str {
    match receipt["error_code"].as_str() {
        Some("policy_denied") => "policy_denied",
        Some("authorization_unresolved") => "authorization_unresolved",
        Some("approval_expired") => "approval_expired",
        _ => "outcome_unknown",
    }
}

/// A gateway request owns its durable dispatch boundary, including caller cancellation.
pub(crate) struct GatewayExecution {
    state: Arc<AppState>,
    task_id: String,
    invocation: String,
    dispatched: bool,
}
impl Drop for GatewayExecution {
    fn drop(&mut self) {
        if self.dispatched {
            if let Err(error) = settle(
                &self.state,
                &self.task_id,
                &self.invocation,
                Value::Null,
                Some("gateway_delivery_interrupted"),
                true,
            ) {
                tracing::error!(code=error.code,invocation_id=%self.invocation,"Failed to preserve interrupted gateway dispatch");
            }
        }
    }
}
impl GatewayExecution {
    pub(crate) fn result(&self) -> Api<Option<Value>> {
        let value = receipt(&self.state, &self.task_id, &self.invocation)?;
        match value["dispatch_state"].as_str().unwrap_or("") {
            "completed" => Ok(Some(value["result"].clone())),
            "ready" | "waiting_approval" => Ok(None),
            _ => Err(Error::new(
                StatusCode::CONFLICT,
                gateway_error_code(&value),
                format!(
                    "SRC execution state: {}; {}",
                    value["dispatch_state"], value["error_code"]
                ),
            )),
        }
    }
    pub(crate) async fn wait(&self) -> Api<()> {
        loop {
            let value = receipt(&self.state, &self.task_id, &self.invocation)?;
            if value["dispatch_state"] == "ready" {
                return Ok(());
            }
            if value["dispatch_state"] != "waiting_approval" {
                return self.result().map(|_| ());
            }
            let row = approval(&self.state, &self.task_id, &self.invocation)?
                .ok_or_else(|| Error::conflict("execution approval is missing"))?;
            let expired = row["payload"]["expires_at"].as_str().is_none_or(|text| {
                chrono::DateTime::parse_from_rfc3339(text)
                    .map_or(true, |time| time <= chrono::Utc::now())
            });
            if row["payload"]["status"] == "approved" && !expired {
                publish(&self.state, &self.task_id, &self.invocation).await?;
                return Ok(());
            }
            if row["payload"]["status"] != "pending" || expired {
                let value = dispatch(&self.state, &self.task_id, &self.invocation)?;
                publish(&self.state, &self.task_id, &self.invocation).await?;
                return Err(Error::new(
                    StatusCode::FORBIDDEN,
                    gateway_error_code(&value),
                    format!(
                        "SRC execution approval rejected or expired: {}",
                        value["error_code"]
                    ),
                ));
            }
            publish(&self.state, &self.task_id, &self.invocation).await?;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    pub(crate) fn dispatch(&mut self) -> Api<()> {
        let value = dispatch(&self.state, &self.task_id, &self.invocation)?;
        if value["dispatch_state"] != "dispatched" {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "policy_denied",
                "SRC execution rejected dispatch",
            ));
        }
        self.dispatched = true;
        Ok(())
    }
    pub(crate) fn complete(&mut self, result: Value) -> Api<()> {
        settle(
            &self.state,
            &self.task_id,
            &self.invocation,
            result,
            None,
            false,
        )?;
        self.dispatched = false;
        Ok(())
    }
}
pub(super) fn gateway_identity_revision(s: &AppState, id: &str, context: &str) -> Api<u64> {
    let row = records(s, id, "browser_identity")?
        .into_iter()
        .find(|row| row["record_id"] == context || row["payload"]["context_id"] == context);
    Ok(row
        .map(|row| {
            row["payload"]["config_revision"]
                .as_u64()
                .or_else(|| row["revision"].as_u64())
                .unwrap_or(0)
        })
        .unwrap_or(0))
}
#[allow(dead_code)]
fn validate_gateway_binding(s: &AppState, id: &str, request: &Value, p: &Value) -> Api<()> {
    if let Some(run) = request["run_id"].as_str().filter(|run| !run.is_empty()) {
        if request["resource_kind"] == "control"
            && !records(s, id, "run_binding")?.iter().any(|row| {
                row["payload"]["run_id"] == run && row["payload"]["destroyed_at"].is_null()
            })
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "task_binding_invalid",
                "Run does not belong to the bound SRC Task",
            ));
        }
        if let Some(req_gen) = request["run_execution_generation"].as_u64()
            && live_run_generation(s, run)?.is_some_and(|live| live != req_gen)
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "authorization_stale",
                "Run execution_generation changed before gateway dispatch",
            ));
        }
    }
    if let Some(epoch) = request["control_epoch"].as_str().filter(|v| !v.is_empty())
        && epoch != s.chat.epoch()
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "authorization_stale",
            "control epoch changed before gateway dispatch",
        ));
    }
    let context = request["identity_ref"].as_str().unwrap_or("default");
    if request["identity_revision"] != gateway_identity_revision(s, id, context)?
        || request["environment_generation"] != p["activation"]["generation"]
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "authorization_stale",
            "SRC identity or environment changed before gateway dispatch",
        ));
    }
    Ok(())
}
#[allow(dead_code)]
pub(crate) fn gateway(
    s: &Arc<AppState>,
    tid: &str,
    invocation: &str,
    name: &str,
    arguments: &Value,
) -> Api<Option<GatewayExecution>> {
    let Some(caller) = resolve_authorized_src_caller(s, tid)? else {
        let doc = s
            .store
            .get("conversation_threads", tid)
            .map_err(db)?
            .unwrap_or(Value::Null);
        if doc["thread"]["mode"] == "src"
            || doc["thread"]["state"]["mode"] == "src"
            || doc["src_policy_gates"].is_object()
            || doc["native_session"]["src_task_id"]
                .as_str()
                .is_some_and(|v| !v.is_empty())
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.task_binding_invalid",
                "SRC thread has no active Task binding",
            ));
        }
        return Ok(None);
    };
    let id = caller.task_id;
    let source = crate::capabilities::source_tool(name);
    let declared_op = source
        .and_then(|row| {
            row["command_type"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| row["query_type"].as_str().filter(|s| !s.is_empty()))
        })
        .unwrap_or("");
    if crate::capabilities::src_query_type(name).is_some()
        || crate::capabilities::is_collab_tool(name)
        || matches!(
            declared_op,
            "src.workbench.suggestion.propose"
                | "src.workbench.todo.save"
                | "src.project.memory.save"
                | "src.coordination.decide"
                | "src.run.intervene"
        )
        || declared_op.starts_with("conversation.subagent.")
        || declared_op.starts_with("conversation.thread.")
        || declared_op.starts_with("conversation.watch.")
    {
        return Ok(None);
    }
    let mut arguments = arguments.clone();
    let map = arguments
        .as_object_mut()
        .ok_or_else(|| Error::invalid("tool arguments must be an object"))?;
    map.remove("idempotent");
    let p = policy(s, &id)?;
    let resource = if name.starts_with("chat_browser_") {
        "browser"
    } else if name.contains("http_request") {
        "http"
    } else if name.starts_with("muteki_") {
        "control"
    } else {
        "mcp"
    };
    let action = if resource == "browser" {
        name.trim_start_matches("chat_browser_")
    } else if !declared_op.is_empty() {
        declared_op
    } else {
        name
    };
    let identity = arguments["context_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("default");
    let revision = gateway_identity_revision(s, &id, identity)?;
    if arguments
        .get("identity_revision")
        .is_some_and(|value| !value.is_null() && value != 0 && value != revision)
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "authorization_stale",
            "supplied identity revision differs from trusted SRC identity",
        ));
    }
    let run_id = arguments["run_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let run_execution_generation =
        run_id.and_then(|rid| live_run_generation(s, rid).ok().flatten());
    let request = json!({"invocation_id":invocation,"task_id":id,"thread_id":tid,"run_id":run_id,"run_execution_generation":run_execution_generation,"control_epoch":s.chat.epoch(),"worker_id":arguments["worker_id"].as_str().filter(|s|!s.trim().is_empty()),"tool_name":name,"action":action,"arguments_hash":format!("{:x}",Sha256::digest(domains::policy_canonical_json(&arguments).as_bytes())),"arguments":arguments,"resource_kind":resource,"resource_facts":{},"identity_ref":identity,"identity_revision":revision,"cwd":arguments["cwd"].as_str().unwrap_or(""),"environment_generation":p["activation"]["generation"],"policy_version":p["version"],"adapter_id":"capability_gateway","gateway_execution":true});
    validate_gateway_binding(s, &id, &request, &p)?;
    admit(s, &id, &request, 900)?;
    Ok(Some(GatewayExecution {
        state: s.clone(),
        task_id: id,
        invocation: invocation.to_owned(),
        dispatched: false,
    }))
}

#[cfg(test)]
mod approval_tests {
    use super::*;

    #[tokio::test]
    async fn approved_invocation_crosses_dispatch_boundary_once() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = super::super::create_task_inner(&state,
            &json!({"title":"Read local evidence","goal":"Read the saved record","idempotency_key":"guard-once"}))
            .await.unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        super::super::update_policy(
            &state,
            id,
            &json!({"base_version":1,"defaults":{"effect":"ask"}}),
        )
        .unwrap();
        let request = json!({"task_id":id,"thread_id":created["output"]["thread_id"],
            "invocation_id":"read-once","resource_kind":"file","action":"read","tool_name":"read",
            "arguments_hash":"local-record","arguments":{"path":"evidence.txt"},"policy_version":2});
        let waiting = admit(&state, id, &request, 300).unwrap();
        let approval = &waiting["approval_id"];
        assert_eq!(waiting["dispatch_state"], "waiting_approval");
        assert_eq!(
            dispatch(&state, id, "read-once").unwrap_err().code,
            "approval_required"
        );
        super::super::decide_approval(
            &state,
            id,
            &json!({"approval_id":approval,
            "base_revision":1,"decision":"approve_once"}),
        )
        .unwrap();
        std::thread::scope(|scope| {
            let dispatch_once =
                || dispatch(&state, id, "read-once").map(|r| r["dispatch_state"].clone());
            let other = scope.spawn(dispatch_once);
            let results = [dispatch_once(), other.join().unwrap()];
            assert_eq!(
                results
                    .iter()
                    .filter(|r| r.as_ref().is_ok_and(|s| *s == "dispatched"))
                    .count(),
                1
            );
            assert_eq!(
                results
                    .iter()
                    .filter(|r| r.as_ref().is_err_and(|e| e.code == "invocation_replayed"))
                    .count(),
                1
            );
        });
        assert!(rec(&state,id,"execution_approval",approval.as_str().unwrap()).unwrap()["payload"]["consumed_at"].is_string());
    }
}
