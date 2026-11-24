//! Agent-safe command and query dispatch for Muteki workbench settings, model catalogs,
//! runtimes, extensions, capabilities, workspaces, and operations controls.
use super::*;
use std::collections::HashMap;

const WORKBENCH_COMMANDS: &[&str] = &[
    "workbench.credential_account.upsert",
    "workbench.credential_account.delete",
    "workbench.model_endpoint.upsert",
    "workbench.model_endpoint.delete",
    "workbench.credential_models.refresh",
    "workbench.credential_account.test",
    "workbench.local_pi.update",
    "workbench.quota.refresh",
    "workbench.pricing.update",
    "workbench.runtime_instance.manage",
    "workbench.capabilities.update",
    "workbench.chat_plugin.manage",
    "workbench.extension.manage",
    "workbench.ui_settings.update",
    "workbench.history_import.apply",
    "workbench.workspace.manage",
    "workbench.folder.manage",
    "workbench.memory.manage",
    "workbench.share.manage",
    "workbench.platform_effect.manage",
    "workbench.maintenance.manage",
    "workbench.domain_module.manage",
    "workbench.runner_resources.update",
    "workbench.system_update.check",
    "workbench.llm_endpoint.test",
    "workbench.worker_image.pull",
    "workbench.run_retention.update",
];

const WORKBENCH_QUERIES: &[&str] = &[
    "workbench.credentials.list",
    "workbench.local_pi.get",
    "workbench.usage_quota.get",
    "workbench.pricing.get",
    "workbench.capabilities.overview",
    "workbench.chat_plugins.list",
    "workbench.extensions.list",
    "workbench.ui_settings.get",
    "workbench.control.state",
    "workbench.history_import.scan",
    "workbench.workspaces.list",
    "workbench.shares.list",
    "workbench.operations.get",
    "workbench.runner_resources.get",
    "workbench.system_status.get",
    "workbench.worker_models.get",
    "workbench.worker_image.get",
    "workbench.run_retention.get",
];

pub(super) fn command_supported(kind: &str) -> bool {
    WORKBENCH_COMMANDS.contains(&kind)
}

pub(super) fn query_supported(kind: &str) -> bool {
    WORKBENCH_QUERIES.contains(&kind)
}

fn named<'a>(value: &'a Value, fields: &[&str]) -> &'a str {
    fields
        .iter()
        .find_map(|field| value[*field].as_str().filter(|v| !v.trim().is_empty()))
        .unwrap_or("")
        .trim()
}

fn strip_account_prefix(raw: &str) -> String {
    raw.trim()
        .strip_prefix("account:")
        .unwrap_or(raw.trim())
        .to_owned()
}

pub(super) fn command_scope(kind: &str, envelope: &Value) -> Result<(&'static str, String), Failure> {
    let payload = &envelope["payload"];
    let aggregate = envelope["aggregate_id"].as_str().unwrap_or("").trim();
    let (scope, candidate, required, err_code, err_msg) = match kind {
        "workbench.credential_account.upsert" | "workbench.credential_account.delete" => (
            "credential",
            named(payload, &["account_id", "id"]),
            true,
            "credential.id_required",
            "account_id is required",
        ),
        "workbench.model_endpoint.upsert" | "workbench.model_endpoint.delete" => (
            "credential",
            named(payload, &["id", "endpoint_id"]),
            true,
            "endpoint.id_required",
            "id is required",
        ),
        "workbench.credential_models.refresh" => (
            "credential",
            named(payload, &["account_id", "id"]),
            false,
            "credential.id_required",
            "account_id is required",
        ),
        "workbench.credential_account.test" | "workbench.llm_endpoint.test" => (
            "credential",
            named(payload, &["account_id", "endpoint_id", "id"]),
            true,
            "credential.id_required",
            "account_id or endpoint_id is required",
        ),
        "workbench.runtime_instance.manage" => (
            "runtime",
            named(payload, &["key", "instance_key", "instance_id", "name"]),
            !matches!(payload["action"].as_str().unwrap_or("probe"), "refresh"),
            "runtime.instance.key_required",
            "Runtime instance key is required",
        ),
        "workbench.capabilities.update" => (
            "capability",
            named(payload, &["thread_id", "resource"]),
            false,
            "capability.id_required",
            "Capability target is required",
        ),
        "workbench.chat_plugin.manage" => (
            "extension",
            named(payload, &["package_id", "name"]),
            false,
            "chat_plugin.id_required",
            "package_id is required",
        ),
        "workbench.extension.manage" => (
            "extension",
            named(payload, &["extension_id", "id"]),
            !matches!(payload["action"].as_str().unwrap_or(""), "install" | "preview"),
            "extension.id_required",
            "extension_id is required",
        ),
        "workbench.workspace.manage" => {
            let need_ws = matches!(
                payload["action"].as_str().unwrap_or(""),
                "update_workspace" | "delete_workspace" | "delete_worktree"
            );
            let need_proj = payload["action"].as_str() == Some("delete_project");
            if need_proj {
                (
                    "project",
                    named(payload, &["project_id", "id"]),
                    true,
                    "conversation.project.id_required",
                    "project_id is required",
                )
            } else {
                (
                    "workspace",
                    named(payload, &["workspace_id", "id"]),
                    need_ws,
                    "conversation.workspace.id_required",
                    "workspace_id is required",
                )
            }
        }
        "workbench.folder.manage" => (
            "folder",
            named(payload, &["folder_id", "id"]),
            matches!(payload["action"].as_str().unwrap_or(""), "update" | "delete"),
            "folder.id_required",
            "folder_id is required",
        ),
        "workbench.memory.manage" | "workbench.share.manage" => (
            "thread",
            named(payload, &["thread_id"]),
            true,
            "conversation.thread.id_required",
            "thread_id is required",
        ),
        "workbench.platform_effect.manage" => (
            "effect",
            named(payload, &["effect_id", "id"]),
            true,
            "effect.id_required",
            "effect_id is required",
        ),
        "workbench.domain_module.manage" => (
            "module",
            named(payload, &["module_id", "id"]),
            true,
            "module.id_required",
            "module_id is required",
        ),
        "workbench.run_retention.update" => (
            "worker_settings",
            named(payload, &["mode"]),
            true,
            "retention.mode_required",
            "mode (ctf or pentest) is required",
        ),
        _ => (
            "workbench",
            named(payload, &["account_id", "id", "target", "mode"]),
            false,
            "workbench.id_required",
            "target id is required",
        ),
    };
    let id = if candidate.is_empty() { aggregate } else { candidate };
    if required && id.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, err_code, err_msg));
    }
    Ok((scope, id.to_owned()))
}

pub(super) fn query_scope(kind: &str, envelope: &Value) -> Result<(&'static str, String), Failure> {
    let aggregate = envelope["aggregate_id"].as_str().unwrap_or("").trim();
    let params = &envelope["params"];
    let (scope, candidate, required, err_code, err_msg) = match kind {
        "workbench.shares.list" => (
            "thread",
            named(params, &["thread_id"]),
            true,
            "conversation.thread.id_required",
            "thread_id is required",
        ),
        "workbench.extensions.list" => (
            "extension",
            named(params, &["extension_id", "id"]),
            params["query_kind"].as_str() == Some("detail"),
            "extension.id_required",
            "extension_id is required for detail query",
        ),
        "workbench.workspaces.list" => match params["kind"].as_str().unwrap_or("workspaces") {
            "workspace" => (
                "workspace",
                named(params, &["workspace_id", "id"]),
                true,
                "conversation.workspace.id_required",
                "workspace_id is required",
            ),
            "project_worktrees" => (
                "project",
                named(params, &["project_id", "id"]),
                true,
                "conversation.project.id_required",
                "project_id is required",
            ),
            _ => ("workspace", named(params, &["project_id", "workspace_id"]), false, "", ""),
        },
        "workbench.credentials.list"
        | "workbench.local_pi.get"
        | "workbench.usage_quota.get"
        | "workbench.pricing.get" => ("credential", named(params, &["account_id"]), false, "", ""),
        "workbench.worker_models.get"
        | "workbench.worker_image.get"
        | "workbench.run_retention.get" => ("worker_settings", "", false, "", ""),
        _ => ("workbench", named(params, &["id", "module_id", "thread_id"]), false, "", ""),
    };
    let id = if candidate.is_empty() { aggregate } else { candidate };
    if required && id.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, err_code, err_msg));
    }
    Ok((scope, id.to_owned()))
}

