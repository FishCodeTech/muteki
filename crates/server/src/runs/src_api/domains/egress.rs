//! Approval-gated, task-scoped structured HTTP egress.
use super::{
    PreparedExchange, browser_runtime, credential_store, parse_raw_request,
    prepare_exchange_dispatch, put_capture, safe_headers,
};
use crate::AppState;
use crate::runs::src_api::{
    Api, COMMANDS, Error, db, digest, now, policy, rec, receipt, records, require, save_rec, task,
};
use axum::http::StatusCode;
use chrono::{Duration as ChronoDuration, Utc};
use reqwest::header::{HeaderName, HeaderValue};
#[path = "raw_http.rs"]
pub(super) mod raw_http;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use uuid::Uuid;

fn gate_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}
fn active_requests() -> &'static Mutex<std::collections::HashSet<String>> {
    static ACTIVE: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}
struct ActiveRequest<'a> {
    state: &'a AppState,
    task: String,
    invocation: String,
}
impl<'a> ActiveRequest<'a> {
    fn new(state: &'a AppState, task: &str, invocation: &str) -> Self {
        active_requests().lock().unwrap().insert(invocation.into());
        Self {
            state,
            task: task.into(),
            invocation: invocation.into(),
        }
    }
}
impl Drop for ActiveRequest<'_> {
    fn drop(&mut self) {
        if receipt_state(self.state, &self.task, &self.invocation)
            .is_ok_and(|receipt| receipt["dispatch_state"] == "dispatched")
        {
            if let Err(error) = settle_failure(
                self.state,
                &self.task,
                &self.invocation,
                "external_outcome_unknown",
                false,
            ) {
                tracing::error!(invocation=%self.invocation,code=error.code,"HTTP cancellation receipt failed");
            }
        }
        active_requests().lock().unwrap().remove(&self.invocation);
    }
}
fn credential_bindings(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    if !body["credential_bindings"].is_null() && !body["credential_bindings"].is_array() {
        return Err(Error::invalid("credential_bindings must be an array"));
    }
    let mut bindings = Vec::new();
    for item in body["credential_bindings"].as_array().into_iter().flatten() {
        let location = require(item, "location")?;
        let name = require(item, "name")?;
        let reference = require(item, "reference")?;
        let format = item["format"].as_str().unwrap_or("raw");
        if !matches!(location, "header" | "query") || !matches!(format, "raw" | "bearer") {
            return Err(Error::invalid("invalid credential binding"));
        }
        if location == "header" {
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::invalid("credential header name invalid"))?;
        } else if name.is_empty()
            || !name
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'_' | b'-'))
        {
            return Err(Error::invalid("credential query parameter invalid"));
        }
        if bindings
            .iter()
            .any(|v: &Value| v["location"] == location && v["name"] == name)
        {
            return Err(Error::invalid("duplicate credential binding"));
        }
        let version = credential_store::version(s, id, reference)?;
        bindings.push(
            json!({"location":location,"name":name,"reference":reference,
            "format":format,"version":version}),
        );
    }
    Ok(json!(bindings))
}
fn scope_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}
fn scope_normalized(name: &str, value: &Value) -> Value {
    let Some(text) = value.as_str() else {
        return value.clone();
    };
    match name {
        "url" | "observed_url" | "request_url" | "page_url" => json!(normalized_url(text)),
        "host" | "hostname" | "connected_ip" | "ip" | "method" | "action" | "protocol"
        | "scheme" | "resource_kind" => {
            json!(text.to_ascii_lowercase())
        }
        "path" | "raw_path" if text.is_empty() => json!("/"),
        _ => value.clone(),
    }
}
fn normalized_url(raw: &str) -> String {
    let raw = raw.trim();
    let Ok(url) = reqwest::Url::parse(raw) else {
        return String::new();
    };
    let Some(host) = url.host_str() else {
        return String::new();
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let authority = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    let path = super::raw_endpoint_path(raw);
    let query = raw
        .split('#')
        .next()
        .unwrap_or(raw)
        .split_once('?')
        .map(|(_, query)| query)
        .filter(|query| !query.is_empty())
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    format!("{}://{authority}{port}{path}{query}", url.scheme())
}
fn scope_cidr(actual: &str, expected: &str) -> bool {
    let (network, prefix) = expected.split_once('/').unwrap_or((expected, ""));
    let (Ok(address), Ok(network)) = (
        actual.parse::<std::net::IpAddr>(),
        network.parse::<std::net::IpAddr>(),
    ) else {
        return false;
    };
    match (address, network) {
        (std::net::IpAddr::V4(address), std::net::IpAddr::V4(network)) => {
            let bits = if prefix.is_empty() {
                32
            } else if let Ok(bits) = prefix.parse::<u32>() {
                bits
            } else {
                return false;
            };
            bits <= 32
                && (bits == 0
                    || (u32::from(address) >> (32 - bits)) == (u32::from(network) >> (32 - bits)))
        }
        (std::net::IpAddr::V6(address), std::net::IpAddr::V6(network)) => {
            let bits = if prefix.is_empty() {
                128
            } else if let Ok(bits) = prefix.parse::<u32>() {
                bits
            } else {
                return false;
            };
            bits <= 128
                && (bits == 0
                    || (u128::from(address) >> (128 - bits))
                        == (u128::from(network) >> (128 - bits)))
        }
        _ => false,
    }
}
pub(super) fn scope_compare(
    name: &str,
    actual: &Value,
    operator: &str,
    expected: &Value,
) -> Api<bool> {
    let value = scope_text(actual);
    let expect = scope_text(expected);
    let normalized = scope_normalized(name, actual);
    let wanted = scope_normalized(name, expected);
    let matched = match operator {
        "exists" => !actual.is_null() == expected.as_bool().unwrap_or(false),
        "eq" => normalized == wanted,
        "neq" => normalized != wanted,
        "in" | "not_in" => {
            let values = expected
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![expected.clone()]);
            let contains = values
                .iter()
                .any(|item| normalized == scope_normalized(name, item));
            if operator == "in" {
                contains
            } else {
                !contains
            }
        }
        "contains" => value.contains(&expect),
        "not_contains" => !value.contains(&expect),
        "prefix" if matches!(name, "path" | "raw_path") => {
            value == expect || value.starts_with(&format!("{}/", expect.trim_end_matches('/')))
        }
        "prefix" => value.starts_with(&expect),
        "wildcard" => {
            let pattern = format!(
                "^{}$",
                expect
                    .split('*')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(".*")
            );
            regex::Regex::new(&pattern).is_ok_and(|regex| regex.is_match(&value))
        }
        "regex" => regex::Regex::new(&expect)
            .map_err(|_| Error::invalid("policy regex invalid"))?
            .is_match(&value),
        "cidr" => scope_cidr(&value, &expect),
        "target" => crate::runs::src_api::target_matches(&value, &expect),
        "lt" | "lte" | "gt" | "gte" => {
            let left = actual.as_f64().or_else(|| value.parse::<f64>().ok());
            let right = expected.as_f64().or_else(|| expect.parse::<f64>().ok());
            match (left, right) {
                (Some(left), Some(right)) => match operator {
                    "lt" => left < right,
                    "lte" => left <= right,
                    "gt" => left > right,
                    _ => left >= right,
                },
                _ => false,
            }
        }
        _ => {
            return Err(Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "policy_unavailable",
                format!("unsupported selector operator {operator}"),
            ));
        }
    };
    Ok(matched)
}
fn retry_safe_method(method: &str) -> bool {
    matches!(method, "GET" | "HEAD" | "OPTIONS" | "TRACE")
}

fn evaluate(
    s: &AppState,
    id: &str,
    prepared: &PreparedExchange,
    identity_ref: &str,
    identity_revision: u64,
    input_ref: &str,
    p: &Value,
    body: &Value,
) -> Api<(Value, Value)> {
    let parsed = super::exchange_queries::parsed_request(
        &prepared.raw_request,
        reqwest::Url::parse(&prepared.url).map_err(db)?.scheme(),
    )?;
    let mut facts = serde_json::Map::new();
    for (name, value) in [
        ("url", json!(prepared.url)),
        ("observed_url", json!(prepared.url)),
        ("request_url", json!(prepared.url)),
        ("scheme", parsed["scheme"].clone()),
        ("host", parsed["host"].clone()),
        ("hostname", parsed["host"].clone()),
        ("port", parsed["port"].clone()),
        ("path", parsed["path"].clone()),
        (
            "raw_path",
            json!(format!(
                "{}{}",
                parsed["path"].as_str().unwrap_or("/"),
                parsed["query"]
                    .as_str()
                    .filter(|query| !query.is_empty())
                    .map(|query| format!("?{query}"))
                    .unwrap_or_default()
            )),
        ),
        ("query", parsed["query"].clone()),
        ("method", parsed["method"].clone()),
        ("headers", parsed["headers"].clone()),
        ("body_sha256", parsed["body"]["sha256"].clone()),
    ] {
        facts.insert(
            name.into(),
            json!({"value":value,"origin":"request","state":"known"}),
        );
    }
    let request = json!({"task_id":id,"thread_id":task(s,id)?["binding"]["thread_id"],"run_id":body["source_run_id"],"tool_name":"src.http.request","resource_kind":"http","action":"request","arguments":{"url":prepared.url,"method":prepared.method,"draft_id":body["draft_id"],"draft_revision":body["draft_revision"],"budget":body["budget"],"idempotent":retry_safe_method(&prepared.method)},"identity_ref":identity_ref,"identity_revision":identity_revision,"environment_id":"src-structured-http","environment_generation":body["browser_generation"].as_u64().unwrap_or(0),"arguments_hash":input_ref,"resource_facts":facts,"policy_version":p["version"]});
    let mut decision = crate::runs::src_api::execution_policy::evaluate(p, &request)?;
    crate::runs::src_api::execution_policy::apply_grant(s, id, p, &request, &mut decision)?;
    Ok((decision, request))
}

