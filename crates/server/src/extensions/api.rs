use super::{ExtError, Result, capabilities, manifest, runtime::Process, source};
use crate::{AppState, native_chat};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};

const RECORDS: &str = "task_extensions";
const PREVIEWS: &str = "task_extension_previews";
fn root(s: &AppState) -> PathBuf {
    native_chat::state_dir(s).join("extensions")
}
fn install_root(s: &AppState) -> PathBuf {
    root(s).join("installed")
}
fn state_root(s: &AppState) -> PathBuf {
    root(s).join("state")
}
fn private_state(s: &AppState, id: &str, version: &str) -> PathBuf {
    state_root(s).join(id).join("state").join(version)
}
fn record(s: &AppState, id: &str) -> Result<Value> {
    s.store
        .get(RECORDS, id)
        .map_err(|e| ExtError::runtime("extension.store_failed", e.to_string()))?
        .ok_or_else(|| ExtError::not_found(id))
}
fn save(s: &AppState, id: &str, mut row: Value) -> Result<Value> {
    row["updated_at"] = json!(chrono::Utc::now().to_rfc3339());
    s.store
        .put(RECORDS, id, &row)
        .map_err(|e| ExtError::runtime("extension.store_failed", e.to_string()))?;
    Ok(row)
}
fn records(s: &AppState) -> Result<Vec<Value>> {
    s.store
        .list(RECORDS)
        .map_err(|e| ExtError::runtime("extension.store_failed", e.to_string()))
}
fn preview_record(s: &AppState, id: &str) -> Result<Value> {
    s.store
        .get(PREVIEWS, id)
        .map_err(|e| ExtError::runtime("extension.store_failed", e.to_string()))?
        .ok_or_else(|| {
            ExtError::validation("extension.preview_not_found", "install preview not found")
        })
}
fn active_version(row: &Value) -> Result<&str> {
    row["active_version"]
        .as_str()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            row["installed_versions"]
                .as_array()
                .and_then(|a| a.last())
                .and_then(Value::as_str)
        })
        .ok_or_else(|| ExtError::state("extension.version_missing", "no installed version"))
}
fn installed_dir(row: &Value, version: &str) -> Result<PathBuf> {
    row["install_dirs"][version]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            ExtError::state(
                "extension.version_missing",
                format!("version {version} not installed"),
            )
        })
}
fn manifest_of(row: &Value, version: &str) -> Result<Value> {
    manifest::load(&installed_dir(row, version)?)
}
fn verification_of<'a>(row: &'a Value, version: &str) -> &'a Value {
    &row["installations"][version]["verification"]
}
fn load_snapshot(row: &Value, version: &str) -> Result<(PathBuf, Value)> {
    let dir = installed_dir(row, version)?;
    if !source::matches_snapshot(&dir, verification_of(row, version))? {
        return Err(ExtError::state(
            "extension.snapshot_invalid",
            "installed package bytes differ from immutable snapshot",
        ));
    }
    let parsed = manifest::load(&dir)?;
    Ok((dir, parsed))
}
fn policy(s: &AppState) -> Value {
    s.store
        .get("settings", "extension_install_policy")
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({}))
}
fn enforce_policy(s: &AppState, m: &Value, v: &Value) -> Result<()> {
    let p = policy(s);
    let id = m["id"].as_str().unwrap_or("");
    let publisher = v["publisher"].as_str().unwrap_or("unknown");
    if p["denied_extensions"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v == id))
    {
        return Err(ExtError::validation(
            "extension.policy_extension_denied",
            "extension denied by policy",
        ));
    }
    if p["denied_publishers"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v == publisher))
    {
        return Err(ExtError::validation(
            "extension.policy_publisher_denied",
            "publisher denied by policy",
        ));
    }
    if p["deny_untrusted_publisher"] == true && v["trust_status"] != "verified" {
        return Err(ExtError::validation(
            "extension.policy_untrusted_publisher_denied",
            "verified publisher required",
        ));
    }
    if p["deny_unsigned"] == true && v["signature"] == "unsigned" {
        return Err(ExtError::validation(
            "extension.policy_unsigned_denied",
            "signed extension required",
        ));
    }
    if p["deny_mutable_source"] == true && v["immutable"] != true {
        return Err(ExtError::validation(
            "extension.policy_mutable_source_denied",
            "immutable source required",
        ));
    }
    if p["deny_high_permissions"] == true
        && m["permissions"]["filesystem"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == "workspace-write"))
    {
        return Err(ExtError::validation(
            "extension.policy_permission_denied",
            "workspace-write denied by policy",
        ));
    }
    Ok(())
}
fn permission_expansion(prior: &Value, next: &Value) -> Vec<String> {
    let mut added = Vec::new();
    for field in ["filesystem", "network", "secrets", "events_write"] {
        let old: HashSet<_> = prior["permissions"][field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        for value in next["permissions"][field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !old.contains(value) {
                added.push(format!("{field}:{value}"));
            }
        }
    }
    added.sort();
    added
}
fn changes(s: &AppState, m: &Value, v: &Value) -> Value {
    let id = m["id"].as_str().unwrap_or("");
    let Ok(prior) = record(s, id) else {
        return json!({"source_changed":false,"publisher_changed":false,"permission_expansion":[],"ui_changed":false,"dependencies_changed":false});
    };
    let Ok(version) = active_version(&prior) else {
        return json!({});
    };
    let old = manifest_of(&prior, version).unwrap_or_else(|_| json!({}));
    let ov = verification_of(&prior, version);
    json!({"source_changed":ov["source"]!=v["source"],"publisher_changed":ov["publisher"]!=v["publisher"],"permission_expansion":permission_expansion(&old,m),"ui_changed":old["ui"]!=m["ui"],"dependencies_changed":old["requires"]!=m["requires"]})
}
fn body_value(body: Option<Json<Value>>) -> Value {
    body.map(|Json(value)| value).unwrap_or_else(|| json!({}))
}
fn source_body(body: &Value) -> Result<&Value> {
    body.get("source").filter(|v| v.is_object()).ok_or_else(|| {
        ExtError::validation("extension.invalid_source", "source object is required")
    })
}

async fn preview_impl(s: &AppState, body: &Value) -> Result<Value> {
    let input = source_body(body)?;
    let temp = tempfile::tempdir()
        .map_err(|e| ExtError::runtime("extension.preview_failed", e.to_string()))?;
    let (package, verification) = source::materialize(input, temp.path()).await?;
    let m = manifest::load(&package)?;
    if body["extension_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty() && m["id"] != id)
    {
        return Err(ExtError::validation(
            "extension.preview_id_mismatch",
            "source extension id differs from requested id",
        ));
    }
    enforce_policy(s, &m, &verification)?;
    let changes = changes(s, &m, &verification);
    let mut required = Vec::new();
    if verification["signature"] == "unsigned" {
        required.push("unsigned");
    }
    if changes["source_changed"] == true {
        required.push("source_change");
    }
    if changes["publisher_changed"] == true {
        required.push("publisher_change");
    }
    if changes["permission_expansion"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
    {
        required.push("permission_expansion");
    }
    let id = format!("ext-preview-{}", uuid::Uuid::new_v4().simple());
    let now = chrono::Utc::now();
    let preview = json!({"preview_id":id,"extension_id":m["id"],"version":m["version"],"source":input,"manifest":m,"verification":verification,"changes":changes,"confirmations_required":required,"created_at":now.to_rfc3339(),"expires_at":(now+chrono::Duration::minutes(30)).to_rfc3339(),"state":"pending_activation"});
    s.store
        .put(PREVIEWS, &id, &preview)
        .map_err(|e| ExtError::runtime("extension.store_failed", e.to_string()))?;
    Ok(preview)
}

async fn install_impl(s: &AppState, body: &Value, expected_id: Option<&str>) -> Result<Value> {
    let pid = body["preview_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            ExtError::validation(
                "extension.preview_required",
                "create and confirm an install preview first",
            )
        })?;
    let preview = preview_record(s, pid)?;
    let expires =
        chrono::DateTime::parse_from_rfc3339(preview["expires_at"].as_str().unwrap_or(""))
            .map_err(|_| {
                ExtError::validation("extension.preview_expired", "preview expiry invalid")
            })?;
    if expires < chrono::Utc::now() {
        return Err(ExtError::validation(
            "extension.preview_expired",
            "install preview expired",
        ));
    }
    let input = source_body(body)?;
    if *input != preview["source"] {
        return Err(ExtError::validation(
            "extension.preview_source_changed",
            "source differs from preview",
        ));
    }
    let confirmations: HashSet<_> = body["confirmations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let missing: Vec<_> = preview["confirmations_required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|v| !confirmations.contains(v))
        .collect();
    if !missing.is_empty() {
        return Err(ExtError::validation(
            "extension.confirmation_required",
            format!("missing confirmations: {}", missing.join(", ")),
        ));
    }
    let temp = tempfile::tempdir()
        .map_err(|e| ExtError::runtime("extension.install_failed", e.to_string()))?;
    let (package, mut v) = source::materialize(input, temp.path()).await?;
    let m = manifest::load(&package)?;
    let id = m["id"].as_str().unwrap_or("");
    let version = m["version"].as_str().unwrap_or("");
    if expected_id.is_some_and(|e| e != id)
        || preview["extension_id"] != id
        || preview["version"] != version
        || !source::matches_snapshot(&package, &preview["verification"])?
    {
        return Err(ExtError::validation(
            "extension.preview_source_changed",
            "package changed after preview",
        ));
    }
    if let Some(id) = preview["verification"]["snapshot_id"].as_str() { v["snapshot_id"] = json!(id); }
    enforce_policy(s, &m, &v)?;
    if s.store.get(RECORDS,id).map_err(|error|ExtError::runtime("extension.store_failed",error.to_string()))?
        .is_some_and(|row|row["state"] == "uninstalling") {
        return Err(ExtError::state("extension.uninstall_pending","Finish the pending uninstall before installing this extension"));
    }
    let target = install_root(s).join(id).join(version);
    if target.exists() {
        return Err(ExtError::state(
            "extension.already_installed",
            format!("{id}@{version} already installed"),
        ));
    }
    fs::create_dir_all(target.parent().unwrap())
        .map_err(|e| ExtError::runtime("extension.install_failed", e.to_string()))?;
    let staging = target.parent().unwrap().join(format!(
        ".{version}.staging-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let committed = (|| -> Result<()> {
        source::copy_tree(&package, &staging)?;
        if !source::matches_snapshot(&staging, &v)? {
            return Err(ExtError::state(
                "extension.snapshot_invalid",
                "staging digest differs from preview",
            ));
        }
        fs::rename(&staging, &target)
            .map_err(|e| ExtError::runtime("extension.install_failed", e.to_string()))?;
        Ok(())
    })();
    if committed.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    committed?;
    let now = chrono::Utc::now().to_rfc3339();
    let mut row=record(s,id).unwrap_or_else(|_|json!({"extension_id":id,"origin":m["origin"],"state":"installed","enabled":false,"active_version":"","installed_versions":[],"install_dirs":{},"config":{},"health":"","last_error":"","capabilities":{},"installations":{},"registry_entries":[],"isolation":{"backend":"none","enforcement":"unavailable"},"secret_usage":[],"start_count":0,"last_exit_code":Value::Null,"last_health_at":Value::Null,"restart_backoff_seconds":0,"manual_stop_reason":"","created_at":now,"updated_at":now}));
    let mut versions: Vec<String> = row["installed_versions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    versions.push(version.to_owned());
    versions.sort_by(|a, b| manifest::version_cmp(a, b));
    versions.dedup();
    row["installed_versions"] = json!(versions);
    row["install_dirs"][version] = json!(target.to_string_lossy());
    row["installations"][version] = json!({"source_kind":input["kind"],"verification":v,"installed_at":now,"phases":v["phases"],"preview_id":pid,"confirmations":body["confirmations"]});
    if row["active_version"].as_str().is_none_or(|v| v.is_empty()) {
        row["active_version"] = json!(version);
    }
    let saved = save(s, id, row)?;
    let _ = s.store.delete(PREVIEWS, pid);
    Ok(saved)
}

fn dependencies(s: &AppState, m: &Value) -> Result<()> {
    let all = records(s)?;
    for req in m["requires"].as_array().into_iter().flatten() {
        let cap = req["capability"].as_str().unwrap_or("");
        let version = req["version"].as_u64().unwrap_or(1);
        let found =
            all.iter()
                .filter(|row| row["enabled"] == true && row["state"] == "ready")
                .any(|row| {
                    let Ok(active) = active_version(row) else {
                        return false;
                    };
                    let Ok(other) = manifest_of(row, active) else {
                        return false;
                    };
                    other["provides"].as_array().into_iter().flatten().any(|p| {
                        p["id"] == cap && p["api_version"].as_u64().unwrap_or(1) == version
                    })
                });
        if !found {
            return Err(ExtError::state(
                "extension.missing_capability",
                format!("required capability {cap}@{version} unavailable"),
            ));
        }
    }
    Ok(())
}

fn activation_request(row: &Value) -> Value {
    let pending = &row["activation"];
    if pending["phase"] == "prepared" && pending["previous_enabled"] == false
        && pending["version"].as_str().is_some_and(|version|!version.is_empty()) {
        json!({"version":pending["version"],"config":pending["config"]})
    } else {
        json!({"version":active_version(row).unwrap_or(""),"config":row["config"]})
    }
}

async fn enable_impl(s: &Arc<AppState>, id: &str, body: &Value) -> Result<Value> {
    let previous = record(s,id)?;
    let target = body["version"].as_str().filter(|value| !value.is_empty()).map(str::to_owned)
        .or_else(|| previous["installed_versions"].as_array().and_then(|items| items.last()).and_then(Value::as_str).map(str::to_owned))
        .ok_or_else(|| ExtError::state("extension.version_missing","No installed extension version"))?;
    let mut request = body.clone();
    request["version"] = json!(target);
    match enable_once(s,id,&request).await {
        Ok(row) => Ok(row),
        Err(error) => {
            let old = previous["active_version"].as_str().unwrap_or("");
            if s.extension_host.get(id).await.is_some() { return Err(error); }
            if body["_allow_rollback"] != false && previous["enabled"] == true && !old.is_empty() {
                if old != target { migrate_state(s,id,&target,old)?; }
                match enable_once(s,id,&json!({"version":old,"config":previous["config"]})).await {
                    Ok(mut row) => {
                        row["last_auto_rollback_from"] = json!(target);
                        row["last_activation_rollback"] = json!({"requested_version":target,"error_code":error.code,
                            "message":error.message,"restored_at":chrono::Utc::now().to_rfc3339()});
                        return save(s,id,row);
                    },
                    Err(rollback) => return Err(ExtError::runtime("extension.rollback_failed",
                        format!("Activation failed: {}; rollback failed: {}",error.message,rollback.message))),
                }
            }
            Err(error)
        }
    }
}

async fn enable_once(s: &Arc<AppState>, id: &str, body: &Value) -> Result<Value> {
    let mut row = record(s, id)?;
    if row["state"] == "uninstalling" {
        return Err(ExtError::state("extension.uninstall_pending","Finish the pending uninstall before enabling this extension"));
    }
    let version = body["version"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| row["installed_versions"].as_array().and_then(|versions| versions.last()).and_then(Value::as_str).unwrap_or("").to_owned());
    let (dir, m) = load_snapshot(&row, &version)?;
    dependencies(s, &m)?;
    let config = body
        .get("config")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| row["config"].clone());
    if let Some(schema) = m["config_schema"].as_str() {
        manifest::validate_schema(&config, &manifest::load_schema(&dir, schema)?, "config")?;
    }
    let activation = uuid::Uuid::new_v4().to_string();
    let previous_enabled = row["enabled"] == true;
    row["enabled"] = json!(true);
    row["state"] = json!("activating");
    row["activation"] = json!({"id":activation,"phase":"prepared","version":version,"config":config,
        "previous_enabled":previous_enabled,"owner_epoch":s.chat.epoch(),"requested_at":chrono::Utc::now().to_rfc3339()});
    capabilities::set_entry_state(&mut row, "unavailable", "extension activation is pending");
    row = save(s, id, row)?;
    if let Some(process) = s.extension_host.remove(id).await {
        process.lock().await.stop().await?;
    }
    let state_dir = private_state(s, id, &version);
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    row["start_count"] = json!(row["start_count"].as_u64().unwrap_or(0) + 1);
    match Process::start(s, &m, &dir, &state_dir, &workspace, &config).await {
        Ok((process, health)) => {
            let capabilities = process.capabilities.clone();
            let isolation = process.isolation.clone();
            let secrets = process.secret_usage.clone();
            row["enabled"] = json!(true);
            row["state"] = json!("ready");
            row["active_version"] = json!(version);
            row["config"] = config;
            row["health"] = health["status"].clone();
            row["last_error"] = json!("");
            row["last_error_code"] = json!("");
            row["capabilities"] = capabilities;
            row["isolation"] = isolation;
            row["secret_usage"] = json!(secrets);
            row["last_health_at"] = json!(chrono::Utc::now().to_rfc3339());
            row["manual_stop_reason"] = json!("");
            let entries = capabilities::registered_entries(s, id, &version, &m, &row["capabilities"]);
            row["registry_entries"] = json!(entries);
            row["activation"]["phase"] = json!("committed");
            row["activation"]["committed_at"] = json!(chrono::Utc::now().to_rfc3339());
            s.extension_host.activate(id.to_owned(), process, || save(s, id, row)).await
        }
        Err(err) => {
            row["enabled"] = json!(false);
            row["state"] = json!("unavailable");
            row["health"] = json!("unhealthy");
            row["last_error"] = json!(err.message.clone());
            row["last_error_code"] = json!(err.code.clone());
            row["activation"]["phase"] = json!("failed");
            row["activation"]["error_code"] = json!(err.code.clone());
            capabilities::set_entry_state(&mut row, "unavailable", "extension process could not start");
            save(s, id, row)?;
            Err(err)
        }
    }
}

async fn disable_impl(s: &AppState, id: &str) -> Result<Value> {
    let mut row = record(s, id)?;
    row["enabled"] = json!(false);
    row["state"] = json!("stopping");
    row["activation"] = json!({"id":uuid::Uuid::new_v4().to_string(),"phase":"stopping","owner_epoch":s.chat.epoch()});
    capabilities::set_entry_state(&mut row, "disabled", "extension stop is pending");
    row = save(s, id, row)?;
    if let Some(process) = s.extension_host.remove(id).await {
        row["last_exit_code"] = json!(process.lock().await.stop().await?);
    }
    row["activation"]["phase"] = json!("stopped");
    row["enabled"] = json!(false);
    row["state"] = json!("disabled");
    row["health"] = json!("unavailable");
    row["manual_stop_reason"] = json!("operator_disabled");
    row["secret_usage"] = json!([]);
    capabilities::set_entry_state(&mut row, "disabled", "extension is disabled");
    save(s, id, row)
}

pub(super) async fn handle_secret_change(state: &Arc<AppState>, old_reference: &str,
    new_reference: &str) -> std::result::Result<Value, String> {
    let mut restarted = Vec::new();
    let mut stopped = Vec::new();
    if old_reference.is_empty() { return Ok(json!({"restarted":restarted,"stopped":stopped})); }
    let _guard = state.extension_host.lock_lifecycle().await;
    for row in records(state).map_err(|error| error.message)?.into_iter().filter(|row| row["enabled"] == true) {
        let Some(id) = row["extension_id"].as_str() else { continue };
        let version = active_version(&row).map_err(|error| error.message)?.to_owned();
        let manifest = manifest_of(&row, &version).map_err(|error| error.message)?;
        if !manifest["permissions"]["secrets"].as_array().is_some_and(|refs| refs.iter().any(|reference| reference == old_reference)) { continue; }
        let config = row["config"].clone();
        let mut disabled = disable_impl(state, id).await.map_err(|error| error.message)?;
        disabled["manual_stop_reason"] = json!("secret_rotated");
        save(state, id, disabled).map_err(|error| error.message)?;
        if new_reference == old_reference {
            enable_impl(state, id, &json!({"version":version,"config":config})).await.map_err(|error| error.message)?;
            restarted.push(id.to_owned());
        } else { stopped.push(id.to_owned()); }
    }
    Ok(json!({"restarted":restarted,"stopped":stopped}))
}

fn migrate_state(s: &AppState, id: &str, from: &str, to: &str) -> Result<Value> {
    let source = private_state(s, id, from);
    let target = private_state(s, id, to);
    let mut files = Vec::new();
    if source.is_dir() {
        for entry in walkdir::WalkDir::new(&source)
            .min_depth(1)
            .follow_links(false)
        {
            let entry = entry
                .map_err(|e| ExtError::runtime("extension.migration_failed", e.to_string()))?;
            let rel = entry
                .path()
                .strip_prefix(&source)
                .map_err(|e| ExtError::runtime("extension.migration_failed", e.to_string()))?;
            if rel.components().any(|c| c.as_os_str() == "logs")
                || rel
                    .file_name()
                    .is_some_and(|name| name == ".extension-sandbox.sb")
                || entry.file_type().is_symlink()
            {
                continue;
            }
            let dest = target.join(rel);
            if entry.file_type().is_dir() {
                fs::create_dir_all(dest)
                    .map_err(|e| ExtError::runtime("extension.migration_failed", e.to_string()))?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        ExtError::runtime("extension.migration_failed", e.to_string())
                    })?;
                }
                fs::copy(entry.path(), dest)
                    .map_err(|e| ExtError::runtime("extension.migration_failed", e.to_string()))?;
                files.push(rel.to_string_lossy().into_owned());
            }
        }
    } else {
        fs::create_dir_all(&target)
            .map_err(|e| ExtError::runtime("extension.migration_failed", e.to_string()))?;
    }
    let receipt = json!({"extension_id":id,"from_version":if from.is_empty(){Value::Null}else{json!(from)},"to_version":to,"migrated_at":chrono::Utc::now().to_rfc3339(),"files":files});
    s.store
        .put(
            "task_extension_migrations",
            &format!("{id}:{from}:{to}"),
            &receipt,
        )
        .map_err(|e| ExtError::runtime("extension.store_failed", e.to_string()))?;
    Ok(receipt)
}
fn migration_receipts(s: &AppState, id: &str) -> Vec<Value> {
    s.store
        .list("task_extension_migrations")
        .unwrap_or_default()
        .into_iter()
        .filter(|v| v["extension_id"] == id)
        .collect()
}

