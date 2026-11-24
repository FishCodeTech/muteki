//! Persistent, scoped SRC monitoring. Online cycles reuse connector dispatch
//! and its durable policy, artifact, and idempotency receipts.
mod generic;
mod lease;
use super::{
    Api, Error, connector_config, connector_query, connectors_online,
    credential_store, now, policy, rec, rec_key, records, require, save_rec, task, verify_ref,
};
use crate::AppState;
use axum::http::StatusCode;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha512};
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, Weak},
};
use uuid::Uuid;

fn lock(id: &str, monitor_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let locks = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, value| value.strong_count() > 0);
    let key = format!("{id}:{monitor_id}");
    if let Some(existing) = locks.get(&key).and_then(Weak::upgrade) {
        return existing;
    }
    let value = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&value));
    value
}
fn stable(prefix: &str, value: &str) -> String {
    format!(
        "{prefix}{}",
        &format!("{:x}", Sha512::digest(value.as_bytes()))[..24]
    )
}
fn digest(value: &Value) -> String {
    crate::runs::src_api::digest(value)
}
fn cycle_timeout(definition: &Value) -> Api<(std::time::Duration, i64)> {
    let configured = definition["schedule"]["cycle_timeout_seconds"]
        .as_f64()
        .unwrap_or(300.0);
    let seconds = if configured == 0.0 { 300.0 } else { configured };
    if !seconds.is_finite() || seconds > i64::MAX as f64 {
        return Err(Error::invalid("monitor cycle timeout is invalid"));
    }
    let wait = if seconds <= 0.0 {
        std::time::Duration::ZERO
    } else {
        std::time::Duration::from_secs_f64(seconds)
    };
    Ok((wait, (seconds.trunc() as i64).max(1)))
}
fn unavailable(message: impl Into<String>) -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "capability.unavailable",
        message,
    )
}
fn blocked_error(reason: &str) -> Error {
    let code = match reason {
        "cost_unknown" => "cost_unknown",
        "external_outcome_unknown" => "external_outcome_unknown",
        "budget_exhausted" => "budget_exhausted",
        "authorization_unresolved" => "authorization_unresolved",
        "capability_unavailable" => "capability_unavailable",
        "cancelled_by_user" => "cancelled_by_user",
        _ => "monitor_state_invalid",
    };
    Error::new(
        StatusCode::CONFLICT,
        code,
        "monitor is blocked; resolve its blocked reason first",
    )
}

/// Check the configured repository before dispatch. A default branch learned
/// from metadata is checked again by the structured HTTP gateway at dispatch.
pub(super) fn preflight(s: &AppState, id: &str, definition: &Value) -> Api<Value> {
    if definition["continuous"] != true {
        return Err(Error::invalid(
            "continuous monitoring requires explicit confirmation",
        ));
    }
    let source = &definition["source"];
    let provider = require(source, "connector_id")?;
    if !matches!(
        provider,
        "github" | "gitlab" | "fofa" | "quake" | "shodan" | "zoomeye" | "aws_ec2" | "scopesentry"
    ) {
        return Err(unavailable(
            "connector has no online monitoring adapter",
        ));
    }
    let version = source["config_version"]
        .as_u64()
        .filter(|v| *v > 0)
        .ok_or_else(|| Error::invalid("source config_version required"))?;
    let config = connector_config(s, id, &json!({"connector_id":provider,"version":version}))?;
    if config["version"] != version || config["revoked_at"].is_string() {
        return Err(Error::conflict(
            "monitor connector config is unavailable or revoked",
        ));
    }
    if source["capability_manifest_ref"] != config["manifest"]["version"] {
        return Err(Error::conflict("monitor connector manifest changed"));
    }
    let manifest_doc = super::connector_config::manifest(provider)?;
    let settings = &config["config"];
    for field in manifest_doc["credentials"]["reference_fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .chain(std::iter::once("credential_ref"))
    {
        if let Some(reference) = settings[field]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty() && v.starts_with("secret://"))
        {
            credential_store::version(s, id, reference)?;
        }
    }
    let p = policy(s, id)?;
    let excluded = |url: &str| -> Api<()> {
        crate::runs::src_api::parse_url(url)?;
        if p["excluded_targets"].as_array().is_some_and(|rows| {
            rows.iter().any(|v| {
                v.as_str()
                    .is_some_and(|ex| crate::runs::src_api::target_matches(url, ex))
            })
        }) {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "authorization_unresolved",
                "monitor target is in excluded_targets",
            ));
        }
        Ok(())
    };
    super::connector_config::enforce_grant_for_purpose(s, id, provider, "monitor", false)?;

    if !matches!(definition["chain_type"].as_str(), Some("asset" | "rule")) {
        return Err(Error::invalid("monitor chain_type must be asset or rule"));
    }

    if matches!(provider, "github" | "gitlab") {
        let repo_fallback = settings["repository"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                let o = settings["owner"].as_str().map(str::trim).filter(|v| !v.is_empty())?;
                let r = settings["repo"].as_str().map(str::trim).filter(|v| !v.is_empty())?;
                Some(format!("{o}/{r}"))
            });
        let repository = repo_fallback
            .as_deref()
            .ok_or_else(|| Error::invalid("repository is required"))?;
        let branch = settings["ref"].as_str().filter(|v| !v.is_empty());
        let base = super::connector_config::connector_base_url(&manifest_doc, Some(settings));
        let metadata = connectors_online::url(&base, provider, repository, None)?;
        excluded(&metadata)?;
        let commit = if let Some(branch) = branch {
            let url = connectors_online::url(&base, provider, repository, Some(branch))?;
            excluded(&url)?;
            json!(url)
        } else {
            Value::Null
        };
        return Ok(json!({
            "provider": provider,
            "connector_id": provider,
            "config_version": version,
            "repository": repository,
            "metadata_url": metadata,
            "commit_url": commit,
            "base_url": base,
            "policy_version": p["version"],
            "policy_digest": p["digest"],
            "calls_per_cycle": 2
        }));
    }

    let base = super::connector_config::connector_base_url(&manifest_doc, Some(settings));
    if !base.is_empty() {
        excluded(&base)?;
    }
    Ok(json!({
        "provider": provider,
        "connector_id": provider,
        "config_version": version,
        "base_url": base,
        "query": source["query"],
        "max_pages": source["max_pages"],
        "max_items": source["max_items"],
        "extra": source["extra"],
        "policy_version": p["version"],
        "policy_digest": p["digest"],
        "calls_per_cycle": 1
    }))
}

pub(super) fn preflight_for_create(s: &AppState, id: &str, definition: &Value) -> Api<()> {
    let source = &definition["source"];
    let connector_id = require(source, "connector_id")?;
    let manifest_doc = super::connector_config::manifest(connector_id)?;
    let version = source["config_version"]
        .as_u64()
        .filter(|v| *v > 0)
        .ok_or_else(|| Error::invalid("source config_version required"))?;
    let cfg = connector_config(s, id, &json!({"connector_id": connector_id, "version": version}))?;
    if cfg["version"] != version || cfg["revoked_at"].is_string() {
        return Err(Error::conflict(
            "monitor connector config is unavailable or revoked",
        ));
    }
    if source["capability_manifest_ref"] != cfg["manifest"]["version"] {
        return Err(Error::conflict("monitor connector manifest changed"));
    }
    super::connector_config::enforce_grant_for_purpose(s, id, connector_id, "monitor", false)?;
    let settings = &cfg["config"];
    if manifest_doc["credentials"]["required"] == true {
        let mut missing = Vec::new();
        for field in manifest_doc["credentials"]["reference_fields"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let val = settings[field].as_str().map(str::trim).unwrap_or("");
            if val.is_empty() {
                missing.push(field.to_owned());
            } else if val.starts_with("secret://") {
                credential_store::version(s, id, val)?;
            }
        }
        if !missing.is_empty() {
            return Err(Error::invalid(format!(
                "连接器 {connector_id} 缺少凭据配置: {}",
                missing.join("、")
            )));
        }
    }
    if connector_id == "scopesentry" && settings["query_enabled"] == true {
        if settings["base_url"]
            .as_str()
            .map(str::trim)
            .is_none_or(|v| v.is_empty())
            || !settings["endpoints"]
                .as_object()
                .is_some_and(|m| !m.is_empty())
        {
            return Err(Error::invalid(
                "ScopeSentry monitor requires base_url and endpoints when query_enabled=true",
            ));
        }
    }
    Ok(())
}