fn receipt_state(s: &AppState, id: &str, invocation: &str) -> Api<Value> {
    Ok(rec(s, id, "execution_receipt", invocation)?["payload"].clone())
}
fn save_receipt(s: &AppState, id: &str, invocation: &str, value: Value) -> Api<Value> {
    let row = rec(s, id, "execution_receipt", invocation)?;
    let saved = save_rec(
        s,
        id,
        "execution_receipt",
        invocation,
        value,
        row["revision"].as_u64(),
    )?;
    Ok(saved["payload"].clone())
}
fn cached_result(s: &AppState, id: &str, old: &Value) -> Api<Value> {
    let mut result = old["receipt"].clone();
    if let Some(invocation) = result["output"]["execution_id"].as_str() {
        let current = receipt_state(s, id, invocation)?;
        result["output"]["execution_state"] = current["dispatch_state"].clone();
        if let Some(exchange_id) = current["external_receipt_ref"]
            .as_str()
            .filter(|value| !value.is_empty())
        {
            let exchange = rec(s, id, "http_exchange", exchange_id)?;
            result["output"]["exchange_id"] = json!(exchange_id);
            result["output"]["exchange"] = exchange["payload"].clone();
        }
        result["output"]["receipt"] = current;
    }
    result["deduplicated"] = json!(true);
    Ok(result)
}
fn is_unsent_interrupted(s: &AppState, id: &str, old: &Value) -> Api<bool> {
    let Some(invocation) = old["receipt"]["output"]["execution_id"].as_str() else {
        return Ok(false);
    };
    let current = receipt_state(s, id, invocation)?;
    if current["dispatch_state"] != "failed"
        || current["error_code"] != "dispatch_interrupted"
        || current["outcome_known"] != true
        || !current["external_receipt_ref"]
            .as_str()
            .unwrap_or("")
            .is_empty()
    {
        return Ok(false);
    }
    let request = rec(s, id, "execution_request", invocation)?;
    Ok(request["payload"]["method"]
        .as_str()
        .is_some_and(retry_safe_method))
}
pub(super) fn recover(s: &AppState, id: &str) -> Api<()> {
    for row in records(s, id, "execution_request")? {
        let request = &row["payload"];
        if !request["draft_id"].is_string() || request["adapter_id"].is_string() {
            continue;
        }
        let invocation = require(request, "invocation_id")?;
        if active_requests().lock().unwrap().contains(invocation) {
            continue;
        }
        match receipt_state(s, id, invocation)?["dispatch_state"].as_str() {
            Some("ready") => settle_failure(s, id, invocation, "dispatch_interrupted", true)?,
            Some("dispatched") => {
                settle_failure(s, id, invocation, "external_outcome_unknown", false)?
            }
            _ => {}
        }
    }
    Ok(())
}

async fn execute_one(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    let _guard = gate_lock().lock().await;
    let draft_id = require(body, "draft_id")?;
    let revision = body["draft_revision"]
        .as_u64()
        .ok_or_else(|| Error::invalid("draft_revision required"))?;
    let policy_version = body["authorization_version"]
        .as_u64()
        .ok_or_else(|| Error::invalid("authorization_version required"))?;
    let key = require(body, "idempotency_key")?;
    if body["budget"]["max_requests"].as_u64() == Some(0) {
        return Err(Error::invalid("request budget is exhausted"));
    }
    let _ = http_timeout(&body["budget"])?;
    if body["resource_facts"]
        .as_object()
        .is_some_and(|v| !v.is_empty())
    {
        return Err(Error::invalid(
            "request facts must be derived from the saved draft",
        ));
    }
    let command_id = format!("{id}:src.exchange.draft.execute:{key}");
    let request_hash = digest(&json!([id, body]));
    if let Some(old) = s.store.get(COMMANDS, &command_id).map_err(db)? {
        if old["request_hash"] != request_hash {
            return Err(Error::conflict(
                "idempotency_key already used for different draft execution",
            ));
        }
        if !is_unsent_interrupted(&s, id, &old)? {
            return cached_result(&s, id, &old);
        }
    }
    let prepared = prepare_exchange_dispatch(&s, id, draft_id, revision, policy_version).map_err(
        |message| Error::new(StatusCode::FORBIDDEN, "authorization_unresolved", message),
    )?;
    let p = policy(&s, id)?;
    if p["digest"] != prepared.policy_digest {
        return Err(Error::conflict("policy digest changed"));
    }
    let identity_ref = body["identity_ref"].as_str().unwrap_or("default");
    let identity_revision = body["identity_revision"].as_u64().unwrap_or(0);
    let bindings = credential_bindings(&s, id, body)?;
    let input_ref = format!("{:x}", Sha256::digest(&prepared.raw_request));
    let (evaluated, mut policy_request) = evaluate(
        &s,
        id,
        &prepared,
        identity_ref,
        identity_revision,
        &input_ref,
        &p,
        body,
    )?;
    super::guardrails::check_runtime_guardrails(
        &s,
        id,
        &prepared.method,
        &prepared.url,
        body["source_run_id"].as_str(),
        &body["budget"],
    )?;
    let effect = evaluated["effect"].as_str().unwrap_or("deny");
    let matched = &evaluated["matched_rule_refs"];
    let reason = evaluated["reason_code"].as_str().unwrap_or("policy_denied");
    let invocation = format!("http-inv-{}", Uuid::new_v4().simple());
    policy_request["invocation_id"] = json!(invocation);
    let decision_id = format!("sdec-{}", Uuid::new_v4().simple());
    let resolved_mutator_id: String = body["mutator_id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            rec(s.as_ref(), id, "request_draft", draft_id)
                .ok()
                .and_then(|r| {
                    r["payload"]["metadata"]["mutator_id"]
                        .as_str()
                        .map(str::to_owned)
                })
        })
        .unwrap_or_default();
    let request = json!({"invocation_id":invocation,"task_id":id,"draft_id":draft_id,
        "draft_revision":revision,"source_exchange_id":prepared.source_exchange_id,
        "method":prepared.method,"url":prepared.url,"raw_request_artifact_id":prepared.raw_request_artifact_id,
        "identity_ref":identity_ref,"identity_revision":body["identity_revision"].as_u64().unwrap_or(0),
        "context_id":body["context_id"].as_str().unwrap_or("default"),
        "mutator_id":resolved_mutator_id,
        "policy_version":policy_version,"policy_digest":prepared.policy_digest,
        "credential_bindings":bindings,"budget":body["budget"],"source_run_id":body["source_run_id"],"actor_ref":body["actor_ref"].as_str().unwrap_or("human"),"browser_generation":body["browser_generation"].as_u64().unwrap_or(0),"redirect_parent":body["redirect_parent"],"root_body":body.get("root_body").unwrap_or(body),
        "expected_side_effect":body["expected_side_effect"],"policy_request":policy_request,"created_at":now()});
    let admitted=s.store.document_scope(||->Api<Value>{
        let existing_cmd = s.store.get(COMMANDS, &command_id).map_err(db)?;
        if let Some(old)=existing_cmd.as_ref(){
            if old["request_hash"]!=request_hash{return Err(Error::conflict("idempotency_key already used for different draft execution"));}
            if !is_unsent_interrupted(&s, id, old)? {
                return cached_result(&s,id,old);
            }
        }
    save_rec(
        &s,
        id,
        "execution_request",
        &invocation,
        request.clone(),
        Some(0),
    )?;
    let mut approval = Value::Null;
    if effect == "ask" {
        let approval_id = format!("sapproval-{}", Uuid::new_v4().simple());
        let expires = (Utc::now() + ChronoDuration::minutes(15)).to_rfc3339();
        approval = json!({"approval_id":approval_id,"invocation_id":invocation,"input_ref":input_ref,
            "request_snapshot":policy_request,
            "matched_rule_refs":matched,"policy_version":policy_version,"status":"pending",
            "requested_at":now(),"expires_at":expires,"decided_by":"","decided_at":null,
            "decision":null,"grant_id":null,"consumed_at":null,"revision":1});
        save_rec(
            &s,
            id,
            "execution_approval",
            &approval_id,
            approval.clone(),
            Some(0),
        )?;
    }
    let decision = json!({"decision_id":decision_id,"invocation_id":invocation,"effect":effect,
        "status":if effect=="ask"{"waiting_approval"}else if effect=="deny"{"denied"}else{"allowed"},
        "matched_rule_refs":matched,"observed_rule_refs":evaluated["observed_rule_refs"],"unknown_fields":evaluated["unknown_fields"],
        "policy_version":policy_version,"approval_id":approval["approval_id"],"grant_id":null,
        "reason_code":reason,"timestamp":now()});
    save_rec(&s, id, "policy_decision", &invocation, decision, Some(0))?;
    let state = if effect == "ask" {
        "waiting_approval"
    } else if effect == "deny" {
        "denied"
    } else {
        "ready"
    };
    let initial = json!({"invocation_id":invocation,"decision_id":decision_id,"dispatch_state":state,
        "started_at":null,"ended_at":null,"external_receipt_ref":"","result_ref":null,
        "result":null,"error_code":if effect=="deny"{"policy_denied"}else{""},
        "outcome_known":effect=="deny","history":[{"state":state,"at":now()}]});
    save_rec(
        &s,
        id,
        "execution_receipt",
        &invocation,
        initial.clone(),
        Some(0),
    )?;
    let reserved = receipt(
        &command_id,
        id,
        json!({"execution_id":invocation,"draft_id":draft_id,"draft_revision":revision,
            "execution_state":state,"approval":approval,"receipt":initial}),
        false,
    );
    if !s
        .store
        .put_many_if(
            &[(COMMANDS, &command_id, existing_cmd.as_ref())],
            &[(
                COMMANDS,
                &command_id,
                &json!({"request_hash":request_hash,"receipt":reserved}),
            )],
        )
        .map_err(db)?
    {
        let old = s
            .store
            .get(COMMANDS, &command_id)
            .map_err(db)?
            .ok_or_else(|| Error::conflict("draft execution reservation disappeared"))?;
        if old["request_hash"] != request_hash {
            return Err(Error::conflict(
                "idempotency_key already used for different draft execution",
            ));
        }
        return cached_result(&s, id, &old);
    }
    Ok(reserved)
    }).map_err(db)??;
    if admitted["output"]["execution_id"] != invocation {
        return Ok(admitted);
    }
    let approval = admitted["output"]["approval"].clone();
    let initial = admitted["output"]["receipt"].clone();
    let state = initial["dispatch_state"].as_str().unwrap_or("denied");
    if effect == "ask" {
        if let Err(error) =
            super::notify_gateway::notify_event(&s, id, "approval_pending", &invocation).await
        {
            tracing::error!(code=error.code, message=%error.message,"SRC approval notification failed");
        }
    }
    let output = if effect == "allow" {
        match dispatch(&s, id, &invocation, &request).await {
            Ok(value) => value,
            Err(error) => {
                let known = receipt_state(&s, id, &invocation)?["dispatch_state"] != "dispatched";
                settle_failure(&s, id, &invocation, error.code, known)?;
                let settled = receipt_state(&s, id, &invocation)?;
                json!({"execution_id":invocation,"draft_id":draft_id,"draft_revision":revision,
                    "execution_state":settled["dispatch_state"],"receipt":settled})
            }
        }
    } else {
        json!({"execution_id":invocation,"draft_id":draft_id,"draft_revision":revision,
        "execution_state":state,"approval":approval,"receipt":initial})
    };
    let result = receipt(&command_id, id, output, false);
    s.store
        .put(
            COMMANDS,
            &command_id,
            &json!({"request_hash":request_hash,"receipt":result}),
        )
        .map_err(db)?;
    Ok(result)
}