pub(super) async fn initialize(s: &Arc<AppState>) -> Result<()> {
    recover_uninstalls(s)?;
    let _guard = s.extension_host.lock_lifecycle().await;
    let desired: Vec<Value> = records(s)?
        .into_iter()
        .filter(|v| v["enabled"] == true)
        .collect();
    let mut manifests = std::collections::BTreeMap::<String, Value>::new();
    let mut rows_by_id = std::collections::BTreeMap::<String, Value>::new();
    let mut failed = std::collections::BTreeMap::<String, String>::new();
    for row in desired {
        let Some(id) = row["extension_id"].as_str().map(str::to_owned) else {
            continue;
        };
        rows_by_id.insert(id.clone(), row.clone());
        let request = activation_request(&row);
        let version = request["version"].as_str().unwrap_or("");
        match manifest_of(&row, version) {
            Ok(m) => {
                manifests.insert(id, m);
            }
            Err(err) => {
                failed.insert(id, err.message);
            }
        }
    }
    let mut providers = HashMap::<String, String>::new();
    for (id, m) in &manifests {
        for provide in m["provides"].as_array().into_iter().flatten() {
            if let Some(cap) = provide["id"].as_str() {
                providers.entry(cap.to_owned()).or_insert_with(|| id.clone());
            }
        }
    }
    let mut deps_map = std::collections::BTreeMap::<String, HashSet<String>>::new();
    for (id, m) in &manifests {
        let mut deps = HashSet::new();
        for req in m["requires"].as_array().into_iter().flatten() {
            let cap = req["capability"].as_str().unwrap_or("");
            if let Some(provider) = providers.get(cap) {
                if provider != id {
                    deps.insert(provider.clone());
                }
            } else {
                failed
                    .entry(id.clone())
                    .or_insert_with(|| format!("missing capability: {cap}"));
            }
        }
        deps_map.insert(id.clone(), deps);
    }
    let mut remaining: std::collections::BTreeSet<String> = manifests
        .keys()
        .filter(|id| !failed.contains_key(*id))
        .cloned()
        .collect();
    let mut order = Vec::new();
    while !remaining.is_empty() {
        let ready: Vec<String> = remaining
            .iter()
            .filter(|id| {
                deps_map
                    .get(*id)
                    .is_none_or(|deps| deps.iter().all(|dep| !remaining.contains(dep)))
            })
            .cloned()
            .collect();
        if ready.is_empty() {
            for id in &remaining {
                failed.insert(id.clone(), "cyclic extension dependency".to_owned());
            }
            break;
        }
        for id in ready {
            remaining.remove(&id);
            order.push(id);
        }
    }
    for id in order {
        if let Some(deps) = deps_map.get(&id) {
            let blocked: Vec<_> = deps.iter().filter(|d| failed.contains_key(*d)).cloned().collect();
            if !blocked.is_empty() {
                failed.insert(id, format!("dependency unavailable: {}", blocked.join(", ")));
                continue;
            }
        }
        if s.extension_host.get(&id).await.is_some() {
            continue;
        }
        if let Some(row) = rows_by_id.get(&id) {
            let request = activation_request(row);
            if let Err(err) = enable_impl(s, &id, &request).await {
                failed.insert(id, err.message);
            }
        }
    }
    for (id, detail) in failed {
        if let Ok(mut row) = record(s, &id) {
            row["enabled"] = json!(false);
            row["state"] = json!("unavailable");
            row["health"] = json!("unavailable");
            row["last_error"] = json!(detail);
            capabilities::set_entry_state(&mut row, "unavailable", "extension recovery failed");
            save(s, &id, row)?;
        }
    }
    Ok(())
}

