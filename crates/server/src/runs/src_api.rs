//! Live SRC tasks. Historical imports remain in `history_api`.
mod control_command;
pub(crate) mod composer_refs;
mod cursors;
mod domain_command;
mod operation_contracts;
mod execution_policy;
pub(crate) mod execution_runtime;
pub(crate) mod native_policy;
mod domains;
pub mod debug_logs;
pub(super) mod run_projection;
use crate::{AppState, chat, runs};
use axum::{
    Json,
    body::{Bytes, to_bytes},
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};

const TASKS: &str = "src_tasks";
const RECORDS: &str = "src_live_records";
const COMMANDS: &str = "src_live_commands";
pub(super) const POLICIES: &str = "src_live_policies";
const APPROVAL_APPLIED: &str = "src_approval_applied_commands";
const SLICE_APPLIED: &str = "src_workbench_slice_applied_commands";
const FINDING_APPLIED: &str = "src_finding_applied_commands";
const ASSET_OBSERVE_APPLIED: &str = "src_asset_observe_applied_commands";
const ASSET_RELATE_APPLIED: &str = "src_asset_relate_applied_commands";
const ASSET_DERIVE_APPLIED: &str = "src_asset_derive_applied_commands";
const ASSET_LIFECYCLE_APPLIED: &str = "src_asset_lifecycle_applied_commands";
const ASSET_RESTRUCTURE_APPLIED: &str = "src_asset_restructure_applied_commands";
const TASK_LIFECYCLE_APPLIED: &str = "src_task_lifecycle_applied_commands";
const RUN_LIFECYCLE_APPLIED: &str = "src_run_lifecycle_applied_commands";
const RUN_DESTROY_APPLIED: &str = "src_run_destroy_applied_commands";
const TASK_DESTROY_APPLIED: &str = "src_task_destroy_applied_commands";
const ARTIFACT_DESTROY_APPLIED: &str = "src_artifact_destroy_applied_commands";
const RESEARCH_PLAN_APPLIED: &str = "src_research_plan_applied_commands";
const RESEARCH_AUTH_APPLIED: &str = "src_research_authorization_applied_commands";
const RESEARCH_SKILL_APPLIED: &str = "src_research_skill_applied_commands";
const RESEARCH_OPENAPI_APPLIED: &str = "src_research_openapi_applied_commands";
const RESEARCH_JAVASCRIPT_APPLIED: &str = "src_research_javascript_applied_commands";
const RESEARCH_OBSERVATION_APPLIED: &str = "src_research_observation_applied_commands";
const RESEARCH_BUSINESS_APPLIED: &str = "src_research_business_applied_commands";
const RESEARCH_WORKFLOW_APPLIED: &str = "src_research_workflow_applied_commands";
const COVERAGE_APPLIED: &str = "src_coverage_applied_commands";
const REPORT_APPLIED: &str = "src_report_applied_commands";
const REPORT_HEADS: &str = "src_report_sequence_heads";
const TASK_EVENTS: &str = "src_task_events";
const TASK_EVENT_HEADS: &str = "src_task_event_heads";
const SOURCE_WATERMARKS: &str = "src_source_watermarks";
const CHAT_CONFIG_KEYS: &str = "src_chat_model_config_keys";
const WORKER_CONFIG_KEYS: &str = "src_worker_model_config_keys";

const AGENT_FORBIDDEN_RECORDS: &[&str] = &[];

pub(crate) fn agent_forbidden_record_kind(kind: &str) -> bool {
    AGENT_FORBIDDEN_RECORDS.contains(&kind)
}
pub(crate) fn supports_domain_command(kind: &str) -> bool {
    domain_command::supported(kind)
}
pub(crate) fn agent_allowed_command(kind: &str) -> bool {
    domain_command::agent_allowed(kind)
}
pub(crate) fn agent_settings_command(kind: &str) -> bool {
    domain_command::agent_settings_command(kind)
}

fn source_key(task_id: &str, source_id: &str) -> String {
    json!([task_id, source_id]).to_string()
}
pub(in crate::runs::src_api) fn source_watermarks(s: &AppState, task_id: &str) -> Api<Vec<Value>> {
    let mut rows: Vec<Value> = s
        .store
        .list(SOURCE_WATERMARKS)
        .map_err(db)?
        .into_iter()
        .filter(|row| row["task_id"] == task_id)
        .map(|mut row| {
            if let Some(object) = row.as_object_mut() {
                object.remove("task_id");
            }
            row
        })
        .collect();
    rows.sort_by(|a, b| a["source_id"].as_str().cmp(&b["source_id"].as_str()));
    Ok(rows)
}
pub(in crate::runs::src_api) fn advance_source(
    s: &AppState,
    task_id: &str,
    source_id: &str,
    generation: u64,
    next_seq: u64,
    extractor_version: &str,
    state: &str,
    error: &str,
) -> Api<Value> {
    let key = source_key(task_id, source_id);
    loop {
        let current = s.store.get(SOURCE_WATERMARKS, &key).map_err(db)?;
        if current
            .as_ref()
            .is_some_and(|row| row["generation"].as_u64().unwrap_or(0) > generation)
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "authorization_stale",
                "source generation is stale",
            ));
        }
        if current
            .as_ref()
            .is_some_and(|row| row["next_seq"].as_u64().unwrap_or(0) > next_seq)
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "version_conflict",
                "source watermark cannot move backwards",
            ));
        }
        let row = json!({"task_id":task_id,"source_id":source_id,"generation":generation,
            "next_seq":next_seq,"extractor_version":extractor_version,"state":state,
            "error":error,"updated_at":now()});
        if s.store
            .put_many_if(
                &[(SOURCE_WATERMARKS, &key, current.as_ref())],
                &[(SOURCE_WATERMARKS, &key, &row)],
            )
            .map_err(db)?
        {
            return Ok(row);
        }
    }
}
fn watermark_partial_reason(rows: &[Value]) -> Option<String> {
    let reasons: Vec<String> = rows
        .iter()
        .filter(|row| matches!(row["state"].as_str(), Some("failed" | "pending")))
        .map(|row| {
            format!(
                "{}: {}",
                row["source_id"].as_str().unwrap_or(""),
                row["error"]
                    .as_str()
                    .filter(|error| !error.is_empty())
                    .unwrap_or(row["state"].as_str().unwrap_or("pending"))
            )
        })
        .collect();
    if reasons.is_empty() {
        None
    } else {
        Some(reasons.join("；"))
    }
}

#[derive(Debug)]
pub(crate) struct Error {
    pub(crate) status: StatusCode,
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) detail: Value,
}
impl Error {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            detail: json!({}),
        }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "src.invalid_request", message)
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "version_conflict", message)
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error":{"code":self.code,"message":self.message,"detail":self.detail}})),
        )
            .into_response()
    }
}
type Api<T> = Result<T, Error>;
fn db(error: impl std::fmt::Display) -> Error {
    tracing::error!(%error,"SRC store failure");
    Error::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "src.store_failed",
        "SRC 数据存储失败",
    )
}
fn respond(result: Api<Value>) -> Response {
    match result {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}
fn require<'a>(v: &'a Value, k: &str) -> Api<&'a str> {
    v[k].as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::invalid(format!("{k} is required")))
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn digest(v: &Value) -> String {
    format!("{:x}", Sha256::digest(source_compact_json(v).as_bytes()))
}
pub(crate) fn source_compact_json(value: &Value) -> String {
    match value {
        Value::Object(fields) => {
            let mut keys = fields.keys().collect::<Vec<_>>();
            keys.sort();
            format!("{{{}}}", keys.into_iter()
                .map(|key| format!("{}:{}",serde_json::to_string(key).unwrap(),
                    source_compact_json(&fields[key])))
                .collect::<Vec<_>>().join(","))
        }
        Value::Array(rows) => format!("[{}]",rows.iter()
            .map(source_compact_json).collect::<Vec<_>>().join(",")),
        _ => value.to_string(),
    }
}
fn policy_digest(value: &Value) -> String {
    let mut source = value.clone();
    source["digest"] = json!("");
    source["activation"] = json!({});
    digest(&source)
}
fn browser_mutation_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}
fn receipt(command: &str, id: &str, output: Value, deduplicated: bool) -> Value {
    json!({"command_id":command,"receipt_id":command,"state":"completed","aggregate":{"type":"task","id":id},"output":output,"error":null,"deduplicated":deduplicated})
}
fn rec_key(id: &str, kind: &str, key: &str) -> String {
    format!("srec:{id}:{kind}:{key}")
}
fn kind_matches(actual:&str,requested:&str)->bool{actual==requested}

pub(crate) fn get_task_raw(s: &AppState, id: &str) -> Api<Value> {
    task(s, id)
}
pub(crate) fn guardrail_platform_policy_banner(s: &AppState, task_id: &str) -> Api<String> {
    domains::guardrails_banner(s, task_id)
}
pub(crate) fn guardrail_refusal_clarify(
    s: &AppState,
    task_id: &str,
    chain_key: &str,
    stop_reason: &str,
    model_id: &str,
) -> Api<(bool, Option<String>)> {
    domains::guardrails_refusal(s, task_id, chain_key, stop_reason, model_id)
}
pub(crate) fn guardrail_coverage_gate_met(s: &AppState, task_id: &str) -> Api<(bool, Value)> {
    domains::guardrails_coverage(s, task_id)
}
pub(crate) fn guardrail_run_coverage_gate_met(
    s: &AppState,
    task_id: &str,
    run_id: &str,
) -> Api<(bool, Value)> {
    domains::guardrails_run_coverage(s, task_id, run_id)
}
fn task(s: &AppState, id: &str) -> Api<Value> {
    let task = s.store.get(TASKS, id).map_err(db)?.ok_or_else(|| {
        Error::new(
            StatusCode::NOT_FOUND,
            "src.task_not_found",
            "SRC Task 不存在",
        )
    })?;
    let binding = &task["binding"];
    if binding["task_id"] == id && binding["thread_id"].as_str().is_some_and(|thread| !thread.is_empty()) {
        let key = rec_key(id, "task_binding", id);
        if s.store.get(RECORDS, &key).map_err(db)?.is_none() {
            let created_at = task["created_at"].as_str().unwrap_or("");
            let record = json!({"version":1,"values":{
                "record_id":id,"kind":"task_binding","task_id":id,
                "revision":1,"parent_id":"","natural_key":binding["thread_id"],
                "generation":0,"created_at":created_at,"updated_at":created_at,
                "payload":binding}});
            s.store.put_if_absent(RECORDS, &key, &record).map_err(db)?;
        }
    }
    Ok(task)
}
fn public_task(mut task: Value) -> Value {
    if let Some(object) = task.as_object_mut() {
        object.remove("request_hash");
    }
    task
}
pub fn has_task(s: &AppState, id: &str) -> Result<bool, rusqlite::Error> {
    Ok(s.store.get(TASKS, id)?.is_some())
}
fn rec(s: &AppState, id: &str, kind: &str, rid: &str) -> Api<Value> {
    let saved = match s.store.get(RECORDS, rid).map_err(db)? {
        Some(row) if row["values"]["task_id"] == id && kind_matches(row["values"]["kind"].as_str().unwrap_or(""), kind) => Some(row),
        _ => {
            let direct_new = rec_key(id, kind, rid);
            if let Some(row) = s.store.get(RECORDS, &direct_new).map_err(db)?.filter(|row| row["values"]["task_id"] == id && kind_matches(row["values"]["kind"].as_str().unwrap_or(""), kind)) {
                Some(row)
            } else {
                s.store.list_src_live_records(id, kind).map_err(db)?.into_iter().find(|v| {
                    v["values"]["task_id"] == id
                        && kind_matches(v["values"]["kind"].as_str().unwrap_or(""), kind)
                        && (v["values"]["natural_key"] == rid
                            || v["values"]["record_id"] == rid
                            || [
                                "record_id",
                                "document_id",
                                "material_id",
                                "source_id",
                                "query_id",
                                "claim_id",
                                "report_id",
                                "review_id",
                                "approval_id",
                                "reproduction_id",
                                "publication_id",
                                "dataset_id",
                                "entry_id",
                                "version_id",
                                "batch_id",
                                "retest_id",
                                "monitor_id",
                                "exchange_id",
                                "draft_id",
                                "observation_id",
                                "relation_id",
                                "tab_id",
                                "merge_id",
                                "verdict_id",
                                "todo_id",
                                "suggestion_id",
                                "feedback_id",
                                "status_id",
                            ]
                            .iter()
                            .any(|field| v["values"]["payload"][*field] == rid))
                })
            }
        }
    }
    .ok_or_else(|| {
        Error::new(
            StatusCode::NOT_FOUND,
            "src.record_not_found",
            "SRC 记录不存在",
        )
    })?;
    let row = &saved["values"];
    if row["task_id"] != id || !kind_matches(row["kind"].as_str().unwrap_or(""), kind) {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "src.record_not_found",
            "SRC 记录不存在",
        ));
    }
    Ok(row.clone())
}
fn records(s: &AppState, id: &str, kind: &str) -> Api<Vec<Value>> {
    task(s, id)?;
    let mut rows: Vec<_> = s
        .store
        .list_src_live_records(id, kind)
        .map_err(db)?
        .into_iter()
        .map(|v| v["values"].clone())
        .filter(|v| v["task_id"] == id && (kind.is_empty() || kind_matches(v["kind"].as_str().unwrap_or(""), kind)))
        .collect();
    rows.sort_by(|a, b| {
        a["created_at"]
            .as_str()
            .cmp(&b["created_at"].as_str())
            .then(a["record_id"].as_str().cmp(&b["record_id"].as_str()))
    });
    Ok(rows)
}
/// Complete current SRC research objects for a Run prompt. The newest inserted
/// immutable context version is selected across the Task's full history.
pub fn worker_surface_context(s: &AppState, id: &str) -> Result<Value, String> {
    let latest = records_with_rowid(s, id, "business_context").map_err(|error| error.message)?
        .into_iter().max_by_key(|(rowid, _)| *rowid).map(|(_, row)| row);
    let audit = query_projection(s, id, "src.coverage.audit", &json!({}))
        .map_err(|(status, body)| format!("SRC coverage audit failed ({status}): {body}"))?;
    let critical = audit["critical_unresolved_items"].as_array().cloned().unwrap_or_default();
    let blockers = audit["open_blockers"].as_array().cloned().unwrap_or_default();
    if latest.is_none() && critical.is_empty() && blockers.is_empty() { return Ok(Value::Null); }
    // Keep the selected record and its payload intact, including source IDs,
    // parameters, all ID-flow consumers, and the persisted version/reference.
    Ok(json!({"taskId":id,"businessContext":latest,
        "unresolvedHighPriorityHypotheses":critical,"openTargetBlockers":blockers}))
}