pub(super) async fn after_approval(
    s: Arc<AppState>,
    id: &str,
    approval: &Value,
) -> Result<(), String> {
    let invocation = approval["invocation_id"]
        .as_str()
        .ok_or("approval invocation missing")?;
    let current = receipt_state(&s, id, invocation).map_err(|e| e.message)?;
    if current["dispatch_state"] != "waiting_approval" {
        return Ok(());
    }
    if approval["status"] == "denied" {
        let mut next = current.clone();
        next["dispatch_state"] = json!("denied");
        next["ended_at"] = json!(now());
        next["error_code"] = json!("operator_denied");
        next["outcome_known"] = json!(true);
        next["history"]
            .as_array_mut()
            .unwrap()
            .push(json!({"state":"denied","at":now()}));
        save_receipt(&s, id, invocation, next).map_err(|e| e.message)?;
        return Ok(());
    }
    if approval["status"] != "approved" {
        return Ok(());
    }
    let request =
        rec(&s, id, "execution_request", invocation).map_err(|e| e.message)?["payload"].clone();
    let _guard = gate_lock().lock().await;
    if let Err(error) = dispatch(&s, id, invocation, &request).await {
        let state = receipt_state(&s, id, invocation).map_err(|e| e.message)?["dispatch_state"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if error.code == "version_conflict" && state != "waiting_approval" {
            return Ok(());
        }
        if error.code == "src.store_failed" && state == "waiting_approval" {
            return Err(error.message);
        }
        let known = state != "dispatched";
        settle_failure(&s, id, invocation, error.code, known).map_err(|e| e.message)?;
    }
    drop(_guard);
    if receipt_state(&s, id, invocation).map_err(|error| error.message)?["dispatch_state"]
        == "completed"
        && request["root_body"].is_object()
    {
        execute_draft(s.clone(), id, &request["root_body"])
            .await
            .map_err(|error| error.message)?;
    }
    Ok(())
}

fn settle_failure(s: &AppState, id: &str, invocation: &str, code: &str, known: bool) -> Api<()> {
    let mut next = receipt_state(s, id, invocation)?;
    if matches!(
        next["dispatch_state"].as_str(),
        Some("completed" | "denied" | "failed" | "outcome_unknown")
    ) {
        return Ok(());
    }
    next["dispatch_state"] = json!(if known { "failed" } else { "outcome_unknown" });
    next["ended_at"] = json!(now());
    next["error_code"] = json!(code);
    next["outcome_known"] = json!(known);
    next["retry_safe"] = json!(
        known
            && code == "dispatch_interrupted"
            && rec(s, id, "execution_request", invocation)?["payload"]["method"]
                .as_str()
                .is_some_and(retry_safe_method)
    );
    let state = next["dispatch_state"].clone();
    next["history"]
        .as_array_mut()
        .unwrap()
        .push(json!({"state":state,"at":now(),"error_code":code}));
    save_receipt(s, id, invocation, next)?;
    Ok(())
}
fn request_body(raw: &[u8]) -> Api<&[u8]> {
    if let Some(index) = raw.windows(4).position(|v| v == b"\r\n\r\n") {
        return Ok(&raw[index + 4..]);
    }
    if let Some(index) = raw.windows(2).position(|v| v == b"\n\n") {
        return Ok(&raw[index + 2..]);
    }
    Err(Error::invalid("raw request headers incomplete"))
}
async fn dispatch(s: &Arc<AppState>, id: &str, invocation: &str, request: &Value) -> Api<Value> {
    let draft_id = require(request, "draft_id")?;
    let revision = request["draft_revision"]
        .as_u64()
        .ok_or_else(|| Error::invalid("draft revision missing"))?;
    let policy_version = request["policy_version"]
        .as_u64()
        .ok_or_else(|| Error::invalid("policy version missing"))?;
    let prepared = prepare_exchange_dispatch(s, id, draft_id, revision, policy_version).map_err(
        |message| Error::new(StatusCode::FORBIDDEN, "authorization_unresolved", message),
    )?;
    if request["policy_digest"] != prepared.policy_digest
        || request["url"] != prepared.url
        || request["method"] != prepared.method
    {
        return Err(Error::conflict("saved request changed before dispatch"));
    }
    let current = receipt_state(s, id, invocation)?;
    if !matches!(
        current["dispatch_state"].as_str(),
        Some("ready" | "waiting_approval")
    ) {
        return Err(Error::conflict("request invocation already dispatched"));
    }
    if current["dispatch_state"] == "waiting_approval" {
        let approved = records(s, id, "execution_approval")?.into_iter().any(|v| {
            v["payload"]["invocation_id"] == invocation && v["payload"]["status"] == "approved"
        });
        if !approved {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "approval_required",
                "approval remains pending",
            ));
        }
    }
    let scheme = reqwest::Url::parse(&prepared.url)
        .map_err(db)?
        .scheme()
        .to_owned();
    let (_, _, mut headers) = parse_raw_request(&prepared.raw_request, &scheme)?;
    let lengths = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim())
        .collect::<Vec<_>>();
    if let Some(length) = lengths.first() {
        if lengths.iter().any(|value| value != length)
            || length.is_empty()
            || !length.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(Error::invalid("Conflicting or invalid Content-Length"));
        }
        if headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        {
            return Err(Error::invalid(
                "Transfer-Encoding and Content-Length cannot be sent together",
            ));
        }
        let size = length
            .parse::<usize>()
            .map_err(|_| Error::invalid("Content-Length exceeds the request range"))?;
        if size != request_body(&prepared.raw_request)?.len() {
            return Err(Error::invalid(
                "Content-Length differs from the saved request body",
            ));
        }
    }
    let mut send_url = reqwest::Url::parse(&prepared.url).map_err(db)?;
    for binding in request["credential_bindings"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let reference = require(binding, "reference")?;
        let name = require(binding, "name")?;
        let version = binding["version"]
            .as_u64()
            .ok_or_else(|| Error::invalid("credential binding version missing"))?;
        let secret = credential_store::resolve(s, id, reference, Some(version))?;
        let value = if binding["format"] == "bearer" {
            format!("Bearer {secret}")
        } else {
            secret
        };
        match binding["location"].as_str() {
            Some("header") => {
                HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| Error::invalid("credential header name invalid"))?;
                if headers
                    .iter()
                    .any(|(known, _)| known.eq_ignore_ascii_case(name))
                {
                    return Err(Error::invalid(
                        "credential header already exists in saved draft",
                    ));
                }
                let bytes = value
                    .chars()
                    .map(|value| u8::try_from(value as u32))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| Error::invalid("credential header is not ISO-8859-1"))?;
                HeaderValue::from_bytes(&bytes)
                    .map_err(|_| Error::invalid("credential value is invalid for HTTP header"))?;
                headers.push((name.into(), value));
            }
            Some("query") => {
                if send_url.query_pairs().any(|(key, _)| key == name) {
                    return Err(Error::invalid(
                        "credential query parameter already exists in saved draft",
                    ));
                }
                send_url.query_pairs_mut().append_pair(name, &value);
            }
            _ => return Err(Error::invalid("credential binding location invalid")),
        }
    }
    browser_runtime::target_scope(s, id, send_url.as_str())?;
    let jar_applied = super::identity::apply_jar_to_request(
        s,
        id,
        request["context_id"].as_str().unwrap_or(""),
        &send_url,
        &mut headers,
    )?;
    let mut body_bytes = request_body(&prepared.raw_request)?.to_vec();
    let mutator_id = request["mutator_id"].as_str().unwrap_or("");
    if !mutator_id.is_empty() {
        super::request_mutator::apply_to_request(
            s,
            id,
            mutator_id,
            &prepared.method,
            &mut send_url,
            &mut headers,
            &mut body_bytes,
        )
        .await?;
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-length"));
        if !body_bytes.is_empty() || !matches!(prepared.method.as_str(), "GET" | "HEAD") {
            headers.push(("Content-Length".to_owned(), body_bytes.len().to_string()));
        }
    }
    let mut wire_request = prepared.raw_request.clone();
    if request["credential_bindings"]
        .as_array()
        .is_some_and(|bindings| !bindings.is_empty())
        || jar_applied
        || !mutator_id.is_empty()
    {
        let first = prepared
            .raw_request
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or(&[]);
        let first = std::str::from_utf8(first)
            .map_err(|_| Error::invalid("request line is not ASCII"))?
            .trim_end_matches('\r');
        let parts = first.split(' ').collect::<Vec<_>>();
        let target = if send_url.query() != reqwest::Url::parse(&prepared.url).map_err(db)?.query()
        {
            format!(
                "{}{}",
                super::raw_endpoint_path(&prepared.url),
                send_url
                    .query()
                    .map(|query| format!("?{query}"))
                    .unwrap_or_default()
            )
        } else {
            parts.get(1).copied().unwrap_or("/").into()
        };
        wire_request = format!(
            "{} {target} {}\r\n",
            prepared.method,
            parts.get(2).copied().unwrap_or("HTTP/1.1")
        )
        .into_bytes();
        for (name, value) in &headers {
            for value in format!("{name}: {value}\r\n").chars() {
                wire_request.push(
                    u8::try_from(value as u32)
                        .map_err(|_| Error::invalid("request header is not ISO-8859-1"))?,
                );
            }
        }
        wire_request.extend_from_slice(b"\r\n");
        wire_request.extend_from_slice(&body_bytes);
    }
    let started_at = now();
    let _active = ActiveRequest::new(s, id, invocation);
    s.store.document_scope(||->Api<()>{
        let mut next=receipt_state(s,id,invocation)?;if !matches!(next["dispatch_state"].as_str(),Some("ready"|"waiting_approval")){return Err(Error::conflict("request invocation already dispatched"));}
        let p=super::super::execution_runtime::current_authority(s,id,request["thread_id"].as_str(),&request["policy_version"],&request["policy_digest"])?;
        let hash=format!("{:x}",Sha256::digest(&prepared.raw_request));
        let(decision,_)=evaluate(s,id,&prepared,request["identity_ref"].as_str().unwrap_or("default"),request["identity_revision"].as_u64().unwrap_or(0),&hash,&p,request)?;
        if decision["effect"]=="deny"||decision["status"]=="unresolved"{return Err(Error::new(StatusCode::FORBIDDEN,"authorization_unresolved","HTTP policy no longer authorizes this request"));}
        if next["dispatch_state"]=="ready"&&decision["effect"]!="allow"{return Err(Error::new(StatusCode::FORBIDDEN,"approval_required","HTTP grant changed before dispatch"));}
        if next["dispatch_state"]=="waiting_approval"{
            let row=records(s,id,"execution_approval")?.into_iter().find(|row|row["payload"]["invocation_id"]==invocation).ok_or_else(||Error::conflict("HTTP approval disappeared"))?;
            super::super::execution_runtime::consume_approval(s,id,require(&row,"record_id")?,&p)?;
        }
        next["dispatch_state"]=json!("dispatched");next["started_at"]=json!(started_at);next["history"].as_array_mut().unwrap().push(json!({"state":"dispatched","at":started_at,"url":prepared.url,"method":prepared.method}));save_receipt(s,id,invocation,next)?;Ok(())
    }).map_err(db)??;
    let limit = request["budget"]["capture_limit"]
        .as_u64()
        .unwrap_or(64 * 1024 * 1024)
        .min(64 * 1024 * 1024) as usize;
    let selected_upstream = super::proxypool::select_upstream(s, id)?;
    let capture = raw_http::exchange_via_proxy(
        &send_url,
        &prepared.method,
        &wire_request,
        http_timeout(&request["budget"])?,
        limit,
        selected_upstream.as_ref(),
    )
    .await;
    if let Some(upstream) = selected_upstream.as_ref() {
        let ok = capture.complete;
        super::proxypool::record_outcome(s, id, &upstream.node_id, ok, capture.error)?;
    }
    let raw_response = capture.raw;
    let truncated = !capture.complete;
    let parsed_response =
        super::exchange_queries::parsed_response(&raw_response, &prepared.method).ok();
    let status = parsed_response
        .as_ref()
        .and_then(|parsed| parsed["status_code"].as_u64());
    let protocol = parsed_response
        .as_ref()
        .and_then(|parsed| parsed["version"].as_str())
        .unwrap_or("HTTP/1.1")
        .to_ascii_lowercase();
    let response_headers = parsed_response
        .as_ref()
        .and_then(|parsed| parsed["headers"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|pair| Some((pair[0].as_str()?.to_owned(), pair[1].as_str()?.to_owned())))
        .collect::<Vec<_>>();
    let execution_state = if capture.complete {
        "completed"
    } else if capture.sent == 0 {
        "failed"
    } else {
        "outcome_unknown"
    };
    let outcome_known = capture.complete || capture.sent == 0;
    let exchange_id = format!("sexchange-{}", Uuid::new_v4().simple());
    s.store.document_scope(||->Api<Value>{
    let mut response_ref = put_capture(
        s,
        id,
        &raw_response,
        &format!("{exchange_id}-response.http"),
    )?;
    response_ref["original"] = json!(true);
    response_ref["complete"] = json!(if truncated { "partial" } else { "complete" });
    let p = policy(s, id)?;
    let authorization_ref = format!(
        "{}:v{}",
        p["policy_id"].as_str().unwrap_or("src-policy"),
        policy_version
    );
    let mut capture_limitations = Vec::new();
    if truncated {
        capture_limitations.push(capture.error.unwrap_or("response_incomplete"));
    }
    if request["credential_bindings"]
        .as_array()
        .is_some_and(|v| !v.is_empty())
    {
        capture_limitations.push("credential_injected_from_reference");
    }
    let mut exchange = json!({"exchange_id":exchange_id,"task_id":id,"source_run_id":request["source_run_id"],"source_event_seq":null,
        "source_kind":"http_tool","actor_ref":request["actor_ref"],"context_id":request["context_id"],
        "identity_revision":request["identity_revision"],"browser_generation":request["browser_generation"],
        "declared_target":prepared.url,"observed_url":prepared.url,"method":prepared.method,
        "started_at":started_at,"finished_at":now(),
        "request_raw_ref":put_capture(s,id,&wire_request,&format!("{exchange_id}-request.http"))?,"response_raw_ref":response_ref,
        "request_headers":safe_headers(&headers),"response_headers":safe_headers(&response_headers),
        "status_code":status,"protocol":protocol,"redirect_parent":request["redirect_parent"],
        "replay_of":prepared.source_exchange_id,"execution_state":execution_state,
        "capture_state":if truncated{"partial"}else{"complete"},
        "capture_limitations":capture_limitations,
        "authorization_ref":authorization_ref,"decision_ref":invocation,
        "metadata":{"gateway":"src-raw-http-v1","redirect_followed":false,"raw_request_artifact_id":request["raw_request_artifact_id"],
            "credential_bindings":request["credential_bindings"]}});
    let parsed_request=super::exchange_queries::parsed_request(&wire_request,&scheme)?;exchange["metadata"]["request_body_sha256"]=parsed_request["body"]["sha256"].clone();exchange["metadata"]["request_body_size"]=parsed_request["body"]["size"].clone();exchange["metadata"]["raw_representation"]=json!("original");
    if let Ok(parsed)=super::exchange_queries::parsed_response(&raw_response,&prepared.method){exchange["metadata"]["response_body_sha256"]=parsed["body"]["sha256"].clone();exchange["metadata"]["response_body_size"]=parsed["body"]["size"].clone();}
    exchange["metadata"]["transport"]=json!({"request_bytes_sent":capture.sent,"response_bytes_received":raw_response.len(),"peer":capture.peer,"tls":scheme=="https","response_complete":capture.complete});
    exchange["metadata"]["media_type"]=response_headers.iter().find(|(name,_)|name.eq_ignore_ascii_case("content-type")).map(|(_,value)|json!(value.split(';').next().unwrap_or("").trim())).unwrap_or(json!(""));
    save_rec(
        s,
        id,
        "http_exchange",
        &exchange_id,
        exchange.clone(),
        Some(0),
    )?;
    super::traffic_index::index_exchange(s, id, &exchange, &wire_request, Some(&raw_response))?;
    super::identity::ingest_set_cookies_from_response(
        s,
        id,
        request["context_id"].as_str().unwrap_or(""),
        &prepared.url,
        &response_headers,
    )?;
    let mut next = receipt_state(s, id, invocation)?;
    next["dispatch_state"] = json!(execution_state);
    next["ended_at"] = json!(now());
    next["external_receipt_ref"] = json!(exchange_id);
    next["result_ref"] = response_ref.clone();
    next["result"] = json!({"status_code":status,"exchange_id":exchange_id});
    next["error_code"] = json!(capture.error.unwrap_or(""));
    next["outcome_known"] = json!(outcome_known);
    next["retry_safe"] = json!(execution_state == "failed" && capture.sent == 0 && retry_safe_method(&prepared.method));
    next["history"]
        .as_array_mut()
        .unwrap()
        .push(json!({"state":execution_state,"at":now(),"status_code":status}));
    let settled = save_receipt(s, id, invocation, next)?;
    let execution=save_rec(s,id,"http_execution",invocation,json!({"invocation_id":invocation,"draft_id":draft_id,"draft_revision":revision,"exchange_id":exchange_id,"replay_of":prepared.source_exchange_id,"receipt":settled}),Some(0))?;
    Ok(
        json!({"execution_id":invocation,"draft_id":draft_id,"draft_revision":revision,
        "execution_state":execution_state,"exchange_id":exchange_id,"exchange":exchange,"receipt":settled,"execution_record_revision":execution["revision"],"redirect_results":[]}),
    )
    }).map_err(db)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn interrupted_http_replay_uses_recorded_method_and_known_outcome() {
        let state = AppState::new(crate::store::Store::open(":memory:").unwrap());
        for (method, outcome, retry) in [
            ("GET", "failed", true),
            ("HEAD", "failed", true),
            ("OPTIONS", "failed", true),
            ("TRACE", "failed", true),
            ("PUT", "failed", false),
            ("DELETE", "failed", false),
            ("GET", "outcome_unknown", false),
        ] {
            let invocation = format!("{method}-{outcome}");
            save_rec(
                &state,
                "test",
                "execution_request",
                &invocation,
                json!({"method":method,"arguments":{"idempotent":true}}),
                Some(0),
            )
            .unwrap();
            save_rec(&state, "test", "execution_receipt", &invocation,
                json!({"dispatch_state":outcome,"outcome_known":outcome=="failed","error_code":"dispatch_interrupted","external_receipt_ref":""}), Some(0)).unwrap();
            let command = json!({"receipt":{"output":{"execution_id":invocation}}});
            assert_eq!(
                is_unsent_interrupted(&state, "test", &command).unwrap(),
                retry,
                "{invocation}"
            );
        }
    }

    #[tokio::test]
    async fn unsafe_request_waits_for_approval_and_stale_policy_blocks_it() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = crate::runs::src_api::create_task_inner(
            &state,
            &json!({"title":"Approval gate","goal":"Inspect one URL",
                "authorization":{"targets":["http://127.0.0.1:9/demo"]},
                "idempotency_key":"src-egress-gate-test"}),
        )
        .await
        .unwrap();
        let task_id = created["output"]["task_id"].as_str().unwrap();
        crate::runs::src_api::update_policy(
            &state,
            task_id,
            &json!({"base_version":1,"defaults":{"effect":"ask"}}),
        )
        .unwrap();
        let raw = b"POST /demo HTTP/1.1\r\nHost: 127.0.0.1:9\r\nContent-Length: 4\r\n\r\nping";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let source = super::super::record_exchange(
            &state,
            task_id,
            &json!({"request_raw_b64":encoded,"observed_url":"http://127.0.0.1:9/demo"}),
        )
        .unwrap();
        let draft = super::super::save_exchange_draft(
            &state,
            task_id,
            &json!({"source_exchange_id":source["exchange_id"],"base_revision":1,
                "raw_request_b64":encoded}),
        )
        .unwrap();
        let body = json!({"draft_id":draft["draft"]["draft_id"],"draft_revision":1,
            "authorization_version":2,"idempotency_key":"one-post",
            "budget":{"max_requests":1}});
        let first = execute_draft(state.clone(), task_id, &body).await.unwrap();
        assert_eq!(first["output"]["execution_state"], "waiting_approval");
        assert_eq!(first["output"]["receipt"]["started_at"], Value::Null);
        let second = execute_draft(state.clone(), task_id, &body).await.unwrap();
        assert_eq!(second["deduplicated"], true);
        assert_eq!(
            second["output"]["execution_id"],
            first["output"]["execution_id"]
        );

        crate::runs::src_api::update_policy(
            &state,
            task_id,
            &json!({"base_version":2,"authorized_targets":[]}),
        )
        .unwrap();
        let approval = &first["output"]["approval"];
        assert_eq!(
            crate::runs::src_api::decide_approval(
                &state,
                task_id,
                &json!({"approval_id":approval["approval_id"],
                    "base_revision":approval["revision"],"decision":"approve_once"}),
            )
            .unwrap_err()
            .status,
            StatusCode::CONFLICT
        );
        let mut retry = body.clone();
        retry["idempotency_key"] = json!("new-post");
        retry["authorization_version"] = json!(3);
        assert_eq!(
            execute_draft(state, task_id, &retry)
                .await
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
    }
}