pub(super) fn preflight_query(s: &AppState, id: &str, params: &Value) -> Api<Value> {
    let definition = if let Some(mid) = params["monitor_id"].as_str().filter(|v| !v.trim().is_empty()) {
        rec(s, id, "monitor", mid)?["payload"]["definition"].clone()
    } else if params["definition"].is_object() {
        params["definition"].clone()
    } else if params["source"].is_object() {
        json!({
            "source": params["source"],
            "chain_type": params["chain_type"].as_str().unwrap_or("asset"),
            "continuous": params["continuous"].as_bool().unwrap_or(true),
            "authorization_ref": params["authorization_ref"],
        })
    } else {
        return Err(Error::invalid("monitor_id, definition, or source is required"));
    };

    let p = policy(s, id)?;
    let policy_version = p["version"].as_u64().unwrap_or(0);
    let mut blocking_reasons: Vec<Value> = Vec::new();
    let mut warnings: Vec<Value> = Vec::new();
    let mut connector_readiness = json!("unknown");

    let connector_id = definition["source"]["connector_id"]
        .as_str()
        .map(str::trim)
        .unwrap_or("");
    if connector_id.is_empty() {
        blocking_reasons.push(json!({
            "code": "connector_id_missing",
            "message": "monitor source.connector_id is required"
        }));
    } else {
        let status_res = super::connector_config::status(s, id)?;
        if let Some(entry) = status_res["connectors"]
            .as_array()
            .and_then(|arr| arr.iter().find(|c| c["connector_id"] == connector_id))
        {
            connector_readiness = entry["readiness"].clone();
            if entry["readiness"] == "unconnected" {
                blocking_reasons.push(json!({
                    "code": "connector_unconnected",
                    "message": format!("连接器 {connector_id} 尚未配置")
                }));
            } else if entry["readiness"] == "permission_denied" {
                blocking_reasons.push(json!({
                    "code": "connector_permission_denied",
                    "message": format!("连接器 {connector_id} 凭据权限校验失败")
                }));
            } else if entry["readiness"] == "file_import_only" {
                warnings.push(json!({
                    "code": "file_import_only",
                    "message": format!("连接器 {connector_id} 仅支持文件导入，无法在线轮询")
                }));
            } else if entry["readiness"] == "configured_unverified" {
                let missing = entry["missing_fields"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if !missing.is_empty() {
                    blocking_reasons.push(json!({
                        "code": "connector_missing_fields",
                        "message": format!("连接器 {connector_id} 缺少必要配置或凭据"),
                        "missing_fields": missing
                    }));
                } else {
                    warnings.push(json!({
                        "code": "connector_unverified",
                        "message": format!("连接器 {connector_id} 尚未完成连通性验证")
                    }));
                }
            }
        } else {
            blocking_reasons.push(json!({
                "code": "unknown_connector",
                "message": format!("未知连接器: {connector_id}")
            }));
        }

        if matches!(
            connector_id,
            "fofa" | "quake" | "shodan" | "zoomeye"
        ) && definition["source"]["query"]
            .as_str()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            blocking_reasons.push(json!({
                "code": "monitor_query_missing",
                "message": format!("在线资产连接器 {connector_id} 监控需要提供查询表达式 source.query")
            }));
        }
        if !matches!(connector_id, "github" | "gitlab")
            && let Ok(manifest_doc) = super::connector_config::manifest(connector_id)
        {
            let cfg_opt = super::connector_config::latest_config_optional(s, id, connector_id)
                .ok()
                .flatten();
            let base = super::connector_config::connector_base_url(
                &manifest_doc,
                cfg_opt.as_ref().map(|c| &c["config"]),
            );
            if !base.is_empty()
                && p["excluded_targets"].as_array().is_some_and(|rows| {
                    rows.iter().any(|v| {
                        v.as_str().is_some_and(|ex| {
                            crate::runs::src_api::target_matches(&base, ex)
                        })
                    })
                })
            {
                blocking_reasons.push(json!({
                    "code": "authorization_unresolved",
                    "message": format!("connector base URL {base} is in excluded_targets")
                }));
            }
        }
        if matches!(connector_id, "github" | "gitlab") {
            if let Ok(Some(cfg)) =
                super::connector_config::latest_config_optional(s, id, connector_id)
            {
                let settings = &cfg["config"];
                let repo = settings["repository"]
                    .as_str()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        let o = settings["owner"].as_str().map(str::trim).filter(|v| !v.is_empty())?;
                        let r = settings["repo"].as_str().map(str::trim).filter(|v| !v.is_empty())?;
                        Some(format!("{o}/{r}"))
                    });
                if let Some(repo) = repo
                    && let Ok(manifest_doc) = super::connector_config::manifest(connector_id)
                {
                    let base = super::connector_config::connector_base_url(
                        &manifest_doc,
                        Some(settings),
                    );
                    if let Ok(meta_url) =
                        connectors_online::url(&base, connector_id, &repo, None)
                    {
                        if p["excluded_targets"].as_array().is_some_and(|rows| {
                            rows.iter().any(|v| {
                                v.as_str().is_some_and(|ex| {
                                    crate::runs::src_api::target_matches(&meta_url, ex)
                                })
                            })
                        }) {
                            blocking_reasons.push(json!({
                                "code": "authorization_unresolved",
                                "message": "repository metadata URL is in excluded_targets"
                            }));
                        }
                    }
                    if let Some(branch) = settings["ref"].as_str().filter(|v| !v.is_empty())
                        && let Ok(commit_url) =
                            connectors_online::url(&base, connector_id, &repo, Some(branch))
                    {
                        if p["excluded_targets"].as_array().is_some_and(|rows| {
                            rows.iter().any(|v| {
                                v.as_str().is_some_and(|ex| {
                                    crate::runs::src_api::target_matches(&commit_url, ex)
                                })
                            })
                        }) {
                            blocking_reasons.push(json!({
                                "code": "authorization_unresolved",
                                "message": "repository commit URL is in excluded_targets"
                            }));
                        }
                    }
                }
            }
        }
    }

    Ok(json!({
        "ok": blocking_reasons.is_empty(),
        "blocking_reasons": blocking_reasons,
        "warnings": warnings,
        "connector_readiness": connector_readiness,
        "policy_version": policy_version
    }))
}

pub(super) fn active_monitor(s: &AppState, id: &str, definition: &Value) -> Api<Value> {
    task(s, id)?;
    let _ = definition;
    Ok(
        json!({"status":"active","activation_reason":null,"next_wake_at":now(),
        "blocked_reason":"","calls_used":0,"cost_used":0.0,"cost_unknown":false,"current_cycle_id":null,
        "last_success_at":null}),
    )
}

fn monitor_row(s: &AppState, id: &str, monitor_id: &str) -> Api<Value> {
    rec(s, id, "monitor", monitor_id)
}
fn cycle_definition(
    s: &AppState,
    id: &str,
    monitor_id: &str,
    version: u64,
    monitor: &Value,
) -> Api<Value> {
    if version == 0 {
        return Ok(monitor["definition"].clone());
    }
    let key = format!("{monitor_id}:v{version}");
    if let Ok(row) = rec(s, id, "monitor_definition", &key) {
        return Ok(row["payload"].clone());
    }
    if monitor["version"] == version {
        return Ok(monitor["definition"].clone());
    }
    Err(Error::new(
        StatusCode::CONFLICT,
        "monitor_definition_missing",
        "pinned monitor definition version is unavailable",
    ))
}
fn cycle_row(s: &AppState, id: &str, cycle_id: &str) -> Api<Option<Value>> {
    Ok(records(s, id, "monitor_cycle")?
        .into_iter()
        .find(|v| v["natural_key"] == cycle_id))
}
fn save_cycle(s: &AppState, id: &str, cycle_id: &str, value: Value) -> Api<Value> {
    let revision = cycle_row(s, id, cycle_id)?
        .and_then(|v| v["revision"].as_u64())
        .unwrap_or(0);
    Ok(save_rec(s, id, "monitor_cycle", cycle_id, value, Some(revision))?["payload"].clone())
}
fn record_once(s: &AppState, id: &str, kind: &str, key: &str, value: Value) -> Api<Value> {
    if let Some(existing) = records(s, id, kind)?
        .into_iter()
        .find(|v| v["natural_key"] == key)
    {
        return Ok(existing["payload"].clone());
    }
    Ok(save_rec(s, id, kind, key, value, Some(0))?["payload"].clone())
}
type MonitorWrite = (String, String, Value, Option<u64>);

fn monitor_write(kind: &str, key: &str, payload: Value, expected: Option<u64>) -> MonitorWrite {
    (kind.to_owned(), key.to_owned(), payload, expected)
}