fn records_with_rowid(s: &AppState, id: &str, kind: &str) -> Api<Vec<(i64, Value)>> {
    task(s, id)?;
    Ok(s.store.list_inserted_with_rowid(RECORDS).map_err(db)?
        .into_iter()
        .map(|(rowid, stored)| (rowid, stored["values"].clone()))
        .filter(|(_, row)| row["task_id"] == id
            && (kind.is_empty() || kind_matches(row["kind"].as_str().unwrap_or(""), kind)))
        .collect())
}
fn save_rec(
    s: &AppState,
    id: &str,
    kind: &str,
    key: &str,
    payload: Value,
    expected: Option<u64>,
) -> Api<Value> {
    let (rid, version, row) = stage_rec(s, id, kind, key, payload, expected)?;
    match s
        .store
        .compare_and_put(RECORDS, &rid, version, row.clone())
        .map_err(db)?
    {
        Ok(_) => Ok(row),
        Err(_) => Err(Error::conflict("SRC 记录版本已变化，请刷新后重试")),
    }
}
fn stage_rec(
    s: &AppState,
    id: &str,
    kind: &str,
    key: &str,
    payload: Value,
    expected: Option<u64>,
) -> Api<(String, u64, Value)> {
    let new_rid = rec_key(id, kind, key);
    let current = s.store.get(RECORDS, &new_rid).map_err(db)?;
    let rid = new_rid;
    let version = current
        .as_ref()
        .and_then(|v| v["version"].as_u64())
        .unwrap_or(0);
    if expected.is_some_and(|v| v != version) {
        return Err(Error::conflict("SRC 记录版本已变化，请刷新后重试"));
    }
    let created = current
        .as_ref()
        .and_then(|v| v["values"]["created_at"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(now);
    let mut parent = match kind {
        "asset_observation" | "asset_relation" | "asset_lifecycle" | "asset_version" => payload["asset_id"]
            .as_str()
            .or_else(|| payload["from_asset_id"].as_str()),
        "request_draft" => payload["source_exchange_id"].as_str(),
        "browser_tab" | "browser_screenshot" | "browser_page_event" | "browser_execution" => {
            payload["context_id"].as_str()
        }
        "image_review_read" | "image_review" => payload["screenshot_id"].as_str(),
        "execution_approval" => payload["invocation_id"].as_str(),
        "object_experiment" => payload["object_record_id"].as_str().or_else(||
            current.as_ref().and_then(|row| row["values"]["parent_id"].as_str())),
        "object_state_event" => payload["experiment_id"].as_str(),
        "task_grant_environment" => Some(key),
        "frozen_report" | "finding_claim" | "finding_review" | "finding_stale"
        | "finding_dedup" => payload["finding_id"].as_str(),
        "platform_status" | "platform_feedback" => payload["report_id"].as_str(),
        "finding_retest" => payload["finding_id"].as_str(),
        "retest_review" => payload["retest_id"].as_str(),
        "knowledge_publication" => payload["entry_id"].as_str(),
        "import_batch_revert" => payload["batch_id"].as_str(),
        _ => current
            .as_ref()
            .and_then(|v| v["values"]["parent_id"].as_str())
            .filter(|v| !v.is_empty()),
    }
    .unwrap_or("")
    .to_owned();
    if kind == "exchange_comparison" {
        parent = format!("{}:{}", payload["baseline_id"].as_str().unwrap_or(""),
            payload["experiment_id"].as_str().unwrap_or(""));
    }
    let record_id = if let Some(existing_id) = current
        .as_ref()
        .and_then(|row| row["values"]["record_id"].as_str())
        .filter(|v| !v.is_empty())
    {
        existing_id.to_owned()
    } else {
        match kind {
            "asset" | "asset_tombstone" | "task_asset_link" => payload["asset_id"]
                .as_str()
                .unwrap_or(&rid)
                .to_owned(),
            "asset_observation" => payload["observation_id"]
                .as_str()
                .unwrap_or(&rid)
                .to_owned(),
            "asset_relation" => payload["relation_id"]
                .as_str()
                .unwrap_or(&rid)
                .to_owned(),
            "asset_alias" => payload["source_asset_id"]
                .as_str()
                .unwrap_or(&rid)
                .to_owned(),
            "asset_version" => payload["version_id"]
                .as_str()
                .unwrap_or(&rid)
                .to_owned(),
            "knowledge_dataset" => payload["dataset_id"]
                .as_str()
                .unwrap_or(key)
                .to_owned(),
            "knowledge_publication" => payload["publication_id"]
                .as_str()
                .unwrap_or(key)
                .to_owned(),
            "knowledge_entry" => payload["entry_id"]
                .as_str()
                .unwrap_or(key)
                .to_owned(),
            "execution_approval" => payload["approval_id"]
                .as_str()
                .unwrap_or(key)
                .to_owned(),
            "test_object" => format!("sobject-{}", uuid::Uuid::new_v4().simple()),
            "object_experiment" | "object_state_event" | "exchange_comparison" => key.to_owned(),
            "task_grant" => payload["grant_id"]
                .as_str()
                .unwrap_or(key)
                .to_owned(),
            "task_grant_environment" => key.to_owned(),
            "finding_retest" => payload["retest_id"]
                .as_str()
                .unwrap_or(key)
                .to_owned(),
            "retest_review" => payload["review_id"]
                .as_str()
                .unwrap_or(key)
                .to_owned(),
            "import_batch" => payload["batch_id"]
                .as_str()
                .unwrap_or(key)
                .to_owned(),
            "workbench_finding" => payload["finding_id"]
                .as_str()
                .map(|fid| format!("{fid}:meta"))
                .unwrap_or_else(|| rid.clone()),
            "authorization_snapshot" => payload["record_id"]
                .as_str()
                .unwrap_or(key)
                .to_owned(),
            "src_thread_delegation" => key.to_owned(),
            _ => rid.clone(),
        }
    };
    let natural_key = if kind == "asset_observation" {
        current
            .as_ref()
            .and_then(|row| row["values"]["natural_key"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                payload["source_event_id"]
                    .as_str()
                    .filter(|event| !event.is_empty())
                    .map(|event| {
                        format!(
                            "{}:{}:{event}",
                            payload["source_kind"].as_str().unwrap_or(""),
                            payload["source_id"].as_str().unwrap_or("")
                        )
                    })
                    .unwrap_or_default()
            })
    } else if kind == "finding_retest" {
        format!("{}:{}:{}", payload["finding_id"].as_str().unwrap_or(""),
            payload["report_id"].as_str().unwrap_or(""),
            payload["idempotency_key"].as_str().unwrap_or(""))
    } else if kind == "retest_review" {
        String::new()
    } else if matches!(kind, "execution_approval" | "task_grant" | "task_grant_environment"
        | "object_experiment" | "object_state_event" | "exchange_comparison") {
        String::new()
    } else {
        key.to_owned()
    };
    let mut stored_payload = payload;
    if kind == "object_experiment" {
        stored_payload.as_object_mut().map(|fields| fields.remove("object_record_id"));
    }
    let row = json!({"record_id":record_id,"kind":kind,"task_id":id,"revision":version+1,
        "parent_id":parent,"natural_key":natural_key,"generation":if matches!(kind,"browser_service"|"browser_context"){stored_payload["browser_generation"].as_u64().unwrap_or(0)}else{current.as_ref().and_then(|row|row["values"]["generation"].as_u64()).unwrap_or(0)},"created_at":created,"updated_at":now(),"payload":stored_payload});
    Ok((rid, version, row))
}
fn public_runtime_config(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.to_ascii_lowercase().as_str(),
                        "api_key"
                            | "apikey"
                            | "password"
                            | "secret"
                            | "secret_key"
                            | "access_token"
                            | "refresh_token"
                            | "private_key"
                            | "token"
                            | "credential"
                            | "credentials"
                            | "client_secret"
                    )
                })
                .map(|(key, value)| (key.clone(), public_runtime_config(value)))
                .collect(),
        ),
        Value::Array(rows) => json!(rows.iter().map(public_runtime_config).collect::<Vec<_>>()),
        _ => value.clone(),
    }
}
fn chat_runtime_selection(s: &AppState, task_id: &str, thread_id: &str) -> Api<Value> {
    let thread = s.store.get("conversation_threads", thread_id).map_err(db)?
        .unwrap_or(Value::Null);
    let selected = s.store.get("conversation_runtime_selections", thread_id).map_err(db)?
        .filter(|value| value.as_object().is_some_and(|fields| !fields.is_empty()))
        .or_else(|| thread["runtime_selection"].as_object()
            .filter(|fields| !fields.is_empty()).map(|_| thread["runtime_selection"].clone()))
        .or_else(|| thread["runtime"].as_object()
            .filter(|fields| !fields.is_empty()).map(|_| thread["runtime"].clone()))
        .unwrap_or_else(|| json!({}));
    let task = s.store.get(TASKS, task_id).map_err(db)?.unwrap_or(Value::Null);
    let timestamp = selected["updated_at"].as_str()
        .or_else(|| thread["thread"]["created_at"].as_str())
        .or_else(|| thread["thread"]["updated_at"].as_str())
        .or_else(|| task["created_at"].as_str())
        .unwrap_or("");
    let mut resolved = json!({
        "schema_version":1,"thread_id":thread_id,"adapter_id":"","instance_id":"default",
        "credential_id":"","model":"","effort":"","service_tier":"",
        "access_mode":"supervised","interaction_mode":"default",
        "permission_mode":"","sandbox_mode":"",
        "updated_at":if timestamp.is_empty() {now()} else {timestamp.to_owned()}
    });
    for key in ["adapter_id","instance_id","credential_id","model","effort",
        "service_tier","access_mode","interaction_mode","permission_mode",
        "sandbox_mode","updated_at"] {
        if !selected[key].is_null() {
            resolved[key] = selected[key].clone();
        }
    }
    if resolved["credential_id"] == "" {
        if let Some(reference) = selected["credential_ref"].as_str() {
            resolved["credential_id"] = json!(reference);
        } else if let Some(adapter) = resolved["adapter_id"].as_str() {
            let engine = adapter.strip_prefix("cli.").unwrap_or_else(||adapter.split('.').next().unwrap_or(""));
            if matches!(engine,"codex"|"claude"|"cursor"|"grok"|"opencode"|"pi"|
                "kimi"|"omp"|"devin"|"droid") {
                resolved["credential_id"] = json!(format!("system:{engine}"));
            }
        }
    }
    for (key, default) in [("instance_id","default"),("access_mode","supervised"),
        ("interaction_mode","default")] {
        if resolved[key].as_str().is_none_or(str::is_empty) {resolved[key] = json!(default);}
    }
    for key in ["effort","service_tier"] {
        if resolved[key] == "default" { resolved[key] = json!(""); }
    }
    Ok(public_runtime_config(&resolved))
}
fn chat_configuration(s: &AppState, id: &str, thread_id: &str) -> Api<Value> {
    let resolved = chat_runtime_selection(s, id, thread_id)?;
    let existing: Vec<Value> = records(s, id, "model_config")?
        .into_iter()
        .filter(|row| row["payload"]["role"] == "chat")
        .collect();
    if let Some(row) = existing
        .iter()
        .find(|row| row["payload"]["resolved"] == resolved)
    {
        return Ok(row.clone());
    }
    let marker_key = json!([id, resolved]).to_string();
    if let Some(marker) = s.store.get(CHAT_CONFIG_KEYS, &marker_key).map_err(db)? {
        return rec(
            s,
            id,
            "model_config",
            marker["record_id"].as_str().unwrap_or(""),
        );
    }
    let version = format!("{:x}", Sha256::digest(source_compact_json(&resolved).as_bytes()));
    let payload = json!({"role":"chat","version":version,"revision":existing.len()+1,
        "source":{"kind":"conversation.runtime_selection"},"resolved":resolved});
    let (record_key, _, mut row) = stage_rec(s, id, "model_config", &version, payload, Some(0))?;
    row["record_id"] = json!(format!("{id}:chat:{version}"));
    row["parent_id"] = json!("chat");
    row["natural_key"] = json!("");
    let record_doc = json!({"version":1,"values":row});
    let marker = json!({"record_id":row["record_id"]});
    if s.store
        .put_many_if(
            &[
                (RECORDS, &record_key, None),
                (CHAT_CONFIG_KEYS, &marker_key, None),
            ],
            &[
                (RECORDS, &record_key, &record_doc),
                (CHAT_CONFIG_KEYS, &marker_key, &marker),
            ],
        )
        .map_err(db)?
    {
        return Ok(row);
    }
    let marker = s
        .store
        .get(CHAT_CONFIG_KEYS, &marker_key)
        .map_err(db)?
        .ok_or_else(|| Error::conflict("chat configuration changed"))?;
    rec(
        s,
        id,
        "model_config",
        marker["record_id"].as_str().unwrap_or(""),
    )
}
fn worker_configuration(s: &AppState, task_id: &str, run_id: &str) -> Api<Value> {
    let mut resolved = crate::settings_metadata::configuration(s)
        .map_err(|_| db("Worker configuration is unavailable"))?;
    if let Some(fields) = resolved.as_object_mut() {
        fields.retain(|key, _| !matches!(key.as_str(), "effective_network" | "network_requested"
            | "network_reason" | "network_error" | "in_container" | "review" | "verifier"));
    }
    let resolved = public_runtime_config(&resolved);
    let version = format!("{:x}", Sha256::digest(source_compact_json(&resolved).as_bytes()));
    let existing: Vec<Value> = records(s, task_id, "model_config")?
        .into_iter()
        .filter(|row| row["parent_id"] == run_id)
        .collect();
    if let Some(row) = existing.iter().find(|row| row["payload"]["version"] == version) {
        return Ok(row.clone());
    }
    let marker_key = json!([task_id, run_id, version]).to_string();
    if let Some(marker) = s.store.get(WORKER_CONFIG_KEYS, &marker_key).map_err(db)? {
        return rec(s, task_id, "model_config", marker["record_id"].as_str().unwrap_or(""));
    }
    let payload = json!({"role":"worker","version":version,"revision":existing.len()+1,
        "source":{"kind":"WorkerConfigStore","adopted_at":now()},"resolved":resolved});
    let key = format!("{run_id}:{version}");
    let (record_key, _, mut row) = stage_rec(s, task_id, "model_config", &key, payload, Some(0))?;
    row["record_id"] = json!(format!("{task_id}:{run_id}:{version}"));
    row["parent_id"] = json!(run_id);
    row["natural_key"] = json!("");
    let record_doc = json!({"version":1,"values":row});
    let marker = json!({"record_id":row["record_id"]});
    if s.store.put_many_if(&[(RECORDS,&record_key,None),(WORKER_CONFIG_KEYS,&marker_key,None)],
        &[(RECORDS,&record_key,&record_doc),(WORKER_CONFIG_KEYS,&marker_key,&marker)]).map_err(db)? {
        return Ok(row);
    }
    let marker = s.store.get(WORKER_CONFIG_KEYS,&marker_key).map_err(db)?
        .ok_or_else(|| Error::conflict("worker configuration changed"))?;
    rec(s, task_id, "model_config", marker["record_id"].as_str().unwrap_or(""))
}
fn full_task(s: &AppState, id: &str) -> Api<Value> {
    let mut row = task(s, id)?;
    let all = records(s, id, "")?;
    row["run_count"] = json!(
        all.iter()
            .filter(|record| record["kind"] == "run_binding")
            .count()
    );
    if let Some(lifecycle) = all
        .iter()
        .rev()
        .find(|record| record["kind"] == "task_lifecycle")
    {
        row["lifecycle"] = lifecycle["payload"].clone();
    }
    if let Some(tombstone) = all.iter().rev().find(|record| record["kind"] == "task_tombstone") {
        row["lifecycle"] = tombstone["payload"].clone();
        row["chat_configuration"] = Value::Null;
        row["worker_configurations"] = json!([]);
        row["watermarks"] = json!(source_watermarks(s, id)?);
        return Ok(public_task(row));
    }
    let thread_id = row["binding"]["thread_id"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    row["chat_configuration"] = chat_configuration(s, id, &thread_id)?;
    row["worker_configurations"] = json!(
        all.iter()
            .filter(
                |record| record["kind"] == "model_config" && record["payload"]["role"] == "worker"
            )
            .collect::<Vec<_>>()
    );
    row["watermarks"] = json!(source_watermarks(s, id)?);
    Ok(public_task(row))
}
pub fn task_view(s: &AppState, id: &str) -> Response {
    respond(full_task(s, id))
}
pub fn tasks(s: &AppState) -> Result<Vec<Value>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in s.store.list(TASKS).map_err(|e| e.to_string())? {
        let Some(tid) = row["task_id"].as_str().filter(|v| !v.is_empty()) else {
            continue;
        };
        if seen.insert(tid.to_owned()) {
            out.push(full_task(s, tid).map_err(|error| error.message)?);
        }
    }
    Ok(out)
}
fn task_event_watermark(s: &AppState, id: &str) -> Api<u64> {
    let head_seq = s
        .store
        .get(TASK_EVENT_HEADS, id)
        .map_err(db)?
        .and_then(|v| v["version"].as_u64())
        .unwrap_or(0);
    let platform_wm = s
        .store
        .chat_event_watermark()
        .map_err(db)?;
    let record_rev_sum: u64 = s
        .store
        .list_src_live_records(id, "")
        .map_err(db)?
        .into_iter()
        .map(|stored| {
            stored["version"]
                .as_u64()
                .or_else(|| stored["values"]["revision"].as_u64())
                .unwrap_or(1)
        })
        .sum();
    Ok(head_seq.saturating_add(record_rev_sum).max(platform_wm))
}
fn source_truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        Value::Bool(true) => true,
    }
}

fn source_record_list(s: &AppState, id: &str, p: &Value, agent: bool, scope: &str) -> Api<Value> {
    task(s, id)?;
    let kind = p["kind"].as_str().unwrap_or("");
    if agent && agent_forbidden_record_kind(kind) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "task_binding_invalid",
            "Agent cannot enumerate this control-plane record",
        ));
    }
    let limit = match p.get("limit").filter(|value| source_truthy(value)) {
        None => 100,
        Some(Value::Bool(true)) => 1,
        Some(Value::Number(value)) => value.as_i64()
            .or_else(|| value.as_f64().map(|number| number.trunc() as i64))
            .ok_or_else(|| Error::new(StatusCode::BAD_REQUEST, "query.invalid_limit", "limit must be an integer"))?,
        Some(Value::String(value)) => value
            .parse::<i64>()
            .map_err(|_| Error::new(StatusCode::BAD_REQUEST, "query.invalid_limit", "limit must be an integer"))?,
        _ => return Err(Error::new(StatusCode::BAD_REQUEST, "query.invalid_limit", "limit must be an integer")),
    };
    if !(1..=1000).contains(&limit) {
        return Err(Error::new(StatusCode::BAD_REQUEST, "query.invalid_limit", "limit must be between 1 and 1000"));
    }
    let rows: Vec<(i64, Value)> = records_with_rowid(s, id, kind)?
        .into_iter()
        .filter(|(_, row)| {
            p["parent_id"]
                    .as_str()
                    .is_none_or(|parent| row["parent_id"] == parent)
                && (!agent
                    || !row["kind"]
                        .as_str()
                        .is_some_and(agent_forbidden_record_kind))
        })
        .collect();
    let raw_cursor = p.get("cursor").filter(|value| source_truthy(value))
        .or_else(|| p.get("after_cursor").filter(|value| source_truthy(value)));
    let cursor = match raw_cursor {
        None | Some(Value::Null) => "",
        Some(Value::String(value)) => value.as_str(),
        _ => return Err(Error::new(StatusCode::BAD_REQUEST, "query.cursor_invalid", "malformed event cursor")),
    };
    let aggregate_id = format!("{id}:{kind}:{}", p["parent_id"].as_str().unwrap_or(""));
    let after = cursors::after(s, cursor, "src-record", &aggregate_id, scope)?;
    let total = rows.len();
    let mut matching: Vec<_> = rows.into_iter().filter(|(rowid, _)| *rowid > after)
        .take(limit as usize + 1).collect();
    let has_more = matching.len() > limit as usize;
    matching.truncate(limit as usize);
    let next_cursor = if has_more {
        matching.last().map(|(rowid, _)| cursors::signed(s, "src-record", &aggregate_id, *rowid, scope)).transpose()?
    } else {
        None
    };
    let page: Vec<_> = matching.into_iter().map(|(_, row)| row).collect();
    let watermarks = source_watermarks(s, id)?;
    let partial_reason = watermark_partial_reason(&watermarks);
    let event_watermark = task_event_watermark(s, id)?;
    Ok(json!({"records":page,"total":total,"returned":page.len(),
        "next_cursor":next_cursor,"complete":partial_reason.is_none(),
        "partial_reason":partial_reason,"watermarks":watermarks,
        "snapshot":{"event_watermark":event_watermark}}))
}
fn research_record_list(s: &AppState, id: &str, p: &Value, agent: bool) -> Api<Value> {
    task(s, id)?;
    let kind = p["kind"].as_str().unwrap_or("");
    if agent && agent_forbidden_record_kind(kind) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "task_binding_invalid",
            "Agent cannot read this control-plane record",
        ));
    }
    let number = |field: &str, default: i64| -> Api<i64> {
        match p.get(field) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Number(value)) => value
                .as_i64()
                .ok_or_else(|| Error::invalid(format!("{field} must be an integer"))),
            Some(Value::String(value)) => value
                .parse::<i64>()
                .map_err(|_| Error::invalid(format!("{field} must be an integer"))),
            _ => Err(Error::invalid(format!("{field} must be an integer"))),
        }
    };
    let limit = number("limit", 100)?.clamp(1, 1001) as usize;
    let after = number("after_rowid", 0)?;
    let mut rows = Vec::new();
    for (rowid, stored) in s.store.list_inserted_with_rowid(RECORDS).map_err(db)? {
        let row = &stored["values"];
        if rowid <= after || row["task_id"] != id || (!kind.is_empty() && row["kind"] != kind) {
            continue;
        }
        rows.push(row.clone());
        if rows.len() == limit {
            break;
        }
    }
    if agent {
        rows.retain(|row| {
            !row["kind"]
                .as_str()
                .is_some_and(agent_forbidden_record_kind)
        });
    }
    Ok(json!(rows))
}
pub(crate) fn query_research_records_for_actor(
    s: &AppState,
    id: &str,
    params: &Value,
    agent: bool,
) -> Result<Value, (StatusCode, Value)> {
    research_record_list(s, id, params, agent).map_err(|error| {
        (
            error.status,
            json!({"error":{"code":error.code,"message":error.message,"detail":error.detail}}),
        )
    })
}
pub(crate) fn query_records_for_actor(
    s: &AppState,
    id: &str,
    params: &Value,
    agent: bool,
    scope: &str,
) -> Result<Value, (StatusCode, Value)> {
    source_record_list(s, id, params, agent, scope).map_err(|error| {
        (
            error.status,
            json!({"error":{"code":error.code,"message":error.message,"detail":error.detail}}),
        )
    })
}
pub fn record_view(s: &AppState, id: &str, kind: &str, rid: &str) -> Response {
    respond(rec(s, id, kind, rid))
}
pub fn record_page(s: &AppState, id: &str, p: &Value) -> Response {
    respond((|| {
        let limit = p["limit"]
            .as_u64()
            .or_else(|| p["limit"].as_str().and_then(|v| v.parse().ok()))
            .unwrap_or(100)
            .clamp(1, 1000) as usize;
        let offset = p["cursor"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(str::parse::<usize>)
            .transpose()
            .map_err(|_| Error::invalid("invalid cursor"))?
            .unwrap_or(0);
        let rows: Vec<_> = records(s, id, p["kind"].as_str().unwrap_or(""))?
            .into_iter()
            .filter(|v| {
                p["parent_id"]
                    .as_str()
                    .is_none_or(|parent| v["parent_id"] == parent)
            })
            .collect();
        let total = rows.len();
        let page: Vec<_> = rows.into_iter().skip(offset).take(limit).collect();
        let next = if offset + page.len() < total {
            Some((offset + page.len()).to_string())
        } else {
            None
        };
        let watermarks = source_watermarks(s, id)?;
        let partial_reason = watermark_partial_reason(&watermarks);
        let event_watermark = task_event_watermark(s, id)?;
        Ok(json!({
            "records": page,
            "total": total,
            "returned": page.len(),
            "next_cursor": next,
            "complete": partial_reason.is_none(),
            "partial_reason": partial_reason,
            "watermarks": watermarks,
            "snapshot": {"event_watermark": event_watermark}
        }))
    })())
}

fn parse_url(raw: &str) -> Api<reqwest::Url> {
    let u = reqwest::Url::parse(raw)
        .map_err(|_| Error::invalid("完整的 HTTP(S)/WS(S) 目标 URL 是必需的"))?;
    if !matches!(u.scheme(), "http" | "https" | "ws" | "wss")
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.fragment().is_some()
    {
        return Err(Error::invalid(
            "目标必须是无凭据、无片段的完整 HTTP(S)/WS(S) URL",
        ));
    }
    Ok(u)
}
fn parse_scope_url(raw: &str) -> Api<(reqwest::Url, bool)> {
    let trimmed = raw.trim();
    let (normalized, wildcard_subdomains) = if let Some((scheme, rest)) = trimmed.split_once("://*.") {
        (format!("{scheme}://{rest}"), true)
    } else {
        (trimmed.to_owned(), false)
    };
    let u = parse_url(&normalized)?;
    Ok((u, wildcard_subdomains))
}
fn targets(v: &Value) -> Api<Vec<String>> {
    let rows = v
        .as_array()
        .ok_or_else(|| Error::invalid("authorized_targets must be an array"))?;
    if rows.len() > 128 {
        return Err(Error::invalid("授权目标最多 128 个"));
    }
    let mut out = Vec::new();
    for row in rows {
        let s = row
            .as_str()
            .ok_or_else(|| Error::invalid("授权目标必须是 URL 字符串"))?
            .trim();
        parse_scope_url(s)?;
        if !out.iter().any(|v| v == s) {
            out.push(s.to_owned());
        }
    }
    Ok(out)
}
fn target_matches(target: &str, allowed: &str) -> bool {
    let (Ok(a), Ok((b, wildcard_subdomains))) = (parse_url(target), parse_scope_url(allowed)) else {
        return false;
    };
    let host_ok = match (a.host_str(), b.host_str()) {
        (Some(ah), Some(bh)) if wildcard_subdomains => {
            let ah = ah.trim_end_matches('.').to_ascii_lowercase();
            let bh = bh.trim_end_matches('.').to_ascii_lowercase();
            ah == bh || ah.ends_with(&format!(".{bh}"))
        }
        (Some(ah), Some(bh)) => ah.eq_ignore_ascii_case(bh),
        _ => false,
    };
    if a.scheme() != b.scheme()
        || !host_ok
        || a.port_or_known_default() != b.port_or_known_default()
    {
        return false;
    }
    let root = b.path();
    let path = a.path();
    (path == root
        || (root.ends_with('/') && path.starts_with(root))
        || (path.starts_with(root) && path.as_bytes().get(root.len()) == Some(&b'/')))
        && (b.query().is_none() || a.query() == b.query())
}
fn in_policy(p: &Value, url: &str) -> bool {
    p["authorized_targets"].as_array().is_some_and(|r| {
        r.iter()
            .any(|v| v.as_str().is_some_and(|s| target_matches(url, s)))
    }) && !p["excluded_targets"].as_array().is_some_and(|r| {
        r.iter()
            .any(|v| v.as_str().is_some_and(|s| target_matches(url, s)))
    })
}

/// Recheck a stored SRC contract against current task and policy before Run dispatch.
pub fn bound_contract(s: &AppState, run_id: &str, challenge: &Value) -> Result<Value, String> {
    let run = s
        .store
        .get("runs", run_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Run 不存在".to_owned())?;
    let id = run["src_task_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "Run 未绑定 SRC Task".to_owned())?;
    if challenge["src_task_id"] != id {
        return Err("SRC Task 绑定不匹配".to_owned());
    }
    let task = task(s, id).map_err(|e| e.message)?;
    if task["lifecycle"]["state"] != "active" {
        return Err("SRC Task 未处于 active 状态".to_owned());
    }
    let policy = policy(s, id).map_err(|e| e.message)?;
    let contract = &run["pentest_contract"];
    let target = contract["target"]
        .as_str()
        .ok_or_else(|| "SRC 授权目标缺失".to_owned())?;
    if challenge["target"] != target
        || challenge["description"] != contract["goal"]
        || contract["authorization"]["source"] != "src_task_policy"
        || run["src_policy_version"] != policy["version"]
        || run["src_policy_digest"] != policy["digest"]
        || !in_policy(&policy, target)
        || !task["input"]["authorization"]["targets"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter().any(|v| {
                    v.as_str()
                        .is_some_and(|allowed| target_matches(target, allowed))
                })
            })
    {
        return Err("SRC 授权契约已过期或不匹配".to_owned());
    }
    let expected = runs::pentest::compile_src_contract(
        contract["goal"].as_str().unwrap_or(""),
        target,
        &[target.to_owned()],
    )?;
    if expected != *contract {
        return Err("SRC 授权契约内容不匹配".to_owned());
    }
    Ok(contract.clone())
}
fn policy(s: &AppState, id: &str) -> Api<Value> {
    task(s, id)?;
    s.store
        .get(POLICIES, id)
        .map_err(db)?
        .map(|v| v["values"].clone())
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.policy_not_found",
                "执行策略不存在",
            )
        })
}
pub fn policy_view(s: &AppState, id: &str) -> Response {
    respond(policy(s, id))
}
pub fn capabilities_view(s: &AppState, id: &str) -> Response {
    respond(capabilities_projection(s, id))
}
fn capabilities_projection(s: &AppState, id: &str) -> Api<Value> {
    task(s, id).and_then(|_| {
        let mut tools=json!({"src.http.request":{
                "trusted_input_fields":["saved_draft","task_authorization","current_policy"],
                "observed_resource_fields":["method","url","raw_request_artifact_id"],
                "pre_dispatch_gate":true,"environment_boundary":"server_structured_http",
                "network_scope":["exact_authorized_origin_and_path"],
                "content_inspection":[],"cancellation":"none_after_dispatch",
                "receipt_support":"durable"}});
        if let Some(browser)=domains::browser_verified_status(s,id)? {
            if browser["state"]=="ready" {
                tools["src.browser"]=json!({"trusted_input_fields":["task_authorization","current_policy","url"],
                    "observed_resource_fields":["page_url","gateway_exchange","browser_generation"],
                    "pre_dispatch_gate":true,"environment_boundary":"task_browser_gateway_netns",
                    "network_scope":["exact_authorized_origin_and_path","current_task_policy"],
                    "content_inspection":["http_proxy_enforce"],"cancellation":"best_effort",
                    "receipt_support":"gateway_exchange_and_screenshot","generation":browser["browser_generation"]});
            }
        }
        let worker = domains::worker_gateway_status(s,id).unwrap_or_else(|error|
            json!({"state":"unavailable","network_enforced":false,"content_enforced":false,
                "reason":error.message,"health":{}}));
        let ready = worker["state"] == "ready";
        if ready {
            tools["src.worker"] = json!({"trusted_input_fields":["task_authorization","current_policy"],
                "observed_resource_fields":["gateway_exchange","worker_network","policy_digest"],
                "pre_dispatch_gate":true,"environment_boundary":"task_worker_gateway_netns",
                "network_scope":["current_task_policy"],"content_inspection":["http_proxy"],
                "cancellation":"best_effort","receipt_support":"gateway_exchange",
                "generation":worker["environment_generation"]});
        }
        let network = ready && worker["network_enforced"] == true;
        let content = ready && worker["content_enforced"] == true;
        Ok(json!({"available":true,"network_enforced":network,"content_enforced":content,
            "state":if network{"ready"}else{"partial"},"health":worker["health"],"tools":tools,
            "adapter_coverage":native_policy::coverage(),"worker_gateway":worker,"reason":if network{Value::Null}else{json!("Task Worker network enforcement is not active")}}))
    })
}
pub async fn prepare_worker_environment(s: Arc<AppState>, id: &str) -> Result<Value, String> {
    domains::ensure_worker_gateway(&s, id)
        .await
        .map_err(|error| error.message)
}
pub async fn oast_callback(
    State(s): State<Arc<AppState>>,
    Path(token): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let _guard = s.chat.active.lock().await;
    match domains::oast_callback(&s, &token, method.as_str(), uri.path(), &headers, &body) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    }
}
fn policy_template(id: &str, authorized: Vec<String>) -> Value {
    let mut v = json!({"policy_id":format!("src-policy-{id}"),"version":1,"product_mode":"src","task_id":id,
        "source_refs":["operator.task.create"],"rules":[],"defaults":{"effect":"allow"},
        "authorized_targets":authorized,"excluded_targets":[],"exclusions":[],"environment_requirements":{},
        "network_mode":"declared","inspection_requirements":{"mode":"off"},
        "created_by":"local-operator","created_at":now(),"digest":"","supersedes":null,
        "activation":{"state":"declared","generation":1}});
    v["digest"] = json!(policy_digest(&v));
    v
}
mod task_creation;