fn http_timeout(budget: &Value) -> Api<Duration> {
    let seconds = budget
        .get("timeout_seconds")
        .map(|value| {
            value
                .as_f64()
                .ok_or_else(|| Error::invalid("timeout_seconds must be numeric"))
        })
        .transpose()?
        .unwrap_or(30.);
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|value| !value.is_zero() && std::time::Instant::now().checked_add(*value).is_some())
        .ok_or_else(|| {
            Error::invalid("timeout_seconds must be positive finite seconds within the clock range")
        })
}
pub(super) async fn execute_draft(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    execute_draft_inner(s, id, body, true).await
}

fn validate_connector_base_url(target_raw: &str, allowed_base_url: &str) -> Api<reqwest::Url> {
    let allowed = crate::runs::src_api::parse_url(allowed_base_url)?;
    let target = crate::runs::src_api::parse_url(target_raw)?;
    if !matches!(allowed.scheme(), "http" | "https") || !matches!(target.scheme(), "http" | "https")
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.connector_egress_forbidden",
            "connector URL scheme must be http or https",
        ));
    }
    let same_host = match (target.host_str(), allowed.host_str()) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
    let base_path = allowed.path().trim_end_matches('/');
    let target_path = target.path();
    let path_ok = base_path.is_empty()
        || target_path == base_path
        || target_path.starts_with(&format!("{base_path}/"));
    if target.scheme() != allowed.scheme()
        || !same_host
        || target.port_or_known_default() != allowed.port_or_known_default()
        || !path_ok
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.connector_egress_forbidden",
            "connector request URL does not match configured allowed base URL",
        ));
    }
    Ok(target)
}