fn api_err((status, Json(body)): Failure) -> Failure {
    if body.get("error").is_some_and(Value::is_object) {
        return (status, Json(body));
    }
    if body["receipt"]["error"].is_object() {
        let mut body = body;
        body["error"] = body["receipt"]["error"].clone();
        return (status, Json(body));
    }
    if let Some(detail) = body.get("detail").filter(|d| d.is_object()) {
        let code = detail["code"].as_str().unwrap_or("workbench.request_failed");
        let msg = detail["message"]
            .as_str()
            .unwrap_or_else(|| status.canonical_reason().unwrap_or("Request failed"));
        return fail(status, code, msg);
    }
    let msg = body["detail"]
        .as_str()
        .or_else(|| body["message"].as_str())
        .or_else(|| body["error"].as_str())
        .unwrap_or_else(|| status.canonical_reason().unwrap_or("Request failed"));
    let code = match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "platform.request.invalid",
        StatusCode::NOT_FOUND => "workbench.not_found",
        StatusCode::CONFLICT => "workbench.conflict",
        StatusCode::FORBIDDEN => "workbench.forbidden",
        StatusCode::UNAUTHORIZED => "workbench.unauthorized",
        _ => "workbench.request_failed",
    };
    fail(status, code, msg)
}

async fn response_json(resp: Response) -> Result<Value, Failure> {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .map_err(storage)?;
    let body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}));
    if status.is_success() {
        Ok(body)
    } else {
        Err(api_err((status, Json(body))))
    }
}

fn canonicalize_within_existing(path: &std::path::Path) -> Result<PathBuf, Failure> {
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        if cursor
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(fail(
                StatusCode::FORBIDDEN,
                "conversation.import.path_forbidden",
                "Symlinks are not permitted in agent history import paths",
            ));
        }
        let Some(name) = cursor.file_name() else {
            break;
        };
        missing.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            break;
        };
        cursor = parent;
    }
    let mut resolved = std::fs::canonicalize(cursor).map_err(|e| {
        fail(
            StatusCode::FORBIDDEN,
            "conversation.import.path_forbidden",
            format!("Cannot resolve history directory: {e}"),
        )
    })?;
    for segment in missing.into_iter().rev() {
        resolved.push(segment);
    }
    Ok(resolved)
}

fn validate_agent_history_path(adapter_raw: &str, raw_path: &str) -> Result<String, Failure> {
    let adapter = if adapter_raw.trim().is_empty() {
        "claude"
    } else {
        adapter_raw.trim()
    };
    if !matches!(adapter, "claude" | "codex") {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "conversation.import.provider_invalid",
            "adapter must be 'claude' or 'codex'",
        ));
    }
    if std::env::var("MUTEKI_HOST_DISCOVERY").is_ok_and(|v| v.trim() == "0") {
        return Err(fail(
            StatusCode::CONFLICT,
            "conversation.import.host_discovery_disabled",
            "Host history discovery is disabled on this service host",
        ));
    }
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        fail(
            StatusCode::BAD_REQUEST,
            "conversation.import.scan_failed",
            "Home directory is unavailable",
        )
    })?;
    let provider_dir = if adapter == "codex" { ".codex" } else { ".claude" };
    let allowed_root = home.join(provider_dir);
    let expanded = if trimmed == "~" {
        home
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    if !expanded.is_absolute()
        || expanded
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(fail(
            StatusCode::FORBIDDEN,
            "conversation.import.path_forbidden",
            format!(
                "Agent history path must be an absolute path inside ~/{provider_dir} without '..'"
            ),
        ));
    }
    let normalized: PathBuf = expanded
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    let normalized_root: PathBuf = allowed_root
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    let canon_root = canonicalize_within_existing(&normalized_root)?;
    let canon_target = canonicalize_within_existing(&normalized)?;
    if !canon_target.starts_with(&canon_root) {
        return Err(fail(
            StatusCode::FORBIDDEN,
            "conversation.import.path_forbidden",
            format!("Agent history scan/import is restricted to ~/{provider_dir}"),
        ));
    }
    Ok(canon_target.to_string_lossy().into_owned())
}

fn string_query(params: &Value, allowed: &[&str]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for key in allowed {
        if let Some(val) = params.get(*key) {
            if let Some(s) = val.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                map.insert((*key).to_owned(), s.to_owned());
            } else if let Some(b) = val.as_bool() {
                map.insert((*key).to_owned(), b.to_string());
            } else if let Some(n) = val.as_i64() {
                map.insert((*key).to_owned(), n.to_string());
            }
        }
    }
    map
}

fn strip_keys(value: &Value, excluded: &[&str]) -> Value {
    let mut out = value.as_object().cloned().unwrap_or_default();
    for key in excluded {
        out.remove(*key);
    }
    Value::Object(out)
}

fn complete_receipt(
    state: &AppState,
    envelope: &Value,
    aggregate_type: &str,
    aggregate_id: &str,
    output: Value,
) -> Result<Response, Failure> {
    let command_id = envelope["command_id"].as_str().unwrap_or("");
    let prior = state
        .store
        .get(COMMANDS, command_id)
        .map_err(storage)?
        .unwrap_or(Value::Null);
    let cursor = domain_event_cursor(
        state,
        aggregate_type,
        aggregate_id,
        aggregate_version(state, aggregate_type, aggregate_id)?,
        envelope["_verified_scope"]
            .as_str()
            .unwrap_or("operator:local-user"),
    )
    .map_err(storage)?;
    let receipt = json!({
        "schema_version": 1,
        "command_id": command_id,
        "receipt_id": prior["response"]["receipt"]["receipt_id"],
        "state": "completed",
        "aggregate": {"type": aggregate_type, "id": aggregate_id},
        "run_id": null,
        "event_cursor": cursor,
        "submitted_at": prior["response"]["receipt"]["submitted_at"],
        "completed_at": now(),
        "deduplicated": false,
        "output": output,
        "effect_ids": [],
        "error": null,
    });
    Ok(Json(json!({"receipt": receipt})).into_response())
}

async fn normalize_or_wrap_receipt(
    state: &AppState,
    envelope: &Value,
    aggregate_type: &str,
    aggregate_id: &str,
    resp: Response,
) -> Result<Response, Failure> {
    let body = response_json(resp).await?;
    if body["receipt"].is_object() {
        Ok(Json(body).into_response())
    } else {
        complete_receipt(state, envelope, aggregate_type, aggregate_id, body)
    }
}