pub(crate) async fn generic_task_create(s: Arc<AppState>, envelope: &Value, principal: &str) -> Response {
    task_creation::execute(s, envelope, principal).await
}

pub async fn create_task(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    respond(create_task_inner(&s, &body).await)
}
async fn create_task_inner(s: &Arc<AppState>, body: &Value) -> Api<Value> {
    create_task_transaction(s, body, None).await
}
fn is_chat_workspace_project(s: &AppState, project_id: &str) -> Result<bool, Error> {
    if let Some(project) = s.store.get("projects", project_id).map_err(db)?
        && project["scope"].as_str() == Some("chat")
    {
        return Ok(true);
    }
    Ok(s.store
        .list("workspaces")
        .map_err(db)?
        .into_iter()
        .any(|ws| ws["project_id"] == project_id && ws["kind"] == "local"))
}
fn validate_src_project_id(s: &AppState, project_id: &str) -> Result<(), Error> {
    if is_chat_workspace_project(s, project_id)? {
        return Err(Error::invalid(
            "SRC 研究任务不能绑定聊天本地代码目录，请选择或新建 SRC 研究项目",
        ));
    }
    Ok(())
}
async fn create_task_transaction(s: &Arc<AppState>, body: &Value, generic: Option<&task_creation::Context>) -> Api<Value> {
    if let Some(context) = generic && let Some(receipt) = context.replay(s)? { return Ok(receipt); }
    let actor_id = generic.map(|ctx| ctx.actor_id.as_str()).unwrap_or("local-operator");
    let title = body["title"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("SRC 赏金研究");
    let goal = body["goal"]
        .as_str()
        .filter(|value| !value.is_empty())
        .or_else(|| body["description"].as_str())
        .unwrap_or("")
        .trim();
    let key = generic.map(|ctx| &ctx.request["idempotency_key"]).unwrap_or(&body["idempotency_key"])
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let project_id = match body["project_id"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(pid) => {
            validate_src_project_id(s, pid)?;
            json!(pid)
        }
        None => Value::Null,
    };
    let authorization = body["authorization"]
        .as_object()
        .cloned()
        .map(Value::Object)
        .unwrap_or_else(|| json!({}));
    let authorized = if authorization["targets"].is_null() {
        Vec::new()
    } else {
        if let Some(arr) = authorization["targets"].as_array() {
            let raw_strings = arr.iter().filter_map(Value::as_str).map(ToOwned::to_owned).collect::<Vec<_>>();
            execution_policy::validate_scope_fuses(
                &raw_strings,
                authorization["scope_fuse_override_reason"].as_str(),
            )?;
        }
        let parsed_targets = targets(&authorization["targets"])?;
        execution_policy::validate_scope_fuses(
            &parsed_targets,
            authorization["scope_fuse_override_reason"].as_str(),
        )?;
        parsed_targets
    };
    let mut input = body["input"].as_object().cloned().unwrap_or_default();
    input.insert("product_mode".to_owned(), json!("src"));
    input.insert("goal".to_owned(), json!(goal));
    input.insert("authorization".to_owned(), authorization);
    input.insert(
        "company".to_owned(),
        json!(body["company"].as_str().unwrap_or("")),
    );
    input.insert(
        "program".to_owned(),
        body["program"]
            .as_object()
            .cloned()
            .map(Value::Object)
            .unwrap_or_else(|| json!({})),
    );
    let cmd = generic.map(|ctx| ctx.command_id.clone()).unwrap_or_else(|| format!("src-create-{key}"));
    let hash = generic.map(|ctx| ctx.semantic.to_string()).unwrap_or_else(|| digest(body));
    if let Some(old) = s.store.get(COMMANDS, &cmd).map_err(db)? {
        if old["request_hash"] != hash {
            return Err(Error::conflict("idempotency_key 已用于其他请求"));
        }
        let mut r = old["receipt"].clone();
        r["deduplicated"] = json!(true);
        return Ok(r);
    }
    let id = format!("task-{}", uuid::Uuid::new_v4().simple());
    let thread = format!("thr-{}", uuid::Uuid::new_v4().simple());
    let mut thread_body = json!({"command_id":cmd,"thread_id":thread,"title":title,"title_source":"user",
        "project_id":project_id,"runtime":body.get("runtime").cloned().unwrap_or_else(||json!({}))});
    if thread_body["project_id"].is_null() {
        thread_body.as_object_mut().unwrap().remove("project_id");
    }
    let row = json!({"task_id":id,"thread_id":thread,"title":title,"kind":"src.bounty","revision":1,
        "input":input,
        "binding":{"task_id":id,"thread_id":thread,"project_id":project_id,"principal_id":actor_id,"revision":1},
        "lifecycle":{"state":"active"},"project_id":project_id,
        "created_at":now(),"updated_at":now(),"request_hash":hash});
    let mut excluded = Vec::new();
    if let Some(arr) = row["input"]["authorization"]["excluded_targets"].as_array() {
        for item in arr {
            if let Some(raw) = item.as_str().map(str::trim).filter(|v| !v.is_empty()) {
                parse_scope_url(raw)?;
                if !excluded.iter().any(|v| v == raw) {
                    excluded.push(raw.to_owned());
                }
            }
        }
    }
    let mut policy = policy_template(&id, authorized);
    if !excluded.is_empty() {
        policy["excluded_targets"] = json!(excluded);
        policy["digest"] = json!("");
        policy["digest"] = json!(policy_digest(&policy));
    }
    let event_key = format!("{id}:1");
    let event = json!({"event_id":format!("evt-src-{}",uuid::Uuid::new_v4().simple()),
        "aggregate_type":"task","aggregate_id":id,"stream_seq":1,
        "event_type":"src.task.created","occurred_at":now(),"producer":"builtin.src",
        "actor_id":actor_id,"command_id":cmd,"causation_id":null,
        "correlation_id":cmd,"idempotency_key":generic.map(|ctx|ctx.request["idempotency_key"].clone()).unwrap_or_else(||json!(key)),
        "payload":{"task":public_task(row.clone()),"binding":row["binding"]}});
    let head = json!({"version":1});
    let mut result = receipt(&cmd, &id, json!({"task_id":id,"thread_id":thread}), false);
    if s.store.get("system", "state_dir").map_err(db)?.is_some() {
        result["event_cursor"] = json!(
            crate::operations_api::task_event_cursor(s, &id, 1, generic.map(|ctx|ctx.principal.as_str()).unwrap_or("operator:local-user"),)
                .map_err(db)?
        );
    }
    let command_row = json!({"request_hash":hash,"receipt":result});
    let policy_record = json!({"version":1,"values":policy});
    let binding_record_key = rec_key(&id, "task_binding", &id);
    let binding_record = json!({"record_id":id,"kind":"task_binding","task_id":id,
        "revision":1,"parent_id":"","natural_key":thread,"generation":0,
        "created_at":row["created_at"],"updated_at":row["created_at"],
        "payload":row["binding"]});
    let binding_stored = json!({"version":1,"values":binding_record});
    let initial_auth_snapshot = (!policy["authorized_targets"].as_array().is_none_or(Vec::is_empty)
        || row["input"]["authorization"]["notes"].as_str().is_some_and(|v| !v.trim().is_empty()))
    .then(|| {
        let snap_id = format!("sauth-v1-{}", &id);
        let snap_key = rec_key(&id, "authorization_snapshot", &snap_id);
        let snap_payload = json!({
            "record_id": snap_id,
            "version": 1,
            "policy_version": 1,
            "policy_digest": policy["digest"],
            "allowed_targets": policy["authorized_targets"],
            "excluded_targets": policy["excluded_targets"],
            "conditions": row["input"]["authorization"]["conditions"].as_array().cloned().unwrap_or_default(),
            "raw_policy_text": row["input"]["authorization"]["notes"].as_str().unwrap_or(""),
            "diff_summary": "初始授权范围",
            "created_by": actor_id,
            "created_at": row["created_at"],
        });
        let snap_rec = json!({
            "record_id": snap_id, "kind": "authorization_snapshot", "task_id": id,
            "revision": 1, "parent_id": "", "natural_key": snap_id, "generation": 0,
            "created_at": row["created_at"], "updated_at": row["created_at"],
            "payload": snap_payload
        });
        (snap_key, json!({"version": 1, "values": snap_rec}))
    });
    let mut conditions = vec![
        (TASKS, id.as_str(), None), (POLICIES, id.as_str(), None),
        (TASK_EVENT_HEADS, id.as_str(), None), (TASK_EVENTS, event_key.as_str(), None),
        (COMMANDS, cmd.as_str(), None), (RECORDS, binding_record_key.as_str(), None),
    ];
    let mut writes = vec![
        (TASKS, id.as_str(), &row), (POLICIES, id.as_str(), &policy_record),
        (TASK_EVENT_HEADS, id.as_str(), &head), (TASK_EVENTS, event_key.as_str(), &event),
        (COMMANDS, cmd.as_str(), &command_row),
        (RECORDS, binding_record_key.as_str(), &binding_stored),
    ];
    if let Some((snap_key, snap_doc)) = initial_auth_snapshot.as_ref() {
        conditions.push((RECORDS, snap_key.as_str(), None));
        writes.push((RECORDS, snap_key.as_str(), snap_doc));
    }
    let stored = generic.map(|ctx|ctx.stored(&result)).unwrap_or(Value::Null);
    let keyed = generic.map(|ctx|ctx.keyed(&result)).unwrap_or(Value::Null);
    if let Some(context) = generic { context.extend(&stored, &keyed, &mut conditions, &mut writes); }
    if chat::create_src_thread_with_records(s, &thread_body, &conditions, &writes)
    .await
    .map_err(|(status, _)| {
        if status.is_server_error() {
            Error::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "src.store_failed",
                "SRC 数据存储失败",
            )
        } else {
            Error::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "src.thread_create_failed",
                "SRC 会话创建失败",
            )
        }
    })? {
        return Ok(result);
    }
    if let Some(old) = s.store.get(COMMANDS, &cmd).map_err(db)? {
        if old["request_hash"] != hash {
            return Err(Error::conflict("idempotency_key 已用于其他请求"));
        }
        let mut replay = old["receipt"].clone();
        replay["deduplicated"] = json!(true);
        return Ok(replay);
    }
    if let Some(context) = generic && let Some(receipt) = context.replay(s)? { return Ok(receipt); }
    Err(Error::conflict("SRC Task creation conflicted"))
}

pub fn runs_view(s: &AppState, id: &str) -> Response {
    match crate::operations_api::run_list_projection(s, id) {
        Ok(rows) => Json(rows).into_response(),
        Err((status, body)) => (status, body).into_response(),
    }
}
pub fn execution_view(s: &AppState, id: &str, invocation: &str) -> Response {
    respond((|| {
        records(s, id, "execution_receipt")?
            .into_iter()
            .find(|v| v["record_id"] == invocation || v["payload"]["invocation_id"] == invocation)
            .map(|v| v["payload"].clone())
            .ok_or_else(|| {
                Error::new(
                    StatusCode::NOT_FOUND,
                    "src.receipt_not_found",
                    "执行回执不存在",
                )
            })
    })())
}

pub fn query_view(s: &AppState, id: &str, kind: &str, p: &Value) -> Response {
    if kind == "src.run.list" {
        return runs_view(s, id);
    }
    respond(query(s, id, kind, p))
}
pub(crate) fn query_projection(
    s: &AppState,
    id: &str,
    kind: &str,
    params: &Value,
) -> Result<Value, (StatusCode, Value)> {
    query(s, id, kind, params).map_err(|error| {
        (
            error.status,
            json!({"error":{"code":error.code,"message":error.message,"detail":error.detail}}),
        )
    })
}
pub(crate) fn query_workbench_evidence_projection(
    s: &AppState,
    id: &str,
    params: &Value,
    actor_id: &str,
) -> Result<Value, (StatusCode, Value)> {
    domains::verify_workbench_evidence(s, id, params, actor_id).map_err(|error| {
        (
            error.status,
            json!({"error":{"code":error.code,"message":error.message,"detail":error.detail}}),
        )
    })
}
#[derive(Clone, Debug)]
pub(crate) struct AuthorizedSrcCaller {
    pub(crate) task: Value,
    pub(crate) task_id: String,
    pub(crate) main_thread_id: String,
    pub(crate) caller_thread_id: String,
    pub(crate) principal_id: String,
    pub(crate) delegation: Option<Value>,
}

fn normalize_src_ref_kind(kind: &str) -> &str {
    match kind {
        "http_exchange" | "import_exchange" | "exchange" | "evidence" => "exchange",
        "asset" | "source" | "passage" => "asset",
        "finding" | "finding_claim" | "clue" => "finding",
        "report" | "frozen_report" => "report",
        "artifact" | "src_artifact" => "artifact",
        "feedback" | "platform_feedback" => "feedback",
        "todo" | "workbench_todo" | "task" => "todo",
        other => other,
    }
}