fn resolve_connector_request_input(
    s: &AppState,
    id: &str,
    body: &Value,
) -> Api<(
    String,
    String,
    Vec<(String, String)>,
    Vec<u8>,
    Option<String>,
    Option<String>,
    Value,
)> {
    use base64::Engine;
    if let Some(draft_id) = body["draft_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let row = rec(s, id, "request_draft", draft_id)?;
        let draft = &row["payload"];
        if let Some(expected_rev) = body["draft_revision"].as_u64()
            && draft["revision"].as_u64().unwrap_or(1) != expected_rev
        {
            return Err(Error::conflict("connector draft revision mismatch"));
        }
        let artifact = super::verify_ref(s, id, &draft["raw_request_ref"])?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(artifact["data"].as_str().unwrap_or(""))
            .map_err(|_| Error::invalid("connector draft raw request invalid"))?;
        let url_str = draft["target_url"]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::invalid("connector draft target_url missing"))?
            .to_owned();
        let scheme = reqwest::Url::parse(&url_str)
            .map_err(db)?
            .scheme()
            .to_owned();
        let (method, parsed_url, headers) = parse_raw_request(&raw, &scheme)?;
        return Ok((
            method,
            parsed_url,
            headers,
            raw,
            Some(draft_id.to_owned()),
            draft["source_exchange_id"].as_str().map(str::to_owned),
            draft["raw_request_ref"].clone(),
        ));
    }
    let url_str = require(body, "url")?.to_owned();
    let parsed_url = crate::runs::src_api::parse_url(&url_str)?;
    if let Some(encoded) = body["raw_request_b64"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| Error::invalid("connector raw_request_b64 invalid"))?;
        let (method, req_url, headers) = parse_raw_request(&raw, parsed_url.scheme())?;
        return Ok((method, req_url, headers, raw, None, None, Value::Null));
    }
    let method = body["method"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let authority = parsed_url
        .port()
        .map(|p| format!("{}:{p}", parsed_url.host_str().unwrap_or("")))
        .unwrap_or_else(|| parsed_url.host_str().unwrap_or("").to_owned());
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut has_host = false;
    if let Some(map) = body["headers"].as_object() {
        for (k, v) in map {
            if let Some(vs) = v.as_str() {
                if k.eq_ignore_ascii_case("host") {
                    has_host = true;
                }
                headers.push((k.clone(), vs.to_owned()));
            }
        }
    } else if let Some(arr) = body["headers"].as_array() {
        for item in arr {
            if let Some(pair) = item.as_array()
                && pair.len() >= 2
                && let (Some(k), Some(v)) = (pair[0].as_str(), pair[1].as_str())
            {
                if k.eq_ignore_ascii_case("host") {
                    has_host = true;
                }
                headers.push((k.to_owned(), v.to_owned()));
            }
        }
    }
    if !has_host {
        headers.insert(0, ("Host".to_owned(), authority));
    }
    let body_bytes: Vec<u8> = if let Some(b64) = body["body_b64"].as_str() {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| Error::invalid("connector body_b64 invalid"))?
    } else if let Some(text) = body["body"].as_str() {
        text.as_bytes().to_vec()
    } else if !body["body"].is_null() {
        serde_json::to_vec(&body["body"]).map_err(db)?
    } else {
        Vec::new()
    };
    if !body_bytes.is_empty()
        && !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
    {
        headers.push(("Content-Length".to_owned(), body_bytes.len().to_string()));
    }
    let target_path = format!(
        "{}{}",
        super::raw_endpoint_path(&url_str),
        parsed_url
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    );
    let mut raw = format!("{method} {target_path} HTTP/1.1\r\n").into_bytes();
    for (k, v) in &headers {
        raw.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    raw.extend_from_slice(b"\r\n");
    raw.extend_from_slice(&body_bytes);
    Ok((method, url_str, headers, raw, None, None, Value::Null))
}