pub(super) fn start_monitor(s: Arc<AppState>) {
    if !s.extension_host.try_begin_monitor() {
        return;
    }
    let weak = Arc::downgrade(&s);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(state) = weak.upgrade() else { break };
            if !state.admission.draining() {check_health_cycle(&state).await;}
        }
    });
}

async fn check_health_cycle(s: &Arc<AppState>) {
    let _guard = s.extension_host.lock_lifecycle().await;
    let rows = match records(s) {
        Ok(rows) => rows,
        Err(error) => { tracing::warn!(error=%error.message,"extension reconciliation could not read desired state"); return; }
    };
    for id in s.extension_host.ids().await {
        if !rows.iter().any(|row| row["extension_id"] == id && row["enabled"] == true) {
            if let Some(process) = s.extension_host.remove(&id).await {
                if let Err(error) = process.lock().await.stop().await {
                    tracing::warn!(extension_id=%id,error=%error.message,"extension stop reconciliation failed");
                }
            }
        }
    }
    for row in rows.iter().filter(|row| row["enabled"] != true && row["state"] == "stopping") {
        if let Some(id) = row["extension_id"].as_str() {
            let mut stopped = row.clone();
            stopped["state"] = json!("disabled");
            stopped["health"] = json!("unavailable");
            stopped["activation"]["phase"] = json!("stopped");
            stopped["secret_usage"] = json!([]);
            if let Err(error) = save(s,id,stopped) {
                tracing::warn!(extension_id=id,error=%error.message,"Extension stop receipt remains pending");
            }
        }
    }
    for mut row in rows.into_iter().filter(|row| row["enabled"] == true) {
        let Some(id) = row["extension_id"].as_str().map(str::to_owned) else {
            continue;
        };
        if row["activation"]["phase"] == "prepared" {
            let mut request = activation_request(&row);
            request["_allow_rollback"] = json!(false);
            if let Err(error) = enable_impl(s, &id, &request).await {
                tracing::warn!(extension_id=%id,error=%error.message,"extension activation reconciliation failed");
            }
            continue;
        }
        let status = if let Some(process) = s.extension_host.get(&id).await {
            process
                .lock()
                .await
                .rpc("health/read", json!({}))
                .await
                .ok()
                .and_then(|v| v["status"].as_str().map(str::to_owned))
        } else {
            None
        };
        if status.as_deref() == Some("healthy") {
            if row["state"] != "ready" {
                if let Ok(version) = active_version(&row) {
                    let version = version.to_owned();
                    if let Ok((_, manifest)) = load_snapshot(&row, &version) {
                        if let Some(process) = s.extension_host.get(&id).await {
                            let reported = process.lock().await.capabilities.clone();
                            row["registry_entries"] = json!(capabilities::registered_entries(
                                s, &id, &version, &manifest, &reported));
                        }
                    }
                }
            }
            row["state"] = json!("ready");
            row["health"] = json!("healthy");
            row["health_failures"] = json!(0);
            row["last_health_at"] = json!(chrono::Utc::now().to_rfc3339());
            let _ = save(s, &id, row);
            continue;
        }
        let failures = row["health_failures"].as_u64().unwrap_or(0) + 1;
        row["health_failures"] = json!(failures);
        row["health"] = json!(status.unwrap_or_else(|| "unhealthy".to_owned()));
        row["state"] = json!("degraded");
        capabilities::set_entry_state(&mut row, "unavailable", "extension health check failed");
        row["last_health_at"] = json!(chrono::Utc::now().to_rfc3339());
        let _ = save(s, &id, row.clone());
        if failures < 2 {
            continue;
        }
        let active = active_version(&row).unwrap_or("").to_owned();
        let previous = row["installed_versions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .rev()
            .find(|version| *version != active && row["last_auto_rollback_from"] != *version)
            .map(str::to_owned);
        if let Some(target) = previous {
            if let Ok(mut restored) = rollback_impl(s, &id, &json!({"version":target})).await {
                restored["last_auto_rollback_from"] = json!(active);
                restored["health_failures"] = json!(0);
                let _ = save(s, &id, restored);
                continue;
            }
        }
        if let Some(process) = s.extension_host.remove(&id).await {
            let _ = process.lock().await.stop().await;
        }
        row["enabled"] = json!(false);
        row["state"] = json!("unavailable");
        row["last_error"] = json!("health check failed after two attempts");
        row["last_error_code"] = json!("extension.health_failed");
        capabilities::set_entry_state(&mut row, "unavailable", "extension health check failed");
        let _ = save(s, &id, row);
    }
}

async fn upgrade_impl(s: &Arc<AppState>, id: &str, body: &Value) -> Result<Value> {
    let previous = record(s, id)?;
    let old_version = active_version(&previous)?.to_owned();
    let old_manifest = manifest_of(&previous, &old_version).ok();
    let was_enabled = previous["enabled"] == true;
    let new_version = preview_record(s, body["preview_id"].as_str().unwrap_or(""))?["version"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    if new_version == old_version {
        return Err(ExtError::state(
            "extension.upgrade_noop",
            format!("upgrade source resolved to the active version {new_version}"),
        ));
    }
    install_impl(s, body, Some(id)).await?;
    migrate_state(s, id, &old_version, &new_version)?;
    let current_row = record(s, id)?;
    let new_manifest = manifest_of(&current_row, &new_version)?;
    let _ = old_manifest;
    let retain = new_manifest["retain_previous_versions"] != false;
    let activation = if was_enabled {
        let mut enable = json!({"version":new_version,"_allow_rollback":retain});
        if let Some(config) = body.get("config") { enable["config"] = config.clone(); }
        enable_impl(s,id,&enable).await
    } else {
        let mut row = record(s,id)?;
        row["active_version"] = json!(new_version);
        save(s,id,row)
    };
    if !retain {
        let row = prune_versions(s,id,&new_version,activation.is_ok() && was_enabled)?;
        return activation.map(|_|row);
    }
    activation
}

fn prune_versions(s: &AppState, id: &str, target: &str, active: bool) -> Result<Value> {
    let mut row = record(s,id)?;
    for (version,path) in row["install_dirs"].as_object().into_iter().flatten() {
        if version == target { continue; }
        if let Some(path) = path.as_str() {
            let path = PathBuf::from(path);
            if !path.starts_with(install_root(s).join(id)) {
                return Err(ExtError::state("extension.snapshot_invalid","Installed version leaves its extension directory"));
            }
            if path.is_dir() { fs::remove_dir_all(path).map_err(|error| ExtError::runtime("extension.prune_failed",error.to_string()))?; }
        }
        let private = private_state(s,id,version);
        if private.is_dir() { fs::remove_dir_all(private).map_err(|error| ExtError::runtime("extension.prune_failed",error.to_string()))?; }
    }
    let target_dir = row["install_dirs"][target].clone();
    let target_install = row["installations"][target].clone();
    row["installed_versions"] = json!([target]);
    row["install_dirs"] = json!({target:target_dir});
    row["installations"] = json!({target:target_install});
    row["active_version"] = json!(target);
    if !active {
        row["enabled"] = json!(false);
        if row["state"] != "unavailable" { row["state"] = json!("installed"); }
    }
    save(s,id,row)
}

async fn rollback_impl(s: &Arc<AppState>, id: &str, body: &Value) -> Result<Value> {
    let row = record(s, id)?;
    let active = active_version(&row)?.to_owned();
    let versions: Vec<_> = row["installed_versions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let target = body["version"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            versions
                .iter()
                .rev()
                .find(|v| **v != active)
                .map(|v| (*v).to_owned())
        })
        .ok_or_else(|| {
            ExtError::state(
                "extension.rollback_unavailable",
                "no previous installed version",
            )
        })?;
    if !versions.contains(&target.as_str()) {
        return Err(ExtError::state(
            "extension.version_missing",
            "rollback target is not installed",
        ));
    }
    migrate_state(s, id, &active, &target)?;
    enable_impl(s, id, &json!({"version":target,"config":row["config"]})).await
}

async fn uninstall_impl(s: &AppState, id: &str, body: &Value) -> Result<Value> {
    if body["confirm"] != true {
        return Err(ExtError::validation("extension.uninstall_confirmation_required","Uninstall requires explicit retention choices"));
    }
    let row = record(s,id)?;
    if let Some(process) = s.extension_host.remove(id).await { process.lock().await.stop().await?; }
    let stamp = format!("{}-{}",chrono::Utc::now().format("%Y%m%dT%H%M%S%fZ"),uuid::Uuid::new_v4().simple());
    let archive = state_root(s).join("_uninstalled").join(id).join(&stamp);
    fs::create_dir_all(&archive).map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&archive,fs::Permissions::from_mode(0o700)).map_err(|error|ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
    }
    let base = state_root(s).join(id);
    let preserve_state = body["preserve_state"].as_bool().unwrap_or(true);
    let preserve_logs = body["preserve_logs"].as_bool().unwrap_or(true);
    let preserve_artifacts = body["preserve_artifacts"].as_bool().unwrap_or(true);
    let mut archived = Vec::new();
    let mut artifact_count = 0;
    if base.is_dir() {
        let canonical_base = base.canonicalize().map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
        for entry in walkdir::WalkDir::new(&base).min_depth(1).follow_links(false) {
            let entry = entry.map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
            if entry.file_type().is_dir() { continue; }
            let relative = entry.path().strip_prefix(&base).map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
            let artifact = relative.components().any(|part| part.as_os_str() == "artifacts");
            let log = relative.components().any(|part| part.as_os_str() == "logs") || relative.extension().is_some_and(|extension| extension == "log");
            let keep = if artifact { preserve_artifacts } else if log { preserve_logs } else { preserve_state };
            if !keep { continue; }
            let resolved = entry.path().canonicalize().map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
            if !resolved.starts_with(&canonical_base) || !resolved.is_file() {
                return Err(ExtError::state("extension.archive_unsafe_path","Retained extension file leaves its private state directory"));
            }
            let target = archive.join("data").join(relative);
            fs::create_dir_all(target.parent().unwrap()).map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
            fs::copy(&resolved,&target).map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
            fs::File::open(&target).and_then(|file| file.sync_all()).map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
            archived.push(format!("{stamp}/data/{}",relative.to_string_lossy()));
            if artifact { artifact_count += 1; }
        }
    }
    let retention = json!({"extension_id":id,"uninstalled_at":chrono::Utc::now().to_rfc3339(),
        "preserve_state":preserve_state,"preserve_logs":preserve_logs,"preserve_artifacts":preserve_artifacts,
        "archived_files":archived,"artifact_count":artifact_count,"archive_dir":archive.to_string_lossy()});
    let receipt_path = archive.join("receipt.json");
    fs::write(&receipt_path,serde_json::to_vec_pretty(&retention).unwrap()).map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
    fs::File::open(&receipt_path).and_then(|file|file.sync_all()).map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
    for entry in walkdir::WalkDir::new(&archive).contents_first(true).follow_links(false) {
        let entry = entry.map_err(|error|ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(entry.path(),fs::Permissions::from_mode(if entry.file_type().is_dir(){0o700}else{0o600}))
                .map_err(|error|ExtError::runtime("extension.uninstall_failed",error.to_string()))?;
        }
        if entry.file_type().is_dir() { fs::File::open(entry.path()).and_then(|file|file.sync_all())
            .map_err(|error|ExtError::runtime("extension.uninstall_failed",error.to_string()))?; }
    }
    let job_id = format!("{id}:{stamp}");
    let mut pending = row.clone();
    pending["enabled"] = json!(false);
    pending["state"] = json!("uninstalling");
    pending["uninstall_job"] = json!(job_id);
    capabilities::set_entry_state(&mut pending,"unavailable","extension is being uninstalled");
    let job = json!({"job_id":job_id,"extension_id":id,"state":"prepared","retention":retention,
        "original_envelope":body["_platform_envelope"],"install_dirs":row["install_dirs"]});
    if !s.store.put_many_if(&[(RECORDS,id,Some(&row)),("task_extension_uninstalls",job_id.as_str(),None)],
        &[(RECORDS,id,&pending),("task_extension_uninstalls",job_id.as_str(),&job)])
        .map_err(|error| ExtError::runtime("extension.store_failed",error.to_string()))? {
        return Err(ExtError::state("extension.revision_conflict","Extension changed before uninstall admission"));
    }
    finish_uninstall(s,&job)
}