pub(crate) fn resolve_authorized_src_caller_from_store(
    store: &crate::store::Store,
    thread_id: &str,
) -> Api<Option<AuthorizedSrcCaller>> {
    if thread_id.is_empty() || thread_id.starts_with("prewarm:") {
        return Ok(None);
    }
    let Some(thread_doc) = store.get("conversation_threads", thread_id).map_err(db)? else {
        return Ok(None);
    };
    let tasks = store.list(TASKS).map_err(db)?;
    let direct: Vec<Value> = tasks
        .iter()
        .filter(|row| {
            row["lifecycle"]["state"].as_str() != Some("destroyed")
                && row["binding"]["thread_id"].as_str() == Some(thread_id)
        })
        .cloned()
        .collect();
    if direct.len() > 1 {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "task_binding_invalid",
            "Thread is bound to multiple SRC Tasks",
        ));
    }
    if let Some(task_row) = direct.into_iter().next() {
        let task_id = task_row["task_id"].as_str().unwrap_or("").to_owned();
        let main_thread_id = task_row["binding"]["thread_id"]
            .as_str()
            .unwrap_or(thread_id)
            .to_owned();
        let principal_id = task_row["binding"]["principal_id"]
            .as_str()
            .unwrap_or("local-operator")
            .to_owned();
        return Ok(Some(AuthorizedSrcCaller {
            task: task_row,
            task_id,
            main_thread_id,
            caller_thread_id: thread_id.to_owned(),
            principal_id,
            delegation: None,
        }));
    }
    let Some(pointer) = thread_doc["thread"]
        .get("src_delegation")
        .filter(|v| v.is_object())
    else {
        return Ok(None);
    };
    let task_id = pointer["task_id"].as_str().unwrap_or("").trim();
    if task_id.is_empty() {
        return Ok(None);
    }
    let Some(task_row) = tasks.into_iter().find(|row| {
        row["task_id"].as_str() == Some(task_id)
            && row["lifecycle"]["state"].as_str() != Some("destroyed")
    }) else {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.delegation_revoked",
            "Delegated SRC Task is no longer active",
        ));
    };
    let key = rec_key(task_id, "src_thread_delegation", thread_id);
    let Some(delegation_doc) = store.get(RECORDS, &key).map_err(db)? else {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.delegation_revoked",
            "SRC subthread delegation record is missing",
        ));
    };
    let delegation = delegation_doc["values"]["payload"].clone();
    if !delegation["revoked_at"].is_null() {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.delegation_revoked",
            "SRC subthread delegation has been revoked",
        ));
    }
    let main_thread_id = task_row["binding"]["thread_id"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let principal_id = task_row["binding"]["principal_id"]
        .as_str()
        .unwrap_or("local-operator")
        .to_owned();
    Ok(Some(AuthorizedSrcCaller {
        task: task_row,
        task_id: task_id.to_owned(),
        main_thread_id,
        caller_thread_id: thread_id.to_owned(),
        principal_id,
        delegation: Some(delegation),
    }))
}

pub(crate) fn resolve_authorized_src_caller(
    s: &AppState,
    thread_id: &str,
) -> Api<Option<AuthorizedSrcCaller>> {
    resolve_authorized_src_caller_from_store(&s.store, thread_id)
}

pub(crate) fn check_delegated_ref_access(
    _caller: &AuthorizedSrcCaller,
    _kind: &str,
    _id: &str,
) -> Api<()> {
    Ok(())
}

pub(crate) fn bind_subthread_delegation(
    s: &AppState,
    caller_thread_id: &str,
    child_thread_id: &str,
    input: &Value,
) -> Api<Option<Value>> {
    let Some(caller) = resolve_authorized_src_caller(s, caller_thread_id)? else {
        return Ok(None);
    };
    if caller.task["lifecycle"]["state"].as_str() != Some("active") {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.task_paused",
            "SRC Task 已暂停，请先恢复后再委派子线程",
        ));
    }
    let profile = input["delegation_profile"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("readonly_analysis");
    if !matches!(profile, "readonly_analysis" | "scoped_execution") {
        return Err(Error::invalid(
            "delegation_profile must be readonly_analysis or scoped_execution",
        ));
    }
    if caller
        .delegation
        .as_ref()
        .is_some_and(|d| d["profile"] == "readonly_analysis" && profile == "scoped_execution")
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.delegation_escalation_denied",
            "只读分析子线程不可派生带执行权限的子线程",
        ));
    }
    let p = policy(s, &caller.task_id)?;
    let todo_id = input["todo_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned);
    let raw_refs = input.get("allowed_refs").filter(|v| !v.is_null());
    let allowed_refs_restricted = raw_refs.is_some();
    let allowed_refs: Vec<Value> = raw_refs
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| {
            entry["id"]
                .as_str()
                .is_some_and(|id| !id.trim().is_empty())
        })
        .collect();
    let allowed_tools: Vec<String> = input["allowed_tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    let stamp = now();
    let delegation_payload = json!({
        "task_id": caller.task_id,
        "main_thread_id": caller.main_thread_id,
        "child_thread_id": child_thread_id,
        "parent_thread_id": caller_thread_id,
        "todo_id": todo_id,
        "profile": profile,
        "allowed_refs": allowed_refs,
        "allowed_refs_restricted": allowed_refs_restricted,
        "allowed_tools": allowed_tools,
        "policy_version": p["version"],
        "policy_digest": p["digest"],
        "created_at": stamp,
        "revoked_at": Value::Null,
        "revocation_reason": Value::Null,
    });
    save_rec(
        s,
        &caller.task_id,
        "src_thread_delegation",
        child_thread_id,
        delegation_payload.clone(),
        None,
    )?;
    if let Some(tid) = todo_id.as_deref()
        && let Ok(row) = rec(s, &caller.task_id, "workbench_todo", tid)
    {
        let mut todo_payload = row["payload"].clone();
        todo_payload["executor"] = json!({"kind": "subthread", "id": child_thread_id});
        todo_payload["execution_state"] = json!("running");
        todo_payload["status"] = json!("doing");
        todo_payload["updated_by"] = json!(format!("subthread:{child_thread_id}"));
        todo_payload["updated_at"] = json!(stamp);
        let rev = row["revision"].as_u64();
        let _ = save_rec(s, &caller.task_id, "workbench_todo", tid, todo_payload, rev);
    }
    let mut resolved_refs = Vec::new();
    for entry in delegation_payload["allowed_refs"].as_array().into_iter().flatten() {
        let kind = entry["kind"].as_str().unwrap_or("");
        let ref_id = entry["id"].as_str().unwrap_or("").trim();
        let rec_kind = match normalize_src_ref_kind(kind) {
            "exchange" => "http_exchange",
            "asset" => "asset",
            "finding" => "finding_claim",
            "report" => "frozen_report",
            "artifact" => "src_artifact",
            "feedback" => "platform_feedback",
            "todo" => "workbench_todo",
            other => other,
        };
        let resolved = rec(s, &caller.task_id, rec_kind, ref_id)
            .or_else(|_| {
                if rec_kind == "http_exchange" {
                    rec(s, &caller.task_id, "import_exchange", ref_id)
                } else {
                    Err(Error::invalid("missing"))
                }
            })
            .ok();
        resolved_refs.push(json!({
            "kind": kind,
            "id": ref_id,
            "found": resolved.is_some(),
            "record": resolved,
        }));
    }
    Ok(Some(json!({
        "task_id": caller.task_id,
        "main_thread_id": caller.main_thread_id,
        "delegation": delegation_payload,
        "resolved_refs": resolved_refs,
    })))
}

pub(crate) fn revoke_subthread_delegation(s: &AppState, child_thread_id: &str, reason: &str) -> Api<()> {
    revoke_subthread_delegation_for_task(s, None, child_thread_id, reason)
}

pub(crate) fn revoke_subthread_delegation_for_task(
    s: &AppState,
    explicit_task_id: Option<&str>,
    child_thread_id: &str,
    reason: &str,
) -> Api<()> {
    let resolved_task_id = explicit_task_id
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            s.store
                .get("conversation_threads", child_thread_id)
                .ok()
                .flatten()
                .and_then(|doc| {
                    doc["thread"]["src_delegation"]["task_id"]
                        .as_str()
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                })
        });
    let Some(task_id) = resolved_task_id else {
        return Ok(());
    };
    let mut payload = Value::Null;
    for attempt in 0..3 {
        let Ok(row) = rec(s, &task_id, "src_thread_delegation", child_thread_id) else {
            return Ok(());
        };
        payload = row["payload"].clone();
        if !payload["revoked_at"].is_null() {
            break;
        }
        payload["revoked_at"] = json!(now());
        payload["revocation_reason"] = json!(reason);
        let rev = row["revision"].as_u64();
        match save_rec(s, &task_id, "src_thread_delegation", child_thread_id, payload.clone(), rev) {
            Ok(_) => break,
            Err(err) if err.status == StatusCode::CONFLICT && attempt < 2 => continue,
            Err(err) => return Err(err),
        }
    }
    if let Some(todo_id) = payload["todo_id"].as_str().filter(|v| !v.is_empty()) {
        for attempt in 0..3 {
            let Ok(todo_row) = rec(s, &task_id, "workbench_todo", todo_id) else {
                break;
            };
            let mut todo = todo_row["payload"].clone();
            if todo["status"] == "deleted"
                || todo["execution_state"] == "done"
                || todo["executor"]["id"]
                    .as_str()
                    .is_some_and(|owner| !owner.is_empty() && owner != child_thread_id)
            {
                break;
            }
            if todo["execution_state"] == "blocked"
                && todo["blocker_reason"].as_str() == Some(reason)
            {
                break;
            }
            todo["execution_state"] = json!("blocked");
            todo["blocker_reason"] = json!(reason);
            if let Some(obj) = todo.as_object_mut() {
                obj.remove("waiting_for");
            }
            todo["updated_at"] = json!(now());
            todo["updated_by"] = json!(format!("subthread:{child_thread_id}"));
            match save_rec(s, &task_id, "workbench_todo", todo_id, todo, todo_row["revision"].as_u64()) {
                Ok(_) => break,
                Err(err) if err.status == StatusCode::CONFLICT && attempt < 2 => continue,
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
}

pub(crate) fn sync_todo_from_subthread(
    s: &AppState,
    child_thread_id: &str,
    execution_state: &str,
    waiting_for: Option<&str>,
    blocker_reason: Option<&str>,
    report_back: Option<&str>,
) -> Api<()> {
    let Some(thread_doc) = s.store.get("conversation_threads", child_thread_id).map_err(db)? else {
        return Ok(());
    };
    let Some(task_id) = thread_doc["thread"]["src_delegation"]["task_id"]
        .as_str()
        .filter(|v| !v.is_empty())
    else {
        return Ok(());
    };
    let Some(todo_id) = thread_doc["thread"]["src_delegation"]["todo_id"]
        .as_str()
        .filter(|v| !v.is_empty())
    else {
        return Ok(());
    };
    if let Ok(delegation_row) = rec(s, task_id, "src_thread_delegation", child_thread_id)
        && !delegation_row["payload"]["revoked_at"].is_null()
    {
        return Ok(());
    }
    if let Ok(todo_row) = rec(s, task_id, "workbench_todo", todo_id) {
        let existing = &todo_row["payload"];
        if existing["status"] == "deleted" {
            return Ok(());
        }
        if let Some(owner) = existing["executor"]["id"].as_str().filter(|v| !v.is_empty())
            && owner != child_thread_id
        {
            return Ok(());
        }
        let mut todo = existing.clone();
        todo["executor"] = json!({"kind": "subthread", "id": child_thread_id});
        todo["execution_state"] = json!(execution_state);
        if execution_state == "done" {
            todo["status"] = json!("done");
        } else if execution_state == "running" {
            todo["status"] = json!("doing");
        }
        if let Some(wf) = waiting_for {
            todo["waiting_for"] = json!(wf);
        } else if !matches!(execution_state, "waiting_input" | "waiting_approval")
            && let Some(obj) = todo.as_object_mut()
        {
            obj.remove("waiting_for");
        }
        if let Some(br) = blocker_reason {
            todo["blocker_reason"] = json!(br);
        } else if matches!(execution_state, "running" | "done")
            && let Some(obj) = todo.as_object_mut()
        {
            obj.remove("blocker_reason");
        }
        if let Some(rb) = report_back {
            todo["report_back"] = json!(rb);
        }
        let changed = todo["executor"] != existing["executor"]
            || todo["execution_state"] != existing["execution_state"]
            || todo["status"] != existing["status"]
            || todo["waiting_for"] != existing["waiting_for"]
            || todo["blocker_reason"] != existing["blocker_reason"]
            || todo["report_back"] != existing["report_back"];
        if !changed {
            return Ok(());
        }
        todo["updated_at"] = json!(now());
        todo["updated_by"] = json!(format!("subthread:{child_thread_id}"));
        let _ = save_rec(
            s,
            task_id,
            "workbench_todo",
            todo_id,
            todo,
            todo_row["revision"].as_u64(),
        );
    }
    Ok(())
}

pub(crate) fn is_src_coordinator_thread(store: &crate::store::Store, thread_id: &str) -> bool {
    matches!(
        resolve_authorized_src_caller_from_store(store, thread_id),
        Ok(Some(caller)) if caller.delegation.is_none()
    )
}

pub(crate) fn update_delegated_todo_on_report(s: &AppState, child_thread_id: &str, report_text: &str) {
    let _ = sync_todo_from_subthread(s, child_thread_id, "done", None, None, Some(report_text));
}

pub(crate) fn sync_src_session_files_before_turn(
    s: &AppState,
    tid: &str,
    cwd: &std::path::Path,
) {
    domains::sync_session_files_before_turn(s, tid, cwd);
}

pub(crate) fn sync_src_session_files_after_turn(
    s: &AppState,
    tid: &str,
    cwd: &std::path::Path,
) {
    domains::sync_session_files_after_turn(s, tid, cwd);
}

pub(crate) fn src_thread_instructions(s: &AppState, tid: &str) -> String {
    domains::src_thread_instructions(s, tid)
}

pub(crate) fn task_for_thread(s: &AppState, thread_id: &str) -> Result<Value, (StatusCode, Value)> {
    match resolve_authorized_src_caller(s, thread_id) {
        Ok(Some(caller)) => Ok(caller.task),
        Ok(None) => Err((
            StatusCode::FORBIDDEN,
            json!({"error":{"code":"task_binding_invalid",
                "message":"Thread is not bound to exactly one SRC Task","detail":{}}}),
        )),
        Err(error) => Err((
            error.status,
            json!({"error":{"code":error.code,"message":error.message,"detail":error.detail}}),
        )),
    }
}
fn query(s: &AppState, id: &str, kind: &str, p: &Value) -> Api<Value> {
    operation_contracts::validate(kind,p,None)?;
    let normalized=operation_contracts::normalize(kind,p);let p=&normalized;
    if kind == "src.asset.published" {
        return domains::query(s, id, kind, p);
    }
    task(s, id)?;
    let all = records(s, id, "")?;
    let of_kind =
        |k: &str| -> Vec<Value> { all.iter().filter(|v| v["kind"] == k).cloned().collect() };
    let payloads =
        |k: &str| -> Vec<Value> { of_kind(k).iter().map(|v| v["payload"].clone()).collect() };
    match kind {
        "src.task.get" => full_task(s, id),
        "src.task.list" => Ok(json!(tasks(s).map_err(db)?)),
        "src.run.list" => {
            let mut out = Vec::new();
            for v in of_kind("run_binding") {
                out.push(v["payload"].clone());
            }
            Ok(json!(out))
        }
        "src.record.list" => source_record_list(s, id, p, false, "operator:local-user"),
        "src.record.get" => rec(s, id, require(p, "kind")?, require(p, "record_id")?),
        "src.policy.current" => policy(s, id),
        "src.policy.preview" => policy_preview_value(s, id, &p["request"]),
        "src.execution.capabilities" => capabilities_projection(s, id),
        "src.workbench.snapshot" => {
            let mut slices = serde_json::Map::new();
            for row in of_kind("workbench_slice") {
                if let Some(k) = row["natural_key"].as_str() {
                    slices.insert(k.to_owned(),json!({"revision":row["revision"],"updated_at":row["updated_at"],"client_id":row["payload"]["client_id"],"data":row["payload"]["data"]}));
                }
            }
            Ok(json!({"slices":slices}))
        }
        "src.workbench.evidence.verify" => {
            domains::verify_workbench_evidence(s, id, p, "local-user")
        }
        "src.workbench.slice.get" => {
            let slice = require(p, "slice")?;
            let row = of_kind("workbench_slice")
                .into_iter()
                .find(|v| v["natural_key"] == slice)
                .ok_or_else(|| {
                    Error::new(
                        StatusCode::NOT_FOUND,
                        "src.slice_not_found",
                        "工作台切片不存在",
                    )
                })?;
            Ok(
                json!({"slice":slice,"revision":row["revision"],"updated_at":row["updated_at"],"client_id":row["payload"]["client_id"],"data":row["payload"]["data"]}),
            )
        }
        "src.workbench.history" | "src.workbench.todo.list" | "src.workbench.report.drafts"
        | "src.workbench.suggestions" | "src.workbench.report.diff"
        | "src.workbench.finding.merges" | "src.workbench.finding.meta"
        | "src.workbench.asset.links" | "src.workbench.asset.tasklinks"
        | "src.workbench.duplicate.verdicts" | "src.project.memory.get" =>
            domains::workbench_record_query(s, id, kind, p),
        "src.workbench.asset.bulk" => domains::asset_bulk(s, id, p),
        "src.asset.query" => domains::query(s, id, kind, p),
        "src.research.plan" => payloads("research_plan").pop().ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.plan_not_found",
                "研究计划不存在",
            )
        }),
        "src.monitor.list" => of_kind("monitor")
            .into_iter()
            .map(|row| domains::monitor_get(s, id, &json!({"monitor_id":row["natural_key"]})))
            .collect::<Api<Vec<_>>>()
            .map(|rows| json!(rows)),
        "src.execution.receipt" => {
            let inv = require(p, "invocation_id")?;
            rec(s, id, "execution_receipt", inv).map_err(|error| {
                if error.status == StatusCode::NOT_FOUND {
                    Error::new(
                        StatusCode::NOT_FOUND,
                        "artifact_missing",
                        format!("execution_receipt/{inv} is unavailable"),
                    )
                } else {
                    error
                }
            })
        }
        _ => domains::query(s, id, kind, p),
    }
}

pub fn preview_policy(s: &AppState, id: &str, request: &Value) -> Response {
    respond(policy_preview_value(s, id, request))
}
fn policy_preview_value(s:&AppState,id:&str,request:&Value)->Api<Value>{
    let mut req = if request.is_object() { request.clone() } else { json!({}) };
    let t = task(s, id)?;
    let p = policy(s, id)?;
    let default_args_hash = digest(req.get("arguments").unwrap_or(&json!({})));
    if let Some(obj) = req.as_object_mut() {
        obj.entry("task_id").or_insert_with(|| json!(id));
        obj.entry("thread_id").or_insert_with(|| t["binding"]["thread_id"].clone());
        obj.entry("tool_name").or_insert_with(|| json!("src.http.transparent_proxy"));
        obj.entry("action").or_insert_with(|| json!("request"));
        obj.entry("resource_kind").or_insert_with(|| json!("http"));
        obj.entry("policy_version").or_insert_with(|| p["version"].clone());
        obj.entry("arguments_hash").or_insert_with(|| json!(default_args_hash));
    }
    execution_policy::validate(&req)?;
    if req["task_id"]!=id{return Err(Error::new(StatusCode::FORBIDDEN,"task_binding_invalid","execution request belongs to another Task"));}
    execution_policy::evaluate(&p,&req)
}