pub(super) async fn execute_connector_draft(
    s: Arc<AppState>,
    id: &str,
    body: &Value,
    connector_id: &str,
    allowed_base_url: &str,
    purpose: &str,
) -> Api<Value> {
    let _t = task(&s, id)?;
    if body["budget"]["max_requests"].as_u64() == Some(0) {
        return Err(Error::invalid("request budget is exhausted"));
    }
    let timeout = http_timeout(&body["budget"])?;
    let key = body["idempotency_key"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let command_id = format!("{id}:src.connector.egress:{connector_id}:{key}");
    let request_hash = digest(&json!([id, connector_id, allowed_base_url, purpose, body]));
    if let Some(old) = s.store.get(COMMANDS, &command_id).map_err(db)? {
        if old["request_hash"] != request_hash {
            return Err(Error::conflict(
                "idempotency_key already used for different connector request",
            ));
        }
        let mut cached = old["receipt"].clone();
        cached["deduplicated"] = json!(true);
        return Ok(cached);
    }

    let (method, url_str, mut headers, raw_request, draft_id, replay_of, raw_request_ref) =
        resolve_connector_request_input(&s, id, body)?;
    let prepared_parsed = validate_connector_base_url(&url_str, allowed_base_url)?;
    let p = policy(&s, id)?;
    if p["excluded_targets"].as_array().is_some_and(|rows| {
        rows.iter().any(|v| {
            v.as_str()
                .is_some_and(|ex| crate::runs::src_api::target_matches(&url_str, ex))
        })
    }) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "connector target is in excluded_targets",
        ));
    }

    let bindings = credential_bindings(&s, id, body)?;
    let mut send_url = prepared_parsed.clone();
    for binding in bindings.as_array().into_iter().flatten() {
        let reference = require(binding, "reference")?;
        let name = require(binding, "name")?;
        let version = binding["version"]
            .as_u64()
            .ok_or_else(|| Error::invalid("credential binding version missing"))?;
        let secret = credential_store::resolve(&s, id, reference, Some(version))?;
        let value = if binding["format"] == "bearer" {
            format!("Bearer {secret}")
        } else {
            secret
        };
        match binding["location"].as_str() {
            Some("header") => {
                HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| Error::invalid("credential header name invalid"))?;
                if headers
                    .iter()
                    .any(|(known, _)| known.eq_ignore_ascii_case(name))
                {
                    return Err(Error::invalid(
                        "credential header already exists in connector request",
                    ));
                }
                let bytes = value
                    .chars()
                    .map(|ch| u8::try_from(ch as u32))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| Error::invalid("credential header is not ISO-8859-1"))?;
                HeaderValue::from_bytes(&bytes)
                    .map_err(|_| Error::invalid("credential value is invalid for HTTP header"))?;
                headers.push((name.into(), value));
            }
            Some("query") => {
                if send_url.query_pairs().any(|(k, _)| k == name) {
                    return Err(Error::invalid(
                        "credential query parameter already exists in connector request",
                    ));
                }
                send_url.query_pairs_mut().append_pair(name, &value);
            }
            _ => return Err(Error::invalid("credential binding location invalid")),
        }
    }

    let grant =
        super::connector_config::enforce_grant_for_purpose(&s, id, connector_id, purpose, true)?;

    let req_body_bytes = request_body(&raw_request)?.to_vec();
    let mut wire_request = raw_request.clone();
    if bindings.as_array().is_some_and(|v| !v.is_empty()) {
        let first = raw_request
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or(&[]);
        let first = std::str::from_utf8(first)
            .map_err(|_| Error::invalid("request line is not ASCII"))?
            .trim_end_matches('\r');
        let parts = first.split(' ').collect::<Vec<_>>();
        let target = if send_url.query() != prepared_parsed.query() {
            format!(
                "{}{}",
                super::raw_endpoint_path(&url_str),
                send_url
                    .query()
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default()
            )
        } else {
            parts.get(1).copied().unwrap_or("/").into()
        };
        wire_request = format!(
            "{} {target} {}\r\n",
            method,
            parts.get(2).copied().unwrap_or("HTTP/1.1")
        )
        .into_bytes();
        for (name, value) in &headers {
            for ch in format!("{name}: {value}\r\n").chars() {
                wire_request.push(
                    u8::try_from(ch as u32)
                        .map_err(|_| Error::invalid("request header is not ISO-8859-1"))?,
                );
            }
        }
        wire_request.extend_from_slice(b"\r\n");
        wire_request.extend_from_slice(&req_body_bytes);
    }

    let invocation = format!("conn-inv-{}", Uuid::new_v4().simple());
    let started_at = now();
    let limit = body["budget"]["capture_limit"]
        .as_u64()
        .unwrap_or(64 * 1024 * 1024)
        .min(64 * 1024 * 1024) as usize;
    let selected_upstream = super::proxypool::select_upstream(&s, id)?;
    let capture = raw_http::exchange_via_proxy(
        &send_url,
        &method,
        &wire_request,
        timeout,
        limit,
        selected_upstream.as_ref(),
    )
    .await;
    if let Some(upstream) = selected_upstream.as_ref() {
        super::proxypool::record_outcome(
            &s,
            id,
            &upstream.node_id,
            capture.complete,
            capture.error,
        )?;
    }

    let raw_response = capture.raw;
    let truncated = !capture.complete;
    let parsed_response = super::exchange_queries::parsed_response(&raw_response, &method).ok();
    let status = parsed_response
        .as_ref()
        .and_then(|parsed| parsed["status_code"].as_u64());
    let protocol = parsed_response
        .as_ref()
        .and_then(|parsed| parsed["version"].as_str())
        .unwrap_or("HTTP/1.1")
        .to_ascii_lowercase();
    let response_headers = parsed_response
        .as_ref()
        .and_then(|parsed| parsed["headers"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|pair| Some((pair[0].as_str()?.to_owned(), pair[1].as_str()?.to_owned())))
        .collect::<Vec<_>>();
    let execution_state = if capture.complete {
        "completed"
    } else if capture.sent == 0 {
        "failed"
    } else {
        "outcome_unknown"
    };
    let outcome_known = capture.complete || capture.sent == 0;
    let exchange_id = format!("sexchange-{}", Uuid::new_v4().simple());
    let finished_at = now();
    let scheme = prepared_parsed.scheme().to_owned();
    let safe_req_headers = safe_headers(&headers);
    let safe_resp_headers = safe_headers(&response_headers);

    s.store
        .document_scope(|| -> Api<Value> {
            let mut response_ref = put_capture(
                &s,
                id,
                &raw_response,
                &format!("{exchange_id}-response.http"),
            )?;
            response_ref["original"] = json!(true);
            response_ref["complete"] = json!(if truncated { "partial" } else { "complete" });
            let stored_req_ref = if raw_request_ref.is_object() {
                raw_request_ref.clone()
            } else {
                put_capture(&s, id, &raw_request, &format!("{exchange_id}-request.http"))?
            };
            let mut capture_limitations = Vec::new();
            if truncated {
                capture_limitations.push(capture.error.unwrap_or("response_incomplete"));
            }
            if bindings.as_array().is_some_and(|v| !v.is_empty()) {
                capture_limitations.push("credential_injected_from_reference");
            }
            let authorization_ref = format!(
                "connector-egress:{connector_id}:v{}",
                p["version"].as_u64().unwrap_or(1)
            );
            let mut exchange = json!({
                "exchange_id": exchange_id,
                "task_id": id,
                "source_run_id": body["source_run_id"],
                "source_event_seq": null,
                "source_kind": "http_tool",
                "actor_ref": body["actor_ref"].as_str().unwrap_or("connector"),
                "context_id": body["context_id"].as_str().unwrap_or("default"),
                "identity_revision": 0,
                "browser_generation": 0,
                "declared_target": url_str,
                "observed_url": url_str,
                "method": method,
                "started_at": started_at,
                "finished_at": finished_at,
                "request_raw_ref": stored_req_ref.clone(),
                "response_raw_ref": response_ref.clone(),
                "request_headers": safe_req_headers,
                "response_headers": safe_resp_headers,
                "status_code": status,
                "protocol": protocol,
                "redirect_parent": null,
                "replay_of": replay_of,
                "execution_state": execution_state,
                "capture_state": if truncated { "partial" } else { "complete" },
                "capture_limitations": capture_limitations,
                "authorization_ref": authorization_ref,
                "decision_ref": invocation,
                "metadata": {
                    "gateway": "src-connector-egress-v1",
                    "connector_id": connector_id,
                    "purpose": purpose,
                    "allowed_base_url": allowed_base_url,
                    "credential_bindings": bindings,
                    "raw_representation": "original"
                }
            });
            exchange["metadata"]["transport"] = json!({
                "request_bytes_sent": capture.sent,
                "response_bytes_received": raw_response.len(),
                "peer": capture.peer,
                "tls": scheme == "https",
                "response_complete": capture.complete
            });
            exchange["metadata"]["media_type"] = response_headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| json!(value.split(';').next().unwrap_or("").trim()))
                .unwrap_or(json!(""));
            save_rec(
                &s,
                id,
                "http_exchange",
                &exchange_id,
                exchange.clone(),
                Some(0),
            )?;
            super::traffic_index::index_exchange(
                &s,
                id,
                &exchange,
                &raw_request,
                Some(&raw_response),
            )?;

            let egress_receipt = json!({
                "invocation_id": invocation,
                "task_id": id,
                "connector_id": connector_id,
                "purpose": purpose,
                "allowed_base_url": allowed_base_url,
                "grant_id": grant.as_ref().and_then(|g| g["grant_id"].as_str()),
                "draft_id": draft_id,
                "request": {
                    "method": method,
                    "url": url_str,
                    "headers": safe_req_headers,
                    "raw_ref": stored_req_ref
                },
                "response": {
                    "status_code": status,
                    "headers": safe_resp_headers,
                    "exchange_id": exchange_id,
                    "raw_ref": response_ref
                },
                "execution_state": execution_state,
                "outcome_known": outcome_known,
                "error_code": capture.error.unwrap_or(""),
                "started_at": started_at,
                "finished_at": finished_at
            });
            save_rec(
                &s,
                id,
                "connector_egress",
                &invocation,
                egress_receipt.clone(),
                Some(0),
            )?;
            let settled_receipt = json!({
                "invocation_id": invocation,
                "dispatch_state": execution_state,
                "started_at": started_at,
                "ended_at": finished_at,
                "external_receipt_ref": exchange_id,
                "result_ref": response_ref,
                "result": {"status_code": status, "exchange_id": exchange_id},
                "error_code": capture.error.unwrap_or(""),
                "outcome_known": outcome_known
            });
            let output = json!({
                "execution_id": invocation,
                "draft_id": draft_id,
                "draft_revision": body["draft_revision"],
                "connector_id": connector_id,
                "purpose": purpose,
                "execution_state": execution_state,
                "exchange_id": exchange_id,
                "exchange": exchange,
                "receipt": settled_receipt,
                "connector_egress": egress_receipt
            });
            let result = receipt(&command_id, id, output, false);
            s.store
                .put(
                    COMMANDS,
                    &command_id,
                    &json!({"request_hash": request_hash, "receipt": result}),
                )
                .map_err(db)?;
            Ok(result)
        })
        .map_err(db)?
}