fn finish_uninstall(s: &AppState, job: &Value) -> Result<Value> {
    let id = job["extension_id"].as_str().unwrap_or("");
    let job_id = job["job_id"].as_str().unwrap_or("");
    let current = s.store.get(RECORDS,id).map_err(|error| ExtError::runtime("extension.store_failed",error.to_string()))?;
    if current.as_ref().is_some_and(|row| row["uninstall_job"] != job_id) {
        return Err(ExtError::state("extension.revision_conflict","Uninstall job no longer owns the extension"));
    }
    for directory in job["install_dirs"].as_object().into_iter().flatten().map(|(_,value)|value) {
        let path = PathBuf::from(directory.as_str().ok_or_else(|| ExtError::state("extension.snapshot_invalid","Invalid installed directory"))?);
        if !path.starts_with(install_root(s).join(id)) {
            return Err(ExtError::state("extension.snapshot_invalid","Installed package leaves its extension directory"));
        }
        if path.is_dir() { fs::remove_dir_all(path).map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?; }
    }
    let base = state_root(s).join(id);
    if base.is_dir() { fs::remove_dir_all(base).map_err(|error| ExtError::runtime("extension.uninstall_failed",error.to_string()))?; }
    let mut completed = job.clone();
    completed["state"] = json!("completed");
    let retention = &job["retention"];
    if !s.store.put_many_if_with_deletes(&[(RECORDS,id,current.as_ref()),("task_extension_uninstalls",job_id,Some(job))],
        &[("task_extension_uninstalls",job_id,&completed),("task_extension_uninstall_receipts",job_id,retention)],
        |_|Ok(Some(vec![(RECORDS.to_owned(),id.to_owned())])))
        .map_err(|error| ExtError::runtime("extension.store_failed",error.to_string()))? {
        return Err(ExtError::state("extension.revision_conflict","Uninstall completion changed concurrently"));
    }
    Ok(json!({"extension_id":id,"uninstalled":true,"retention":retention}))
}