fn save_many(s: &AppState, id: &str, writes: Vec<MonitorWrite>) -> Api<Vec<Value>> {
    const RECORDS: &str = "src_live_records";
    let mut prepared = Vec::with_capacity(writes.len());
    for (kind, key, payload, expected) in writes {
        let record_id = rec_key(id, &kind, &key);
        let current = s
            .store
            .get(RECORDS, &record_id)
            .map_err(crate::runs::src_api::db)?;
        let version = current
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(0);
        if expected.is_some_and(|expected| expected != version) {
            return Err(Error::conflict("SRC monitor record version changed"));
        }
        let created_at = current
            .as_ref()
            .and_then(|row| row["values"]["created_at"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(now);
        let parent_id = match kind.as_str() {
            "monitor_definition" | "monitor_baseline" | "monitor_notification" => {
                payload["monitor_id"].as_str()
            }
            "monitor_cycle" => payload["monitor_id"].as_str(),
            "monitor_source_receipt"
            | "monitor_observation"
            | "monitor_cost"
            | "monitor_change"
            | "monitor_candidate" => payload["cycle_id"].as_str(),
            _ => None,
        }
        .unwrap_or("");
        let row = json!({"record_id":record_id,"kind":kind,"task_id":id,
            "revision":version+1,"parent_id":parent_id,"natural_key":key,
            "generation":0,"created_at":created_at,"updated_at":now(),"payload":payload});
        prepared.push((record_id, version, row));
    }
    let operations = prepared
        .iter()
        .map(|(record_id, version, row)| (RECORDS, record_id.as_str(), *version, row.clone()))
        .collect::<Vec<_>>();
    match s
        .store
        .compare_and_put_many(&operations)
        .map_err(crate::runs::src_api::db)?
    {
        Ok(saved) => Ok(saved.into_iter().map(|row| row["values"].clone()).collect()),
        Err(_) => Err(Error::conflict(
            "SRC monitor records changed during transaction",
        )),
    }
}

struct MonitorBatch<'a> {
    state: &'a AppState,
    task_id: &'a str,
    writes: Vec<MonitorWrite>,
    change_notices: Vec<(String, String, String)>,
}

impl<'a> MonitorBatch<'a> {
    fn new(state: &'a AppState, task_id: &'a str) -> Self {
        Self {
            state,
            task_id,
            writes: Vec::new(),
            change_notices: Vec::new(),
        }
    }

    fn once(&mut self, kind: &str, key: &str, payload: Value) -> Api<Value> {
        if let Some((_, _, staged, _)) = self
            .writes
            .iter()
            .find(|(staged_kind, staged_key, _, _)| staged_kind == kind && staged_key == key)
        {
            return Ok(staged.clone());
        }
        match rec(self.state, self.task_id, kind, key) {
            Ok(existing) => return Ok(existing["payload"].clone()),
            Err(error) if error.status == StatusCode::NOT_FOUND => {}
            Err(error) => return Err(error),
        }
        self.writes
            .push(monitor_write(kind, key, payload.clone(), Some(0)));
        Ok(payload)
    }

    fn update(&mut self, kind: &str, key: &str, payload: Value, expected: u64) {
        self.writes
            .push(monitor_write(kind, key, payload, Some(expected)));
    }

    fn notice_change(&mut self, change_id: &str, candidate_id: &str, root: &str) {
        self.change_notices.push((
            change_id.to_owned(),
            candidate_id.to_owned(),
            root.to_owned(),
        ));
    }

    fn commit(self) -> Api<Vec<(String, String, String)>> {
        save_many(self.state, self.task_id, self.writes)?;
        Ok(self.change_notices)
    }
}
fn cycle_notice_payload(
    s: &AppState,
    id: &str,
    monitor_id: &str,
    cycle: &Value,
) -> Api<Option<(String, Value)>> {
    let monitor = monitor_row(s, id, monitor_id)?;
    let version = cycle["definition_version"].as_u64().unwrap_or(0);
    let definition = cycle_definition(s, id, monitor_id, version, &monitor["payload"])?;
    let preferences = &definition["notification"];
    let status = cycle["status"].as_str().unwrap_or("");
    let reason = cycle["end_reason"].as_str().unwrap_or("");
    let failure = matches!(status, "failed" | "partial" | "budget_exhausted")
        || !matches!(reason, "" | "source_incomplete");
    let completion = status == "completed" && preferences["notify_on_completion"] == true;
    if !failure && !completion {
        return Ok(None);
    }
    let cycle_id = cycle["cycle_id"].as_str().unwrap_or("");
    let notification_id = stable("smnotice_", &format!("{id}:{cycle_id}:{status}:{reason}"));
    let payload = json!({
        "notification_id":notification_id,"task_id":id,"monitor_id":monitor_id,
        "root_object_key":monitor_id,"kind":if failure{"cycle_failed"}else{"cycle_completed"},
        "change_ids":[],"candidate_ids":[],"cycle_id":cycle["cycle_id"],
        "status":"pending","created_at":now(),"delivered_at":null,
        "window_start":now(),"window_end":now(),
        "channel":if preferences["external_enabled"]==true{"external"}else{"task"},
        "external_channel":if preferences["external_enabled"]==true{preferences["external_channel"].clone()}else{json!("")},
        "detail":{"end_reason":reason,"error":cycle["error"]}
    });
    Ok(Some((notification_id, payload)))
}
fn queue_repository_change_notice(
    s: &AppState,
    id: &str,
    monitor_id: &str,
    definition: &Value,
    cycle_id: &str,
    root: &str,
    change_id: &str,
    candidate_id: &str,
) -> Api<()> {
    if definition["notification"]["enabled"] == false {
        return Ok(());
    }
    let window = definition["notification"]["merge_window_seconds"]
        .as_i64()
        .unwrap_or(0)
        .max(0);
    let pending = records(s, id, "monitor_notification")?
        .into_iter()
        .rev()
        .find(|row| {
            row["payload"]["monitor_id"] == monitor_id
                && row["payload"]["root_object_key"] == root
                && row["payload"]["status"] == "pending"
                && row["payload"]["window_end"]
                    .as_str()
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    .is_some_and(|value| value >= Utc::now())
        });
    if let Some(row) = pending {
        let mut notification = row["payload"].clone();
        if !notification["change_ids"]
            .as_array()
            .is_some_and(|values| values.contains(&json!(change_id)))
        {
            notification["change_ids"]
                .as_array_mut()
                .unwrap()
                .push(json!(change_id));
            notification["candidate_ids"]
                .as_array_mut()
                .unwrap()
                .push(json!(candidate_id));
            notification["window_end"] =
                json!((Utc::now() + Duration::seconds(window)).to_rfc3339());
            save_rec(
                s,
                id,
                "monitor_notification",
                row["natural_key"].as_str().unwrap_or(""),
                notification,
                row["revision"].as_u64(),
            )?;
        }
    } else {
        let notification_id = stable("smnotice_", &format!("{id}:{monitor_id}:{cycle_id}"));
        record_once(
            s,
            id,
            "monitor_notification",
            &notification_id,
            json!({
                "notification_id":notification_id,"task_id":id,"monitor_id":monitor_id,
                "root_object_key":root,"kind":"meaningful_change","change_ids":[change_id],
                "candidate_ids":[candidate_id],"window_start":now(),
                "window_end":(Utc::now()+Duration::seconds(window)).to_rfc3339(),
                "status":"pending","channel":if definition["notification"]["external_enabled"]==true{"external"}else{"task"},
                "external_channel":if definition["notification"]["external_enabled"]==true{
                    definition["notification"]["external_channel"].clone()}else{json!("")},
                "created_at":now(),"delivered_at":null,"external_delivery":false
            }),
        )?;
    }
    Ok(())
}
fn source_signature(definition: &Value, preflight: &Value) -> String {
    digest(
        &json!({"chain_type":definition["chain_type"],"query_ref":definition["query_ref"],
        "target_selection":definition["target_selection"],"source":definition["source"],
        "authorization_ref":definition["authorization_ref"],"policy_digest":preflight["policy_digest"]}),
    )
}
fn source_receipt_payload(id: &str, cycle_id: &str, batch: &Value) -> Value {
    let receipt_id = stable(
        "sreceipt_",
        &format!("{id}:{cycle_id}:{}", batch["batch_id"]),
    );
    json!({
        "receipt_id":receipt_id,"task_id":id,"cycle_id":cycle_id,
        "source":batch["connector_id"],"source_receipt":batch["batch_id"],
        "raw_artifacts":batch["source_refs"],"source_watermarks":{
            "repository_commit":batch["repository"]["commit_sha"]},
        "complete":batch["acquisition"]["complete"],"created_at":now()
    })
}
fn finish_cycle(
    s: &AppState,
    id: &str,
    monitor_id: &str,
    cycle_id: &str,
    batch: &Value,
    preflight: &Value,
) -> Api<Value> {
    if batch["schema"] != "muteki.src.connector-batch.v1"
        || batch["external_state"] != "completed"
        || batch["acquisition"]["complete"] != true
        || batch["acquisition"]["truncated"] == true
    {
        return Err(Error::conflict("monitor source batch is incomplete"));
    }
    let refs = batch["source_refs"]
        .as_array()
        .ok_or_else(|| Error::invalid("monitor source refs absent"))?;
    if refs.len() != 2 {
        return Err(Error::invalid(
            "repository cycle must carry two source refs",
        ));
    }
    for reference in refs {
        verify_ref(s, id, reference)?;
    }
    let receipt = source_receipt_payload(id, cycle_id, batch);
    let current = monitor_row(s, id, monitor_id)?;
    let mut monitor = current["payload"].clone();
    if monitor["status"] != "active"
        || rec(s, id, "monitor_cycle", cycle_id)?["payload"]["cancel_requested"] == true
    {
        return finish_cancelled(s, id, monitor_id, cycle_id, batch);
    }
    let definition_version =
        rec(s, id, "monitor_cycle", cycle_id)?["payload"]["definition_version"]
            .as_u64()
            .unwrap_or_else(|| monitor["version"].as_u64().unwrap_or(1));
    let definition = cycle_definition(s, id, monitor_id, definition_version, &monitor)?;
    let snapshot = &batch["repository"];
    if !snapshot.is_object() || snapshot["commit_sha"].as_str().is_none_or(str::is_empty) {
        return Err(Error::invalid("repository snapshot has no commit SHA"));
    }
    let limit = definition["budget"]["call_limit"].as_u64().unwrap_or(0);
    if limit > 0 && 2 > limit {
        let cycle_record = rec(s, id, "monitor_cycle", cycle_id)?;
        let mut cycle = cycle_record["payload"].clone();
        cycle["status"] = json!("budget_exhausted");
        cycle["completeness"] = json!("complete");
        cycle["end_reason"] = json!("budget_exhausted");
        cycle["source_refs"] = json!(refs);
        cycle["source_receipts"] = json!([receipt["receipt_id"]]);
        cycle["external_state"] = json!("completed");
        cycle["dispatch_state"] = json!("finished");
        cycle["cost"] = json!({"reserved":cycle["cost"]["reserved"],"actual":0.0,"unknown":false,"calls":2,"currency":"USD"});
        cycle["finished_at"] = json!(now());
        monitor["blocked_reason"] = json!("budget_exhausted");
        monitor["activation_reason"] = json!("budget_exhausted");
        monitor["current_cycle_id"] = Value::Null;
        monitor["next_wake_at"] = Value::Null;
        monitor["last_cycle_id"] = json!(cycle_id);
        monitor["calls_used"] = json!(monitor["calls_used"].as_u64().unwrap_or(0) + 2);
        let notice = cycle_notice_payload(s, id, monitor_id, &cycle)?;
        let mut atomic = MonitorBatch::new(s, id);
        atomic.once(
            "monitor_source_receipt",
            receipt["receipt_id"].as_str().unwrap_or(""),
            receipt.clone(),
        )?;
        atomic.update(
            "monitor",
            monitor_id,
            monitor,
            current["revision"].as_u64().unwrap_or(0),
        );
        atomic.update(
            "monitor_cycle",
            cycle_id,
            cycle.clone(),
            cycle_record["revision"].as_u64().unwrap_or(0),
        );
        if let Some((key, payload)) = notice {
            atomic.once("monitor_notification", &key, payload)?;
        }
        atomic.commit()?;
        return Ok(cycle);
    }
    let signature = source_signature(&definition, preflight);
    let snapshot_digest = digest(snapshot);
    let already_committed = monitor["last_cycle_id"] == cycle_id;
    let previous = if already_committed {
        Value::Null
    } else {
        monitor["baseline"].clone()
    };
    let comparable = !previous.is_null() && previous["source_signature"] == signature;
    let changed =
        previous.is_null() || !comparable || previous["snapshot_digest"] != snapshot_digest;
    let mut change_id = Value::Null;
    let mut baseline_id = previous["baseline_id"].clone();
    let mut change_notice: Option<(String, String, String)> = None;
    let mut atomic = MonitorBatch::new(s, id);
    if changed {
        let next_baseline_id = stable("smbase_", &format!("{id}:{monitor_id}:{cycle_id}"));
        let baseline = atomic.once(
            "monitor_baseline",
            &next_baseline_id,
            json!({"baseline_id":next_baseline_id,"monitor_id":monitor_id,"cycle_id":cycle_id,
            "source_signature":signature,"snapshot":snapshot,"snapshot_digest":snapshot_digest,
            "source_refs":refs,"completeness":"complete","created_at":now()}),
        )?;
        baseline_id = json!(next_baseline_id);
        monitor["baseline"] = baseline;
        if !previous.is_null() {
            let id_value = stable("smchange_", &format!("{id}:{monitor_id}:{cycle_id}"));
            let candidate_id = stable("smcandidate_", &format!("{id}:{monitor_id}:{cycle_id}"));
            let root_object_key = snapshot["full_name"].as_str().unwrap_or("");
            atomic.once(
                "monitor_candidate",
                &candidate_id,
                json!({"candidate_id":candidate_id,
                "task_id":id,"monitor_id":monitor_id,"cycle_id":cycle_id,"change_id":id_value,
                "projection_kind":"observation","question":format!("核对 {root_object_key} 的变化是否改变当前研究前提"),
                "related_objects":[root_object_key],"evidence_refs":[id_value],"missing_conditions":[],
                "suggested_action":"review_evidence","status":"pending","created_at":now()}),
            )?;
            let change = json!({"change_id":id_value,"monitor_id":monitor_id,"cycle_id":cycle_id,
                "chain_type":definition["chain_type"],"change_type":if comparable{"repository_changed"}else{"incomparable_source_changed"},
                "subject_ref":{"repository":snapshot["full_name"],"id":snapshot["full_name"]},"comparable":comparable,
                "meaningful":true,"root_object_key":root_object_key,"candidate_id":candidate_id,
                "incomparable_reason":if comparable{Value::Null}else{json!("definition_or_scope_changed")},
                "completeness":"complete","before_ref":previous["source_refs"],"after_ref":refs,
                "raw_diff":{"before":previous["snapshot"],"after":snapshot},
                "denoised_diff":{"before_commit":previous["snapshot"]["commit_sha"],"after_commit":snapshot["commit_sha"]},
                "created_at":now()});
            atomic.once("monitor_change", &id_value, change)?;
            change_notice = Some((root_object_key.to_owned(), id_value.clone(), candidate_id));
            change_id = json!(id_value);
        }
    }
    let interval = definition["schedule"]["interval_seconds"]
        .as_i64()
        .unwrap_or(1)
        .max(1);
    let next = (Utc::now() + Duration::seconds(interval)).to_rfc3339();
    monitor["current_cycle_id"] = Value::Null;
    monitor["next_wake_at"] = json!(next);
    monitor["last_success_at"] = json!(now());
    monitor["calls_used"] =
        json!(monitor["calls_used"].as_u64().unwrap_or(0) + if already_committed { 0 } else { 2 });
    monitor["last_cycle_id"] = json!(cycle_id);
    monitor["cost_used"] = json!(0.0);
    monitor["cost_unknown"] = json!(false);
    monitor["updated_at"] = json!(now());
    let cycle_row = rec(s, id, "monitor_cycle", cycle_id)?;
    let mut cycle = cycle_row["payload"].clone();
    cycle["status"] = json!("completed");
    cycle["completeness"] = json!("complete");
    cycle["end_reason"] = json!("");
    cycle["error"] = json!("");
    cycle["finished_at"] = json!(now());
    cycle["baseline_id"] = baseline_id;
    cycle["compared_baseline_id"] = previous["baseline_id"].clone();
    cycle["change_id"] = change_id;
    cycle["source_refs"] = json!(refs);
    cycle["source_receipts"] = json!([receipt["receipt_id"]]);
    cycle["external_state"] = json!("completed");
    cycle["dispatch_state"] = json!("finished");
    cycle["snapshot_digest"] = json!(snapshot_digest);
    cycle["source_watermarks"] = json!({"repository_commit":snapshot["commit_sha"]});
    cycle["cost"] = json!({"reserved":cycle["cost"]["reserved"],"actual":0.0,"currency":"USD","unknown":false,"calls":2,"basis":"public_repository_read"});
    let notice = cycle_notice_payload(s, id, monitor_id, &cycle)?;
    let receipt_id = receipt["receipt_id"].as_str().unwrap_or("").to_owned();
    atomic.once("monitor_source_receipt", &receipt_id, receipt)?;
    if !already_committed {
        atomic.update(
            "monitor",
            monitor_id,
            monitor,
            current["revision"].as_u64().unwrap_or(0),
        );
    }
    atomic.update(
        "monitor_cycle",
        cycle_id,
        cycle.clone(),
        cycle_row["revision"].as_u64().unwrap_or(0),
    );
    if let Some((key, payload)) = notice {
        atomic.once("monitor_notification", &key, payload)?;
    }
    atomic.commit()?;
    if let Some((root, change_id, candidate_id)) = change_notice {
        queue_repository_change_notice(
            s,
            id,
            monitor_id,
            &definition,
            cycle_id,
            &root,
            &change_id,
            &candidate_id,
        )?;
    }
    Ok(cycle)
}

fn fail_cycle(s: &AppState, id: &str, monitor_id: &str, cycle_id: &str, error: &Error) -> Api<()> {
    let unknown = matches!(
        error.code,
        "external_outcome_unknown"
            | "src.connector_response_missing"
            | "src.store_failed"
            | "cycle_timeout"
    );
    let blocked = if unknown {
        "external_outcome_unknown"
    } else if error.code == "authorization_unresolved" {
        "authorization_unresolved"
    } else if error.code == "capability.unavailable" {
        "capability_unavailable"
    } else {
        ""
    };
    let cycle_record = rec(s, id, "monitor_cycle", cycle_id)?;
    let mut cycle = cycle_record["payload"].clone();
    cycle["status"] = json!("failed");
    cycle["external_state"] = json!(if unknown { "outcome_unknown" } else { "failed" });
    cycle["dispatch_state"] = json!(if unknown {
        "outcome_unknown"
    } else {
        "finished"
    });
    cycle["completeness"] = json!("unavailable");
    cycle["end_reason"] = json!(if error.code == "cycle_timeout" {
        "cycle_timeout"
    } else {
        blocked.is_empty().then_some(error.code).unwrap_or(blocked)
    });
    cycle["error"] = json!(error.message);
    cycle["finished_at"] = json!(now());
    let definition_version = cycle["definition_version"].as_u64().unwrap_or(0);
    let row = monitor_row(s, id, monitor_id)?;
    let mut monitor = row["payload"].clone();
    let definition = cycle_definition(s, id, monitor_id, definition_version, &monitor)?;
    let interval = definition["schedule"]["interval_seconds"]
        .as_i64()
        .unwrap_or(1)
        .max(1);
    monitor["current_cycle_id"] = Value::Null;
    monitor["last_cycle_id"] = json!(cycle_id);
    monitor["blocked_reason"] = json!(blocked);
    monitor["activation_reason"] = json!(if blocked.is_empty() {
        Value::Null
    } else {
        json!(blocked)
    });
    monitor["next_wake_at"] = if blocked.is_empty() {
        json!((Utc::now() + Duration::seconds(interval)).to_rfc3339())
    } else {
        Value::Null
    };
    let notice = cycle_notice_payload(s, id, monitor_id, &cycle)?;
    let mut atomic = MonitorBatch::new(s, id);
    atomic.update(
        "monitor_cycle",
        cycle_id,
        cycle,
        cycle_record["revision"].as_u64().unwrap_or(0),
    );
    atomic.update(
        "monitor",
        monitor_id,
        monitor,
        row["revision"].as_u64().unwrap_or(0),
    );
    if let Some((key, payload)) = notice {
        atomic.once("monitor_notification", &key, payload)?;
    }
    atomic.commit()?;
    Ok(())
}

fn finish_cancelled(
    s: &AppState,
    id: &str,
    monitor_id: &str,
    cycle_id: &str,
    batch: &Value,
) -> Api<Value> {
    let row = monitor_row(s, id, monitor_id)?;
    let mut monitor = row["payload"].clone();
    let cycle_record = rec(s, id, "monitor_cycle", cycle_id)?;
    let mut cycle = cycle_record["payload"].clone();
    cycle["status"] = json!("cancelled");
    cycle["completeness"] = json!("unavailable");
    cycle["end_reason"] = json!("cancelled_by_user");
    cycle["external_state"] = json!("completed");
    cycle["dispatch_state"] = json!("finished");
    cycle["source_refs"] = batch["source_refs"].clone();
    cycle["finished_at"] = json!(now());
    cycle["cost"]["calls"] = json!(2);
    cycle["cost"]["actual"] = json!(0.0);
    cycle["cost"]["unknown"] = json!(false);
    monitor["current_cycle_id"] = Value::Null;
    monitor["last_cycle_id"] = json!(cycle_id);
    monitor["next_wake_at"] = Value::Null;
    monitor["calls_used"] = json!(monitor["calls_used"].as_u64().unwrap_or(0) + 2);
    let notice = cycle_notice_payload(s, id, monitor_id, &cycle)?;
    let mut atomic = MonitorBatch::new(s, id);
    atomic.update(
        "monitor",
        monitor_id,
        monitor,
        row["revision"].as_u64().unwrap_or(0),
    );
    atomic.update(
        "monitor_cycle",
        cycle_id,
        cycle.clone(),
        cycle_record["revision"].as_u64().unwrap_or(0),
    );
    if let Some((key, payload)) = notice {
        atomic.once("monitor_notification", &key, payload)?;
    }
    atomic.commit()?;
    Ok(cycle)
}
pub(super) async fn run_once(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    let monitor_id = require(body, "monitor_id")?.to_owned();
    let monitor_lock = lock(id, &monitor_id);
    let _guard = monitor_lock.lock().await;
    let row = monitor_row(&s, id, &monitor_id)?;
    let monitor = &row["payload"];
    if monitor["status"] != "active" {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "monitor_state_invalid",
            "monitor is not active",
        ));
    }
    if let Some(reason) = monitor["blocked_reason"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        return Err(blocked_error(reason));
    }
    if monitor["current_cycle_id"]
        .as_str()
        .is_some_and(|value| !value.is_empty())
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "resource_busy",
            "previous monitor cycle is unresolved",
        ));
    }
    let cycle_id = format!(
        "{monitor_id}:v{}:{}",
        monitor["version"],
        Uuid::new_v4().simple()
    );
    run_cycle_inner(&s, id, &monitor_id, &cycle_id, true).await
}