fn strip_credential_from_raw(
    raw: &[u8],
    scheme: &str,
    location: &str,
    name: &str,
) -> Api<Option<Vec<u8>>> {
    let (method, url_str, headers) = parse_raw_request(raw, scheme)?;
    let mut parsed = reqwest::Url::parse(&url_str).map_err(db)?;
    let mut changed = false;
    if location == "query" {
        let remaining: Vec<(String, String)> = parsed
            .query_pairs()
            .filter(|(k, _)| k != name)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let old_count = parsed.query_pairs().count();
        if remaining.len() != old_count {
            changed = true;
            if remaining.is_empty() {
                parsed.set_query(None);
            } else {
                let mut pairs = parsed.query_pairs_mut();
                pairs.clear();
                for (k, v) in remaining {
                    pairs.append_pair(&k, &v);
                }
            }
        }
    }
    let filtered_headers: Vec<(String, String)> = headers
        .into_iter()
        .filter(|(h, _)| {
            let drop = location == "header" && h.eq_ignore_ascii_case(name);
            if drop {
                changed = true;
            }
            !drop
        })
        .collect();
    if !changed {
        return Ok(None);
    }
    let target = format!(
        "{}{}",
        super::raw_endpoint_path(parsed.as_str()),
        parsed.query().map(|q| format!("?{q}")).unwrap_or_default()
    );
    let mut out = format!("{method} {target} HTTP/1.1\r\n").into_bytes();
    for (h, v) in filtered_headers {
        for ch in format!("{h}: {v}\r\n").chars() {
            out.push(
                u8::try_from(ch as u32).map_err(|_| Error::invalid("header is not ISO-8859-1"))?,
            );
        }
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(request_body(raw)?);
    Ok(Some(out))
}

pub(super) async fn execute_draft_inner(
    s: Arc<AppState>,
    id: &str,
    body: &Value,
    allow_reauth: bool,
) -> Api<Value> {
    if let Some(run) = body["source_run_id"].as_str().filter(|run| !run.is_empty()) {
        if !records(&s, id, "run_binding")?
            .iter()
            .any(|row| row["payload"]["run_id"] == run)
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.run_not_bound",
                "HTTP source Run is outside this Task",
            ));
        }
    }
    let key = require(body, "idempotency_key")?;
    let maximum = body["budget"]["max_redirects"].as_u64().unwrap_or(0);
    if maximum > 1024 {
        return Err(Error::invalid("max_redirects exceeds 1024"));
    }
    let mut hop = body.clone();
    hop["root_body"] = body.clone();
    let mut redirects = Vec::new();
    let mut first = None;
    for index in 0..=maximum {
        let mut result = execute_one(s.clone(), id, &hop).await?;
        if body["wait_for_approval"] == true {
            while result["output"]["execution_state"] == "waiting_approval" {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let command = format!(
                    "{id}:src.exchange.draft.execute:{}",
                    require(&hop, "idempotency_key")?
                );
                let saved = s
                    .store
                    .get(COMMANDS, &command)
                    .map_err(db)?
                    .ok_or_else(|| Error::conflict("HTTP command disappeared while waiting"))?;
                result = cached_result(&s, id, &saved)?;
                let expired = result["output"]["approval"]["expires_at"]
                    .as_str()
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    .is_some_and(|value| value <= Utc::now());
                if expired {
                    let invocation = require(&result["output"]["receipt"], "invocation_id")?;
                    s.store
                        .document_scope(|| -> Api<()> {
                            if receipt_state(&s, id, invocation)?["dispatch_state"]
                                == "waiting_approval"
                            {
                                let approval_id =
                                    require(&result["output"]["approval"], "approval_id")?;
                                let row = rec(&s, id, "execution_approval", approval_id)?;
                                if row["payload"]["status"] == "pending" {
                                    let mut approval = row["payload"].clone();
                                    approval["status"] = json!("expired");
                                    save_rec(
                                        &s,
                                        id,
                                        "execution_approval",
                                        approval_id,
                                        approval,
                                        row["revision"].as_u64(),
                                    )?;
                                }
                                settle_failure(&s, id, invocation, "approval_expired", true)?;
                            }
                            Ok(())
                        })
                        .map_err(db)??;
                    return Err(Error::new(
                        StatusCode::REQUEST_TIMEOUT,
                        "approval_expired",
                        "HTTP approval expired",
                    ));
                }
            }
        }
        let output = result["output"].clone();
        if first.is_none() {
            first = Some(result.clone());
        } else {
            redirects.push(output.clone());
        }
        let mut finish = first.clone().unwrap();
        finish["output"]["redirect_results"] = json!(redirects);
        for field in [
            "exchange_id",
            "exchange",
            "execution_state",
            "receipt",
            "execution_record_revision",
        ] {
            if let Some(value) = output.get(field) {
                finish["output"][field] = value.clone();
            }
        }
        let exchange = &output["exchange"];
        let status = exchange["status_code"].as_u64().unwrap_or(0);
        let location = exchange["response_headers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|row| {
                row[0]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("location"))
            })
            .and_then(|row| row[1].as_str());
        let will_redirect = output["execution_state"] == "completed"
            && index < maximum
            && matches!(status, 301 | 302 | 303 | 307 | 308)
            && location.is_some();
        if !will_redirect {
            if allow_reauth
                && output["execution_state"] == "completed"
                && result["deduplicated"] != true
            {
                let ctx_id = body["context_id"]
                    .as_str()
                    .or_else(|| output["exchange"]["context_id"].as_str())
                    .unwrap_or("");
                let status_code = output["exchange"]["status_code"].as_u64();
                if let Some(reauth_info) = super::identity::observe_exchange_auth_status(
                    s.clone(),
                    id,
                    ctx_id,
                    status_code,
                )
                .await?
                {
                    if reauth_info["triggered"] == true
                        && reauth_info["succeeded"] == true
                        && exchange["method"].as_str().is_some_and(retry_safe_method)
                        && first
                            .as_ref()
                            .and_then(|r| r["output"]["exchange"]["method"].as_str())
                            .is_some_and(retry_safe_method)
                    {
                        let inject = &reauth_info["recipe"]["inject"];
                        let loc = inject["location"].as_str().unwrap_or("header");
                        let name = inject["name"].as_str().unwrap_or("Authorization");
                        let fmt = inject["format"].as_str().unwrap_or("bearer");
                        let target_ref = reauth_info["refreshed"]["credential_ref"]
                            .as_str()
                            .unwrap_or("");
                        let new_ver = &reauth_info["refreshed"]["credential_version"];
                        let mut retry_body = body.clone();
                        let draft_id = require(&hop, "draft_id")?;
                        let draft = rec(&s, id, "request_draft", draft_id)?["payload"].clone();
                        let scheme = draft["target_url"]
                            .as_str()
                            .and_then(|u| reqwest::Url::parse(u).ok())
                            .map(|u| u.scheme().to_owned())
                            .unwrap_or_else(|| "http".to_owned());
                        let artifact = super::verify_ref(&s, id, &draft["raw_request_ref"])?;
                        use base64::Engine;
                        let raw = base64::engine::general_purpose::STANDARD
                            .decode(artifact["data"].as_str().unwrap_or(""))
                            .map_err(|_| Error::invalid("draft artifact invalid"))?;
                        if let Some(stripped) = strip_credential_from_raw(&raw, &scheme, loc, name)?
                        {
                            let encoded =
                                base64::engine::general_purpose::STANDARD.encode(&stripped);
                            let source_eid = draft["source_exchange_id"].as_str().unwrap_or("");
                            let next_draft = super::create_next_draft(
                                &s,
                                id,
                                source_eid,
                                Some(&encoded),
                                json!({"reauth_strip_stale_credential":name}),
                                false,
                            )?;
                            retry_body["draft_id"] = next_draft["draft"]["draft_id"].clone();
                            retry_body["draft_revision"] = next_draft["draft"]["revision"].clone();
                        } else {
                            retry_body["draft_id"] = json!(draft_id);
                            retry_body["draft_revision"] = draft["revision"].clone();
                        }
                        let mut bindings: Vec<Value> = retry_body["credential_bindings"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|b| {
                                !(b["location"] == loc
                                    && b["name"]
                                        .as_str()
                                        .is_some_and(|n| n.eq_ignore_ascii_case(name)))
                            })
                            .cloned()
                            .collect();
                        bindings.push(
                            json!({"location":loc,"name":name,"reference":target_ref,"format":fmt}),
                        );
                        retry_body["credential_bindings"] = json!(bindings);
                        retry_body["authorization_version"] = policy(&s, id)?["version"].clone();
                        retry_body["idempotency_key"] = json!(format!("{key}:reauth:v{new_ver}"));
                        let mut retried =
                            Box::pin(execute_draft_inner(s.clone(), id, &retry_body, false))
                                .await?;
                        retried["output"]["reauth"] = json!({
                            "triggered":true,
                            "succeeded":true,
                            "initial_exchange_id":output["exchange_id"],
                            "initial_status_code":status_code,
                            "login_exchange_id":reauth_info["refreshed"]["login_exchange_id"],
                            "credential_version":new_ver
                        });
                        return Ok(retried);
                    }
                    finish["output"]["reauth"] = reauth_info;
                }
            }
            return Ok(finish);
        }
        if body["budget"]["max_requests"]
            .as_u64()
            .is_some_and(|limit| index + 1 >= limit)
        {
            finish["output"]["redirect_stop_reason"] = json!("request_budget_exhausted");
            return Ok(finish);
        }
        let old = reqwest::Url::parse(require(exchange, "observed_url")?).map_err(db)?;
        let target = old.join(location.unwrap()).map_err(db)?;
        browser_runtime::target_scope(&s, id, target.as_str())?;
        let original = rec(&s, id, "request_draft", require(&hop, "draft_id")?)?["payload"].clone();
        let artifact = super::verify_ref(&s, id, &original["raw_request_ref"])?;
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(artifact["data"].as_str().unwrap_or(""))
            .map_err(|_| Error::invalid("redirect draft artifact invalid"))?;
        let cross_origin = old.origin() != target.origin();
        let raw = redirect_request(&raw, target.as_str(), cross_origin)?;
        let draft_id = format!("sredirect_{}", &digest(&json!([id, key, index + 1]))[..24]);
        if rec(&s, id, "request_draft", &draft_id).is_err() {
            let reference = put_capture(&s, id, &raw, &format!("{draft_id}.http"))?;
            let (method, url, _) = parse_raw_request(&raw, target.scheme())?;
            save_rec(
                &s,
                id,
                "request_draft",
                &draft_id,
                json!({"draft_id":draft_id,"task_id":id,"source_exchange_id":original["source_exchange_id"],"revision":1,"raw_request_ref":reference,"target_url":url,"method":method,"changes":{"redirect_of":exchange["exchange_id"]},"created_at":now()}),
                Some(0),
            )?;
        }
        hop["draft_id"] = json!(draft_id);
        hop["draft_revision"] = json!(1);
        hop["idempotency_key"] = json!(format!("{key}:redirect:{}", index + 1));
        hop["redirect_parent"] = exchange["exchange_id"].clone();
        if cross_origin {
            hop["credential_ref"] = json!("");
            hop["credential_bindings"] = json!([]);
        }
    }
    Err(Error::conflict("redirect chain did not terminate"))
}
fn redirect_request(raw: &[u8], target: &str, strip_credentials: bool) -> Api<Vec<u8>> {
    let url = reqwest::Url::parse(target).map_err(db)?;
    let (method, _, headers) = parse_raw_request(raw, url.scheme())?;
    let path = format!(
        "{}{}",
        super::raw_endpoint_path(target),
        url.query()
            .map(|value| format!("?{value}"))
            .unwrap_or_default()
    );
    let mut result = format!("{method} {path} HTTP/1.1\r\n").into_bytes();
    let authority = target
        .split_once("://")
        .map(|(_, tail)| tail.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    result.extend_from_slice(format!("Host: {authority}\r\n").as_bytes());
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if lower == "host"
            || (strip_credentials
                && matches!(
                    lower.as_str(),
                    "authorization" | "cookie" | "cookie2" | "proxy-authorization"
                ))
        {
            continue;
        }
        for ch in format!("{name}: {value}\r\n").chars() {
            result.push(
                u8::try_from(ch as u32).map_err(|_| Error::invalid("header is not ISO-8859-1"))?,
            );
        }
    }
    result.extend_from_slice(b"\r\n");
    result.extend_from_slice(request_body(raw)?);
    Ok(result)
}