fn recover_uninstalls(s: &AppState) -> Result<()> {
    for job in s.store.list("task_extension_uninstalls").map_err(|error| ExtError::runtime("extension.store_failed",error.to_string()))? {
        if job["state"] == "prepared" { finish_uninstall(s,&job)?; }
    }
    Ok(())
}

fn manifest_summary(row: &Value) -> Value {
    let Ok(version) = active_version(row) else {
        return json!({"error":"no installed version"});
    };
    let Ok(m) = manifest_of(row, version) else {
        return json!({"error":"installed manifest unavailable"});
    };
    let dir = installed_dir(row, version).ok();
    let verification = verification_of(row, version);
    json!({"id":m["id"],"version":m["version"],"plugin_version":m["plugin_version"],
        "description":m["description"],"author":m["author"],"homepage":m["homepage"],
        "plugin_schema":m["plugin_schema"],"client_namespace":m["client_namespace"],
        "origin":m["origin"],"requires_core":m["requires_core"],"entrypoints":m["entrypoints"],
        "provides":m["provides"],"requires":m["requires"],"permissions":m["permissions"],
        "has_ui":m["ui"].is_string(),"has_config_schema":m["config_schema"].is_string(),
        "install_dir":dir.map(|p|p.to_string_lossy().into_owned()).unwrap_or_default(),
        "verification":verification,"portable_components":verification["agent_plugin"]["components"],
        "registry_state":if row["enabled"]==true&&row["state"]=="ready"{"enabled"}else if row["state"]=="unavailable"{"unavailable"}else{"installed"}})
}