pub(super) async fn run_cycle(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    let monitor_id = require(body, "monitor_id")?.to_owned();
    let cycle_id = require(body, "cycle_id")?.to_owned();
    let monitor_lock = lock(id, &monitor_id);
    let _guard = monitor_lock.lock().await;
    run_cycle_inner(&s, id, &monitor_id, &cycle_id, false).await
}

async fn run_cycle_inner(
    s: &Arc<AppState>,
    id: &str,
    monitor_id: &str,
    cycle_id: &str,
    create: bool,
) -> Api<Value> {
    let prior = cycle_row(&s, id, &cycle_id)?;
    if prior.is_none() && !create {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "src.record_not_found",
            "monitor cycle does not exist",
        ));
    }
    if prior
        .as_ref()
        .is_some_and(|v| v["payload"]["monitor_id"] != monitor_id)
    {
        return Err(Error::conflict("cycle ID belongs to another monitor"));
    }
    if prior.as_ref().is_some_and(|v| {
        matches!(
            v["payload"]["status"].as_str(),
            Some("completed" | "failed" | "budget_exhausted" | "cancelled" | "partial")
        )
    }) {
        return Ok(prior.unwrap()["payload"].clone());
    }
    if prior
        .as_ref()
        .is_some_and(|v| v["payload"]["status"] == "running")
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "resource_busy",
            "monitor cycle is already running; reconcile it before replay",
        ));
    }
    let row = monitor_row(&s, id, monitor_id)?;
    let mut monitor = row["payload"].clone();
    if monitor["status"] != "active" {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "monitor_state_invalid",
            "monitor is not active",
        ));
    }
    if let Some(reason) = monitor["blocked_reason"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        return Err(blocked_error(reason));
    }
    if task(&s, id)?["lifecycle"]["state"] != "active" {
        return Err(Error::conflict("SRC Task is not active"));
    }
    let definition_version = prior
        .as_ref()
        .and_then(|row| row["payload"]["definition_version"].as_u64())
        .unwrap_or_else(|| monitor["version"].as_u64().unwrap_or(1));
    let definition = cycle_definition(&s, id, monitor_id, definition_version, &monitor)?;
    if monitor["current_cycle_id"]
        .as_str()
        .is_some_and(|v| v != cycle_id)
    {
        return Err(Error::conflict("another monitor cycle is unresolved"));
    }
    let (timeout, ttl) = cycle_timeout(&definition)?;
    let lease = if prior.is_none() {
        let total = definition["budget"]["total"].as_f64().unwrap_or(0.0);
        let used = monitor["cost_used"].as_f64().unwrap_or(0.0);
        if total > 0.0 && used >= total {
            let cycle = json!({"cycle_id":cycle_id,"task_id":id,"monitor_id":monitor_id,
                "definition_version":monitor["version"],"status":"budget_exhausted",
                "completeness":"unavailable","scheduled_at":now(),"finished_at":now(),
                "end_reason":"budget_exhausted","cost":{"reserved":0.0,"actual":0.0,"unknown":false}});
            monitor["last_cycle_id"] = json!(cycle_id);
            monitor["blocked_reason"] = json!("budget_exhausted");
            monitor["next_wake_at"] = Value::Null;
            let notice = cycle_notice_payload(&s, id, monitor_id, &cycle)?;
            let mut writes = vec![
                monitor_write("monitor_cycle", cycle_id, cycle, Some(0)),
                monitor_write("monitor", monitor_id, monitor, row["revision"].as_u64()),
            ];
            if let Some((key, payload)) = notice {
                writes.push(monitor_write(
                    "monitor_notification",
                    &key,
                    payload,
                    Some(0),
                ));
            }
            let saved_rows = save_many(&s, id, writes)?;
            let saved = saved_rows[0]["payload"].clone();
            return Ok(saved);
        }
        let per_cycle = definition["budget"]["per_cycle"].as_f64().unwrap_or(0.0);
        let reserved = if total > 0.0 {
            per_cycle.min((total - used).max(0.0))
        } else {
            per_cycle
        };
        let lease = lease::Guard::acquire(s.clone(), id, monitor_id, cycle_id, ttl)?;
        monitor["current_cycle_id"] = json!(cycle_id);
        monitor["updated_at"] = json!(now());
        let scheduled = json!({"cycle_id":cycle_id,"task_id":id,"monitor_id":monitor_id,
            "definition_version":monitor["version"],"status":"scheduled","completeness":"unavailable",
            "scheduled_at":now(),"started_at":null,"finished_at":null,
            "source_signature":digest(&definition),"policy_digest":policy(&s,id)?["digest"],
            "source_refs":[],"source_receipts":[],"dispatch_state":"created",
            "cancel_requested":false,"cost":{"reserved":reserved,"actual":0.0,"unknown":false,"calls":0,
                "currency":definition["budget"]["currency"].as_str().unwrap_or("USD")},
            "external_state":"prepared"});
        let ledger = json!({
            "ledger_id":stable("scost_", &format!("{id}:{cycle_id}:reserved")),
            "task_id":id,"cycle_id":cycle_id,"kind":"reserved","amount":reserved,
            "unknown":false,"detail":{"budget_version":monitor["version"]},"created_at":now()
        });
        save_many(
            &s,
            id,
            vec![
                monitor_write(
                    "monitor",
                    monitor_id,
                    monitor.clone(),
                    row["revision"].as_u64(),
                ),
                monitor_write("monitor_cycle", cycle_id, scheduled, Some(0)),
                monitor_write(
                    "monitor_cost",
                    &format!("{cycle_id}:reserved"),
                    ledger,
                    Some(0),
                ),
            ],
        )?;
        lease
    } else {
        lease::Guard::acquire(s.clone(), id, monitor_id, cycle_id, ttl)?
    };
    if prior.is_none()
        || prior
            .as_ref()
            .is_some_and(|v| v["payload"]["status"] == "scheduled")
    {
        let mut cycle = rec(&s, id, "monitor_cycle", cycle_id)?["payload"].clone();
        cycle["status"] = json!("running");
        cycle["completeness"] = json!("unknown");
        cycle["started_at"] = json!(now());
        cycle["dispatch_state"] = json!("preflight");
        save_cycle(&s, id, cycle_id, cycle)?;
    }
    lease.assert_active()?;
    let repository = matches!(
        definition["source"]["connector_id"].as_str(),
        Some("github" | "gitlab")
    );
    let preflight = match if repository {
        preflight(&s, id, &definition)
    } else {
        generic::preflight(&s, id, &definition)
    } {
        Ok(value) => value,
        Err(error) => {
            lease.assert_active()?;
            fail_cycle(&s, id, monitor_id, &cycle_id, &error)?;
            return Ok(rec(&s, id, "monitor_cycle", &cycle_id)?["payload"].clone());
        }
    };
    lease.assert_active()?;
    let mut dispatched = rec(&s, id, "monitor_cycle", cycle_id)?["payload"].clone();
    dispatched["dispatch_state"] = json!("dispatched");
    save_cycle(&s, id, cycle_id, dispatched)?;
    let dispatch = async {
        if repository {
            connectors_online::repository(s.clone(), id, &json!({"provider":preflight["provider"],
            "config_version":preflight["config_version"],"idempotency_key":format!("monitor:{cycle_id}")})).await
        } else {
            connector_query::query(
                s.clone(),
                id,
                &json!({"connector_id":preflight["connector_id"],
            "config_version":preflight["config_version"],"query":preflight["query"],
            "max_pages":preflight["max_pages"],"max_items":preflight["max_items"],
            "extra":preflight["extra"],"idempotency_key":cycle_id}),
            )
            .await
        }
    };
    let result = match tokio::time::timeout(timeout, dispatch).await {
        Ok(result) => result,
        Err(_) => {
            lease.assert_fence()?;
            let error = Error::new(
                StatusCode::GATEWAY_TIMEOUT,
                "cycle_timeout",
                "monitor cycle timed out",
            );
            fail_cycle(&s, id, monitor_id, cycle_id, &error)?;
            return Ok(rec(&s, id, "monitor_cycle", cycle_id)?["payload"].clone());
        }
    };
    lease.assert_active()?;
    let batch = match result {
        Ok(value) => value,
        Err(error) => {
            fail_cycle(&s, id, monitor_id, &cycle_id, &error)?;
            return Ok(rec(&s, id, "monitor_cycle", &cycle_id)?["payload"].clone());
        }
    };
    if !matches!(
        batch["external_state"].as_str(),
        Some("completed" | "partial")
    ) {
        let mut cycle = rec(&s, id, "monitor_cycle", &cycle_id)?["payload"].clone();
        if batch["external_state"] == "waiting_approval"
            && cycle["status"] == "waiting_approval"
            && cycle["external_state"] == batch["external_state"]
            && cycle["execution_id"] == batch["execution_id"]
        {
            return Ok(cycle);
        }
        cycle["external_state"] = batch["external_state"].clone();
        cycle["approval"] = batch["approval"].clone();
        cycle["execution_id"] = batch["execution_id"].clone();
        cycle["external_receipt"] = batch["receipt"].clone();
        if batch["external_state"] == "waiting_approval" {
            cycle["status"] = json!("waiting_approval");
            cycle["dispatch_state"] = json!("waiting_approval");
            return save_cycle(&s, id, &cycle_id, cycle);
        }
        save_cycle(&s, id, &cycle_id, cycle)?;
        let code = if batch["external_state"] == "outcome_unknown" {
            "external_outcome_unknown"
        } else {
            "src.connector_upstream_failed"
        };
        let error = Error::new(
            StatusCode::BAD_GATEWAY,
            code,
            format!(
                "monitor source stage {} ended {}",
                batch["stage"], batch["external_state"]
            ),
        );
        fail_cycle(&s, id, monitor_id, &cycle_id, &error)?;
        return Ok(rec(&s, id, "monitor_cycle", &cycle_id)?["payload"].clone());
    }
    let finished = if repository {
        finish_cycle(&s, id, monitor_id, cycle_id, &batch, &preflight)
    } else {
        generic::finish(&s, id, monitor_id, cycle_id, &batch, &preflight)
    };
    match finished {
        Ok(mut cycle) => {
            if batch["external_state"] == "partial" || batch["acquisition"]["truncated"] == true {
                let err_msg = batch["error"]["message"]
                    .as_str()
                    .or_else(|| batch["errors"].as_array().and_then(|a| a.first()).and_then(|e| e["message"].as_str()))
                    .unwrap_or("上游分页截断或部分失败，已保留可用观测数据");
                cycle["partial_warning"] = json!(err_msg);
                cycle["external_state"] = json!("partial");
                cycle = save_cycle(&s, id, &cycle_id, cycle)?;
            }
            Ok(cycle)
        }
        Err(error) => {
            let current = rec(&s, id, "monitor_cycle", cycle_id)?["payload"].clone();
            if matches!(
                current["status"].as_str(),
                Some("completed" | "partial" | "failed" | "budget_exhausted" | "cancelled")
            ) {
                tracing::warn!(task_id=id, monitor_id, cycle_id, code=error.code,
                    %error.message, "SRC monitor post-commit projection failed");
                return Ok(current);
            }
            fail_cycle(&s, id, monitor_id, cycle_id, &error)?;
            Ok(rec(&s, id, "monitor_cycle", cycle_id)?["payload"].clone())
        }
    }
}