pub async fn command_view(s: Arc<AppState>, id: &str, kind: &str, body: &Value) -> Response {
    if matches!(
        kind,
        "src.task.archive"
            | "src.task.restore"
            | "src.task.pause"
            | "src.task.resume"
            | "src.task.destroy"
            | "src.task.authorization.renew"
            | "src.run.archive"
            | "src.run.restore"
            | "src.run.destroy"
            | "src.artifact.destroy"
            | "src.approval.decide"
            | "src.policy.update"
            | "src.grant.revoke"
            | "src.workbench.slice.save"
            | "src.workbench.todo.save"
            | "src.workbench.question.status"
            | "src.workbench.history.record"
            | "src.workbench.history.revert"
            | "src.workbench.report.draft.save"
            | "src.workbench.report.draft.discard"
            | "src.workbench.suggestion.propose"
            | "src.workbench.suggestion.resolve"
            | "src.workbench.asset.importance"
            | "src.workbench.asset.link"
            | "src.workbench.object.link"
            | "src.workbench.asset.tasklink"
            | "src.workbench.duplicate.verdict"
            | "src.workbench.finding.merge"
            | "src.workbench.finding.unmerge"
            | "src.workbench.finding.meta.save"
            | "src.workbench.finding.stale"
            | "src.workbench.finding.stale.clear"
            | "src.asset.version.pin"
            | "src.feedback.record"
            | "src.project.memory.save"
            | "src.finding.claim.create"
            | "src.finding.review.record"
            | "src.review.record"
            | "src.finding.deduplicate"
            | "src.finding.invalidate"
            | "src.asset.observe"
            | "src.asset.correct"
            | "src.asset.relate"
            | "src.asset.derive"
            | "src.asset.lifecycle"
            | "src.asset.destroy"
            | "src.asset.merge"
            | "src.asset.split"
            | "src.asset.merge.revert"
            | "src.asset.publish"
            | "src.asset.reference.refresh"
            | "src.asset.publication.revoke"
            | "src.asset.judgement.save"
            | "src.connector.config.save"
            | "src.connector.config.revoke"
            | "src.connector.grant.save"
            | "src.connector.grant.revoke"
            | "src.connector.verify"
            | "src.query.save"
            | "src.query.delete"
            | "src.vocabulary.save"
            | "src.tag.save"
            | "src.tag.assign"
            | "src.note.save"
            | "src.research.plan.save"
            | "src.research.authorization.save"
            | "src.research.skill.register"
            | "src.research.openapi.ingest"
            | "src.research.javascript.ingest"
            | "src.research.javascript.sourcemap_unpack"
            | "src.research.graphql_grpc.ingest"
            | "src.research.javascript.collect"
            | "src.research.runtime.register"
            | "src.research.dataflow.register"
            | "src.research.page_action.register"
            | "src.research.business.save"
            | "src.research.business.derive"
            | "src.research.permission_question.add"
            | "src.research.experiment.record"
            | "src.research.capability_chain.record"
            | "src.research.precondition_trial.record"
            | "src.coverage.item.save"
            | "src.coverage.item.claim"
            | "src.coverage.attempt.record"
            | "src.coverage.blocker.record"
            | "src.coverage.seed"
            | "src.coordination.policy.save"
            | "src.coordination.decide"
            | "src.coordination.wake"
            | "src.coordination.notice.ack"
            | "src.identity.login_recipe.save"
            | "src.identity.login_recipe.replay"
            | "src.identity.session.import"
            | "src.identity.session.probe"
            | "src.proxy_bridge.start"
            | "src.proxy_bridge.stop"
            | "src.proxy_bridge.bookmark"
            | "src.proxypool.save"
            | "src.proxypool.reset_circuit"
            | "src.mutator.save"
            | "src.mutator.test"
            | "src.oast.token.mint"
            | "src.oast.callback.ingest"
            | "src.intel.advisory.import"
            | "src.intel.match_run"
            | "src.run.intervene"
            | "src.exchange.race_probe"
            | "src.workbench.attachment.autobind"
            | "src.notify.channel.save"
            | "src.notify.test"
            | "src.notify.flush_carryover"
            | "src.archive.export"
            | "src.archive.import"
            | "src.diagnostics.mcp_reset"
            | "src.report.freeze"
            | "src.report.reproduction.record"
            | "src.report.reproduction.auto"
            | "src.report.render"
            | "src.submission.export"
            | "src.exchange.record"
            | "src.exchange.draft.save"
            | "src.exchange.draft.execute"
            | "src.exchange.differential_probe"
    ) {
        let command_id = body["command_id"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                body["idempotency_key"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(|key| format!("{id}:{kind}:{key}"))
            })
            .unwrap_or_else(|| format!("cmd-{}", uuid::Uuid::new_v4().simple()));
        let envelope = json!({"command_id":command_id,"command_type":kind,
            "aggregate_type":"task","aggregate_id":id,"payload":body,
            "idempotency_key":body["idempotency_key"]});
        let result = domain_command::dispatch(s, id, &envelope, "operator:local-user")
            .await
            .map(|mut receipt| {
                if kind != "src.workbench.slice.save" {
                    receipt["output"] = receipt["output"]["result"].clone();
                }
                receipt
            });
        return respond(result);
    }
    if kind == "src.run.start" {
        return respond(start_command(&s, id, body).await);
    }
    if kind == "src.retest.dispatch" {
        let result = domains::dispatch_retest(s, id, body).await.map(|output| {
            receipt(
                &format!(
                    "{id}:src.retest.dispatch:{}",
                    body["retest_id"].as_str().unwrap_or("")
                ),
                id,
                output,
                false,
            )
        });
        return respond(result);
    }
    if kind == "src.connector.repository" {
        let result = domains::repository(s, id, body).await.map(|output| {
            receipt(
                &format!(
                    "{id}:src.connector.repository:{}",
                    body["idempotency_key"].as_str().unwrap_or("")
                ),
                id,
                output,
                false,
            )
        });
        return respond(result);
    }
    if kind == "src.connector.query" {
        let result = domains::query_connector(s, id, body).await.map(|output| {
            receipt(
                &format!(
                    "{id}:src.connector.query:{}",
                    output["acquisition_id"].as_str().unwrap_or("")
                ),
                id,
                output,
                false,
            )
        });
        return respond(result);
    }
    if matches!(kind, "src.monitor.run_once" | "src.monitor.cycle.run") {
        let result = if kind == "src.monitor.run_once" {
            domains::monitor_once(s, id, body).await
        } else {
            domains::monitor_cycle(s, id, body).await
        }
        .map(|output| {
            receipt(
                &format!("{id}:{kind}:{}", output["cycle_id"].as_str().unwrap_or("")),
                id,
                output,
                false,
            )
        });
        return respond(result);
    }
    if kind == "src.monitor.wake" {
        let result = domains::monitor_wake(s, id).await.map(|output| {
            receipt(
                &format!("{id}:src.monitor.wake:{}", now()),
                id,
                output,
                false,
            )
        });
        return respond(result);
    }
    if kind == "src.monitor.notifications.deliver" {
        let result = domains::deliver_monitor_notifications(s, id)
            .await
            .map(|output| {
                receipt(
                    &format!("{id}:src.monitor.notifications.deliver:{}", now()),
                    id,
                    output,
                    false,
                )
            });
        return respond(result);
    }
    if matches!(
        kind,
        "src.stream.websocket.connect"
            | "src.stream.websocket.send"
            | "src.stream.websocket.receive"
            | "src.stream.websocket.close"
            | "src.stream.sse.connect"
            | "src.stream.sse.read"
            | "src.stream.sse.close"
    ) {
        let result = domains::stream_command(s, id, kind, body)
            .await
            .map(|output| {
                receipt(
                    &format!("{id}:{kind}:{}", output["stream_id"].as_str().unwrap_or("")),
                    id,
                    output,
                    false,
                )
            });
        return respond(result);
    }
    if matches!(
        kind,
        "src.browser.ensure"
            | "src.browser.context.ensure"
            | "src.browser.tab.open"
            | "src.browser.screenshot.capture"
            | "src.browser.task.close"
    ) {
        let _guard = browser_mutation_lock().lock().await;
        let result = async {
            task(&s, id)?;
            let key = body["idempotency_key"]
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            let command_id = format!("{id}:{kind}:{key}");
            let hash = digest(&json!([id, kind, body]));
            if let Some(old) = s.store.get(COMMANDS, &command_id).map_err(db)? {
                if old["request_hash"] != hash {
                    return Err(Error::conflict(
                        "browser idempotency_key used for different input",
                    ));
                }
                let mut prior = old["receipt"].clone();
                prior["deduplicated"] = json!(true);
                return Ok(prior);
            }
            let output = domains::browser_command(s.clone(), id, kind, body).await?;
            let result = receipt(&command_id, id, output, false);
            s.store
                .put(
                    COMMANDS,
                    &command_id,
                    &json!({"request_hash":hash,"receipt":result}),
                )
                .map_err(db)?;
            Ok(result)
        }
        .await;
        return respond(result);
    }
    let _guard = s.chat.active.lock().await;
    let _chat = crate::chat::ChatLockFlag::enter();
    respond(command(&s, id, kind, body))
}
pub async fn generic_domain_command(s: Arc<AppState>, envelope: &Value, scope: &str) -> Response {
    if let Err(error)=crate::business::require(&s,crate::business::Module::Src){return error.into_response();}
    let Some(id) = envelope["aggregate_id"].as_str() else {
        return Error::invalid("Task aggregate_id is required").into_response();
    };
    match domain_command::dispatch(s, id, envelope, scope).await {
        Ok(receipt) => Json(json!({"receipt":receipt})).into_response(),
        Err(error) => error.into_response(),
    }
}
pub async fn initialize(state:&Arc<AppState>)->Result<(),String>{
    control_command::recover(state).await.map_err(|error|error.message)?;
    if let Err(error)=domains::recover_oast_receiver(state.clone()).await{
        tracing::warn!(code=error.code,%error.message,"OAST local receiver could not be restored");
    }
    for task in state.store.list(TASKS).map_err(|error|error.to_string())? {
        let Some(id)=task["task_id"].as_str() else{continue};
        if task["lifecycle"]["state"] == "destroyed" {
            if let Err(error) = domains::cleanup_destroyed_task_cas(state, id) {
                tracing::warn!(task_id=id, code=error.code, %error.message,
                    "Destroyed Task CAS cleanup remains pending; continuing SRC recovery");
            }
            continue;
        }
        run_projection::recover_task(state,id).map_err(|error|error.message)?;
        native_policy::recover(state,id).map_err(|error|error.message)?;
        domains::recover_http(state,id).map_err(|error|error.message)?;
        if let Err(error) = domains::backfill_traffic_index(state, id) {
            tracing::warn!(task_id = id, %error.message, "SRC traffic index backfill failed");
        }
        domains::recover_retests(state,id).map_err(|error|error.message)?;
        domains::recover_browser(state,id).map_err(|error|error.message)?;
        if task["lifecycle"]["state"]=="active" {
            let key=format!("src-monitor-scheduler:{id}");
            if let Some(mut health)=state.store.get("system",&key).map_err(|error|error.to_string())? {
                health["state"]=json!("recovering");
                state.store.put("system",&key,&health).map_err(|error|error.to_string())?;
            }
        }
        domains::recover_streams(state.clone(),id).await.map_err(|error|error.message)?;
        domains::recover_monitors(state.clone(),id).await.map_err(|error|error.message)?;
    }
    Ok(())
}
static MONITOR_LOOPS:OnceLock<std::sync::Mutex<std::collections::HashSet<String>>>=OnceLock::new();
struct MonitorLoopGuard(String);
impl Drop for MonitorLoopGuard{fn drop(&mut self){if let Some(loops)=MONITOR_LOOPS.get(){if let Ok(mut loops)=loops.lock(){loops.remove(&self.0);}}}}
pub(super) fn monitor_loop_live(s:&AppState)->bool{MONITOR_LOOPS.get().and_then(|loops|loops.lock().ok()).is_some_and(|loops|loops.contains(s.chat.epoch()))}
pub fn spawn_monitor_loop(state:Arc<AppState>){
    let recovery=state.clone();
    crate::business::spawn(state.clone(),crate::business::Module::Src,"src.reconciliation",async move {
        let mut interval=tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ticks=0u64;
        loop {
            interval.tick().await;
            if recovery.admission.draining() || crate::business::require(&recovery,crate::business::Module::Src).is_err(){continue;}
            let full_scan=ticks%30==0;
            ticks=ticks.wrapping_add(1);
            if let Err(error)=domain_command::reconcile_once(recovery.clone(),full_scan).await{tracing::warn!(code=error.code,%error.message,"SRC domain reconciliation failed");}
        }
    });
    crate::business::spawn(state.clone(),crate::business::Module::Src,"src.monitors",async move {
        let _live=MonitorLoopGuard(state.chat.epoch().to_owned());
        MONITOR_LOOPS.get_or_init(||std::sync::Mutex::new(std::collections::HashSet::new())).lock().unwrap_or_else(|error|error.into_inner()).insert(state.chat.epoch().to_owned());
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if state.admission.draining() || crate::business::require(&state,crate::business::Module::Src).is_err(){continue;}
            let tasks = match state.store.list(TASKS) {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::error!(%error, "SRC monitor scheduler could not read Tasks");
                    continue;
                }
            };
            for task in tasks {
                if task["lifecycle"]["state"] != "active" {
                    continue;
                }
                let Some(id) = task["task_id"].as_str() else {
                    continue;
                };
                let _ = domains::sync_inspection(&state, id);
                let _ = domains::coordination_wake(state.clone(), id).await;
                if state.store.get("system", &format!("src-monitor-scheduler:{id}"))
                    .ok().flatten().is_some_and(|health| health["state"] == "stopped") {
                    continue;
                }
                if let Err(error) = domains::monitor_wake(state.clone(), id).await {
                    tracing::warn!(task_id=id,code=error.code,%error.message,"SRC monitor scheduler tick failed");
                    let _=domains::monitor_health_error(&state,id,&error);
                } else {
                    let _ = domains::deliver_monitor_notifications(state.clone(), id).await;
                }
            }
        }
    });
}
fn command(s: &AppState, id: &str, kind: &str, body: &Value) -> Api<Value> {
    task(s, id)?;
    operation_contracts::validate(kind, body, body["idempotency_key"].as_str())?;
    let normalized = operation_contracts::normalize(kind, body);
    let body = if operation_contracts::auto_payload(kind) { &normalized } else { body };
    let key = body["idempotency_key"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let command_id = format!("{id}:{kind}:{key}");
    let request_identity = if matches!(kind, "src.connector.import" | "src.ingestion.ingest") {
        json!([id, kind, body])
    } else {
        json!(digest(&json!([id, kind, body])))
    };
    if let Some(old) = s.store.get(COMMANDS, &command_id).map_err(db)? {
        if old["request_hash"] != request_identity {
            return Err(Error::conflict("idempotency_key 已用于不同的 SRC 命令"));
        }
        let mut r = old["receipt"].clone();
        r["deduplicated"] = json!(true);
        if kind == "src.connector.import" {
            r["output"]["deduplicated"] = json!(true);
        }
        return Ok(r);
    }
    let lifecycle = task(s, id)?["lifecycle"]["state"]
        .as_str()
        .unwrap_or("active")
        .to_owned();
    if lifecycle == "destroyed"
        || (lifecycle == "archived" && !matches!(kind, "src.task.restore" | "src.artifact.destroy"))
    {
        return Err(Error::conflict("SRC Task 当前不可修改；请先恢复归档任务"));
    }
    if lifecycle == "paused" && !domain_command::allowed_while_paused(kind) {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.task_paused",
            "SRC Task 已暂停，请先恢复后再执行",
        ));
    }
    let actor=task(s,id)?["binding"]["principal_id"].as_str().unwrap_or("local-operator").to_owned();
    let output=if domain_command::workbench_record_command(kind){domains::apply_workbench_record_command(s,id,kind,body,&command_id,&actor)?}
        else if domain_command::asset_query_command(kind){domains::apply_asset_query_command(s,id,kind,body,&command_id,&actor)?}
        else{match kind{
        "src.policy.update" => update_policy(s, id, body)?,
        "src.approval.decide" => decide_approval(s, id, body)?,
        "src.grant.revoke" => revoke_grant(s, id, body)?,
        "src.run.ensure" => ensure_run(s, id, body)?,
        "src.workbench.slice.save" => save_slice(s, id, body)?,
        _ => domains::command(s, id, kind, body)?,
    }};
    let result = receipt(&command_id, id, output, false);
    s.store
        .put(
            COMMANDS,
            &command_id,
            &json!({"request_hash":request_identity,"receipt":result}),
        )
        .map_err(db)?;
    Ok(result)
}

fn policy_candidate(s: &AppState, id: &str, body: &Value, actor_id: &str) -> Api<(u64, Value)> {
    let current = policy(s, id)?;
    let version = current["version"].as_u64().unwrap_or(1);
    if body["base_version"]
        .as_u64()
        .is_some_and(|base| base != version)
    {
        return Err(Error::conflict("执行策略版本已变化，请刷新后重试"));
    }
    let network = body["network_mode"]
        .as_str()
        .unwrap_or(current["network_mode"].as_str().unwrap_or("declared"));
    let inspect = body["inspection_requirements"]["mode"].as_str().unwrap_or(
        current["inspection_requirements"]["mode"]
            .as_str()
            .unwrap_or("off"),
    );
    if !matches!(network, "declared" | "enforced") {
        return Err(Error::invalid("invalid network mode"));
    }
    if !matches!(inspect, "off" | "record" | "enforce") {
        return Err(Error::invalid("invalid inspection mode"));
    }
    if network == "declared" && inspect == "enforce" {
        return Err(Error::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "inspection_unavailable",
            "content enforce requires the enforced Worker network",
        ));
    }
    let mut next = current.clone();
    for field in [
        "rules",
        "authorized_targets",
        "excluded_targets",
        "exclusions",
        "network_mode",
        "inspection_requirements",
        "environment_requirements",
        "source_refs",
        "defaults",
    ] {
        if let Some(v) = body.get(field) {
            next[field] = v.clone();
        }
    }
    if let Some(arr) = next["authorized_targets"].as_array() {
        let raw_strings = arr.iter().filter_map(Value::as_str).map(ToOwned::to_owned).collect::<Vec<_>>();
        execution_policy::validate_scope_fuses(
            &raw_strings,
            body["scope_fuse_override_reason"].as_str(),
        )?;
    }
    let next_targets = targets(&next["authorized_targets"])?;
    execution_policy::validate_scope_fuses(
        &next_targets,
        body["scope_fuse_override_reason"].as_str(),
    )?;
    next["authorized_targets"] = json!(next_targets);
    if !next.get("exclusions").is_some_and(|v| !v.is_null()) {
        next["exclusions"] = json!([]);
    }
    execution_policy::validate_exclusions(&next["exclusions"])?;
    if !next["excluded_targets"].is_array() {
        return Err(Error::invalid("excluded_targets must be an array"));
    }
    for row in next["excluded_targets"].as_array().unwrap() {
        parse_scope_url(
            row.as_str()
                .ok_or_else(|| Error::invalid("excluded target must be a URL"))?,
        )?;
    }
    for field in ["defaults","environment_requirements","inspection_requirements"]{if !next[field].is_object(){return Err(Error::invalid(format!("{field} must be an object")));}}
    if !matches!(next["defaults"]["effect"].as_str(),Some("allow"|"ask"|"deny")){return Err(Error::invalid("invalid policy default effect"));}
    let mut rule_ids=std::collections::HashSet::new();
    let rules = next["rules"]
        .as_array_mut()
        .ok_or_else(|| Error::invalid("rules must be an array"))?;
    for rule in rules {
        let object=rule.as_object_mut().ok_or_else(||Error::invalid("execution rule must be an object"))?;
        for(key,value)in [("rule_id",json!(format!("srule-{}",uuid::Uuid::new_v4().simple()))),("version",json!(1)),("enabled",json!(true)),("evaluation_point",json!("tool")),("mode",json!("enforce")),("selector",json!({})),("conditions",json!({})),("actions",json!([])),("reason",json!("")),("source_refs",json!([])),("reusable_approval",json!(false))]{object.entry(key).or_insert(value);}
        execution_policy::validate_rule(rule)?;
        let rule_id = require(rule, "rule_id")?;
        if !rule_ids.insert(rule_id.to_owned()){return Err(Error::invalid("Execution rule IDs must be unique"));}
        if !matches!(rule["effect"].as_str(), Some("allow" | "ask" | "deny"))
            || !matches!(rule["mode"].as_str(), Some("enforce" | "observe"))
        {
            return Err(Error::invalid("invalid execution rule"));
        }
        if !rule["selector"].is_object() || !rule["conditions"].is_object() {
            return Err(Error::invalid(
                "rule selector and conditions must be objects",
            ));
        }
        if current["rules"].as_array().is_some_and(|previous| {
            previous.iter().any(|old| {
                old["rule_id"] == rule_id && old["version"] == rule["version"] && old != &*rule
            })
        }) {
            return Err(Error::conflict(format!(
                "rule {rule_id} content changed without a version increment"
            )));
        }
    }
    next["version"] = json!(version + 1);
    next["supersedes"] = json!(format!(
        "{}:v{version}",
        current["policy_id"].as_str().unwrap_or("src-policy")
    ));
    next["created_at"] = json!(now());
    next["created_by"] = json!(actor_id);
    next["activation"] = json!({"state":"declared","generation":version+1});
    next["digest"] = json!("");
    next["digest"] = json!(policy_digest(&next));
    Ok((version, next))
}
fn update_policy(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let (version, next) = policy_candidate(s, id, body, "local-operator")?;
    match s
        .store
        .compare_and_put(POLICIES, id, version, next.clone())
        .map_err(db)?
    {
        Ok(_) => {
            for row in records(s, id, "execution_approval")? {
                if row["payload"]["status"] != "pending"
                    || row["payload"]["policy_version"] == next["version"]
                {
                    continue;
                }
                let mut approval = row["payload"].clone();
                approval["status"] = json!("superseded");
                save_rec(
                    s,
                    id,
                    "execution_approval",
                    row["record_id"].as_str().unwrap_or(""),
                    approval,
                    row["revision"].as_u64(),
                )?;
            }
            Ok(next)
        }
        Err(_) => Err(Error::conflict("执行策略版本已变化，请刷新后重试")),
    }
}