async fn live_health(s: &AppState, id: &str, enabled: bool) -> Value {
    if !enabled {
        return Value::Null;
    }
    let Some(process) = s.extension_host.get(id).await else {
        return json!({"status":"unavailable","detail":"extension process is not running"});
    };
    process
        .lock()
        .await
        .rpc("health/read", json!({}))
        .await
        .unwrap_or_else(|e| json!({"status":"error","detail":e.message}))
}

fn logs(s: &AppState, id: &str, limit: usize) -> Result<Vec<String>> {
    record(s, id)?;
    let root = state_root(s).join(id).join("state");
    let mut paths = Vec::new();
    if root.is_dir() {
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .flatten()
        {
            if entry.file_type().is_file()
                && entry
                    .path()
                    .parent()
                    .is_some_and(|p| p.file_name().is_some_and(|n| n == "logs"))
            {
                paths.push(entry.into_path());
            }
        }
    }
    paths.sort();
    let mut lines = Vec::new();
    for path in paths {
        let raw = fs::read_to_string(path).unwrap_or_default();
        lines.extend(raw.lines().map(str::to_owned));
    }
    if lines.len() > limit {
        lines = lines.split_off(lines.len() - limit);
    }
    Ok(lines)
}

fn response(result: Result<Value>) -> Response {
    result
        .map(|v| Json(v).into_response())
        .unwrap_or_else(|e| e.response())
}

pub(crate) fn supports_lifecycle_command(kind: &str) -> bool {
    matches!(kind,"extension.install" | "extension.enable" | "extension.disable" | "extension.upgrade"
        | "extension.rollback" | "extension.uninstall" | "extension.command" | "extension.propose_event"
        | "extension.health_refresh")
}

pub(super) fn commit_proposed_event(s: &AppState, envelope: &Value) -> Result<Value> {
    use crate::operations_api::{command_fingerprint, domain_event_cursor, current_aggregate_version, domain_events};
    let command_id = envelope["command_id"].as_str().filter(|value| !value.is_empty())
        .ok_or_else(|| ExtError::validation("extension.command_id_required","Event proposal requires command_id"))?;
    let payload = &envelope["payload"];
    let ext_id = payload["extension_id"].as_str().or_else(|| envelope["aggregate_id"].as_str()).unwrap_or("");
    let kind = payload["aggregate_type"].as_str().filter(|value| !value.is_empty()).unwrap_or("extension");
    let id = payload["aggregate_id"].as_str().filter(|value| !value.is_empty()).unwrap_or(ext_id);
    let event_type = payload["event_type"].as_str().unwrap_or("");
    if ext_id.is_empty() || id.is_empty() || event_type.is_empty() || !payload["payload"].is_object() {
        return Err(ExtError::validation("extension.event_invalid","Event type, aggregate identity and object payload are required"));
    }
    let previous = s.store.get("platform_commands",command_id).map_err(|error| ExtError::runtime("extension.store_failed",error.to_string()))?;
    let fingerprint = command_fingerprint(envelope);
    if let Some(prior) = &previous {
        if prior["request_fingerprint"] != fingerprint {
            return Err(ExtError::state("command.idempotency_conflict","Event command identity was used with different content"));
        }
        if prior["response"]["receipt"]["state"] == "completed" {
            let mut receipt = prior["response"]["receipt"].clone();
            receipt["deduplicated"] = json!(true);
            return Ok(json!({"receipt":receipt,"event_id":receipt["output"]["event_id"]}));
        }
    }
    let mut event = domain_events::event(envelope,kind,id,event_type,&format!("ext.{ext_id}"),payload["payload"].clone());
    if let Some(cause) = payload["causation_id"].as_str().filter(|value| !value.is_empty()) { event["causation_id"] = json!(cause); }
    let version = current_aggregate_version(s,kind,id).map_err(|message| ExtError::runtime("extension.store_failed",message))?;
    event["stream_seq"] = json!(version+1);
    let cursor = domain_event_cursor(s,kind,id,version+1,envelope["_verified_scope"].as_str().unwrap_or("operator:local-user"))
        .map_err(|message| ExtError::runtime("extension.store_failed",message))?;
    let receipt = json!({"schema_version":1,"command_id":command_id,"receipt_id":format!("rcpt-{command_id}"),"state":"completed",
        "aggregate":{"type":kind,"id":id},"run_id":null,"event_cursor":cursor,"submitted_at":chrono::Utc::now().to_rfc3339(),
        "completed_at":chrono::Utc::now().to_rfc3339(),"deduplicated":false,"effect_ids":[],"error":null,
        "next":["receipt","events","wait"],"output":{"event_id":event["event_id"]}});
    let stored = json!({"request":{"command_type":"extension.propose_event","aggregate_type":kind,"aggregate_id":id},
        "request_fingerprint":fingerprint,"status":200,"response":{"receipt":receipt}});
    let saved = domain_events::commit(s,&[("platform_commands",command_id,previous.as_ref())],
        &[("platform_commands",command_id,&stored)],&[event.clone()])
        .map_err(|error| ExtError::runtime("extension.store_failed",error.1.0.to_string()))?;
    if saved.is_none() { return Err(ExtError::state("command.version_conflict","Event aggregate changed; retry with the same command identity")); }
    Ok(json!({"receipt":receipt,"event_id":event["event_id"]}))
}