pub(super) async fn wake(s: Arc<AppState>, id: &str) -> Api<Value> {
    task(&s, id)?;
    recover(s.clone(), id).await?;
    let mut results = Vec::new();
    for row in records(&s, id, "monitor")? {
        let monitor = &row["payload"];
        if monitor["status"] != "active" {
            continue;
        }
        if monitor["blocked_reason"]
            .as_str()
            .is_some_and(|v| !v.is_empty())
        {
            continue;
        }
        let monitor_id = monitor["monitor_id"].as_str().unwrap_or("");
        let pending = monitor["current_cycle_id"]
            .as_str()
            .filter(|v| !v.is_empty());
        let due = monitor["next_wake_at"]
            .as_str()
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .is_some_and(|v| v <= Utc::now());
        if !due && pending.is_none() {
            continue;
        }
        let result = if let Some(cycle_id) = pending {
            run_cycle(
                s.clone(),
                id,
                &json!({"monitor_id":monitor_id,"cycle_id":cycle_id}),
            )
            .await
        } else {
            run_once(s.clone(), id, &json!({"monitor_id":monitor_id})).await
        };
        match result {
            Ok(value)=>results.push(value),
            Err(error)=>results.push(json!({"monitor_id":monitor_id,"status":"blocked","error_code":error.code,"message":error.message,"error":{"code":error.code,"message":error.message}})),
        }
    }
    health_result(&s,id,&json!(results),None)?;
    Ok(json!(results))
}