pub(super) async fn query(state: &Arc<AppState>, envelope: &Value) -> Result<Value, Failure> {
    let kind = envelope["query_type"].as_str().unwrap_or("");
    let (_, target) = query_scope(kind, envelope)?;
    let params = &envelope["params"];
    if !params.is_object() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "params must be an object",
        ));
    }

    match kind {
        "workbench.credentials.list" => {
            let section = params["section"].as_str().unwrap_or("all");
            let q = string_query(params, &["environment"]);
            let creds = if section != "endpoints" {
                Some(
                    crate::compat::credentials(State(state.clone()), Query(q))
                        .await
                        .map_err(api_err)?
                        .0,
                )
            } else {
                None
            };
            let endpoints = if section != "credentials" {
                Some(
                    crate::compat::model_endpoints(State(state.clone()))
                        .await
                        .map_err(api_err)?
                        .0,
                )
            } else {
                None
            };
            let mut out = serde_json::Map::new();
            if let Some(c) = creds {
                out.insert("credentials".into(), c);
            }
            if let Some(e) = endpoints {
                out.insert("model_endpoints".into(), e);
            }
            Ok(Value::Object(out))
        }
        "workbench.local_pi.get" => {
            let catalog = response_json(
                crate::runs::local_pi_catalog_route(State(state.clone())).await,
            )
            .await?;
            let settings_resp = response_json(
                crate::runs::local_pi_get_settings_route(State(state.clone())).await,
            )
            .await?;
            Ok(json!({
                "catalog": catalog,
                "settings": settings_resp["settings"],
            }))
        }
        "workbench.usage_quota.get" => {
            let section = params["section"].as_str().unwrap_or("all");
            let mut out = serde_json::Map::new();
            if matches!(section, "all" | "limits") {
                let q = string_query(params, &["refresh"]);
                let Json(v) = crate::quota::limits(State(state.clone()), Query(q))
                    .await
                    .map_err(api_err)?;
                out.insert("limits".into(), v);
            }
            if matches!(section, "all" | "quota") {
                let Json(v) = crate::quota::quota(State(state.clone()))
                    .await
                    .map_err(api_err)?;
                out.insert("quota".into(), v);
            }
            if matches!(section, "all" | "usage_summary") {
                let q = string_query(params, &["days", "engine"]);
                let Json(v) = crate::compat::usage_summary(State(state.clone()), Query(q))
                    .await
                    .map_err(api_err)?;
                out.insert("usage_summary".into(), v);
            }
            if matches!(section, "usage") || (section == "all" && params["include_usage"] == true) {
                let mut q = string_query(
                    params,
                    &["limit", "offset", "days", "engine", "provider", "run_id", "thread_id"],
                );
                q.entry("limit".into()).or_insert_with(|| "50".into());
                let Json(v) = crate::compat::usage(State(state.clone()), Query(q))
                    .await
                    .map_err(api_err)?;
                out.insert("usage".into(), v);
            }
            if matches!(section, "all" | "usage_settings") {
                let Json(v) = crate::quota::usage_settings(State(state.clone())).await;
                out.insert("usage_settings".into(), v);
            }
            if matches!(section, "cursor")
                || (section == "all" && params["include_cursor"] == true)
            {
                let q = string_query(params, &["days", "tz", "refresh"]);
                let Json(v) = crate::quota::cursor_account::read(State(state.clone()), Query(q))
                    .await
                    .map_err(api_err)?;
                out.insert("cursor".into(), v);
            }
            Ok(Value::Object(out))
        }
        "workbench.pricing.get" => {
            if params["items"].is_array() {
                let Json(v) = crate::compat::resolve_prices(
                    State(state.clone()),
                    Json(json!({"items": params["items"]})),
                )
                .await
                .map_err(api_err)?;
                Ok(v)
            } else {
                let q = string_query(params, &["refresh", "search", "engine"]);
                let Json(v) = crate::compat::pricing_catalog(State(state.clone()), Query(q))
                    .await
                    .map_err(api_err)?;
                Ok(v)
            }
        }
        "workbench.capabilities.overview" => Ok(crate::capabilities::overview(State(state.clone()))
            .await
            .map_err(api_err)?
            .0),
        "workbench.chat_plugins.list" => {
            response_json(crate::plugins::api::listing_with(state, &string_query(params, &["engine", "mode", "run_id"])).await).await
        }
        "workbench.extensions.list" => {
            let query_type = match params["query_kind"].as_str().unwrap_or("list") {
                "overview" => "extension.overview",
                "catalog" => "extension.catalog",
                "detail" => "extension.detail",
                _ => "extension.list",
            };
            let mut sub_env = json!({
                "query_id": envelope["query_id"],
                "query_type": query_type,
                "params": strip_keys(params, &["query_kind"]),
            });
            if !target.is_empty() {
                sub_env["aggregate_id"] = json!(target);
            }
            let resp = crate::extensions::dispatch_lifecycle_query(state, &sub_env)
                .await
                .ok_or_else(|| {
                    fail(
                        StatusCode::NOT_IMPLEMENTED,
                        "capability.unavailable",
                        "Extension query unavailable",
                    )
                })?;
            let body = response_json(resp).await?;
            Ok(body.get("result").cloned().unwrap_or(body))
        }
        "workbench.ui_settings.get" => {
            let section = params["section"].as_str().unwrap_or("all");
            if crate::workbench_settings::TARGETS.contains(&section) {
                return crate::workbench_settings::read(state, section).await
                    .map_err(api_err);
            }
            let mut out = serde_json::Map::new();
            if matches!(section, "all" | "ui") {
                out.insert(
                    "ui".into(),
                    crate::compat::get_ui(State(state.clone()))
                        .await
                        .map_err(api_err)?
                        .0,
                );
            }
            if matches!(section, "all" | "conversation_preferences") {
                let q = string_query(params, &["thread_id"]);
                out.insert(
                    "conversation_preferences".into(),
                    crate::compat::get_conversation_preferences(State(state.clone()), Query(q))
                        .await
                        .map_err(api_err)?
                        .0,
                );
            }
            if matches!(section, "all" | "sidebar") {
                let query = crate::workspaces::SidebarQuery {
                    scope: params["scope"].as_str().map(str::to_owned),
                };
                out.insert(
                    "sidebar".into(),
                    crate::workspaces::get_sidebar(State(state.clone()), Query(query))
                        .await
                        .map_err(api_err)?
                        .0,
                );
            }
            Ok(Value::Object(out))
        }
        "workbench.control.state" => {
            let limit = params["limit"].as_u64().unwrap_or(50) as usize;
            let parent = params["parent_thread_id"].as_str().unwrap_or("");
            Ok(crate::control::state_snapshot(state.clone(), limit, parent)
                .await
                .map_err(api_err)?
                .0)
        }
        "workbench.history_import.scan" => {
            let mut clean = params.clone();
            let adapter = match clean.get("adapter") {
                None | Some(Value::Null) => "claude".to_owned(),
                Some(Value::String(s)) => s.clone(),
                Some(_) => {
                    return Err(fail(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "platform.request.invalid",
                        "adapter must be a string",
                    ));
                }
            };
            let raw_path = match clean.get("path") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(_) => {
                    return Err(fail(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "platform.request.invalid",
                        "path must be a string",
                    ));
                }
            };
            let safe_path = validate_agent_history_path(&adapter, &raw_path)?;
            clean["path"] = json!(safe_path);
            let query: crate::provider_history_import::ScanQuery =
                serde_json::from_value(clean).map_err(|e| {
                    fail(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "platform.request.invalid",
                        format!("Invalid scan parameters: {e}"),
                    )
                })?;
            Ok(crate::provider_history_import::scan(Query(query))
                .await
                .map_err(api_err)?
                .0)
        }
        "workbench.workspaces.list" => match params["kind"].as_str().unwrap_or("workspaces") {
            "workspace" => Ok(crate::workspaces::get_workspace(State(state.clone()), Path(target))
                .await
                .map_err(api_err)?
                .0),
            "project_worktrees" => Ok(
                crate::workspaces::list_project_worktrees(State(state.clone()), Path(target))
                    .await
                    .map_err(api_err)?
                    .0,
            ),
            "folders" => Ok(crate::workspaces::list_folders(State(state.clone()))
                .await
                .map_err(api_err)?
                .0),
            _ => {
                let q: crate::workspaces::WorkspaceQuery = serde_json::from_value(params.clone())
                    .unwrap_or_else(|_| serde_json::from_value(json!({})).expect("empty query"));
                Ok(
                    crate::workspaces::list_workspaces(State(state.clone()), Query(q))
                        .await
                        .map_err(api_err)?
                        .0,
                )
            }
        },
        "workbench.shares.list" => Ok(crate::shares::list(State(state.clone()), Path(target))
            .await
            .map_err(api_err)?
            .0),
        "workbench.operations.get" => match params["kind"].as_str().unwrap_or("overview") {
            "outbox" => {
                let q = OutboxQuery {
                    limit: params["limit"].as_u64().map(|n| n as usize),
                };
                Ok(super::outbox(State(state.clone()), Query(q)).await?.0)
            }
            "recovery_preview" => Ok(super::recovery_preview(State(state.clone())).await?.0),
            "maintenance_preview" => Ok(super::maintenance_preview(State(state.clone())).await?.0),
            "effects" => {
                let q = EffectQuery {
                    state: params["state"].as_str().map(str::to_owned),
                    limit: params["limit"].as_u64().map(|n| n as usize),
                };
                Ok(super::effects(State(state.clone()), Query(q)).await?.0)
            }
            "domain_modules" => {
                if !target.is_empty() {
                    response_json(super::module(State(state.clone()), Path(target)).await).await
                } else {
                    Ok(super::modules(State(state.clone())).await?.0)
                }
            }
            _ => Ok(super::overview(State(state.clone())).await?.0),
        },
        "workbench.runner_resources.get" => {
            if let Some(node) = params["node_id"].as_str() {
                return Ok(crate::runner_resources::node_status(State(state.clone()), Path(node.to_owned())).await.map_err(api_err)?.0);
            }
            let resources = crate::runner_resources::status(State(state.clone()))
                .await
                .map_err(api_err)?
                .0;
            if params["include_nodes"].as_bool().unwrap_or(true) {
                let (_, Json(remote_nodes)) = crate::runner_remote::nodes(State(state.clone()))
                    .await
                    .map_err(api_err)?;
                Ok(json!({
                    "resources": resources,
                    "remote_nodes": remote_nodes,
                }))
            } else {
                Ok(resources)
            }
        }
        "workbench.system_status.get" => {
            let mode = params["kind"].as_str().unwrap_or("all");
            let mut out = serde_json::Map::new();
            if matches!(mode, "all" | "readiness") {
                let readiness = response_json(
                    crate::system_api::readiness(State(state.clone())).await,
                )
                .await?;
                out.insert("readiness".into(), readiness);
            }
            if matches!(mode, "all" | "update") {
                let Json(update) = crate::system_api::update_status().await;
                if let Some(val) = update.get("update").cloned() {
                    out.insert("update".into(), val);
                } else {
                    out.insert("update".into(), update);
                }
            }
            Ok(Value::Object(out))
        }
        "workbench.worker_models.get" => {
            let Json(models) = crate::runs::settings::get_worker_models(State(state.clone())).await;
            let q = string_query(params, &["backend"]);
            let Json(profiles_health) =
                crate::runs::settings::profiles_health_get(State(state.clone()), Query(q)).await;
            Ok(json!({
                "worker_models": models,
                "profiles_health": profiles_health,
            }))
        }
        "workbench.worker_image.get" => Ok(crate::runs::settings::get_worker_image().await.0),
        "workbench.run_retention.get" => Ok(crate::runs::get_retention(State(state.clone())).await.map_err(api_err)?.0),
        _ => Err(fail(
            StatusCode::NOT_IMPLEMENTED,
            "capability.unavailable",
            "Unknown workbench query",
        )),
    }
}