fn decide_approval(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    decide_approval_marked(s, id, body, None, None)
}
fn decide_approval_marked(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: Option<&str>,
) -> Api<Value> {
    let approval_id = require(body, "approval_id")?;
    let row = rec(s, id, "execution_approval", approval_id)?;
    let rev = row["revision"].as_u64().unwrap_or(0);
    if body["base_revision"].as_u64() != Some(rev) {
        return Err(Error::conflict("审批版本已变化，请刷新后重试"));
    }
    let mut approval = row["payload"].clone();
    if approval["status"] != "pending" {
        return Err(Error::conflict("审批已经处理"));
    }
    let decision = require(body, "decision")?;
    if !matches!(decision, "approve_once" | "deny" | "approve_task_scope") {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "policy_denied",
            "unknown approval decision",
        ));
    }
    if let Some(expires) = approval["expires_at"].as_str() {
        if chrono::DateTime::parse_from_rfc3339(expires)
            .ok()
            .is_some_and(|v| v < chrono::Utc::now())
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "approval_expired",
                "审批已过期",
            ));
        }
    }
    let decided_by = match actor_id {
        Some(actor_id) => json!(actor_id),
        None => task(s, id)?["binding"]["principal_id"].clone(),
    };
    let mut grant_id = Value::Null;
    let mut staged = Vec::new();
    if decision == "approve_task_scope" {
        let p = policy(s, id)?;
        let refs = approval["matched_rule_refs"]
            .as_array()
            .ok_or_else(|| Error::invalid("matched rules unavailable"))?;
        if refs.is_empty()
            || refs.iter().any(|r| {
                !p["rules"].as_array().is_some_and(|rules| {
                    rules.iter().any(|rule| {
                        r == &json!(format!(
                            "{}:v{}",
                            rule["rule_id"].as_str().unwrap_or(""),
                            rule["version"].as_u64().unwrap_or(0)
                        )) && rule["reusable_approval"] == true
                    })
                })
            })
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "policy_denied",
                "匹配规则不允许保存 Task 范围授权",
            ));
        }
        let request = &approval["request_snapshot"];
        let action = require(request, "action")?;
        let selected = body["scope"].as_object().cloned().unwrap_or_default();
        if !body["scope"].is_null() && !body["scope"].is_object() {
            return Err(Error::invalid("scope must be an object"));
        }
        let mut default_resource = json!({"tool_name":request["tool_name"]});
        if let Some(target) = request["resource_facts"]["observed_url"]["value"]
            .as_str()
            .or_else(|| request["arguments"]["url"].as_str())
            .filter(|target| !target.is_empty())
        {
            default_resource["url"] = json!({"target":target});
        }
        if let Some(cwd) = request["cwd"].as_str().filter(|cwd| !cwd.is_empty()) {
            default_resource["cwd"] = json!(cwd);
        }
        let mut default_identity = json!({});
        if let Some(identity_ref) = request["identity_ref"]
            .as_str()
            .filter(|value| !value.is_empty())
        {
            default_identity["identity_ref"] = json!(identity_ref);
            if request["identity_revision"].as_u64().unwrap_or(0) > 0 {
                default_identity["identity_revision"] = request["identity_revision"].clone();
            }
        }
        let default_environment = if request["environment_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
        {
            json!({"environment_id":request["environment_id"],
                "environment_generation":request["environment_generation"]})
        } else {
            json!({})
        };
        let mut default_input = json!({});
        if request["arguments_ref"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
        {
            default_input["arguments_ref"] = request["arguments_ref"].clone();
        } else if request["arguments_hash"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
        {
            default_input["arguments_hash"] = request["arguments_hash"].clone();
        }
        let selected_object =
            |field: &str, fallback: Value, empty_uses_default: bool| -> Api<Value> {
                match selected.get(field) {
                    None => Ok(fallback),
                    Some(value) if empty_uses_default && value.is_null() => Ok(fallback),
                    Some(value) if value.is_object() => {
                        if empty_uses_default
                            && value.as_object().is_some_and(serde_json::Map::is_empty)
                        {
                            Ok(fallback)
                        } else {
                            Ok(value.clone())
                        }
                    }
                    _ => Err(Error::invalid(format!("{field} must be an object"))),
                }
            };
        let resource = selected_object("resource_selector", default_resource, true)?;
        let identity = selected_object("identity_constraints", default_identity, false)?;
        let environment = selected_object("environment_constraints", default_environment, true)?;
        let input = selected_object("input_constraints", default_input, false)?;
        let actions = match selected.get("actions") {
            Some(value) if value.as_array().is_some_and(|actions| !actions.is_empty()) => {
                value.clone()
            }
            Some(value) if value.is_null() => json!([action]),
            Some(value) if !value.is_array() => {
                return Err(Error::invalid("actions must be an array"));
            }
            _ => json!([action]),
        };
        let ttl = selected
            .get("ttl_seconds")
            .and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            })
            .unwrap_or(0);
        let expires_at = if let Some(value) = selected
            .get("expires_at")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            chrono::DateTime::parse_from_rfc3339(value)
                .map_err(|_| Error::invalid("expires_at must be an ISO timestamp"))?;
            value.to_owned()
        } else if ttl > 0 {
            (chrono::Utc::now() + chrono::Duration::seconds(ttl)).to_rfc3339()
        } else {
            (chrono::Utc::now() + chrono::Duration::hours(4)).to_rfc3339()
        };
        let gid = format!("sgrant-{}", uuid::Uuid::new_v4().simple());
        let payload = json!({"grant_id":gid,"task_id":id,"rule_refs":refs,"resource_selector":resource,
            "actions":actions,"identity_constraints":identity,"environment_constraints":environment,
            "input_constraints":input,"source_approval_id":approval_id,
            "created_by":decided_by.clone(),"product_mode":"src",
            "created_at":now(),"expires_at":expires_at,"revoked_at":null,"version":1,
            "policy_digest":p["digest"]});
        staged.push(stage_rec(s, id, "task_grant", &gid, payload, Some(0))?);
        staged.push(stage_rec(
            s,
            id,
            "task_grant_environment",
            &gid,
            json!({"environment_requirements":p["environment_requirements"],
                "network_mode":p["network_mode"],
                "inspection_requirements":p["inspection_requirements"]}),
            Some(0),
        )?);
        grant_id = json!(gid);
    }
    approval["status"] = json!(if decision == "deny" {
        "denied"
    } else {
        "approved"
    });
    approval["decision"] = json!(decision);
    approval["decided_by"] = decided_by;
    approval["decided_at"] = json!(now());
    approval["grant_id"] = grant_id;
    let key = row["record_id"].as_str().unwrap_or(approval_id);
    staged.push(stage_rec(
        s,
        id,
        "execution_approval",
        key,
        approval.clone(),
        Some(rev),
    )?);
    let mut before = Vec::with_capacity(staged.len());
    let mut documents = Vec::with_capacity(staged.len());
    for (rid, version, record) in &staged {
        let previous = s.store.get(RECORDS, rid).map_err(db)?;
        if previous
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0)
            != *version
        {
            return Err(Error::conflict("SRC 记录版本已变化，请刷新后重试"));
        }
        before.push(previous);
        documents.push(json!({"version":version+1,"values":record}));
    }
    let mut conditions = Vec::with_capacity(staged.len() + 1);
    let mut writes = Vec::with_capacity(staged.len() + 1);
    for (index, (rid, _, _)) in staged.iter().enumerate() {
        conditions.push((RECORDS, rid.as_str(), before[index].as_ref()));
        writes.push((RECORDS, rid.as_str(), &documents[index]));
    }
    let marker_key = command_id.map(|_| format!("{id}:{approval_id}"));
    let marker =
        command_id.map(|command_id| json!({"command_id":command_id,"approval_id":approval_id}));
    if let (Some(marker_key), Some(marker)) = (marker_key.as_ref(), marker.as_ref()) {
        conditions.push((APPROVAL_APPLIED, marker_key.as_str(), None));
        writes.push((APPROVAL_APPLIED, marker_key.as_str(), marker));
    }
    if !s.store.put_many_if(&conditions, &writes).map_err(db)? {
        return Err(Error::conflict("SRC 记录版本已变化，请刷新后重试"));
    }
    Ok(approval)
}
fn revoke_grant(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let grant_id = require(body, "grant_id")?;
    let row = rec(s, id, "task_grant", grant_id)?;
    if row["payload"]["gateway_grant"].is_object() {
        return domains::revoke_inspection_grant(s, id, body);
    }
    let rev = row["revision"].as_u64().unwrap_or(0);
    if body["base_revision"].as_u64() != Some(rev) {
        return Err(Error::conflict("授权版本已变化，请刷新后重试"));
    }
    let mut payload = row["payload"].clone();
    if payload["revoked_at"].is_null() {
        payload["revoked_at"] = json!(now());
    }
    save_rec(
        s,
        id,
        "task_grant",
        row["record_id"].as_str().unwrap_or(grant_id),
        payload,
        Some(rev),
    )?;
    Ok(json!({"grant_id":grant_id,"revoked":true}))
}

fn save_slice(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    save_slice_marked(s, id, body, None, None)
}
fn save_slice_marked(
    s: &AppState,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor_id: Option<&str>,
) -> Api<Value> {
    let slice = require(body, "slice")?;
    if ![
        "projects",
        "sources",
        "clues",
        "tasks",
        "dossier",
        "delivery",
        "governance",
        "threads",
        "chat",
        "imports",
        "records",
    ]
    .contains(&slice)
        || !body["data"].is_object()
    {
        return Err(Error::invalid("工作台切片或 data 无效"));
    }
    let base = body["base_revision"].as_u64().unwrap_or(0);
    let slice_rid = rec_key(id, "workbench_slice", slice);
    let current_slice = s.store.get(RECORDS, &slice_rid).map_err(db)?;
    let actual_rev = current_slice
        .as_ref()
        .and_then(|row| row["version"].as_u64())
        .unwrap_or(0);
    if let Some(existing_row) = current_slice.as_ref()
        && actual_rev == base
        && existing_row["values"]["payload"]["data"] == body["data"]
    {
        return Ok(json!({
            "result": {
                "slice": slice,
                "revision": actual_rev,
                "updated_at": existing_row["values"]["updated_at"],
                "unchanged": true
            }
        }));
    }
    let slice_conflict = |actual: u64, msg: &str| {
        let mut err = Error::conflict(msg);
        err.detail = json!({"slice": slice, "expected": base, "actual": actual});
        err
    };
    let latest_actual_rev = || -> u64 {
        s.store
            .get(RECORDS, &slice_rid)
            .ok()
            .flatten()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(actual_rev)
    };
    if actual_rev != base {
        return Err(slice_conflict(actual_rev, "工作台切片版本已变化，请刷新或合并后再保存"));
    }
    let payload = json!({"schema":1,"slice":slice,"data":body["data"],
        "client_id":body["client_id"].as_str().unwrap_or(""),
        "saved_by":actor_id.or_else(||body["actor_id"].as_str()).unwrap_or("local-operator")});
    let actual = payload.to_string().len();
    const MAX_BYTES: usize = 16 * 1024 * 1024;
    if actual > MAX_BYTES {
        let mut error = Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "workbench_slice_too_large",
            "workbench slice payload exceeds 16 MiB",
        );
        error.detail = json!({"limit":MAX_BYTES,"actual":actual});
        return Err(error);
    }
    let marker_key = format!("{id}:{slice}:{base}");
    if let Some(command_id) = command_id {
        if let Some(marker) = s.store.get(SLICE_APPLIED, &marker_key).map_err(db)? {
            if marker["command_id"] == command_id {
                return Ok(marker["output"].clone());
            }
            return Err(slice_conflict(
                latest_actual_rev(),
                "workbench slice revision was saved by another command",
            ));
        }
    }
    let row = if let Some(command_id) = command_id {
        let (rid, version, row) =
            match stage_rec(s, id, "workbench_slice", slice, payload, Some(base)) {
                Ok(staged) => staged,
                Err(_) => {
                    if let Some(saved) = s.store.get(SLICE_APPLIED, &marker_key).map_err(db)? {
                        if saved["command_id"] == command_id {
                            return Ok(saved["output"].clone());
                        }
                    }
                    return Err(slice_conflict(
                        latest_actual_rev(),
                        "工作台切片版本已变化，请刷新或合并后再保存",
                    ));
                }
            };
        let previous = s.store.get(RECORDS, &rid).map_err(db)?;
        let document = json!({"version":version+1,"values":row});
        let output = json!({"result":{"slice":slice,"revision":row["revision"],"updated_at":row["updated_at"]}});
        let marker = json!({"command_id":command_id,"output":output});
        if !s
            .store
            .put_many_if(
                &[
                    (RECORDS, &rid, previous.as_ref()),
                    (SLICE_APPLIED, &marker_key, None),
                ],
                &[
                    (RECORDS, &rid, &document),
                    (SLICE_APPLIED, &marker_key, &marker),
                ],
            )
            .map_err(db)?
        {
            if let Some(saved) = s.store.get(SLICE_APPLIED, &marker_key).map_err(db)? {
                if saved["command_id"] == command_id {
                    return Ok(saved["output"].clone());
                }
            }
            return Err(slice_conflict(latest_actual_rev(), "workbench slice revision changed"));
        }
        row
    } else {
        match save_rec(s, id, "workbench_slice", slice, payload, Some(base)) {
            Ok(row) => row,
            Err(_) => {
                return Err(slice_conflict(
                    latest_actual_rev(),
                    "工作台切片版本已变化，请刷新或合并后再保存",
                ));
            }
        }
    };
    Ok(json!({"result":{"slice":slice,"revision":row["revision"],"updated_at":row["updated_at"]}}))
}

fn replay_src_run(s:&AppState,id:&str,binding:&Value,revision:u64,target:&str,target_version:&str)->Api<Value>{
    if binding["destroyed_at"].as_str().is_some_and(|value|!value.is_empty()){return Err(Error::new(StatusCode::CONFLICT,"source_revoked","Run has been destroyed"));}
    if binding["binding_revision"].as_u64()!=Some(revision){return Err(Error::new(StatusCode::CONFLICT,"authorization_stale","Run binding revision is stale"));}
    if binding["contract"]["target"]!=target||binding["contract"]["target_version"]!=target_version{return Err(Error::conflict("a changed target requires a new binding_key"));}
    let run_id=require(binding,"run_id")?;let run=s.store.get("runs",run_id).map_err(db)?.ok_or_else(||Error::new(StatusCode::NOT_FOUND,"src.run_missing","bound Run snapshot is missing"))?;
    if task(s,id)?["lifecycle"]["state"]!="active"||run["archived"]==true||run["lifecycle"]["state"].as_str().is_some_and(|value|value!="active")||run["archived_at"].as_str().is_some_and(|value|!value.is_empty())||records(s,id,"run_disposal")?.iter().any(|row|row["payload"]["run_id"]==run_id&&row["payload"]["state"]=="destroyed"){return Err(Error::conflict("bound Run lifecycle is not active"));}
    if run["src_task_id"]!=id||run["bound_to_src"]!=true||binding["task_id"]!=id||binding["thread_id"]!=task(s,id)?["binding"]["thread_id"]{return Err(Error::new(StatusCode::FORBIDDEN,"task_binding_invalid","Run snapshot does not match SRC binding"));}
    domains::record_duplicate_run_prevented(s, id);
    Ok(json!({"binding":binding,"run":{"run_id":run_id,"state":run["status"],"generation":run["execution_generation"],"task_kind":"pentest.target","executor_id":"swarm.coordinator","detail":run},"deduplicated":true}))
}
fn ensure_run(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let target = require(body, "target_url").or_else(|_| require(body, "target"))?;
    parse_url(target)?;
    let p = policy(s, id)?;
    if !in_policy(&p, target) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "Run 目标不在当前明确授权范围内",
        ));
    }
    let t = task(s, id)?;
    if t["lifecycle"]["state"] == "paused" {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.task_paused",
            "SRC Task 已暂停，请先恢复后再执行",
        ));
    }
    if t["lifecycle"]["state"] != "active" {
        return Err(Error::conflict("SRC Task 未处于 active 状态"));
    }
    let revision = t["binding"]["revision"].as_u64().unwrap_or(1);
    if body["binding_revision"]
        .as_u64()
        .is_some_and(|v| v != revision)
    {
        return Err(Error::conflict("SRC Task 绑定版本已变化"));
    }
    let question = body["question"].as_str().unwrap_or("");
    let target_version = body["target_version"].as_str().unwrap_or("");
    let key = body["binding_key"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| digest(&json!({"target":target,"question":question,"target_version":target_version})));
    let binding_key = format!("src:{id}:{key}");
    let rid = rec_key(id, "run_binding", &binding_key);
    if let Some(saved)=s.store.get(RECORDS,&rid).map_err(db)?{return replay_src_run(s,id,&saved["values"]["payload"],revision,target,target_version);}
    let run_id = format!("crun_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    let before_run = runs::draft_run_record(s, &run_id);
    let contract = json!({"target":target,"question":question,
        "evidence_refs":body["evidence_refs"].as_array().cloned().unwrap_or_default(),
        "identity":body["identity"].as_object().cloned().map(Value::Object).unwrap_or_else(||json!({})),
        "object_state":body["object_state"].as_object().cloned().map(Value::Object).unwrap_or_else(||json!({})),
        "budget":body["budget"].as_object().cloned().map(Value::Object).unwrap_or_else(||json!({})),
        "target_version":target_version});
    let contract_bytes = contract.to_string().into_bytes();
    let contract_sha256 = format!("{:x}",Sha256::digest(&contract_bytes));
    let artifact_id = format!("sart_{}",uuid::Uuid::new_v4().simple());
    let artifact_key = format!("{id}:{artifact_id}");
    let artifact_name = format!("{run_id}-contract.json");
    let contract_artifact = json!({"artifact_id":artifact_id,"sha256":contract_sha256,
        "media_type":"application/json","size":contract_bytes.len(),"task_id":id,
        "source_run_id":run_id,"source_event_seq":null,"version":1,"original":true,
        "complete":"complete","name":artifact_name});
    let stored_artifact = json!({"artifact_id":artifact_id,"sha256":contract_sha256,
        "media_type":"application/json","size":contract_bytes.len(),"task_id":id,
        "source_run_id":run_id,"source_event_seq":null,"version":1,"original":true,
        "complete":"complete","name":artifact_name,
        "data":base64::Engine::encode(&base64::engine::general_purpose::STANDARD,&contract_bytes),
        "created_at":now()});
    let binding = json!({"schema_version":1,
        "binding_id":format!("srunbind-{}",uuid::Uuid::new_v4().simple()),
        "task_id":id,"thread_id":t["binding"]["thread_id"],"run_id":run_id,
        "binding_key":binding_key,"binding_revision":revision,
        "product_mode":"src","execution_mode":"pentest","target_contract_ref":contract_sha256,
        "plan_ref":body["plan_ref"].as_str().unwrap_or(""),
        "authorization_ref":body["authorization_ref"].as_str().unwrap_or(""),
        "source_message_id":body["source_message_id"].as_str().unwrap_or(""),
        "related_run_ids":[],"created_at":now(),"target_url":target,
        "contract":contract,"contract_artifact":contract_artifact});
    loop {
        if let Some(saved)=s.store.get(RECORDS,&rid).map_err(db)?{return replay_src_run(s,id,&saved["values"]["payload"],revision,target,target_version);}
        let mut run = before_run.clone();
        run["src_task_id"] = json!(id);
        run["bound_to_src"] = json!(true);
        run["mode"] = json!("pentest");
        let (_,_,mut record) = stage_rec(s,id,"run_binding",&binding_key,binding.clone(),Some(0))?;
        record["record_id"] = json!(run_id);
        record["natural_key"] = json!(binding_key);
        record["generation"] = json!(1);
        let doc = json!({"version":1,"values":record});
        if s.store.put_many_if(&[("runs",run_id.as_str(),None),
            (RECORDS,rid.as_str(),None),("src_artifacts",artifact_key.as_str(),None)],
            &[("runs",run_id.as_str(),&run),(RECORDS,rid.as_str(),&doc),
                ("src_artifacts",artifact_key.as_str(),&stored_artifact)]).map_err(db)? {
            runs::ensure_run_directory(s, &run_id);
            return Ok(json!({"binding":binding,"run":{"run_id":run_id,"state":run["status"],
                "generation":run["execution_generation"],"task_kind":"pentest.target",
                "executor_id":"swarm.coordinator","detail":run},"deduplicated":false}));
        }
    }
}