pub(super) async fn recover(s: Arc<AppState>, id: &str) -> Api<Value> {
    task(&s, id)?;
    let mut recovered = Vec::new();
    let mut blocked = Vec::new();
    for row in records(&s, id, "monitor_cycle")? {
        let cycle_id = row["natural_key"].as_str().unwrap_or("").to_owned();
        let monitor_id = row["payload"]["monitor_id"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if cycle_id.is_empty() || monitor_id.is_empty() {
            continue;
        }
        let monitor_lock = lock(id, &monitor_id);
        let _guard = monitor_lock.lock().await;
        let Some(current_cycle) = cycle_row(&s, id, &cycle_id)? else {
            continue;
        };
        if !matches!(
            current_cycle["payload"]["status"].as_str(),
            Some("scheduled" | "running")
        ) {
            continue;
        }
        if !lease::fence_for_recovery(&s, id, &monitor_id)? {
            continue;
        }
        let external_unknown = current_cycle["payload"]["dispatch_state"] == "dispatched";
        let mut cycle = current_cycle["payload"].clone();
        cycle["status"] = json!("failed");
        cycle["finished_at"] = json!(now());
        cycle["end_reason"] = json!(if external_unknown {
            "external_outcome_unknown"
        } else {
            "restart_interrupted"
        });
        cycle["dispatch_state"] = json!(if external_unknown {
            "outcome_unknown"
        } else {
            "finished"
        });
        cycle["external_state"] = json!(if external_unknown {
            "outcome_unknown"
        } else {
            "failed"
        });
        let monitor_row = monitor_row(&s, id, &monitor_id)?;
        let mut monitor = monitor_row["payload"].clone();
        monitor["current_cycle_id"] = Value::Null;
        monitor["last_cycle_id"] = json!(cycle_id);
        monitor["blocked_reason"] = json!(if external_unknown {
            "external_outcome_unknown"
        } else {
            ""
        });
        monitor["next_wake_at"] = if !external_unknown && monitor["status"] == "active" {
            json!(now())
        } else {
            Value::Null
        };
        let notice = cycle_notice_payload(&s, id, &monitor_id, &cycle)?;
        let mut atomic = MonitorBatch::new(&s, id);
        atomic.update(
            "monitor_cycle",
            &cycle_id,
            cycle,
            current_cycle["revision"].as_u64().unwrap_or(0),
        );
        atomic.update(
            "monitor",
            &monitor_id,
            monitor,
            monitor_row["revision"].as_u64().unwrap_or(0),
        );
        if let Some((key, payload)) = notice {
            atomic.once("monitor_notification", &key, payload)?;
        }
        atomic.commit()?;
        recovered.push(cycle_id.clone());
        if external_unknown {
            blocked.push(cycle_id);
        }
    }
    for row in records(&s, id, "monitor")? {
        let monitor_id = row["payload"]["monitor_id"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let cycle_id = row["payload"]["current_cycle_id"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if monitor_id.is_empty() || cycle_id.is_empty() || cycle_row(&s, id, &cycle_id)?.is_some() {
            continue;
        }
        let monitor_lock = lock(id, &monitor_id);
        let _guard = monitor_lock.lock().await;
        if !lease::fence_for_recovery(&s, id, &monitor_id)?
            || cycle_row(&s, id, &cycle_id)?.is_some()
        {
            continue;
        }
        let current = monitor_row(&s, id, &monitor_id)?;
        if current["payload"]["current_cycle_id"] != cycle_id {
            continue;
        }
        let mut monitor = current["payload"].clone();
        let cycle = json!({
            "cycle_id":cycle_id,"task_id":id,"monitor_id":monitor_id,
            "definition_version":monitor["version"],"status":"failed",
            "completeness":"unavailable","scheduled_at":monitor["updated_at"],
            "started_at":null,"finished_at":now(),"end_reason":"restart_interrupted",
            "dispatch_state":"finished","external_state":"failed",
            "source_receipts":[],"source_refs":[],"observation_count":0,
            "cost":{"reserved":0.0,"actual":0.0,"unknown":false,"calls":0},"error":""
        });
        monitor["current_cycle_id"] = Value::Null;
        monitor["last_cycle_id"] = json!(cycle_id);
        monitor["next_wake_at"] = if monitor["status"] == "active"
            && monitor["blocked_reason"].as_str().is_none_or(str::is_empty)
        {
            json!(now())
        } else {
            Value::Null
        };
        let notice = cycle_notice_payload(&s, id, &monitor_id, &cycle)?;
        let mut atomic = MonitorBatch::new(&s, id);
        atomic.update("monitor_cycle", &cycle_id, cycle, 0);
        atomic.update(
            "monitor",
            &monitor_id,
            monitor,
            current["revision"].as_u64().unwrap_or(0),
        );
        if let Some((key, payload)) = notice {
            atomic.once("monitor_notification", &key, payload)?;
        }
        atomic.commit()?;
        recovered.push(cycle_id);
    }
    Ok(json!({"recovered":recovered,"blocked_external_unknown":blocked}))
}

pub(super) fn scheduler_health(s: &AppState, id: &str) -> Api<Value> {
    task(s, id)?;
    let monitors = records(s, id, "monitor")?;
    let active = monitors
        .iter()
        .filter(|v| v["payload"]["status"] == "active")
        .count();
    let pending = monitors
        .iter()
        .filter(|v| v["payload"]["current_cycle_id"].is_string())
        .count();
    let mut heartbeat=s.store.get("system",&format!("src-monitor-scheduler:{id}")).map_err(crate::runs::src_api::db)?.unwrap_or(json!({"task_id":id,"state":"stopped","consecutive_failures":0,"restart_count":0,"last_error":null,"last_result":null}));
    let running=crate::runs::src_api::monitor_loop_live(s)&&heartbeat["runtime_epoch"]==s.chat.epoch()&&heartbeat["state"]!="stopped";
    heartbeat["scheduler_running"]=json!(running);if !running&&heartbeat["state"]=="running"{heartbeat["state"]=json!("stopped");}
    heartbeat["active_monitors"]=json!(active);heartbeat["unresolved_cycles"]=json!(pending);heartbeat["external_notifications"]=json!(notify_hook().is_some());Ok(heartbeat)
}

pub(super) fn changes(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let monitor_id = require(body, "monitor_id")?;
    monitor_row(s, id, monitor_id)?;
    let items = records(s, id, "monitor_change")?
        .into_iter()
        .filter(|v| v["payload"]["monitor_id"] == monitor_id)
        .map(|v| v["payload"].clone())
        .collect::<Vec<_>>();
    Ok(json!(items))
}
pub(super) fn notifications(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let monitor_id = require(body, "monitor_id")?;
    monitor_row(s, id, monitor_id)?;
    let items = records(s, id, "monitor_notification")?
        .into_iter()
        .filter(|v| v["payload"]["monitor_id"] == monitor_id)
        .map(|v| v["payload"].clone())
        .collect::<Vec<_>>();
    Ok(json!(items))
}
pub(super) fn handle_candidate(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let candidate_id = require(body, "candidate_id")?;
    let row = rec(s, id, "monitor_candidate", candidate_id)?;
    let revision = row["revision"].as_u64().unwrap_or(0);
    if body["expected_revision"].as_u64() != Some(revision) {
        return Err(Error::conflict("candidate revision is stale"));
    }
    let mut value = row["payload"].clone();
    value["status"] = json!("handled");
    value["handled_at"] = json!(now());
    value["outcome"] = body["outcome"].clone();
    save_rec(
        s,
        id,
        "monitor_candidate",
        candidate_id,
        value.clone(),
        Some(revision),
    )?;
    Ok(value)
}
fn notify_hook() -> Option<(reqwest::Url, Option<String>)> {
    let endpoint = std::env::var("MUTEKI_SRC_MONITOR_NOTIFY_URL").ok()?;
    let url = reqwest::Url::parse(endpoint.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let channel = std::env::var("MUTEKI_SRC_MONITOR_NOTIFY_CHANNEL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    Some((url, channel))
}
pub(super) async fn deliver_notifications(s: Arc<AppState>, id: &str) -> Api<Value> {
    static DELIVER_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = DELIVER_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    task(&s, id)?;
    let mut delivered = Vec::new();
    for row in records(&s, id, "monitor_notification")? {
        let mut notification = row["payload"].clone();
        if notification["status"] != "pending" || notification["channel"] != "task" {
            continue;
        }
        let notice_id = notification["notification_id"].as_str().unwrap_or("");
        let monitor_id = notification["monitor_id"].as_str().unwrap_or("");
        let _ = super::coordination::enqueue_wakeup(
            &s,
            id,
            &format!("monitor_notice:{notice_id}"),
            "monitor_change",
            &format!("监控 `{monitor_id}` 检测到有效变化"),
            notification["summary"].as_str().unwrap_or("资产或规则监控产生新变更"),
            json!({
                "notification_id": notice_id,
                "monitor_id": monitor_id,
                "change_ids": notification["change_ids"],
                "candidate_ids": notification["candidate_ids"]
            }),
        )?;
        notification["status"] = json!("sent");
        notification["delivered_at"] = json!(now());
        notification["delivery_error"] = json!("");
        let saved = save_rec(
            &s,
            id,
            "monitor_notification",
            row["natural_key"].as_str().unwrap_or(""),
            notification,
            row["revision"].as_u64(),
        )?["payload"]
            .clone();
        delivered.push(saved);
    }
    let Some((endpoint, channel)) = notify_hook() else {
        return Ok(json!(delivered));
    };
    let client = crate::network_proxy::http_client_builder().build().map_err(super::db)?;
    for row in records(&s, id, "monitor_notification")? {
        let mut notification = row["payload"].clone();
        if notification["status"] != "pending"
            || notification["channel"] != "external"
            || channel
                .as_ref()
                .is_some_and(|selected| notification["external_channel"] != *selected)
        {
            continue;
        }
        let outcome = client
            .post(endpoint.clone())
            .json(&notification)
            .send()
            .await;
        let error = match outcome {
            Ok(response) if response.status().is_success() => None,
            Ok(response) => Some(format!("HTTP {}", response.status().as_u16())),
            Err(error) => Some(if error.is_timeout() {
                "delivery timeout".to_owned()
            } else {
                "delivery request failed".to_owned()
            }),
        };
        if let Some(error) = error {
            notification["delivery_error"] = json!(error);
        } else {
            notification["status"] = json!("sent");
            notification["delivered_at"] = json!(now());
            notification["delivery_error"] = json!("");
        }
        let saved = save_rec(
            &s,
            id,
            "monitor_notification",
            row["natural_key"].as_str().unwrap_or(""),
            notification,
            row["revision"].as_u64(),
        )?["payload"]
            .clone();
        if saved["status"] == "sent" {
            delivered.push(saved);
        }
    }
    Ok(json!(delivered))
}
#[derive(Clone,Copy,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
#[serde(rename_all="snake_case")]
enum EffectOutcome { Completed, Failed, Cancelled, OutcomeUnknown }
impl EffectOutcome {
    fn known(self)->bool{self!=Self::OutcomeUnknown}
    fn from_cycle(cycle:&Value)->Self{
        let value=cycle.get("effect_outcome").unwrap_or(&cycle["external_state"]);
        serde_json::from_value(value.clone()).unwrap_or(Self::OutcomeUnknown)
    }
}
pub(super) fn reconcile_external(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let cycle_id = require(body, "cycle_id")?;
    let row = rec(s, id, "monitor_cycle", cycle_id)?;
    let mut cycle = row["payload"].clone();
    let receipt = body["receipt"]
        .as_object()
        .map(|_| body["receipt"].clone())
        .unwrap_or_else(|| json!({}));
    let requested_effect=receipt.get("effect_outcome").or_else(||receipt.get("external_state"));
    let previous_effect=EffectOutcome::from_cycle(&cycle);
    let effect=match requested_effect{
        Some(value)=>serde_json::from_value::<EffectOutcome>(value.clone()).map_err(|_|Error::invalid("effect_outcome must be completed, failed, cancelled or outcome_unknown"))?,
        None=>previous_effect,
    };
    if previous_effect.known()&&requested_effect.is_some()&&effect!=previous_effect{
        return Err(Error::conflict("a confirmed monitor effect cannot be overwritten by cost reconciliation"));
    }
    let prior_actual=cycle["cost"]["actual"].as_f64();
    let supplied_cost=receipt.get("cost");
    if supplied_cost.is_some_and(|value|!value.is_object()){return Err(Error::invalid("external cost receipt must be an object"));}
    let cost=supplied_cost.unwrap_or(&cycle["cost"]);
    let actual=cost["actual"].as_f64();
    if actual.is_some_and(|value|!value.is_finite()||value<0.) || (cost.get("actual").is_some_and(|value|!value.is_null())&&actual.is_none()){
        return Err(Error::invalid("external cost receipt is invalid"));
    }
    let known=actual.is_some()&&cost["unknown"]!=true;
    if prior_actual.is_some()&&cycle["cost"]["unknown"]!=true&&(!known||actual!=prior_actual){
        return Err(Error::conflict("a confirmed monitor cost cannot be overwritten during effect reconciliation"));
    }
    if cycle["dispatch_state"]=="reconciled"&&previous_effect.known()&&cycle["cost"]["unknown"]!=true{
        if known&&actual==prior_actual&&effect==previous_effect{return Ok(cycle);}
        return Err(Error::conflict("monitor cycle is already fully reconciled"));
    }
    let monitor_id = require(&cycle, "monitor_id")?.to_owned();
    let current = monitor_row(s, id, &monitor_id)?;
    let mut monitor = current["payload"].clone();
    let definition = cycle_definition(
        s,
        id,
        &monitor_id,
        cycle["definition_version"].as_u64().unwrap_or(0),
        &monitor,
    )?;
    if monitor["current_cycle_id"].as_str().is_some_and(|value|!value.is_empty()) || monitor["last_cycle_id"] != cycle_id {
        return Err(Error::conflict(
            "only the unresolved monitor cycle can be reconciled",
        ));
    }
    if !cycle["external_receipts"].is_array(){cycle["external_receipts"]=json!([]);}
    if !cycle["external_receipts"].as_array().unwrap().contains(&receipt){cycle["external_receipts"].as_array_mut().unwrap().push(receipt.clone());}
    cycle["external_receipt"]=receipt;
    cycle["effect_outcome"]=json!(effect);
    cycle["external_state"]=json!(effect);
    cycle["dispatch_state"]=json!(if effect.known()&&known{"reconciled"}else if effect.known(){"finished"}else{"outcome_unknown"});
    cycle["reconciled_at"]=json!(now());
    let previous_actual = if cycle["cost"]["unknown"]==true {0.0}else{prior_actual.unwrap_or(0.0)};
    let cost_used = monitor["cost_used"].as_f64().unwrap_or(0.0)
        + if known {
            actual.unwrap() - previous_actual
        } else {
            0.0
        };
    let total = definition["budget"]["total"].as_f64().unwrap_or(0.0);
    let reserved = cycle["cost"]["reserved"].as_f64().unwrap_or(0.0);
    let exhausted = known && (actual.unwrap() > reserved || (total > 0.0 && cost_used >= total));
    let blocked = if exhausted {
        "budget_exhausted"
    } else if !effect.known() {
        "external_outcome_unknown"
    } else if !known {
        "cost_unknown"
    } else {
        ""
    };
    cycle["end_reason"] = json!(if blocked.is_empty() {
        "reconciled"
    } else {
        blocked
    });
    let mut atomic = MonitorBatch::new(s, id);
    if known {
        cycle["cost"]["actual"] = json!(actual.unwrap());
        cycle["cost"]["unknown"] = json!(false);
        atomic.once(
            "monitor_cost",
            &format!("{cycle_id}:reconciled"),
            json!({
                "ledger_id":stable("scost_", &format!("{id}:{cycle_id}:reconciled")),
                "task_id":id,"cycle_id":cycle_id,"kind":"reconciled",
                "amount":actual.unwrap()-previous_actual,"unknown":false,
                "detail":{"receipt":cycle["external_receipt"]},"created_at":now()
            }),
        )?;
    }
    if exhausted {
        cycle["status"] = json!("budget_exhausted");
    }
    monitor["current_cycle_id"] = Value::Null;
    monitor["last_cycle_id"] = json!(cycle_id);
    monitor["cost_unknown"] = json!(!known);
    monitor["cost_used"] = json!(cost_used);
    monitor["blocked_reason"] = json!(blocked);
    monitor["activation_reason"] = if blocked.is_empty() {
        Value::Null
    } else {
        json!(blocked)
    };
    monitor["next_wake_at"] = if blocked.is_empty() && monitor["status"] == "active" {
        json!(now())
    } else {
        Value::Null
    };
    atomic.update(
        "monitor",
        &monitor_id,
        monitor,
        current["revision"].as_u64().unwrap_or(0),
    );
    atomic.update(
        "monitor_cycle",
        cycle_id,
        cycle.clone(),
        row["revision"].as_u64().unwrap_or(0),
    );
    atomic.commit()?;
    Ok(cycle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repository_projection_conflict_keeps_the_whole_batch_unwritten() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = crate::runs::src_api::create_task_inner(
            &state,
            &json!({
            "title":"Atomic monitor","goal":"Observe repository",
            "idempotency_key":"monitor-atomic-repository-test"}),
        )
        .await
        .unwrap();
        let task_id = created["output"]["task_id"].as_str().unwrap();
        let monitor = super::super::create_monitor(&state, task_id, &json!({"payload":{
            "name":"Repository","continuous":true,"chain_type":"asset",
            "source":{"connector_id":"cli","config_version":1,"capability_manifest_ref":"source-file.v1"},
            "authorization_ref":format!("src-policy-{task_id}:v1"),
            "schedule":{"interval_seconds":60},"budget":{"total":10,"per_cycle":1}}})).unwrap();
        let monitor_id = monitor["monitor_id"].as_str().unwrap().to_owned();
        let mut atomic = MonitorBatch::new(&state, task_id);
        atomic
            .once(
                "monitor_baseline",
                "baseline-stale",
                json!({
                    "baseline_id":"baseline-stale","monitor_id":monitor_id,"cycle_id":"cycle-stale"
                }),
            )
            .unwrap();
        atomic
            .once(
                "monitor_change",
                "change-stale",
                json!({
                    "change_id":"change-stale","monitor_id":monitor_id,"cycle_id":"cycle-stale"
                }),
            )
            .unwrap();
        atomic.once("monitor_candidate", "candidate-stale", json!({
            "candidate_id":"candidate-stale","monitor_id":monitor_id,"cycle_id":"cycle-stale"
        })).unwrap();
        atomic.update("monitor", &monitor_id, monitor, 0);
        atomic.update(
            "monitor_cycle",
            "cycle-stale",
            json!({
                "cycle_id":"cycle-stale","monitor_id":monitor_id,"status":"completed"
            }),
            0,
        );
        assert_eq!(atomic.commit().unwrap_err().status, StatusCode::CONFLICT);
        for (kind, key) in [
            ("monitor_baseline", "baseline-stale"),
            ("monitor_change", "change-stale"),
            ("monitor_candidate", "candidate-stale"),
            ("monitor_cycle", "cycle-stale"),
        ] {
            assert_eq!(
                rec(&state, task_id, kind, key).unwrap_err().status,
                StatusCode::NOT_FOUND
            );
        }
    }

    #[tokio::test]
    async fn monitor_revision_keeps_the_cycle_definition_version() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = crate::runs::src_api::create_task_inner(
            &state,
            &json!({
            "title":"Monitor versions","goal":"Observe source changes",
            "idempotency_key":"monitor-version-test"}),
        )
        .await
        .unwrap();
        let task_id = created["output"]["task_id"].as_str().unwrap();
        let monitor = super::super::create_monitor(&state, task_id, &json!({"payload":{
            "name":"Versioned monitor","continuous":true,"chain_type":"asset",
            "source":{"connector_id":"cli","config_version":1,"capability_manifest_ref":"source-file.v1"},
            "authorization_ref":format!("src-policy-{task_id}:v1"),
            "schedule":{"interval_seconds":60},"budget":{"total":10,"per_cycle":1}}})).unwrap();
        let monitor_id = monitor["monitor_id"].as_str().unwrap();
        let revised = super::super::revise_monitor(
            &state,
            task_id,
            &json!({
            "monitor_id":monitor_id,"expected_revision":1,
            "payload":{"schedule":{"interval_seconds":120}}}),
        )
        .unwrap();
        assert_eq!(revised["version"], 2);
        assert_eq!(
            cycle_definition(&state, task_id, monitor_id, 1, &revised).unwrap()["schedule"]["interval_seconds"],
            60
        );
        assert_eq!(
            cycle_definition(&state, task_id, monitor_id, 2, &revised).unwrap()["schedule"]["interval_seconds"],
            120
        );
    }

    #[tokio::test]
    async fn dispatched_cycle_recovery_blocks_replay_until_reconciliation() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = crate::runs::src_api::create_task_inner(
            &state,
            &json!({"title":"Monitor recovery","goal":"Observe the target",
                "idempotency_key":"monitor-recovery-test"}),
        )
        .await
        .unwrap();
        let task_id = created["output"]["task_id"].as_str().unwrap();
        let monitor = super::super::create_monitor(
            &state,
            task_id,
            &json!({"payload":{"name":"Repository","continuous":true,"chain_type":"asset",
                "source":{"connector_id":"cli","config_version":1,"capability_manifest_ref":"source-file.v1"},
                "authorization_ref":format!("src-policy-{task_id}:v1"),
                "schedule":{"interval_seconds":60},"budget":{"total":10,"per_cycle":1}}}),
        )
        .unwrap();
        let monitor_id = monitor["monitor_id"].as_str().unwrap();
        let cycle_id = "smcycle_recovery_test";
        let row = monitor_row(&state, task_id, monitor_id).unwrap();
        let mut active = row["payload"].clone();
        active["current_cycle_id"] = json!(cycle_id);
        save_rec(
            &state,
            task_id,
            "monitor",
            monitor_id,
            active,
            row["revision"].as_u64(),
        )
        .unwrap();
        save_cycle(
            &state,
            task_id,
            cycle_id,
            json!({
                "cycle_id":cycle_id,"task_id":task_id,"monitor_id":monitor_id,
                "status":"running","dispatch_state":"dispatched",
                "cost":{"reserved":1.0,"actual":0.0,"unknown":false}
            }),
        )
        .unwrap();
        let result = recover(state.clone(), task_id).await.unwrap();
        assert_eq!(result["blocked_external_unknown"], json!([cycle_id]));
        let cycle = rec(&state, task_id, "monitor_cycle", cycle_id).unwrap();
        assert_eq!(cycle["payload"]["status"], "failed");
        assert_eq!(cycle["payload"]["end_reason"], "external_outcome_unknown");
        let current = monitor_row(&state, task_id, monitor_id).unwrap();
        assert_eq!(
            current["payload"]["blocked_reason"],
            "external_outcome_unknown"
        );
        assert!(current["payload"]["current_cycle_id"].is_null());
        assert_eq!(
            run_once(state, task_id, &json!({"monitor_id":monitor_id}))
                .await
                .unwrap_err()
                .status,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn cancelling_an_inflight_cycle_preserves_the_user_state() {
        let state = Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let created = crate::runs::src_api::create_task_inner(
            &state,
            &json!({"title":"Monitor cancellation","goal":"Observe the target",
                "idempotency_key":"monitor-cancel-test"}),
        )
        .await
        .unwrap();
        let task_id = created["output"]["task_id"].as_str().unwrap();
        let monitor = super::super::create_monitor(
            &state,
            task_id,
            &json!({"payload":{"name":"Repository","continuous":true,"chain_type":"asset",
                "source":{"connector_id":"cli","config_version":1,"capability_manifest_ref":"source-file.v1"},
                "authorization_ref":format!("src-policy-{task_id}:v1"),
                "schedule":{"interval_seconds":60},"budget":{"total":10,"per_cycle":1}}}),
        )
        .unwrap();
        let monitor_id = monitor["monitor_id"].as_str().unwrap();
        let cycle_id = "smcycle_cancel_test";
        let row = monitor_row(&state, task_id, monitor_id).unwrap();
        let mut active = row["payload"].clone();
        active["current_cycle_id"] = json!(cycle_id);
        save_rec(
            &state,
            task_id,
            "monitor",
            monitor_id,
            active,
            row["revision"].as_u64(),
        )
        .unwrap();
        save_cycle(
            &state,
            task_id,
            cycle_id,
            json!({"cycle_id":cycle_id,"monitor_id":monitor_id,"status":"running",
                "cost":{"reserved":1.0,"actual":0.0,"unknown":false,"calls":0}}),
        )
        .unwrap();
        let revision = monitor_row(&state, task_id, monitor_id).unwrap()["revision"]
            .as_u64()
            .unwrap();
        super::super::monitor_state(
            &state,
            task_id,
            &json!({"monitor_id":monitor_id,"state":"cancelled","expected_revision":revision}),
        )
        .unwrap();
        assert_eq!(
            rec(&state, task_id, "monitor_cycle", cycle_id).unwrap()["payload"]["cancel_requested"],
            true
        );
        let cycle = finish_cancelled(
            &state,
            task_id,
            monitor_id,
            cycle_id,
            &json!({"source_refs":[]}),
        )
        .unwrap();
        assert_eq!(cycle["status"], "cancelled");
        let current = monitor_row(&state, task_id, monitor_id).unwrap();
        assert_eq!(current["payload"]["status"], "cancelled");
        assert!(current["payload"]["current_cycle_id"].is_null());
        assert!(current["payload"]["next_wake_at"].is_null());
    }
}

pub(super) fn health_result(s:&AppState,id:&str,result:&Value,error:Option<Value>)->Api<()>{
    let key=format!("src-monitor-scheduler:{id}");
    for _ in 0..32{let before=s.store.get("system",&key).map_err(crate::runs::src_api::db)?;let mut next=before.clone().unwrap_or(json!({"task_id":id,"consecutive_failures":0,"restart_count":0,"last_error":null,"last_success_at":null}));
        if next["runtime_epoch"]!=s.chat.epoch(){next["restart_count"]=json!(next["restart_count"].as_u64().unwrap_or(0)+1);}
        let failures=error.clone().or_else(||{let errors=result.as_array().into_iter().flatten().filter_map(|row|row.get("error").filter(|value|!value.is_null()).cloned()).collect::<Vec<_>>();if errors.is_empty(){None}else{Some(json!(errors))}});
        next["runtime_epoch"]=json!(s.chat.epoch());next["last_tick_at"]=json!(now());next["updated_at"]=json!(now());next["last_result"]=result.clone();
        if let Some(error)=failures{next["state"]=json!("degraded");next["consecutive_failures"]=json!(next["consecutive_failures"].as_u64().unwrap_or(0)+1);next["last_failure_at"]=json!(now());next["last_error"]=error;}
        else{if !next["last_error"].is_null(){next["previous_error"]=next["last_error"].clone();}next["state"]=json!("running");next["last_error"]=Value::Null;next["consecutive_failures"]=json!(0);next["last_success_at"]=json!(now());}
        if s.store.put_many_if(&[("system",&key,before.as_ref())],&[("system",&key,&next)]).map_err(crate::runs::src_api::db)?{return Ok(());}
    }Err(Error::conflict("monitor health changed repeatedly"))
}