pub(super) async fn command(state: Arc<AppState>, envelope: &Value) -> Result<Response, Failure> {
    let kind = envelope["command_type"].as_str().unwrap_or("");
    let (scope_type, target) = command_scope(kind, envelope)?;
    let payload = &envelope["payload"];
    if !payload.is_object() {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "platform.request.invalid",
            "payload must be an object",
        ));
    }

    match kind {
        "workbench.credential_account.upsert" => {
            let raw_id = strip_account_prefix(&target);
            let existing = crate::provider::matching_account(&state.store, &raw_id)
                .map_err(storage)?;
            let mut body = strip_keys(payload, &["account_id", "id"]);
            if let Some(existing) = existing.as_ref() {
                let obj = body.as_object_mut().expect("object checked");
                if !obj.contains_key("worker_engine")
                    && !obj.contains_key("target_engine")
                    && !obj.contains_key("engine")
                {
                    for key in ["worker_engine", "target_engine", "engine"] {
                        if let Some(v) = existing.get(key).filter(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
                            obj.insert("worker_engine".into(), v.clone());
                            break;
                        }
                    }
                }
                if !obj.contains_key("connection") {
                    if let Some(v) = existing.get("connection").filter(|v| !v.is_null()) {
                        obj.insert("connection".into(), v.clone());
                    }
                }
                if !obj.contains_key("base_url")
                    && obj.get("connection").and_then(Value::as_str) == Some("custom_endpoint")
                {
                    if let Some(v) = existing.get("base_url").filter(|v| !v.is_null()) {
                        obj.insert("base_url".into(), v.clone());
                    }
                }
                if !obj.contains_key("provider") {
                    if let Some(v) = existing.get("provider").filter(|v| !v.is_null()) {
                        obj.insert("provider".into(), v.clone());
                    }
                }
            }
            let Json(res) = crate::compat::put_credential_account(
                State(state.clone()),
                Path(raw_id.clone()),
                Json(body),
            )
            .await
            .map_err(api_err)?;
            complete_receipt(&state, envelope, scope_type, &raw_id, res)
        }
        "workbench.credential_account.delete" => {
            let raw_id = strip_account_prefix(&target);
            let q = string_query(payload, &["detach_references"]);
            let Json(res) = crate::compat::delete_credential_account(
                State(state.clone()),
                Path(raw_id.clone()),
                Query(q),
            )
            .await
            .map_err(api_err)?;
            complete_receipt(&state, envelope, scope_type, &raw_id, res)
        }
        "workbench.model_endpoint.upsert" => {
            let raw_id = target.trim_start_matches("endpoint:").trim().to_owned();
            let lookup = format!("endpoint:{raw_id}");
            let existing = crate::provider::matching_account(&state.store, &lookup)
                .map_err(storage)?;
            let mut body = strip_keys(payload, &["endpoint_id"]);
            body["id"] = json!(raw_id);
            if let Some(existing) = existing {
                let obj = body.as_object_mut().expect("object checked");
                if !obj.contains_key("base_url") {
                    if let Some(v) = existing.get("base_url").filter(|v| !v.is_null()) {
                        obj.insert("base_url".into(), v.clone());
                    }
                }
                if !obj.contains_key("model") {
                    let fallback = existing
                        .get("default_model")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            existing
                                .get("models")
                                .and_then(Value::as_array)
                                .and_then(|arr| arr.first())
                                .and_then(Value::as_str)
                        })
                        .unwrap_or("");
                    if !fallback.is_empty() {
                        obj.insert("model".into(), json!(fallback));
                    }
                }
                if !obj.contains_key("provider") {
                    if let Some(v) = existing.get("provider").filter(|v| !v.is_null()) {
                        obj.insert("provider".into(), v.clone());
                    }
                }
            }
            let Json(res) = crate::compat::put_model_endpoint(State(state.clone()), Json(body))
                .await
                .map_err(api_err)?;
            complete_receipt(&state, envelope, scope_type, &raw_id, res)
        }
        "workbench.model_endpoint.delete" => {
            let raw_id = target.trim_start_matches("endpoint:").trim().to_owned();
            let q = string_query(payload, &["detach_references"]);
            let Json(res) = crate::compat::delete_model_endpoint(
                State(state.clone()),
                Path(raw_id.clone()),
                Query(q),
            )
            .await
            .map_err(api_err)?;
            complete_receipt(&state, envelope, scope_type, &raw_id, res)
        }
        "workbench.credential_models.refresh" => {
            if payload["stale_only"] == true || target.is_empty() {
                let q = string_query(payload, &["environment"]);
                let Json(res) =
                    crate::model_settings_api::refresh_stale(State(state.clone()), Query(q))
                        .await
                        .map_err(api_err)?;
                return complete_receipt(&state, envelope, scope_type, "stale", res);
            }
            let clean = strip_keys(payload, &["account_id", "id", "stale_only"]);
            let Json(res) = crate::model_settings_api::refresh_models(
                State(state.clone()),
                Path(target.clone()),
                Some(Json(clean)),
            )
            .await
            .map_err(api_err)?;
            complete_receipt(&state, envelope, scope_type, &target, res)
        }
        "workbench.credential_account.test" => {
            let clean = strip_keys(payload, &["account_id", "endpoint_id", "id"]);
            let Json(res) = crate::model_settings_api::test_credential(
                State(state.clone()),
                Path(target.clone()),
                Some(Json(clean)),
            )
            .await
            .map_err(api_err)?;
            complete_receipt(&state, envelope, scope_type, &target, res)
        }
        "workbench.local_pi.update" => {
            let action = payload["action"].as_str().unwrap_or("update");
            let body = payload
                .get("settings")
                .filter(|v| v.is_object())
                .cloned()
                .unwrap_or_else(|| strip_keys(payload, &["action"]));
            let resp = if action == "probe" {
                crate::runs::local_pi_probe_route(State(state.clone()), Some(Json(body))).await
            } else {
                crate::runs::local_pi_put_settings_route(State(state.clone()), Json(body)).await
            };
            let res = response_json(resp).await?;
            complete_receipt(&state, envelope, scope_type, "local_pi", res)
        }
        "workbench.quota.refresh" => {
            let action = payload["action"].as_str().unwrap_or("refresh");
            if action == "update_usage_settings" {
                let body = strip_keys(payload, &["action"]);
                let Json(res) =
                    crate::quota::put_usage_settings(State(state.clone()), Json(body))
                        .await
                        .map_err(api_err)?;
                return complete_receipt(&state, envelope, scope_type, "usage_settings", res);
            }
            let id = named(payload, &["account_id", "id"]);
            if id.is_empty() {
                let mut q = HashMap::new();
                q.insert("refresh".into(), "true".into());
                let Json(res) = crate::quota::limits(State(state.clone()), Query(q))
                    .await
                    .map_err(api_err)?;
                complete_receipt(&state, envelope, scope_type, "all", res)
            } else {
                let Json(res) =
                    crate::quota::refresh_quota(State(state.clone()), Path(id.to_owned()))
                        .await
                        .map_err(api_err)?;
                complete_receipt(&state, envelope, scope_type, id, res)
            }
        }
        "workbench.pricing.update" => {
            let action = payload["action"].as_str().unwrap_or("");
            let body = strip_keys(payload, &["action"]);
            let Json(res) = match action {
                "put_override" => {
                    crate::compat::put_price_override(State(state.clone()), Json(body)).await
                }
                "delete_override" => {
                    crate::compat::delete_price_override(State(state.clone()), Json(body)).await
                }
                "put_reference" => {
                    crate::compat::put_price_reference(State(state.clone()), Json(body)).await
                }
                "delete_reference" => {
                    crate::compat::delete_price_reference(State(state.clone()), Json(body)).await
                }
                _ => {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "platform.request.invalid",
                        "action must be put_override, delete_override, put_reference, or delete_reference",
                    ));
                }
            }
            .map_err(api_err)?;
            complete_receipt(&state, envelope, scope_type, "pricing", res)
        }
        "workbench.runtime_instance.manage" => {
            let action = payload["action"].as_str().unwrap_or("probe");
            if action == "upsert_profile" {
                let mut body = strip_keys(payload, &["action", "key"]);
                body["command_id"] = envelope["command_id"].clone();
                body["idempotency_key"] = envelope["idempotency_key"].clone();
                let response = crate::runtime_api::upsert_profile(State(state.clone()), Some(Json(body))).await;
                return normalize_or_wrap_receipt(&state, envelope, scope_type, &target, response).await;
            }
            if action == "delete_profile" {
                let name = named(payload, &["name", "key"]).to_owned();
                let mut query = string_query(payload, &["expected_revision"]);
                for field in ["command_id", "idempotency_key"] {
                    if let Some(value) = envelope[field].as_str() { query.insert(field.into(), value.into()); }
                }
                let response = crate::runtime_api::remove_profile(State(state.clone()), Path(name), Query(query)).await;
                return normalize_or_wrap_receipt(&state, envelope, scope_type, &target, response).await;
            }
            let command_type = match action {
                "upsert" | "create" | "update" => "runtime.instance.upsert",
                "remove" | "delete" => "runtime.instance.remove",
                "probe" => "runtime.instance.probe",
                "refresh" => "runtime.instance.refresh",
                "capability_refresh" => "runtime.instance.capability.refresh",
                "auth_refresh" => "runtime.instance.auth.refresh",
                "login" => "runtime.instance.login",
                "logout" => "runtime.instance.logout",
                _ => {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "platform.request.invalid",
                        "Unknown runtime instance action",
                    ));
                }
            };
            let mut inner_payload = strip_keys(payload, &["action", "key"]);
            if !target.is_empty() && inner_payload.get("instance_id").is_none() {
                if let Some((adapter, inst)) = target.split_once(':') {
                    inner_payload
                        .as_object_mut()
                        .unwrap()
                        .entry("adapter_id")
                        .or_insert_with(|| json!(adapter));
                    inner_payload
                        .as_object_mut()
                        .unwrap()
                        .entry("instance_id")
                        .or_insert_with(|| json!(inst));
                }
            }
            let sub_env = json!({
                "command_id": envelope["command_id"],
                "idempotency_key": envelope["idempotency_key"],
                "command_type": command_type,
                "aggregate_type": "runtime",
                "aggregate_id": target,
                "payload": inner_payload,
                "_verified_scope": envelope["_verified_scope"],
            });
            let resp = crate::runtime_api::dispatch_command_envelope(state.clone(), &sub_env).await;
            normalize_or_wrap_receipt(&state, envelope, scope_type, &target, resp).await
        }
        "workbench.capabilities.update" => match payload["action"].as_str().unwrap_or("set_resource") {
            "set_resource" => {
                let kind_seg = payload["kind"].as_str().unwrap_or("").trim().to_owned();
                if !matches!(kind_seg.as_str(), "mcp" | "skills") {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "platform.request.invalid",
                        "kind must be mcp or skills",
                    ));
                }
                let resource = payload["resource"].as_str().unwrap_or("").trim().to_owned();
                let Some(enabled) = payload["enabled"].as_bool() else {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "platform.request.invalid",
                        "enabled must be a boolean",
                    ));
                };
                if resource.is_empty() {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "platform.request.invalid",
                        "resource is required",
                    ));
                }
                let Json(res) = crate::capabilities::update_resource(
                    State(state.clone()),
                    Path((kind_seg.clone(), resource.clone())),
                    Some(Json(json!({"enabled": enabled}))),
                )
                .await
                .map_err(api_err)?;
                complete_receipt(
                    &state,
                    envelope,
                    scope_type,
                    &format!("{kind_seg}:{resource}"),
                    res,
                )
            }
            "replace_binding" => {
                let thread_id = named(payload, &["thread_id"]);
                if thread_id.is_empty() {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "conversation.thread.id_required",
                        "thread_id is required",
                    ));
                }
                let body = strip_keys(payload, &["action", "thread_id"]);
                let Json(res) = crate::capabilities::replace_binding(
                    State(state.clone()),
                    Path(thread_id.to_owned()),
                    Some(Json(body)),
                )
                .await
                .map_err(api_err)?;
                complete_receipt(&state, envelope, scope_type, thread_id, res)
            }
            "revoke_binding" => {
                let thread_id = named(payload, &["thread_id"]);
                if thread_id.is_empty() {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "conversation.thread.id_required",
                        "thread_id is required",
                    ));
                }
                let body = strip_keys(payload, &["action", "thread_id"]);
                let Json(res) = crate::capabilities::revoke_binding(
                    State(state.clone()),
                    Path(thread_id.to_owned()),
                    Some(Json(body)),
                )
                .await
                .map_err(api_err)?;
                complete_receipt(&state, envelope, scope_type, thread_id, res)
            }
            _ => Err(fail(
                StatusCode::BAD_REQUEST,
                "platform.request.invalid",
                "action must be set_resource, replace_binding, or revoke_binding",
            )),
        },
        "workbench.chat_plugin.manage" => {
            let action = payload["action"].as_str().unwrap_or("");
            let pkg = named(payload, &["package_id"]).to_owned();
            let resp = match action {
                "install" => {
                    let source = payload.get("source").cloned().unwrap_or_else(|| {
                        payload
                            .get("url")
                            .or_else(|| payload.get("path"))
                            .cloned()
                            .unwrap_or(Value::Null)
                    });
                    if source.is_null() {
                        return Err(fail(
                            StatusCode::BAD_REQUEST,
                            "platform.request.invalid",
                            "source is required for install",
                        ));
                    }
                    crate::plugins::api::install_with(
                        &state,
                        source,
                        payload.get("modes").cloned(),
                    )
                    .await
                }
                "install_visualize" => crate::plugins::api::install_visualize_with(&state).await,
                "add_mcp" => {
                            let name = named(payload, &["name"]).to_owned();
                    let Some(servers) = payload.get("servers").cloned() else {
                        return Err(fail(
                            StatusCode::BAD_REQUEST,
                            "platform.request.invalid",
                            "servers object is required for add_mcp",
                        ));
                    };
                    if name.is_empty() {
                        return Err(fail(
                            StatusCode::BAD_REQUEST,
                            "platform.request.invalid",
                            "name is required for add_mcp",
                        ));
                    }
                    crate::plugins::api::add_mcp_with(
                        &state,
                        name,
                        servers,
                        payload.get("modes").cloned(),
                    )
                    .await
                }
                "update" => {
                    if pkg.is_empty() {
                        return Err(fail(
                            StatusCode::BAD_REQUEST,
                            "platform.request.invalid",
                            "package_id is required for update",
                        ));
                    }
                    let snap = payload["snapshot_id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .or_else(|| payload["digest"].as_str())
                        .unwrap_or("")
                        .to_owned();
                    crate::plugins::api::update_with(
                        &state,
                        pkg.clone(),
                        payload["enabled"].as_bool(),
                        payload["rollback"] == true,
                        payload["native_hooks"].as_bool(),
                        snap,
                        payload.get("modes").cloned(),
                    )
                    .await
                }
                "uninstall" => {
                    if pkg.is_empty() {
                        return Err(fail(
                            StatusCode::BAD_REQUEST,
                            "platform.request.invalid",
                            "package_id is required for uninstall",
                        ));
                    }
                    crate::plugins::api::uninstall_with(&state, pkg.clone()).await
                }
                "check" => {
                    if pkg.is_empty() {
                        return Err(fail(
                            StatusCode::BAD_REQUEST,
                            "platform.request.invalid",
                            "package_id is required for check",
                        ));
                    }
                    crate::plugins::api::check_with(&state, pkg.clone()).await
                }
                "set_control" => {
                    let Some(enabled) = payload["enabled"].as_bool() else {
                        return Err(fail(
                            StatusCode::BAD_REQUEST,
                            "platform.request.invalid",
                            "enabled boolean is required for set_control",
                        ));
                    };
                    crate::plugins::api::control_with(&state, enabled).await
                }
                _ => {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "platform.request.invalid",
                        "Unknown chat_plugin action",
                    ));
                }
            };
            let res = response_json(resp).await?;
            complete_receipt(
                &state,
                envelope,
                scope_type,
                if pkg.is_empty() { "chat_plugins" } else { &pkg },
                res,
            )
        }
        "workbench.extension.manage" => {
            let action = payload["action"].as_str().unwrap_or("");
            if action == "preview" {
                let response = crate::extensions::preview(State(state.clone()), Json(strip_keys(payload, &["action"]))).await;
                let result = response_json(response).await?;
                return complete_receipt(&state, envelope, scope_type, &target, result);
            }
            let command_type = match action {
                "install" => "extension.install",
                "upgrade" => "extension.upgrade",
                "enable" | "config" => "extension.enable",
                "disable" => "extension.disable",
                "uninstall" => "extension.uninstall",
                "rollback" => "extension.rollback",
                "health_refresh" => "extension.health_refresh",
                _ => {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "platform.request.invalid",
                        "action must be preview, install, upgrade, enable, disable, uninstall, rollback, config, or health_refresh",
                    ));
                }
            };
            let mut inner = strip_keys(payload, &["action"]);
            if !target.is_empty() && inner.get("extension_id").is_none() {
                inner["extension_id"] = json!(target);
            }
            let sub_env = json!({
                "command_id": envelope["command_id"],
                "idempotency_key": envelope["idempotency_key"],
                "command_type": command_type,
                "aggregate_type": "extension",
                "aggregate_id": target,
                "payload": inner,
                "_verified_scope": envelope["_verified_scope"],
            });
            let resp = crate::extensions::dispatch_lifecycle_command(&state, &sub_env).await;
            normalize_or_wrap_receipt(&state, envelope, scope_type, &target, resp).await
        }
        "workbench.ui_settings.update" => match payload["target"].as_str().unwrap_or("ui") {
            target if target == "thread_grant" || crate::workbench_settings::TARGETS.contains(&target) => {
                let values = payload.get("values").cloned()
                    .unwrap_or_else(|| strip_keys(payload, &["target", "replace", "version"]));
                let caller = envelope["actor"]["thread_id"].as_str().or_else(|| envelope["actor"]["id"].as_str()).unwrap_or("");
                let saved = crate::workbench_settings::update(&state, caller, target, values).await
                    .map_err(api_err)?;
                complete_receipt(&state, envelope, scope_type, target, saved)
            }
            "conversation_preferences" => {
                let mut body = payload.get("values").cloned().unwrap_or_else(|| json!({}));
                let fields = body.as_object_mut().ok_or_else(|| fail(StatusCode::BAD_REQUEST, "platform.request.invalid", "values must be an object"))?;
                fields.extend(strip_keys(payload, &["target", "replace", "values"]).as_object().expect("object").clone());
                let thread_id = body["thread_id"].as_str().unwrap_or("").trim().to_owned();
                if body.get("version").is_none() && body.get("interaction_mode").is_none() {
                    let mut q = HashMap::new();
                    if !thread_id.is_empty() {
                        q.insert("thread_id".into(), thread_id.clone());
                    }
                    let Json(curr) =
                        crate::compat::get_conversation_preferences(State(state.clone()), Query(q))
                            .await
                            .map_err(api_err)?;
                    if let Some(v) = curr.get("version").cloned() {
                        body["version"] = v;
                    }
                }
                let mut q = HashMap::new();
                if !thread_id.is_empty() {
                    q.insert("thread_id".into(), thread_id);
                }
                let (status, Json(res)) = crate::compat::put_conversation_preferences(
                    State(state.clone()),
                    Query(q),
                    Json(body),
                )
                .await
                .map_err(api_err)?;
                if !status.is_success() {
                    return Err(api_err((status, Json(res))));
                }
                complete_receipt(&state, envelope, scope_type, "conversation_preferences", res)
            }
            "sidebar" => {
                let query = crate::workspaces::SidebarQuery {
                    scope: payload["scope"].as_str().map(str::to_owned),
                };
                let mut body = strip_keys(payload, &["target", "replace", "values", "scope"]);
                if let Some(nested) = payload.get("values").and_then(Value::as_object) {
                    let map = body.as_object_mut().expect("object checked");
                    for (k, v) in nested {
                        map.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
                if payload["replace"] != true {
                    let Json(curr) =
                        crate::workspaces::get_sidebar(State(state.clone()), Query(query.clone()))
                            .await
                            .map_err(api_err)?;
                    let curr_vals = curr
                        .get("values")
                        .and_then(Value::as_object)
                        .or_else(|| curr.as_object());
                    if let Some(curr_vals) = curr_vals {
                        let map = body.as_object_mut().expect("object checked");
                        for (k, v) in curr_vals {
                            if k != "version" {
                                map.entry(k.clone()).or_insert_with(|| v.clone());
                            }
                        }
                    }
                    if body.get("version").is_none() {
                        if let Some(v) = curr.get("version").cloned() {
                            body["version"] = v;
                        }
                    }
                }
                let Json(res) =
                    crate::workspaces::put_sidebar(State(state.clone()), Query(query), Json(body))
                        .await
                        .map_err(api_err)?;
                complete_receipt(&state, envelope, scope_type, "sidebar", res)
            }
            "ui" => {
                let patch_vals = payload
                    .get("values")
                    .cloned()
                    .unwrap_or_else(|| strip_keys(payload, &["target", "version", "replace"]));
                let Some(patch_obj) = patch_vals.as_object() else {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "platform.request.invalid",
                        "UI preference values must be an object",
                    ));
                };
                let Json(curr) = crate::compat::get_ui(State(state.clone()))
                    .await
                    .map_err(api_err)?;
                let version = payload
                    .get("version")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| curr["version"].as_u64().unwrap_or(0));
                let mut merged = if payload["replace"] == true {
                    serde_json::Map::new()
                } else {
                    curr.get("values")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default()
                };
                for (k, v) in patch_obj {
                    if v.is_null() {
                        merged.remove(k);
                    } else {
                        merged.insert(k.clone(), v.clone());
                    }
                }
                let req = json!({
                    "version": version,
                    "values": Value::Object(merged),
                });
                let (status, Json(res)) =
                    crate::compat::put_ui(State(state.clone()), Json(req))
                        .await
                        .map_err(api_err)?;
                if !status.is_success() {
                    return Err(api_err((status, Json(res))));
                }
                complete_receipt(&state, envelope, scope_type, "ui", res)
            }
            target => Err(fail(StatusCode::BAD_REQUEST, "settings.target_unknown", format!("Unknown settings target: {target}"))),
        },
        "workbench.history_import.apply" => {
            let mut clean = payload.clone();
            let adapter = match clean.get("adapter_id") {
                None | Some(Value::Null) => "claude".to_owned(),
                Some(Value::String(s)) => s.clone(),
                Some(_) => {
                    return Err(fail(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "platform.request.invalid",
                        "adapter_id must be a string",
                    ));
                }
            };
            let raw_path = match clean.get("source_path") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(_) => {
                    return Err(fail(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "platform.request.invalid",
                        "source_path must be a string",
                    ));
                }
            };
            let safe_path = validate_agent_history_path(&adapter, &raw_path)?;
            clean["source_path"] = json!(safe_path);
            let apply_req: crate::provider_history_import::Apply =
                serde_json::from_value(clean).map_err(|e| {
                    fail(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "platform.request.invalid",
                        format!("Invalid history import payload: {e}"),
                    )
                })?;
            let Json(res) =
                crate::provider_history_import::apply(State(state.clone()), Json(apply_req))
                    .await
                    .map_err(api_err)?;
            complete_receipt(&state, envelope, scope_type, "history_import", res)
        }
        "workbench.workspace.manage" => {
            let action = payload["action"].as_str().unwrap_or("");
            match action {
                "create_workspace" => {
                    let mut body = strip_keys(payload, &["action"]);
                    for meta in ["command_id", "idempotency_key"] {
                        if body.get(meta).is_none() && !envelope[meta].is_null() {
                            body[meta] = envelope[meta].clone();
                        }
                    }
                    body["_platform_envelope"] = envelope.clone();
                    let Json(res) =
                        crate::workspaces::bind_workspace(State(state.clone()), Json(body))
                            .await
                            .map_err(api_err)?;
                    Ok(Json(res).into_response())
                }
                "update_workspace" => {
                    let mut body = strip_keys(payload, &["action", "workspace_id", "id"]);
                    for meta in ["command_id", "idempotency_key"] {
                        if body.get(meta).is_none() && !envelope[meta].is_null() {
                            body[meta] = envelope[meta].clone();
                        }
                    }
                    body["_platform_envelope"] = envelope.clone();
                    let Json(res) = crate::workspaces::update_workspace(
                        State(state.clone()),
                        Path(target.clone()),
                        Json(body),
                    )
                    .await
                    .map_err(api_err)?;
                    Ok(Json(res).into_response())
                }
                "delete_workspace" => {
                    let Json(res) = crate::workspaces::delete_workspace(
                        State(state.clone()),
                        Path(target.clone()),
                    )
                    .await
                    .map_err(api_err)?;
                    complete_receipt(&state, envelope, scope_type, &target, res)
                }
                "delete_worktree" => {
                    let mut body = strip_keys(payload, &["action"]);
                    for meta in ["command_id", "idempotency_key"] {
                        if body.get(meta).is_none() && !envelope[meta].is_null() {
                            body[meta] = envelope[meta].clone();
                        }
                    }
                    body["_platform_envelope"] = envelope.clone();
                    let Json(res) =
                        crate::workspaces::delete_worktree(State(state.clone()), Json(body))
                            .await
                            .map_err(api_err)?;
                    if res["receipt"].is_object() {
                        Ok(Json(res).into_response())
                    } else {
                        complete_receipt(&state, envelope, scope_type, &target, res)
                    }
                }
                "delete_project" => {
                    let Json(res) = crate::workspaces::delete_project(
                        State(state.clone()),
                        Path(target.clone()),
                    )
                    .await
                    .map_err(api_err)?;
                    complete_receipt(&state, envelope, scope_type, &target, res)
                }
                _ => Err(fail(
                    StatusCode::BAD_REQUEST,
                    "platform.request.invalid",
                    "action must be create_workspace, update_workspace, delete_workspace, delete_worktree, or delete_project",
                )),
            }
        }
        "workbench.folder.manage" => {
            let action = payload["action"].as_str().unwrap_or("");
            let body = strip_keys(payload, &["action", "folder_id", "id"]);
            let Json(res) = match action {
                "create" => {
                    crate::workspaces::create_folder(State(state.clone()), Json(body))
                        .await
                        .map_err(api_err)?
                }
                "update" => crate::workspaces::update_folder(
                    State(state.clone()),
                    Path(target.clone()),
                    Json(body),
                )
                .await
                .map_err(api_err)?,
                "delete" => crate::workspaces::delete_folder(
                    State(state.clone()),
                    Path(target.clone()),
                )
                .await
                .map_err(api_err)?,
                _ => {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "platform.request.invalid",
                        "action must be create, update, or delete",
                    ));
                }
            };
            complete_receipt(&state, envelope, scope_type, &target, res)
        }
        "workbench.memory.manage" => {
            let action = payload["action"].as_str().unwrap_or("");
            let command_type = match action {
                "record" => "conversation.memory.record",
                "delete" => "conversation.memory.delete",
                _ => {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "platform.request.invalid",
                        "action must be record or delete",
                    ));
                }
            };
            let sub_env = json!({
                "command_id": envelope["command_id"],
                "idempotency_key": envelope["idempotency_key"],
                "command_type": command_type,
                "aggregate_type": "thread",
                "aggregate_id": target,
                "payload": strip_keys(payload, &["action"]),
                "_verified_scope": envelope["_verified_scope"],
            });
            Box::pin(super::agent_dispatch::command(state, &sub_env)).await
        }
        "workbench.share.manage" => match payload["action"].as_str().unwrap_or("") {
            "create" => {
                let body = strip_keys(payload, &["action", "thread_id"]);
                let Json(res) =
                    crate::shares::create_direct(state.clone(), target.clone(), body)
                        .await
                        .map_err(api_err)?;
                complete_receipt(&state, envelope, scope_type, &target, res)
            }
            "revoke" => {
                let share_id = named(payload, &["share_id"]);
                if share_id.is_empty() {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        "share.id_required",
                        "share_id is required for revoke",
                    ));
                }
                let Json(res) = crate::shares::revoke(
                    State(state.clone()),
                    Path((target.clone(), share_id.to_owned())),
                )
                .await
                .map_err(api_err)?;
                complete_receipt(&state, envelope, scope_type, &target, res)
            }
            _ => Err(fail(
                StatusCode::BAD_REQUEST,
                "platform.request.invalid",
                "action must be create or revoke",
            )),
        },
        "workbench.platform_effect.manage" => {
            let action = payload["action"].as_str().unwrap_or("");
            if !matches!(action, "retry" | "cancel") {
                return Err(fail(
                    StatusCode::BAD_REQUEST,
                    "platform.request.invalid",
                    "action must be retry or cancel",
                ));
            }
            if super::competition_api::is_lease_effect(&state, &target).map_err(storage)? {
                let body = json!({
                    "confirm": payload["confirm"] == true,
                    "command_id": envelope["command_id"],
                    "idempotency_key": envelope["idempotency_key"],
                });
                let sub_env = super::lease_effect_envelope(&target, action, &body);
                let resp =
                    super::competition_api::dispatch_command_envelope(state.clone(), sub_env).await;
                return normalize_or_wrap_receipt(&state, envelope, scope_type, &target, resp).await;
            }
            let Json(res) = if action == "retry" {
                super::platform_effects::retry(&state, &target).await?
            } else {
                super::platform_effects::cancel(&state, &target, payload["confirm"] == true)?
            };
            complete_receipt(&state, envelope, scope_type, &target, res)
        }
        "workbench.maintenance.manage" => {
            let body = strip_keys(payload, &["action"]);
            let Json(res) = super::maintenance(State(state.clone()), Json(body)).await?;
            complete_receipt(&state, envelope, scope_type, "maintenance", res)
        }
        "workbench.domain_module.manage" => {
            let action = payload["action"].as_str().unwrap_or("");
            if action == "retry" {
                let module = crate::business::Module::from_descriptor(&target).ok_or_else(|| {
                    fail(
                        StatusCode::NOT_FOUND,
                        "module.not_found",
                        "Unknown built-in module",
                    )
                })?;
                crate::business::retry(state.clone(), module);
                return complete_receipt(
                    &state,
                    envelope,
                    scope_type,
                    &target,
                    json!({"module_id": target, "state": "recovery_requested"}),
                );
            }
            if !matches!(action, "enable" | "disable") {
                return Err(fail(
                    StatusCode::BAD_REQUEST,
                    "platform.request.invalid",
                    "action must be enable, disable, or retry",
                ));
            }
            let mod_env = json!({
                "command_id": envelope["command_id"],
                "idempotency_key": envelope["idempotency_key"],
                "command_type": format!("module.{action}"),
                "aggregate_type": "module",
                "aggregate_id": target,
                "payload": {"module_id": target},
                "expected_version": envelope.get("expected_version").cloned().unwrap_or(Value::Null),
                "_verified_scope": envelope["_verified_scope"],
            });
            let (status, res) = super::run_module_command(&state, &mod_env)?;
            if !status.is_success() {
                return Err(api_err((status, Json(res))));
            }
            if action == "enable" {
                if let Some(module) = crate::business::Module::from_descriptor(&target) {
                    crate::business::retry(state.clone(), module);
                }
            }
            if res["receipt"].is_object() {
                Ok(Json(res).into_response())
            } else {
                complete_receipt(&state, envelope, scope_type, &target, res)
            }
        }
        "workbench.runner_resources.update" => {
            let node = payload["node_id"].as_str().unwrap_or("local").to_owned();
            let action = payload["action"].as_str().unwrap_or("policy");
            let body = strip_keys(payload, &["action", "node_id", "dispatch_id"]);
            let invalid = |e: serde_json::Error| fail(StatusCode::UNPROCESSABLE_ENTITY, "platform.request.invalid", e.to_string());
            let res = match action {
                "policy" => {
                    let policy = serde_json::from_value(body).map_err(invalid)?;
                    if node == "local" {
                        crate::runner_resources::update(State(state.clone()), Json(policy)).await.map_err(api_err)?.0
                    } else {
                        crate::runner_resources::update_node(State(state.clone()), Path(node.clone()), Json(policy)).await.map_err(api_err)?.0
                    }
                }
                "set_priority" => {
                    let id = payload["dispatch_id"].as_str().ok_or_else(|| fail(StatusCode::BAD_REQUEST, "platform.request.invalid", "dispatch_id is required"))?;
                    crate::runner_resources::update_priority(State(state.clone()), Path(id.to_owned()), Json(serde_json::from_value(body).map_err(invalid)?)).await.map_err(api_err)?.0
                }
                "enroll" => crate::runner_remote::issue(State(state.clone()), Json(serde_json::from_value(body).map_err(invalid)?)).await.map_err(api_err)?.1.0,
                "set_state" => crate::runner_remote::set_state(State(state.clone()), Path(node.clone()), Json(serde_json::from_value(body).map_err(invalid)?)).await.map_err(api_err)?.1.0,
                "rotate" => crate::runner_remote::rotate(State(state.clone()), Path(node.clone())).await.map_err(api_err)?.1.0,
                "delete" => crate::runner_remote::remove(State(state.clone()), Path(node.clone())).await.map_err(api_err)?.1.0,
                _ => return Err(fail(StatusCode::BAD_REQUEST, "platform.request.invalid", "Unknown runner resource action")),
            };
            complete_receipt(&state, envelope, scope_type, &node, res)
        }
        "workbench.system_update.check" => {
            let resp = crate::system_api::update_check(Some(Json(payload.clone()))).await;
            let res = response_json(resp).await?;
            complete_receipt(&state, envelope, scope_type, "update", res)
        }
        "workbench.llm_endpoint.test" => {
            let endpoint_id = named(payload, &["endpoint_id", "account_id", "id"]);
            let mut body = payload.clone();
            body["endpoint_id"] = json!(endpoint_id);
            let Json(res) =
                crate::runs::settings::test_llm_endpoint(State(state.clone()), Json(body)).await;
            complete_receipt(&state, envelope, scope_type, endpoint_id, res)
        }
        "workbench.worker_image.pull" => {
            let resp = crate::runs::settings::pull_worker_image().await;
            let res = response_json(resp).await?;
            complete_receipt(&state, envelope, scope_type, "worker_image", res)
        }
        "workbench.run_retention.update" => {
            let mode = target.trim().to_owned();
            let body = strip_keys(payload, &["mode"]);
            let resp =
                crate::runs::put_retention(State(state.clone()), Path(mode.clone()), Json(body))
                    .await.map_err(api_err)?;
            let res = response_json(resp).await?;
            complete_receipt(&state, envelope, scope_type, &mode, res)
        }
        _ => Err(fail(
            StatusCode::NOT_IMPLEMENTED,
            "capability.unavailable",
            "Unknown workbench command",
        )),
    }
}