async fn start_command(s: &Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    static START_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = START_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let run_id = require(body, "run_id")?;
    let t = task(s, id)?;
    if t["lifecycle"]["state"] == "paused" {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.task_paused",
            "SRC Task 已暂停，请先恢复后再执行",
        ));
    }
    if t["lifecycle"]["state"] != "active" {
        return Err(Error::conflict("SRC Task 未处于 active 状态"));
    }
    let p = policy(s, id)?;
    let binding = records(s, id, "run_binding")?
        .into_iter()
        .map(|v| v["payload"].clone())
        .find(|v| v["run_id"] == run_id)
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.run_not_bound",
                "Run 不属于该 SRC Task",
            )
        })?;
    let target = binding["contract"]["target"].as_str().ok_or_else(|| {
        Error::new(
            StatusCode::CONFLICT,
            "src.binding_invalid",
            "Run 目标契约缺失",
        )
    })?;
    if body["target_url"]
        .as_str()
        .is_some_and(|v| !v.trim().is_empty() && v != target)
    {
        return Err(Error::conflict("Run 目标与绑定契约不一致"));
    }
    if !in_policy(&p, target)
        || !t["input"]["authorization"]["targets"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter().any(|v| {
                    v.as_str()
                        .is_some_and(|allowed| target_matches(target, allowed))
                })
            })
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "Run 目标已不在 Task 授权和当前策略范围内",
        ));
    }
    if body["scope"]
        .as_str()
        .is_some_and(|v| !v.trim().is_empty() && v.trim() != target)
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "SRC Run 的 scope 必须与绑定目标一致",
        ));
    }
    let goal = body["goal"]
        .as_str()
        .or_else(|| body["prompt"].as_str())
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            binding["contract"]["question"]
                .as_str()
                .filter(|v| !v.trim().is_empty())
        })
        .or_else(|| t["input"]["goal"].as_str())
        .unwrap_or("核验目标安全性");
    let contract = runs::pentest::compile_src_contract(goal, target, &[target.to_owned()])
        .map_err(|e| Error::new(StatusCode::UNPROCESSABLE_ENTITY, "src.contract_invalid", e))?;
    if !runs::pentest::in_scope_url(target, &contract) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "目标不在编译后的授权契约内",
        ));
    }
    let key = body["idempotency_key"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let command_id = format!("{id}:src.run.start:{key}");
    let hash = digest(&json!([id, "src.run.start", body]));
    if let Some(old) = s.store.get(COMMANDS, &command_id).map_err(db)? {
        if old["request_hash"] != hash {
            return Err(Error::conflict("idempotency_key 已用于不同的 SRC 命令"));
        }
        let mut r = old["receipt"].clone();
        r["deduplicated"] = json!(true);
        return Ok(r);
    }
    let mut run = s
        .store
        .get("runs", run_id)
        .map_err(db)?
        .ok_or_else(|| Error::new(StatusCode::NOT_FOUND, "src.run_not_found", "Run 不存在"))?;
    if run["src_task_id"] != id {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.run_not_bound",
            "Run 不属于该 SRC Task",
        ));
    }
    let max_concurrent = domains::coordination_policy_get(s, id)
        .ok()
        .and_then(|p| p["max_concurrent_runs"].as_u64())
        .unwrap_or(2) as usize;
    let mut active_count = 0usize;
    for row in records(s, id, "run_binding")? {
        if !row["payload"]["destroyed_at"].is_null() {
            continue;
        }
        let Some(other_id) = row["payload"]["run_id"].as_str() else { continue };
        if other_id.is_empty() || other_id == run_id {
            continue;
        }
        if let Some(other_run) = s.store.get("runs", other_id).map_err(db)?
            && other_run["archived"] != true
            && matches!(other_run["status"].as_str(), Some("queued" | "starting" | "running"))
        {
            active_count += 1;
        }
    }
    if active_count >= max_concurrent {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.run_concurrency_limit",
            format!("当前 Task 并发 Run 已达上限 ({active_count}/{max_concurrent})"),
        ));
    }
    worker_configuration(s, id, run_id)?;
    run["pentest_contract"] = contract.clone();
    run["src_policy_version"] = p["version"].clone();
    run["src_policy_digest"] = p["digest"].clone();
    run["src_authorization_ref"] = json!(format!(
        "{}:v{}",
        p["policy_id"].as_str().unwrap_or("src-policy"),
        p["version"].as_u64().unwrap_or(1)
    ));
    s.store.put("runs", run_id, &run).map_err(db)?;
    let challenge = json!({"name":t["title"],"mode":"pentest","target":target,
        "description":goal,"src_task_id":id});
    let mut dispatch=body.clone();
    dispatch["kind"]=json!("swarm");dispatch["challenge"]=challenge;
    dispatch["coordinator"]=json!(true);dispatch["command_id"]=json!(command_id);
    let response = runs::start_run(State(s.clone()), Path(run_id.to_owned()), Json(dispatch)).await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .map_err(db)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(db)?;
    if !status.is_success() {
        let mut error = Error::new(
            status,
            "src.start_failed",
            value["detail"].as_str().unwrap_or("SRC Run 启动失败"),
        );
        error.detail = value;
        return Err(error);
    }
    let result = receipt(
        &command_id,
        id,
        json!({"run_id":run_id,"receipt":value["receipt"],"contract":contract}),
        false,
    );
    s.store
        .put(
            COMMANDS,
            &command_id,
            &json!({"request_hash":hash,"receipt":result}),
        )
        .map_err(db)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_configuration_digest_sorts_keys() {
        let value = json!({"b":2,"a":1});
        assert_eq!(source_compact_json(&value),r#"{"a":1,"b":2}"#);
        assert_eq!(format!("{:x}",Sha256::digest(source_compact_json(&value).as_bytes())),
            "43258cff783fe7036d8a43033f830adfc60ec037382473548ac742b888292777");
    }
    #[tokio::test]
    async fn task_chat_configuration_projects_runtime_selection_once_per_version() {
        let state = Arc::new(AppState::new(crate::store::Store::open(":memory:").unwrap()));
        let created = create_task_inner(&state,&json!({"title":"Runtime selection",
            "goal":"Review a local task","idempotency_key":"src-runtime-selection"})).await.unwrap();
        let task_id = created["output"]["task_id"].as_str().unwrap();
        let thread_id = created["output"]["thread_id"].as_str().unwrap();
        let first = full_task(&state,task_id).unwrap()["chat_configuration"].clone();
        let resolved = &first["payload"]["resolved"];
        assert_eq!(resolved["thread_id"],thread_id);
        assert_eq!(resolved["adapter_id"],"");
        assert_eq!(resolved["instance_id"],"default");
        assert_eq!(resolved["access_mode"],"supervised");
        assert_eq!(resolved["interaction_mode"],"default");
        assert!(resolved["updated_at"].as_str().is_some_and(|value|!value.is_empty()));
        let version = first["payload"]["version"].as_str().unwrap();
        assert_eq!(version.len(),64);
        assert_eq!(first["record_id"],format!("{task_id}:chat:{version}"));
        assert_eq!(first["parent_id"],"chat");
        assert_eq!(full_task(&state,task_id).unwrap()["chat_configuration"]["record_id"],first["record_id"]);

        let mut thread = state.store.get("conversation_threads",thread_id).unwrap().unwrap();
        thread["runtime"] = json!({"adapter_id":"pi.rpc","instance_id":"",
            "model":"test-model","effort":"default",
            "api_key":"must-not-appear"});
        state.store.put("conversation_threads",thread_id,&thread).unwrap();
        let next = full_task(&state,task_id).unwrap()["chat_configuration"].clone();
        assert_eq!(next["payload"]["revision"],2);
        assert_eq!(next["payload"]["resolved"]["adapter_id"],"pi.rpc");
        assert_eq!(next["payload"]["resolved"]["credential_id"],"system:pi");
        assert_eq!(next["payload"]["resolved"]["instance_id"],"default");
        assert_eq!(next["payload"]["resolved"]["effort"],"");
        assert!(next["payload"]["resolved"].get("api_key").is_none());
        assert_eq!(records(&state,task_id,"model_config").unwrap().len(),2);
    }

    #[tokio::test]
    async fn worker_configuration_records_one_revision_per_run_and_version() {
        let state = Arc::new(AppState::new(crate::store::Store::open(":memory:").unwrap()));
        let created = create_task_inner(&state,&json!({"title":"Worker snapshot",
            "goal":"Review local task","idempotency_key":"src-worker-snapshot"})).await.unwrap();
        let task_id = created["output"]["task_id"].as_str().unwrap();
        state.store.put("settings","worker_templates",&json!({"max_workers":1,
            "worker_network":"bridge","api_key":"hidden"})).unwrap();
        let first = worker_configuration(&state,task_id,"run-1").unwrap();
        let version = first["payload"]["version"].as_str().unwrap();
        assert_eq!(version.len(),64);
        assert_eq!(first["record_id"],format!("{task_id}:run-1:{version}"));
        assert_eq!(first["parent_id"],"run-1");
        assert_eq!(first["payload"]["revision"],1);
        assert_eq!(first["payload"]["resolved"]["max_workers"],1);
        assert!(first["payload"]["resolved"].get("api_key").is_none());
        assert!(first["payload"]["resolved"].get("effective_network").is_none());
        assert_eq!(worker_configuration(&state,task_id,"run-1").unwrap()["record_id"],first["record_id"]);
        state.store.put("settings","worker_templates",&json!({"max_workers":2,
            "worker_network":"bridge"})).unwrap();
        let next = worker_configuration(&state,task_id,"run-1").unwrap();
        assert_eq!(next["payload"]["revision"],2);
        assert_eq!(next["payload"]["resolved"]["max_workers"],2);
        assert_ne!(next["record_id"],first["record_id"]);
        let other_run = worker_configuration(&state,task_id,"run-2").unwrap();
        assert_eq!(other_run["payload"]["revision"],1);
        assert_eq!(records(&state,task_id,"model_config").unwrap().len(),3);
    }

    #[test]
    fn scope_is_exact_origin_and_path_boundary() {
        assert!(target_matches(
            "https://example.com/a/b",
            "https://example.com/a"
        ));
        assert!(!target_matches(
            "https://example.com/ab",
            "https://example.com/a"
        ));
        assert!(!target_matches(
            "https://example.com.evil/a",
            "https://example.com/a"
        ));
        assert!(!target_matches(
            "http://example.com/a",
            "https://example.com/a"
        ));
        assert!(!target_matches(
            "https://example.com:444/a",
            "https://example.com/a"
        ));
        assert!(target_matches(
            "ws://example.com/socket",
            "ws://example.com/socket"
        ));
        assert!(!target_matches(
            "ws://example.com/socket2",
            "ws://example.com/socket"
        ));
        assert!(!target_matches(
            "wss://example.com/socket",
            "ws://example.com/socket"
        ));
    }
    #[test]
    fn scope_exclusions_win() {
        let p = json!({"authorized_targets":["https://example.com/a"],"excluded_targets":["https://example.com/a/private"]});
        assert!(in_policy(&p, "https://example.com/a/public"));
        assert!(!in_policy(&p, "https://example.com/a/private/item"));
    }
    #[tokio::test]
    async fn task_creation_is_idempotent_and_binds_operator_scope() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let body = json!({"title":"Scope review","goal":"Review one target",
            "authorization":{"targets":["https://example.com/app"]},
            "idempotency_key":"test-scope-create"});
        let first = create_task_inner(&state, &body).await.unwrap();
        let second = create_task_inner(&state, &body).await.unwrap();
        assert_eq!(first["output"], second["output"]);
        assert_eq!(second["deduplicated"], true);
        let id = first["output"]["task_id"].as_str().unwrap();
        assert_eq!(
            task(&state, id).unwrap()["binding"]["thread_id"],
            first["output"]["thread_id"]
        );
        assert_eq!(
            policy(&state, id).unwrap()["authorized_targets"],
            json!(["https://example.com/app"])
        );
        let mut changed = body.clone();
        changed["goal"] = json!("Different request");
        assert_eq!(
            create_task_inner(&state, &changed)
                .await
                .unwrap_err()
                .status,
            StatusCode::CONFLICT
        );
        let research = create_task_inner(
            &state,
            &json!({"title":"Research","goal":"Collect evidence",
            "idempotency_key":"test-research-create"}),
        )
        .await
        .unwrap();
        let research_id = research["output"]["task_id"].as_str().unwrap();
        assert_eq!(
            policy(&state, research_id).unwrap()["authorized_targets"],
            json!([])
        );
        assert_eq!(
            ensure_run(
                &state,
                research_id,
                &json!({"target_url":"https://example.com/"})
            )
            .unwrap_err()
            .status,
            StatusCode::FORBIDDEN
        );
    }
    #[tokio::test]
    async fn public_repository_monitor_respects_policy_exclusion() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let metadata = "https://api.github.com/repos/octocat/Hello-World";
        let commit = "https://api.github.com/repos/octocat/Hello-World/commits/master";
        for (suffix, allowed) in [("excluded", false), ("allowed", true)] {
            let created = create_task_inner(&state, &json!({"title":"Repository monitor","goal":"Watch public repository",
                "idempotency_key":format!("test-monitor-{suffix}"),"authorization":{"targets":[metadata]}})).await.unwrap();
            let id = created["output"]["task_id"].as_str().unwrap();
            if !allowed {
                let mut narrowed = policy(&state, id).unwrap();
                narrowed["excluded_targets"] = json!([commit]);
                narrowed["digest"] = json!("");
                narrowed["digest"] = json!(digest(&narrowed));
                assert!(
                    state
                        .store
                        .compare_and_put(POLICIES, id, 1, narrowed)
                        .unwrap()
                        .is_ok()
                );
            }
            let config=domains::command(&state,id,"src.connector.config.save",&json!({"connector_id":"github",
                "config":{"repository":"octocat/Hello-World","ref":"master","base_url":"https://api.github.com"}})).unwrap();
            let p = policy(&state, id).unwrap();
            let reference = format!(
                "{}:v{}",
                p["policy_id"].as_str().unwrap(),
                p["version"].as_u64().unwrap()
            );
            let result = domains::command(
                &state,
                id,
                "src.monitor.create",
                &json!({"payload":{"continuous":true,
                "name":"Hello World","chain_type":"asset","source":{"connector_id":"github",
                "config_version":config["version"],"capability_manifest_ref":config["manifest"]["version"]},
                "authorization_ref":reference,"schedule":{"interval_seconds":1},
                "budget":{"total":0,"per_cycle":1,"call_limit":0}}}),
            );
            let monitor = result.unwrap();
            assert_eq!(monitor["status"], "active");
            let scoped = domains::monitor_preflight(&state, id, &monitor["definition"]);
            if allowed {
                assert!(scoped.is_ok());
            } else {
                assert_eq!(scoped.unwrap_err().code, "authorization_unresolved");
            }
        }
    }
    #[tokio::test]
    async fn run_binding_rejects_out_of_scope_targets_and_stale_policy() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        state
            .store
            .put("system", "state_dir", &json!(dir.path().to_string_lossy()))
            .unwrap();
        let created = create_task_inner(
            &state,
            &json!({"title":"Audit","goal":"Review",
            "authorization":{"targets":["http://127.0.0.1:8080/app"]},
            "idempotency_key":"src-binding-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        assert_eq!(
            ensure_run(
                &state,
                id,
                &json!({"target_url":"http://127.0.0.1:8080/private","question":"check"})
            )
            .unwrap_err()
            .status,
            StatusCode::FORBIDDEN
        );
        let ensured = ensure_run(
            &state,
            id,
            &json!({"target_url":"http://127.0.0.1:8080/app/login","question":"check"}),
        )
        .unwrap();
        let rid = ensured["binding"]["run_id"].as_str().unwrap();
        let target = "http://127.0.0.1:8080/app/login";
        let contract =
            runs::pentest::compile_src_contract("Review", target, &[target.to_owned()]).unwrap();
        let mut run = state.store.get("runs", rid).unwrap().unwrap();
        run["pentest_contract"] = contract;
        run["src_policy_version"] = json!(1);
        run["src_policy_digest"] = policy(&state, id).unwrap()["digest"].clone();
        state.store.put("runs", rid, &run).unwrap();
        let challenge = json!({"src_task_id":id,"target":target,"description":"Review"});
        assert!(bound_contract(&state, rid, &challenge).is_ok());
        assert!(bound_contract(&state,rid,&json!({"src_task_id":id,"target":"http://127.0.0.1:8080/private","description":"Review"})).is_err());
        update_policy(
            &state,
            id,
            &json!({"base_version":1,"authorized_targets":["http://127.0.0.1:8080/other"]}),
        )
        .unwrap();
        assert!(bound_contract(&state, rid, &challenge).is_err());
    }

    #[tokio::test]
    async fn run_binding_uses_source_record_identity_and_contract_artifact() {
        use base64::Engine;
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::new(crate::store::Store::open(":memory:").unwrap()));
        state.store.put("system","state_dir",&json!(directory.path().to_string_lossy())).unwrap();
        let created = create_task_inner(&state,&json!({"title":"Binding identity","goal":"Review",
            "authorization":{"targets":["http://127.0.0.1:8080/app"]},
            "idempotency_key":"src-binding-identity"})).await.unwrap();
        let task_id = created["output"]["task_id"].as_str().unwrap();
        let request = json!({"target_url":"http://127.0.0.1:8080/app","question":"check",
            "binding_key":"source-binding"});
        let first = ensure_run(&state,task_id,&request).unwrap();
        let binding = &first["binding"];
        let run_id = binding["run_id"].as_str().unwrap();
        let row = rec(&state,task_id,"run_binding",run_id).unwrap();
        assert_eq!(row["record_id"],run_id);
        assert_eq!(row["natural_key"],format!("src:{task_id}:source-binding"));
        assert_eq!(row["generation"],1);
        assert!(binding["binding_id"].as_str().unwrap().starts_with("srunbind-"));
        let artifact_id = binding["contract_artifact"]["artifact_id"].as_str().unwrap();
        let stored = state.store.get("src_artifacts",&format!("{task_id}:{artifact_id}"))
            .unwrap().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(stored["data"].as_str().unwrap()).unwrap();
        assert_eq!(binding["target_contract_ref"],
            format!("{:x}",Sha256::digest(&bytes)));
        assert_eq!(binding["target_contract_ref"],
            "48128895d21f191c598e520017ed57e3b46a6f0c5b26c9bdfd1120cd21435e82");
        assert_eq!(ensure_run(&state,task_id,&request).unwrap()["deduplicated"],true);
        assert_eq!(state.store.list("src_artifacts").unwrap().len(),1);
    }

    #[tokio::test]
    async fn run_binding_contract_artifact_failure_leaves_no_bound_run() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("muteki.sqlite");
        let state = Arc::new(AppState::new(crate::store::Store::open(&path).unwrap()));
        state.store.put("system","state_dir",&json!(directory.path().to_string_lossy())).unwrap();
        let created = create_task_inner(&state,&json!({"title":"Binding rollback","goal":"Review",
            "authorization":{"targets":["http://127.0.0.1:8080/app"]},
            "idempotency_key":"src-binding-rollback"})).await.unwrap();
        let task_id = created["output"]["task_id"].as_str().unwrap();
        let request = json!({"target_url":"http://127.0.0.1:8080/app","binding_key":"rollback"});
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TRIGGER fail_contract_artifact BEFORE INSERT ON documents
            WHEN NEW.collection='src_artifacts'
            BEGIN SELECT RAISE(ABORT,'contract artifact failed'); END;").unwrap();
        assert!(ensure_run(&state,task_id,&request).is_err());
        assert!(records(&state,task_id,"run_binding").unwrap().is_empty());
        assert!(state.store.list("src_artifacts").unwrap().is_empty());
        assert!(state.store.list("runs").unwrap().iter().all(|run|run["src_task_id"].is_null()));
        db.execute_batch("DROP TRIGGER fail_contract_artifact;").unwrap();
        let created = ensure_run(&state,task_id,&request).unwrap();
        assert_eq!(created["deduplicated"],false);
        assert_eq!(records(&state,task_id,"run_binding").unwrap().len(),1);
        assert_eq!(state.store.list("src_artifacts").unwrap().len(),1);
    }
    #[tokio::test]
    async fn direct_run_start_requires_current_src_binding() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        state
            .store
            .put("system", "state_dir", &json!(dir.path().canonicalize().unwrap().to_string_lossy()))
            .unwrap();
        let created = create_task_inner(
            &state,
            &json!({"title":"Audit","goal":"Review",
            "authorization":{"targets":["http://127.0.0.1:8080/app"]},
            "idempotency_key":"direct-start-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let target = "http://127.0.0.1:8080/app/login";
        let ensured = ensure_run(
            &state,
            id,
            &json!({"target_url":target,"question":"Review"}),
        )
        .unwrap();
        let rid = ensured["binding"]["run_id"].as_str().unwrap();
        let contract =
            runs::pentest::compile_src_contract("Review", target, &[target.to_owned()]).unwrap();
        let mut run = state.store.get("runs", rid).unwrap().unwrap();
        run["pentest_contract"] = contract;
        run["src_policy_version"] = json!(1);
        run["src_policy_digest"] = policy(&state, id).unwrap()["digest"].clone();
        state.store.put("runs", rid, &run).unwrap();
        let challenge = json!({"name":"Audit","mode":"pentest","target":target,
            "description":"Review","src_task_id":id});
        let start = |challenge: Value| json!({"kind":"idle","challenge":challenge});
        let missing = runs::start_run(
            State(state.clone()),
            Path(rid.to_owned()),
            Json(start(
                json!({"name":"Audit","mode":"pentest","target":target,"description":"Review"}),
            )),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);
        let mut forged = challenge.clone();
        forged["src_task_id"] = json!("another-task");
        let forged = runs::start_run(
            State(state.clone()),
            Path(rid.to_owned()),
            Json(start(forged)),
        )
        .await;
        assert_eq!(forged.status(), StatusCode::FORBIDDEN);
        let mut ctf = challenge.clone();
        ctf["mode"] = json!("ctf");
        let downgraded =
            runs::start_run(State(state.clone()), Path(rid.to_owned()), Json(start(ctf))).await;
        assert_eq!(downgraded.status(), StatusCode::FORBIDDEN);
        let valid = runs::start_run(
            State(state.clone()),
            Path(rid.to_owned()),
            Json(start(challenge.clone())),
        )
        .await;
        let status = valid.status();
        let detail = axum::body::to_bytes(valid.into_body(), usize::MAX).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&detail));
        update_policy(
            &state,
            id,
            &json!({"base_version":1,"authorized_targets":[]}),
        )
        .unwrap();
        let stale =
            runs::start_run(State(state), Path(rid.to_owned()), Json(start(challenge))).await;
        assert_eq!(stale.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn transparent_proxy_post_ask_creates_chat_approval_card() {
        let state = Arc::new(AppState::new(crate::store::Store::open(":memory:").unwrap()));
        let created = create_task_inner(&state, &json!({"title":"I85","goal":"Hold the proxy request","idempotency_key":"i85-approval"})).await.unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let thread_id = created["output"]["thread_id"].as_str().unwrap();
        update_policy(&state, id, &json!({
            "base_version":1,
            "rules":[{
                "rule_id":"srule-i85","version":1,"enabled":true,
                "evaluation_point":"request","resource_kind":"http","effect":"ask",
                "mode":"enforce","selector":{"method":{"eq":"POST"}},
                "conditions":{},"actions":["POST"],"reason":"ask","source_refs":["test"],
                "reusable_approval":true
            }],
            "authorized_targets":["http://127.0.0.1:4280/"],
            "inspection_requirements":{"mode":"record","approval_timeout_seconds":300}
        })).unwrap();
        let version = policy(&state, id).unwrap()["version"].clone();
        let invocation = "854e3fe8c96d494f8401b3e059851d1b";
        let request = json!({
            "invocation_id":invocation,"task_id":id,"thread_id":thread_id,
            "tool_name":"src.http.transparent_proxy","action":"request",
            "arguments_hash":"abc","arguments":{"url":"http://127.0.0.1:4280/login.php","method":"POST"},
            "resource_kind":"http",
            "resource_facts":{
                "method":{"value":"POST","origin":"request","state":"known"},
                "url":{"value":"http://127.0.0.1:4280/login.php","origin":"request","state":"known"}
            },
            "identity_ref":"default","environment_id":"gateway",
            "environment_generation":version,"policy_version":version
        });
        let receipt = execution_runtime::admit(&state, id, &request, 300).unwrap();
        assert_eq!(receipt["dispatch_state"], "waiting_approval");
        let approval_id = receipt["approval_id"].as_str().unwrap();
        assert!(approval_id.starts_with("sapproval-"));
        assert_eq!(execution_runtime::dispatch(&state, id, invocation).unwrap_err().code, "approval_required");
        assert_eq!(execution_runtime::receipt(&state, id, invocation).unwrap()["dispatch_state"], "waiting_approval");
        execution_runtime::project_chat(&state, id, invocation).unwrap();
        let doc = chat::load(&state, thread_id).unwrap();
        assert_eq!(doc["thread"]["state"]["pending_approvals"][approval_id]["status"], "pending");
        assert_eq!(doc["thread"]["state"]["pending_approvals"][approval_id]["tool_name"], "src.http.transparent_proxy");
        assert_eq!(doc["thread"]["state"]["pending_approval"]["approval_id"], approval_id);
        let events = state.store.chat_events(thread_id, None, 0, i64::MAX as u64, 50).unwrap();
        assert!(events.iter().any(|event| event["event_type"]=="core.approval.requested" && event["payload"]["approval_id"]==approval_id));
    }

    #[tokio::test]
    async fn session_close_settles_a_dispatched_hook_receipt_and_leaves_an_unscoped_one() {
        let state = Arc::new(AppState::new(crate::store::Store::open(":memory:").unwrap()));
        let created = create_task_inner(&state, &json!({"title":"I72","goal":"Close a dispatched hook","idempotency_key":"i72-session-close"})).await.unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let thread_id = created["output"]["thread_id"].as_str().unwrap();
        let version = policy(&state, id).unwrap()["version"].clone();
        let hook = "claude-hook-session-close";
        let plain = "plain-process-session-close";
        let request = |invocation: &str, adapter: Option<&str>| {
            let mut body = json!({
                "invocation_id":invocation,"task_id":id,"thread_id":thread_id,
                "tool_name":"Bash","action":"Bash","arguments_hash":invocation,
                "arguments":{"command":"sleep 15"},"resource_kind":"process",
                "resource_facts":{},"identity_ref":"default","environment_id":"claude.agent_sdk",
                "environment_generation":version,"policy_version":version
            });
            if let Some(adapter) = adapter { body["adapter_id"] = json!(adapter); }
            body
        };
        execution_runtime::admit(&state, id, &request(hook, Some("claude.agent_sdk")), 900).unwrap();
        execution_runtime::dispatch(&state, id, hook).unwrap();
        execution_runtime::admit(&state, id, &request(plain, None), 900).unwrap();
        execution_runtime::dispatch(&state, id, plain).unwrap();
        native_policy::close_dispatched_hook_receipts(&state, id, thread_id).unwrap();
        let closed = execution_runtime::receipt(&state, id, hook).unwrap();
        assert_eq!(closed["dispatch_state"], "outcome_unknown");
        assert_eq!(closed["error_code"], "native_session_closed");
        assert_eq!(closed["outcome_known"], false);
        assert_eq!(execution_runtime::receipt(&state, id, plain).unwrap()["dispatch_state"], "dispatched");
    }

    #[tokio::test]
    async fn save_slice_marked_skips_noop_and_reports_conflict_details() {
        let state = Arc::new(AppState::new(crate::store::Store::open(":memory:").unwrap()));
        let created = create_task_inner(
            &state,
            &json!({"title":"Slice CAS","goal":"Verify slice save semantics","idempotency_key":"slice-cas-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();

        let first = save_slice_marked(
            &state,
            id,
            &json!({"slice": "clues", "base_revision": 0, "data": {"items": ["c1"]}}),
            Some("cmd-slice-1"),
            Some("op"),
        )
        .unwrap();
        let rev1 = first["result"]["revision"].as_u64().unwrap();
        assert_eq!(rev1, 1);

        let noop = save_slice_marked(
            &state,
            id,
            &json!({"slice": "clues", "base_revision": rev1, "data": {"items": ["c1"]}}),
            Some("cmd-slice-2"),
            Some("op"),
        )
        .unwrap();
        assert_eq!(noop["result"]["unchanged"], true);
        assert_eq!(noop["result"]["revision"], rev1);

        let err = save_slice_marked(
            &state,
            id,
            &json!({"slice": "clues", "base_revision": 0, "data": {"items": ["c2"]}}),
            Some("cmd-slice-3"),
            Some("op"),
        )
        .unwrap_err();
        assert_eq!(err.code, "version_conflict");
        assert_eq!(err.detail["slice"], "clues");
        assert_eq!(err.detail["expected"], 0);
        assert_eq!(err.detail["actual"], 1);
    }

    #[tokio::test]
    async fn proxypool_save_preserves_and_clears_credential_ref() {
        let state = Arc::new(AppState::new(crate::store::Store::open(":memory:").unwrap()));
        let created = create_task_inner(
            &state,
            &json!({"title":"Proxy Pool","goal":"Preserve credential ref","idempotency_key":"proxypool-cred-test"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();

        let first = domains::command(
            &state,
            id,
            "src.proxypool.save",
            &json!({
                "enabled": true,
                "nodes": [{
                    "node_id": "up-1",
                    "scheme": "http",
                    "host": "127.0.0.1",
                    "port": 8080,
                    "username": "u1",
                    "password": "proxy-secret-123"
                }]
            }),
        )
        .unwrap();
        assert_eq!(first["nodes"][0]["has_credential"], true);
        assert!(first["nodes"][0].get("credential_ref").is_none());
        let first_stored = rec(&state, id, "upstream_proxypool", "default").unwrap();
        let first_ref = first_stored["payload"]["nodes"][0]["credential_ref"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!first_ref.is_empty());

        let preserved = domains::command(
            &state,
            id,
            "src.proxypool.save",
            &json!({
                "enabled": true,
                "nodes": [{
                    "node_id": "up-1",
                    "scheme": "http",
                    "host": "127.0.0.1",
                    "port": 8080,
                    "username": "u1"
                }]
            }),
        )
        .unwrap();
        assert_eq!(preserved["nodes"][0]["has_credential"], true);
        let preserved_stored = rec(&state, id, "upstream_proxypool", "default").unwrap();
        assert_eq!(
            preserved_stored["payload"]["nodes"][0]["credential_ref"],
            first_ref
        );

        let cleared = domains::command(
            &state,
            id,
            "src.proxypool.save",
            &json!({
                "enabled": true,
                "nodes": [{
                    "node_id": "up-1",
                    "scheme": "http",
                    "host": "127.0.0.1",
                    "port": 8080,
                    "username": "u1",
                    "clear_credential": true
                }]
            }),
        )
        .unwrap();
        assert_eq!(cleared["nodes"][0]["has_credential"], false);
        let cleared_stored = rec(&state, id, "upstream_proxypool", "default").unwrap();
        assert!(cleared_stored["payload"]["nodes"][0]["credential_ref"].is_null());
    }

    #[tokio::test]
    async fn pause_cascades_to_delegated_subthread() {
        let state = Arc::new(AppState::new(crate::store::Store::open(":memory:").unwrap()));
        let created = create_task_inner(
            &state,
            &json!({"title":"Pause Cascade","goal":"Cascade pause and resume","idempotency_key":"src-pause-cascade"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        let main_tid = created["output"]["thread_id"].as_str().unwrap();
        let child_tid = "subthread-pause-cascade";

        let _ = crate::chat::create(
            axum::extract::State(state.clone()),
            axum::Json(json!({
                "command_id": "create-child-pause-test",
                "thread_id": child_tid,
                "title": "Child"
            })),
        )
        .await
        .unwrap();
        bind_subthread_delegation(&state, main_tid, child_tid, &json!({})).unwrap();

        let pause_receipt = domain_command::dispatch(
            state.clone(),
            id,
            &json!({
                "command_id": "cmd-pause-cascade",
                "command_type": "src.task.pause",
                "aggregate_type": "task",
                "aggregate_id": id,
                "payload": {},
                "idempotency_key": "pause-cascade-1"
            }),
            "operator:local-user",
        )
        .await
        .unwrap();
        assert_eq!(pause_receipt["output"]["result"]["payload"]["state"], "paused");
        let child_paused = state.store.get(crate::chat::COLLECTION, child_tid).unwrap().unwrap();
        assert_eq!(child_paused["thread"]["state"]["queue_paused"], true);
        assert_eq!(child_paused["thread"]["state"]["queue_pause_reason"], "src_task_paused");

        let resume_receipt = domain_command::dispatch(
            state.clone(),
            id,
            &json!({
                "command_id": "cmd-resume-cascade",
                "command_type": "src.task.resume",
                "aggregate_type": "task",
                "aggregate_id": id,
                "payload": {},
                "idempotency_key": "resume-cascade-1"
            }),
            "operator:local-user",
        )
        .await
        .unwrap();
        assert_eq!(resume_receipt["output"]["result"]["payload"]["state"], "active");
        let child_resumed = state.store.get(crate::chat::COLLECTION, child_tid).unwrap().unwrap();
        assert_eq!(child_resumed["thread"]["state"]["queue_paused"], false);
        assert_eq!(child_resumed["thread"]["state"]["queue_pause_reason"], "");
    }
}

pub fn spawn_inspection_bridge(state:Arc<AppState>){
    let weak=Arc::downgrade(&state);crate::business::spawn(state.clone(),crate::business::Module::Src,"src.inspections",async move{let mut interval=tokio::time::interval(std::time::Duration::from_secs(1));interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);loop{interval.tick().await;let Some(s)=weak.upgrade()else{break};if s.admission.draining() || crate::business::require(&s,crate::business::Module::Src).is_err(){continue}let tasks=match s.store.list(TASKS){Ok(rows)=>rows,Err(error)=>{tracing::error!(%error,"SRC inspection Task scan failed");continue}};for task in tasks{let Some(id)=task["task_id"].as_str()else{continue};if task["lifecycle"]["state"]!="active"{continue}match domains::sync_inspection(&s,id){Ok(invocations)=>for invocation in invocations{if let Err(error)=execution_runtime::publish(&s,id,&invocation).await{tracing::warn!(task_id=id,code=error.code,"SRC inspection approval projection failed");}},Err(error)=>tracing::error!(task_id=id,code=error.code,"SRC inspection bridge failed")}}}});
}

/// Node execution entry: explicit operations only, retaining domain authorization checks.
pub(crate) async fn runner_environment(s:Arc<AppState>,id:&str,operation:&str,body:&Value)->Result<Value,Error>{
    match operation {
        "worker_gateway.ensure"=>domains::ensure_worker_gateway(&s,id).await,
        "browser.quiesce"=>domains::quiesce_browser(s,id).await.map(|()|json!({"quiesced":true})),
        "src.browser.ensure"|"src.browser.context.ensure"|"src.browser.tab.open"|"src.browser.screenshot.capture"|"src.browser.task.close"=>domains::browser_command(s,id,operation,body).await,
        _=>Err(Error::invalid("Unsupported Runner SRC operation")),
    }
}

pub(crate) async fn takeover_historical_task(
    s: Arc<AppState>,
    hist_id: &str,
    body: &Value,
) -> Response {
    respond(takeover_historical_task_inner(s, hist_id, body).await)
}

async fn takeover_historical_task_inner(
    s: Arc<AppState>,
    hist_id: &str,
    body: &Value,
) -> Api<Value> {
    let hist = s
        .store
        .get("history_src_tasks", hist_id)
        .map_err(db)?
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.historical_task_not_found",
                "历史导入 SRC Task 不存在",
            )
        })?;
    let raw_targets = body
        .get("targets")
        .filter(|v| !v.is_null())
        .ok_or_else(|| {
            Error::invalid("接管历史研究时必须明确填写新一期授权目标 targets，不会自动继承旧范围")
        })?;
    let parsed_targets = targets(raw_targets)?;
    if parsed_targets.is_empty() {
        return Err(Error::invalid(
            "接管历史研究必须至少包含一个有效的授权目标 URL",
        ));
    }
    execution_policy::validate_scope_fuses(
        &parsed_targets,
        body["scope_fuse_override_reason"].as_str(),
    )?;
    if let Some(excluded) = body["excluded_targets"].as_array() {
        for item in excluded {
            let s_val = item
                .as_str()
                .ok_or_else(|| Error::invalid("excluded_targets 成员必须是 URL 字符串"))?;
            parse_scope_url(s_val)?;
        }
    }
    let old_title = hist["title"].as_str().unwrap_or("历史研究");
    let new_title = body["title"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{old_title}（新一期接管）"));
    let project_id = match body["project_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(pid) => {
            validate_src_project_id(&s, pid)?;
            json!(pid)
        }
        None => match hist["project_id"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            Some(pid) if !is_chat_workspace_project(&s, pid)? => json!(pid),
            _ => Value::Null,
        },
    };
    let goal = body["goal"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .or_else(|| hist["input"]["goal"].as_str())
        .unwrap_or("接管历史研究并按本期新授权范围继续分析");
    let notes = body["notes"].as_str().unwrap_or("");
    let auth_obj = json!({
        "targets": parsed_targets,
        "excluded_targets": body.get("excluded_targets").cloned().unwrap_or_else(|| json!([])),
        "notes": notes,
        "scope_fuse_override_reason": body.get("scope_fuse_override_reason").cloned().unwrap_or(Value::Null),
    });
    let mut create_body = json!({
        "title": new_title,
        "goal": goal,
        "project_id": project_id,
        "company": hist["input"]["company"].as_str().unwrap_or(""),
        "program": hist["input"]["program"].clone(),
        "authorization": auth_obj,
        "input": {
            "source_historical_task_id": hist_id,
            "historical_takeover": {
                "source_task_id": hist_id,
                "source_title": old_title,
                "taken_over_at": now(),
            }
        }
    });
    if let Some(key) = body["idempotency_key"].as_str().filter(|v| !v.is_empty()) {
        create_body["idempotency_key"] = json!(key);
    }
    let created = create_task_inner(&s, &create_body).await?;
    let new_task_id = created["output"]["task_id"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let new_thread_id = created["output"]["thread_id"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    if !new_task_id.is_empty() && created["deduplicated"] != true {
        let hist_note_key = format!("takeover-{hist_id}");
        let note_payload = json!({
            "kind": "historical_takeover",
            "title": format!("接管自历史研究：{old_title}"),
            "source_historical_task_id": hist_id,
            "summary": format!("已由历史只读快照 `{hist_id}` 创建新一期可编辑研究 `{new_task_id}`，授权目标数 {}。", parsed_targets.len()),
            "created_at": now(),
        });
        let _ = save_rec(&s, &new_task_id, "workbench_history", &hist_note_key, note_payload, Some(0));
    }
    Ok(json!({
        "task_id": new_task_id,
        "thread_id": new_thread_id,
        "title": new_title,
        "project_id": project_id,
        "source_historical_task_id": hist_id,
        "authorized_targets": parsed_targets,
    }))
}