pub(crate) async fn dispatch_lifecycle_command(s: &Arc<AppState>, envelope: &Value) -> Response {
    let kind = envelope["command_type"].as_str().unwrap_or("");
    let payload = &envelope["payload"];
    let id = payload["extension_id"].as_str().filter(|value| !value.is_empty())
        .or_else(|| envelope["aggregate_id"].as_str()).unwrap_or("");
    if kind == "extension.propose_event" {
        let validated = (|| -> Result<()> {
            let row = record(s,id)?;
            if row["state"] == "disabled" { return Err(ExtError::state("extension.disabled","Disabled extension cannot propose events")); }
            let version = active_version(&row)?;
            let manifest = manifest_of(&row,version)?;
            let event_type = payload["event_type"].as_str().unwrap_or("");
            let allowed = event_type.starts_with(&format!("ext.{id}.")) && manifest["permissions"]["events_write"].as_array()
                .into_iter().flatten().filter_map(Value::as_str).any(|pattern| pattern.strip_suffix(".*")
                    .map_or(event_type == pattern, |prefix| event_type.starts_with(&format!("{prefix}."))));
            if !allowed { return Err(ExtError::validation("extension.event_namespace_denied","Event namespace is outside extension permissions")); }
            if let Some(schema) = row["capabilities"]["event_schemas"].get(event_type) {
                manifest::validate_schema(&payload["payload"],schema,"event")?;
            }
            Ok(())
        })();
        return response(validated.and_then(|_| commit_proposed_event(s,envelope)));
    }
    if kind == "extension.command" {
        return invoke_raw(s,id,payload).await;
    }
    if kind == "extension.health_refresh" {
        return health_refresh(State(s.clone()), Path(id.to_owned())).await;
    }
    let _guard = s.extension_host.lock_lifecycle().await;
    let result = match kind {
        "extension.install" => install_impl(s,payload,None).await,
        "extension.enable" => enable_impl(s,id,payload).await,
        "extension.disable" => disable_impl(s,id).await,
        "extension.upgrade" => upgrade_impl(s,id,payload).await,
        "extension.rollback" => rollback_impl(s,id,payload).await,
        "extension.uninstall" => {
            let mut request = payload.clone();
            if request.get("confirm").is_none() { request["confirm"] = json!(true); }
            request["_platform_envelope"] = envelope.clone();
            uninstall_impl(s,id,&request).await
        },
        _ => Err(ExtError::validation("extension.command_unknown","Unknown extension lifecycle command")),
    };
    if result.is_ok() { start_monitor(s.clone()); }
    response(result.map(|value| json!({"result":value})))
}

pub(crate) async fn dispatch_lifecycle_query(s: &Arc<AppState>, envelope: &Value) -> Option<Response> {
    let kind = envelope["query_type"].as_str()?;
    if !matches!(kind,"extension.list" | "extension.overview" | "extension.catalog" | "extension.get" | "extension.detail" | "extension.health" | "extension.projection" | "extension.logs") { return None; }
    let params = &envelope["params"];
    if kind == "extension.overview" {
        return Some(overview(State(s.clone())).await);
    }
    if kind == "extension.catalog" {
        let mut query = HashMap::new();
        if let Some(root) = params["root"].as_str().filter(|v| !v.is_empty()) {
            query.insert("root".to_owned(), root.to_owned());
        }
        if let Some(q) = params["q"].as_str().filter(|v| !v.is_empty()) {
            query.insert("q".to_owned(), q.to_owned());
        }
        return Some(catalog(Query(query)).await);
    }
    let id = params["extension_id"].as_str().filter(|value| !value.is_empty())
        .or_else(|| envelope["aggregate_id"].as_str()).unwrap_or("");
    if !matches!(kind, "extension.list" | "extension.overview" | "extension.catalog") && id.is_empty() {
        return Some(ExtError::validation("extension.id_required","Extension id is required").response());
    }
    if kind == "extension.detail" {
        return Some(detail(State(s.clone()), Path(id.to_owned())).await);
    }
    let result = match kind {
        "extension.list" => records(s).map(|rows| json!(rows)),
        "extension.get" => s.store.get(RECORDS,id).map(|row| row.unwrap_or(Value::Null))
            .map_err(|error| ExtError::runtime("extension.store_failed",error.to_string())),
        "extension.logs" => logs(s,id,params["limit"].as_u64().unwrap_or(200).clamp(1,100_000) as usize).map(|lines| json!({"lines":lines})),
        "extension.health" => match record(s,id) {
            Ok(row) => Ok(live_health(s,id,row["enabled"] == true).await),
            Err(error) => Err(error),
        },
        _ => {
            match s.extension_host.get(id).await {
                Some(process) => process.lock().await.rpc("projection/read",json!({"name":params["name"].as_str().unwrap_or("")})).await,
                None => Err(ExtError::runtime("extension.process_unavailable","Extension process is not running")),
            }
        },
    };
    Some(response(result))
}

async fn list(State(s): State<Arc<AppState>>) -> Response {
    response(records(&s).map(|items| json!({"extensions":items})))
}
async fn overview(State(s): State<Arc<AppState>>) -> Response {
    match records(&s) {
        Ok(rows) => {
            let mut cards = Vec::new();
            for row in rows {
                let id = row["extension_id"].as_str().unwrap_or("");
                let live = live_health(&s, id, row["enabled"] == true).await;
                cards.push(json!({"record":row,"manifest":manifest_summary(&row),"live_health":live,"migrations":migration_receipts(&s,id)}));
            }
            Json(json!({"extensions":cards,"install_policy":policy(&s)})).into_response()
        }
        Err(e) => e.response(),
    }
}
async fn catalog(Query(query): Query<HashMap<String, String>>) -> Response {
    let root = query
        .get("root")
        .cloned()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("MUTEKI_EXTENSION_CATALOG")
                .ok()
                .filter(|v| !v.trim().is_empty())
        });
    let Some(root) = root else {
        return Json(json!({"root":"","entries":[],"detail":"未配置 Catalog"})).into_response();
    };
    let needle = query
        .get("q")
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    response(
        source::catalog_entries(FsPath::new(&root)).map(|mut entries| {
            if !needle.is_empty() {
                entries.retain(|v| {
                    format!("{} {} {}", v["id"], v["version"], v["description"])
                        .to_ascii_lowercase()
                        .contains(&needle)
                });
            }
            json!({"root":root,"entries":entries})
        }),
    )
}
async fn get_record(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    response(record(&s, &id))
}
async fn detail(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let row = match record(&s, &id) {
        Ok(v) => v,
        Err(e) => return e.response(),
    };
    let m = active_version(&row).and_then(|v| manifest_of(&row, v));
    let schema = m.as_ref().ok().and_then(|manifest| {
        manifest["config_schema"].as_str().and_then(|rel| {
            let dir = installed_dir(&row, active_version(&row).ok()?).ok()?;
            manifest::load_schema(&dir, rel).ok()
        })
    });
    let live = live_health(&s, &id, row["enabled"] == true).await;
    Json(json!({"record":row,"manifest":m.as_ref().ok(),"manifest_error":m.err().map(|e|e.message).unwrap_or_default(),
        "config_schema":schema,"config_schema_error":"","migrations":migration_receipts(&s,&id),"live_health":live})).into_response()
}
async fn ui(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    response((|| -> Result<Value> {
        let row = record(&s, &id)?;
        let version = active_version(&row)?;
        let (dir, m) = load_snapshot(&row, version)?;
        let Some(relative) = m["ui"].as_str() else {
            return Ok(json!({"declared":false,"contributions":{}}));
        };
        let contributions = manifest::load_ui(&dir, relative)?;
        Ok(
            json!({"declared":true,"file":relative,"schema_version":contributions["schema_version"],
            "source":{"extension_id":id,"version":version,"permissions":m["permissions"]},"contributions":contributions}),
        )
    })())
}
async fn projection(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let row = match record(&s, &id) {
        Ok(v) => v,
        Err(e) => return e.response(),
    };
    if row["enabled"] != true || row["state"] != "ready" {
        return Json(json!({"status":"unavailable","extension_id":id,"state":row["state"],"name":query.get("name")})).into_response();
    }
    let Some(process) = s.extension_host.get(&id).await else {
        return ExtError::runtime(
            "extension.process_unavailable",
            "extension process is not running",
        )
        .response();
    };
    let mut process = process.lock().await;
    let name = query
        .get("name")
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| {
            process.capabilities["projections"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    response(
        process
            .rpc("projection/read", json!({"name":name}))
            .await
            .map(|v| json!({"status":"ok","data":v})),
    )
}
async fn health(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let row = match record(&s, &id) {
        Ok(v) => v,
        Err(e) => return e.response(),
    };
    if row["enabled"] != true {
        return Json(json!({"extension_id":id,"status":"unavailable","state":row["state"]}))
            .into_response();
    }
    let Some(process) = s.extension_host.get(&id).await else {
        return Json(json!({"extension_id":id,"status":"unavailable","state":row["state"]}))
            .into_response();
    };
    response(
        process
            .lock()
            .await
            .rpc("health/read", json!({}))
            .await
            .map(|v| json!({"extension_id":id,"status":v["status"],"detail":v["detail"]})),
    )
}
async fn health_refresh(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let row = match record(&s, &id) {
        Ok(v) => v,
        Err(e) => return e.response(),
    };
    let result = live_health(&s, &id, row["enabled"] == true).await;
    Json(json!({"extension_id":id,"state":row["state"],"enabled":row["enabled"],
        "health":if result.is_null(){json!({"status":"unavailable","detail":"extension is not enabled"})}else{result}})).into_response()
}
async fn logs_route(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 1000);
    response(logs(&s, &id, 100_000).map(|mut lines| {
        if lines.len() > limit {
            lines = lines.split_off(lines.len() - limit);
        }
        json!({"lines":lines})
    }))
}
async fn logs_page(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let offset = query
        .get("offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, 1000);
    response(logs(&s,&id,100_000).map(|lines|{let total=lines.len();let end=total.saturating_sub(offset);let start=end.saturating_sub(limit);
        json!({"lines":&lines[start..end],"total":total,"offset":offset,"limit":limit,"has_more":start>0})}))
}

pub(crate) async fn preview(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    response(preview_impl(&s, &body).await)
}
async fn managed_command(s: Arc<AppState>, kind: &str, id: &str, body: Value) -> Response {
    if !body.is_object() { return ExtError::validation("extension.invalid_payload","Request must be an object").response(); }
    let mut payload = body.clone();
    for field in ["command_id","idempotency_key","expected_version"] { payload.as_object_mut().unwrap().remove(field); }
    let mut envelope = json!({"command_type":kind,"aggregate_type":"extension","aggregate_id":id,"payload":payload});
    for field in ["command_id","idempotency_key","expected_version"] {
        if let Some(value) = body.get(field) { envelope[field] = value.clone(); }
    }
    crate::operations_api::dispatch_local_command(s,envelope).await
}
async fn install(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    managed_command(s,"extension.install","registry",body).await
}
async fn enable(State(s): State<Arc<AppState>>, Path(id): Path<String>, body: Option<Json<Value>>) -> Response {
    managed_command(s,"extension.enable",&id,body_value(body)).await
}
async fn disable(State(s): State<Arc<AppState>>, Path(id): Path<String>, body: Option<Json<Value>>) -> Response {
    managed_command(s,"extension.disable",&id,body_value(body)).await
}
async fn upgrade(State(s): State<Arc<AppState>>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    managed_command(s,"extension.upgrade",&id,body).await
}
async fn rollback(State(s): State<Arc<AppState>>, Path(id): Path<String>, body: Option<Json<Value>>) -> Response {
    managed_command(s,"extension.rollback",&id,body_value(body)).await
}
async fn uninstall(State(s): State<Arc<AppState>>, Path(id): Path<String>, body: Option<Json<Value>>) -> Response {
    managed_command(s,"extension.uninstall",&id,body_value(body)).await
}
async fn invoke(State(s): State<Arc<AppState>>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    managed_command(s,"extension.command",&id,body).await
}
async fn invoke_raw(s: &Arc<AppState>, id: &str, body: &Value) -> Response {
    let row = match record(s,id) { Ok(row) => row, Err(error) => return error.response() };
    if row["enabled"] != true || row["state"] != "ready" {
        return ExtError::state("extension.not_ready","Extension is not enabled and ready").response();
    }
    let kind = body["command_type"].as_str().unwrap_or("").trim();
    if !kind.starts_with(&format!("ext.{id}.")) || kind == format!("ext.{id}.") {
        return ExtError::validation("extension.command_denied","Command type leaves extension namespace").response();
    }
    let params = body.get("params").filter(|value| value.is_object()).cloned().unwrap_or_else(||json!({}));
    let Some(process) = s.extension_host.get(id).await else {
        return ExtError::runtime("extension.process_unavailable","Extension process is not running").response();
    };
    let result = process.lock().await.rpc("command/handle",json!({"command_type":kind,"payload":params})).await;
    response(result.map(|result| json!({"result":result})))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/extensions", get(list))
        .route("/api/extensions/overview", get(overview))
        .route("/api/extensions/catalog", get(catalog))
        .route("/api/extensions/preview", post(preview))
        .route("/api/extensions/install", post(install))
        .route("/api/extensions/{id}", get(get_record).delete(uninstall))
        .route("/api/extensions/{id}/detail", get(detail))
        .route("/api/extensions/{id}/ui", get(ui))
        .route("/api/extensions/{id}/projection", get(projection))
        .route("/api/extensions/{id}/logs", get(logs_route))
        .route("/api/extensions/{id}/logs/page", get(logs_page))
        .route("/api/extensions/{id}/health", get(health))
        .route("/api/extensions/{id}/health/refresh", post(health_refresh))
        .route("/api/extensions/{id}/invoke", post(invoke))
        .route("/api/extensions/{id}/enable", post(enable))
        .route("/api/extensions/{id}/disable", post(disable))
        .route("/api/extensions/{id}/upgrade", post(upgrade))
        .route("/api/extensions/{id}/rollback", post(rollback))
}
