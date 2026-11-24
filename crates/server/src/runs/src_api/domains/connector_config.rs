use super::*;
use base64::Engine;
use std::sync::Arc;
use std::sync::OnceLock;

pub(super) const APPLIED: &str = "src_connector_config_applied_commands";
pub(super) const GRANT_APPLIED: &str = "src_connector_grant_applied_commands";
const HEADS: &str = "src_connector_config_heads";
const ARTIFACTS: &str = "src_artifacts";
const STATUS_KIND: &str = "connector_status";
const GRANT_KIND: &str = "connector_project_grant";

pub(super) fn manifests() -> Value {
    static CATALOG: OnceLock<Value> = OnceLock::new();
    CATALOG
        .get_or_init(|| {
            serde_json::from_str(include_str!("connector_manifests.json"))
                .expect("source connector manifest snapshot is valid JSON")
        })
        .clone()
}

pub(super) fn manifest(id: &str) -> Api<Value> {
    manifests()
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["connector_id"] == id)
        .cloned()
        .ok_or_else(|| {
            Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "capability_unavailable",
                format!("unknown connector {id:?}"),
            )
        })
}

fn rows(s: &AppState, task_id: &str, connector: &str) -> Api<Vec<Value>> {
    Ok(records(s, task_id, "tool_config")?
        .into_iter()
        .filter(|row| row["payload"]["connector_id"] == connector)
        .collect())
}

fn value_uses_task_private_secret(v: &Value, project_id: &str) -> bool {
    let project_prefix = format!("secret://src/project/{project_id}/");
    match v {
        Value::String(raw) => {
            let trimmed = raw.trim();
            trimmed.starts_with("secret://") && !trimmed.starts_with(&project_prefix)
        }
        Value::Array(items) => items
            .iter()
            .any(|item| value_uses_task_private_secret(item, project_id)),
        Value::Object(map) => map
            .values()
            .any(|val| value_uses_task_private_secret(val, project_id)),
        _ => false,
    }
}

fn effective_rows(s: &AppState, task_id: &str, connector: &str) -> Api<(String, Vec<Value>)> {
    let local = rows(s, task_id, connector)?;
    if !local.is_empty() {
        return Ok((task_id.to_owned(), local));
    }
    let Some(pid) = credential_store::task_project_id(s, task_id)? else {
        return Ok((task_id.to_owned(), Vec::new()));
    };
    let mut best: Option<(String, u64, String, Vec<Value>)> = None;
    for task_doc in s.store.list("src_tasks").map_err(db)? {
        if task_doc["lifecycle"]["state"].as_str() == Some("destroyed") {
            continue;
        }
        let doc_pid = task_doc["project_id"]
            .as_str()
            .or_else(|| task_doc["binding"]["project_id"].as_str())
            .map(str::trim);
        if doc_pid != Some(pid.as_str()) {
            continue;
        }
        let Some(sibling_id) = task_doc["task_id"].as_str() else {
            continue;
        };
        if sibling_id == task_id {
            continue;
        }
        let sibling_rows = rows(s, sibling_id, connector)?;
        let Some(latest_active) = sibling_rows
            .iter()
            .filter(|row| !row["payload"]["revoked_at"].is_string())
            .max_by_key(|row| row["payload"]["version"].as_u64().unwrap_or(0))
        else {
            continue;
        };
        if value_uses_task_private_secret(&latest_active["payload"]["config"], &pid) {
            continue;
        }
        let updated_at = latest_active["updated_at"]
            .as_str()
            .or_else(|| latest_active["payload"]["created_at"].as_str())
            .unwrap_or("")
            .to_owned();
        let version = latest_active["payload"]["version"].as_u64().unwrap_or(0);
        let replace = best
            .as_ref()
            .is_none_or(|(best_updated, best_ver, _, _)| {
                (updated_at.as_str(), version) > (best_updated.as_str(), *best_ver)
            });
        if replace {
            best = Some((updated_at, version, sibling_id.to_owned(), sibling_rows));
        }
    }
    if let Some((_, _, owner_task_id, owner_rows)) = best {
        return Ok((owner_task_id, owner_rows));
    }
    Ok((task_id.to_owned(), Vec::new()))
}

pub(super) fn config(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let connector = require(params, "connector_id")?;
    let requested = params["version"]
        .as_u64()
        .or_else(|| params["config_version"].as_u64());
    let (owner_task_id, candidate_rows) = effective_rows(s, task_id, connector)?;
    let found = candidate_rows
        .into_iter()
        .filter(|row| requested.is_none_or(|version| row["payload"]["version"] == version))
        .max_by_key(|row| row["payload"]["version"].as_u64().unwrap_or(0));
    let Some(row) = found else {
        if requested.is_some() {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                "connector config version is unavailable",
            ));
        }
        return Ok(json!({"connector_id":connector,"version":0,"config":{},
            "manifest":manifest(connector)?,"revoked_at":null}));
    };
    let mut payload = row["payload"].clone();
    if payload["revoked_at"].is_string() {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability_unavailable",
            "connector config has been revoked",
        ));
    }
    if let Ok(status_row) = rec(s, task_id, STATUS_KIND, connector) {
        payload["verification"] = status_row["payload"].clone();
    } else if owner_task_id != task_id
        && let Ok(status_row) = rec(s, &owner_task_id, STATUS_KIND, connector)
    {
        payload["verification"] = status_row["payload"].clone();
    }
    Ok(payload)
}

pub(super) fn latest_config_optional(s: &AppState, task_id: &str, connector: &str) -> Api<Option<Value>> {
    let (_, candidate_rows) = effective_rows(s, task_id, connector)?;
    let found = candidate_rows
        .into_iter()
        .max_by_key(|row| row["payload"]["version"].as_u64().unwrap_or(0));
    Ok(found
        .map(|row| row["payload"].clone())
        .filter(|payload| !payload["revoked_at"].is_string()))
}

fn forbidden_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "api_key"
            | "apikey"
            | "token"
            | "secret"
            | "secret_key"
            | "password"
            | "private_key"
            | "credential"
            | "credentials"
            | "access_token"
            | "refresh_token"
            | "client_secret"
            | "token_secret"
            | "secret_access_key"
            | "aws_secret_access_key"
            | "authorization"
            | "proxy_authorization"
    )
}

fn reject_secret(value: &Value, path: &str) -> Api<()> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = if path.is_empty() {
                    key.to_owned()
                } else {
                    format!("{path}.{key}")
                };
                if forbidden_key(key) {
                    return Err(Error::new(
                        StatusCode::BAD_REQUEST,
                        "validation_error",
                        format!(
                            "{child_path} must be a credential reference, not raw secret material"
                        ),
                    ));
                }
                reject_secret(child, &child_path)?;
            }
        }
        Value::Array(rows) => {
            for (index, child) in rows.iter().enumerate() {
                reject_secret(child, &format!("{path}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn executable_on_path(name: &str) -> bool {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let path = dir.join(name);
            #[cfg(unix)]
            {
                path.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            }
            #[cfg(not(unix))]
            {
                path.is_file()
            }
        })
    })
}

fn nonempty(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Number(value) => value.as_f64() != Some(0.0),
    }
}

fn completed(s: &AppState, command_id: &str) -> Api<Option<Value>> {
    Ok(s.store
        .get(APPLIED, command_id)
        .map_err(db)?
        .map(|marker| marker["output"].clone()))
}

fn save(s: &AppState, task_id: &str, body: &Value, command_id: &str, actor_id: &str) -> Api<Value> {
    let connector = require(body, "connector_id")?;
    let manifest = manifest(connector)?;
    let config = &body["config"];
    if !config.is_object() {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "connector config must be an object",
        ));
    }
    reject_secret(config, "")?;
    validate_config_against_manifest(s, task_id, &manifest, config)?;
    if connector == "aws_ec2" && !executable_on_path("aws") {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability_unavailable",
            "AWS CLI is required by this configuration",
        ));
    }
    if connector == "scopesentry"
        && nonempty(&config["query_enabled"])
        && !nonempty(&config["endpoints"])
    {
        return Err(Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capability_unavailable",
            "ScopeSentry online query requires explicit verified endpoints",
        ));
    }
    let config_bytes = browser_runtime::canonical_json(config).into_bytes();
    loop {
        if let Some(output) = completed(s, command_id)? {
            return Ok(output);
        }
        let current_task = task(s, task_id)?;
        if current_task["binding"].is_null() {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "task_binding_invalid",
                "SRC Task binding is unavailable",
            ));
        }
        let head_key = format!("{task_id}:{connector}");
        let head = s.store.get(HEADS, &head_key).map_err(db)?;
        let previous = head
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(
                rows(s, task_id, connector)?
                    .into_iter()
                    .filter_map(|row| row["payload"]["version"].as_u64())
                    .max()
                    .unwrap_or(0),
            );
        let version = previous + 1;
        let record_id = format!("tool-config-{}", Uuid::new_v4().simple());
        let artifact_id = format!("sart_{}", Uuid::new_v4().simple());
        let name = format!("{connector}-config-v{version}.json");
        let stamp = now();
        let artifact_ref = json!({"artifact_id":artifact_id,"task_id":task_id,
            "media_type":"application/json","size":config_bytes.len(),"version":1,
            "source_run_id":null,"source_event_seq":null,"original":true,
            "complete":"complete","name":name});
        let artifact = json!({"artifact_id":artifact_id,"task_id":task_id,
            "media_type":"application/json","size":config_bytes.len(),"version":1,
            "data":base64::engine::general_purpose::STANDARD.encode(&config_bytes),
            "name":name,"created_at":stamp});
        let executable_version = body["executable_version"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or(manifest["version"].as_str().unwrap_or(""));
        let payload = json!({"connector_id":connector,"version":version,"tool_id":connector,
            "executable_version":executable_version,"config_schema_version":1,
            "config":config,"config_artifact":artifact_ref,"created_by":actor_id,
            "created_at":stamp,"revoked_at":null,"manifest":manifest});
        let natural_key = format!("{connector}:{version}");
        let output = json!({"record_id":record_id,"kind":"tool_config","task_id":task_id,
            "revision":1,"parent_id":connector,"natural_key":natural_key,
            "generation":0,"created_at":stamp,"updated_at":stamp,"payload":payload});
        let document = json!({"version":1,"values":output});
        let marker = json!({"command_id":command_id,"output":output});
        let record_key = format!("tool_config:{task_id}:{connector}:{version}");
        let artifact_key = format!("{task_id}:{artifact_id}");
        let next_head = json!({"version":version});
        if s.store
            .put_many_if(
                &[
                    (TASKS, task_id, Some(&current_task)),
                    (HEADS, &head_key, head.as_ref()),
                    (RECORDS, &record_key, None),
                    (ARTIFACTS, &artifact_key, None),
                    (APPLIED, command_id, None),
                ],
                &[
                    (HEADS, &head_key, &next_head),
                    (RECORDS, &record_key, &document),
                    (ARTIFACTS, &artifact_key, &artifact),
                    (APPLIED, command_id, &marker),
                ],
            )
            .map_err(db)?
        {
            return Ok(output);
        }
    }
}

fn revoke(s: &AppState, task_id: &str, body: &Value, command_id: &str) -> Api<Value> {
    let connector = require(body, "connector_id")?;
    let version = body["version"].as_u64().ok_or_else(|| {
        Error::new(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "version is required",
        )
    })?;
    let base = body["base_revision"].as_u64().ok_or_else(|| {
        Error::new(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "base_revision is required",
        )
    })?;
    let natural_key = format!("{connector}:{version}");
    let key = format!("tool_config:{task_id}:{connector}:{version}");
    let key = if s.store.get(RECORDS, &key).map_err(db)?.is_some() {
        key
    } else {
        rec_key(task_id, "tool_config", &natural_key)
    };
    loop {
        if let Some(output) = completed(s, command_id)? {
            return Ok(output);
        }
        let current = s.store.get(RECORDS, &key).map_err(db)?.ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                "connector config version is unavailable",
            )
        })?;
        if current["values"]["task_id"] != task_id
            || current["values"]["kind"] != "tool_config"
            || current["values"]["payload"]["connector_id"] != connector
            || current["values"]["payload"]["version"] != version
        {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                "connector config version is unavailable",
            ));
        }
        let revision = current["version"].as_u64().unwrap_or(0);
        if revision != base {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "version_conflict",
                "connector config revision is stale",
            ));
        }
        let mut payload = current["values"]["payload"].clone();
        payload["revoked_at"] = json!(now());
        let stamp = now();
        let mut output = current["values"].clone();
        output["revision"] = json!(revision + 1);
        output["updated_at"] = json!(stamp);
        output["payload"] = payload;
        let document = json!({"version":revision+1,"values":output});
        let marker = json!({"command_id":command_id,"output":output});
        if s.store
            .put_many_if(
                &[(RECORDS, &key, Some(&current)), (APPLIED, command_id, None)],
                &[(RECORDS, &key, &document), (APPLIED, command_id, &marker)],
            )
            .map_err(db)?
        {
            return Ok(output);
        }
    }
}

pub(super) fn apply_command(
    s: &AppState,
    task_id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    if let Some(output) = completed(s, command_id)? {
        return Ok(output);
    }
    if task(s, task_id)?["binding"].is_null() {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "task_binding_invalid",
            "SRC Task binding is unavailable",
        ));
    }
    match kind {
        "src.connector.config.save" => save(s, task_id, body, command_id, actor_id),
        "src.connector.config.revoke" => revoke(s, task_id, body, command_id),
        "src.connector.grant.save" => grant_save(s, task_id, body, command_id, actor_id),
        "src.connector.grant.revoke" => grant_revoke(s, task_id, body, command_id, actor_id),
        _ => Err(Error::invalid("unknown connector config command")),
    }
}

fn validate_config_against_manifest(
    s: &AppState,
    task_id: &str,
    manifest_doc: &Value,
    config: &Value,
) -> Api<()> {
    let connector = manifest_doc["connector_id"].as_str().unwrap_or("");
    let creds = &manifest_doc["credentials"];
    let ref_fields: Vec<&str> = creds["reference_fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if creds["required"] == true {
        let missing: Vec<&str> = ref_fields
            .iter()
            .copied()
            .filter(|field| {
                config[*field]
                    .as_str()
                    .map(str::trim)
                    .is_none_or(|v| v.is_empty())
            })
            .collect();
        if !missing.is_empty() {
            return Err(Error::invalid(format!(
                "连接器 {connector} 缺少必填凭据引用字段: {}",
                missing.join("、")
            )));
        }
    }
    for field in ref_fields.into_iter().chain(std::iter::once("credential_ref")) {
        if let Some(reference) = config[field]
            .as_str()
            .map(str::trim)
            .filter(|v| v.starts_with("secret://"))
        {
            credential_store::check_reference_scope(s, task_id, reference)?;
        }
    }
    for url_field in ["base_url", "endpoint_url"] {
        if let Some(raw_url) = config[url_field]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            let parsed = parse_url(raw_url)?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(Error::invalid(format!(
                    "{url_field} must use http or https scheme"
                )));
            }
        }
    }
    Ok(())
}

fn missing_fields_for_config(
    s: &AppState,
    task_id: &str,
    manifest_doc: &Value,
    settings: Option<&Value>,
) -> Api<Vec<String>> {
    let connector = manifest_doc["connector_id"].as_str().unwrap_or("");
    let creds = &manifest_doc["credentials"];
    let ref_fields: Vec<&str> = creds["reference_fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let Some(settings) = settings else {
        let mut out = Vec::new();
        if creds["required"] == true {
            out.extend(ref_fields.into_iter().map(str::to_owned));
        }
        if connector == "aws_ec2" {
            out.push("region".to_owned());
        }
        return Ok(out);
    };
    let mut missing = Vec::new();
    for field in &ref_fields {
        let val = settings[*field].as_str().map(str::trim).unwrap_or("");
        if val.is_empty() {
            if creds["required"] == true {
                missing.push((*field).to_owned());
            }
            continue;
        }
        if val.starts_with("secret://") {
            match credential_store::version(s, task_id, val) {
                Ok(_) => {}
                Err(e) if e.status == StatusCode::FORBIDDEN => return Err(e),
                Err(_) => missing.push(format!("credential:{field}")),
            }
        }
    }
    if let Some(cred_ref) = settings["credential_ref"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.starts_with("secret://"))
    {
        match credential_store::version(s, task_id, cred_ref) {
            Ok(_) => {}
            Err(e) if e.status == StatusCode::FORBIDDEN => return Err(e),
            Err(_) => missing.push("credential:credential_ref".to_owned()),
        }
    }
    if connector == "aws_ec2"
        && settings["region"]
            .as_str()
            .map(str::trim)
            .is_none_or(|v| v.is_empty())
    {
        missing.push("region".to_owned());
    }
    if connector == "scopesentry" && nonempty(&settings["query_enabled"]) {
        if settings["base_url"]
            .as_str()
            .map(str::trim)
            .is_none_or(|v| v.is_empty())
        {
            missing.push("base_url".to_owned());
        }
        if !nonempty(&settings["endpoints"]) {
            missing.push("endpoints".to_owned());
        }
    }
    Ok(missing)
}

#[allow(dead_code)]
fn store_key_for_task_grant(task_id: &str, connector_id: &str) -> String {
    rec_key(task_id, GRANT_KIND, connector_id)
}

pub(super) fn grant_for_connector(
    s: &AppState,
    task_id: &str,
    connector_id: &str,
) -> Api<Option<(String, Value)>> {
    let task_pid = credential_store::task_project_id(s, task_id)?;
    if let Ok(row) = rec(s, task_id, GRANT_KIND, connector_id) {
        if let Some(grant_pid) = row["payload"]["project_id"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            && task_pid.as_deref() != Some(grant_pid)
        {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.project_mismatch",
                "connector grant belongs to another project",
            ));
        }
        return Ok(Some((store_key_for_task_grant(task_id, connector_id), row)));
    }
    if let Some(pid) = task_pid {
        let rows = s.store.list_inserted_with_rowid(RECORDS).map_err(db)?;
        for (_, stored) in rows.into_iter().rev() {
            let row = &stored["values"];
            if row["kind"] == GRANT_KIND
                && row["natural_key"] == connector_id
                && row["payload"]["project_id"] == pid
            {
                let owner_task = row["task_id"].as_str().unwrap_or(task_id);
                return Ok(Some((
                    store_key_for_task_grant(owner_task, connector_id),
                    row.clone(),
                )));
            }
        }
    }
    Ok(None)
}

pub(super) fn enforce_grant_for_purpose(
    _s: &AppState,
    _task_id: &str,
    _connector_id: &str,
    _purpose: &str,
    _consume: bool,
) -> Api<Option<Value>> {
    Ok(None)
}

fn grant_save(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    if let Some(marker) = s.store.get(GRANT_APPLIED, command_id).map_err(db)? {
        return Ok(marker["output"].clone());
    }
    let connector_id = require(body, "connector_id")?;
    manifest(connector_id)?;
    let task_pid = credential_store::task_project_id(s, task_id)?;
    let req_pid = body["project_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    if let (Some(rp), Some(tp)) = (req_pid, task_pid.as_deref())
        && rp != tp
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.project_mismatch",
            "grant project_id does not match Task project_id",
        ));
    }
    let project_id = req_pid.or(task_pid.as_deref()).map(str::to_owned);
    let allowed_purposes: Vec<String> = match body.get("allowed_purposes") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .collect(),
        Some(_) => {
            return Err(Error::invalid("allowed_purposes must be a string array"));
        }
    };
    let budget_limit = match body.get("budget_limit") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or_else(|| Error::invalid("budget_limit must be a non-negative integer"))?,
        ),
    };
    let notes = body["notes"].as_str().unwrap_or("").to_owned();
    let base_revision = body.get("base_revision").and_then(Value::as_u64);

    loop {
        if let Some(marker) = s.store.get(GRANT_APPLIED, command_id).map_err(db)? {
            return Ok(marker["output"].clone());
        }
        let existing = grant_for_connector(s, task_id, connector_id)?;
        let (key, current_row, current_doc, prev_rev, grant_id, prev_used) =
            if let Some((k, row)) = existing {
                let doc = s.store.get(RECORDS, &k).map_err(db)?;
                let rev = doc.as_ref().and_then(|d| d["version"].as_u64()).unwrap_or(0);
                let gid = row["payload"]["grant_id"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("cgrant-{}", Uuid::new_v4().simple()));
                let used = if row["payload"]["revoked_at"].is_null() {
                    row["payload"]["used_requests"].as_u64().unwrap_or(0)
                } else {
                    0
                };
                (k, Some(row), doc, rev, gid, used)
            } else {
                (
                    store_key_for_task_grant(task_id, connector_id),
                    None,
                    None,
                    0,
                    format!("cgrant-{}", Uuid::new_v4().simple()),
                    0,
                )
            };
        if let Some(expected) = base_revision
            && prev_rev != expected
        {
            return Err(Error::conflict("connector grant revision is stale"));
        }
        let stamp = now();
        let owner_task_id = current_row
            .as_ref()
            .and_then(|r| r["task_id"].as_str())
            .unwrap_or(task_id);
        let record_id = current_row
            .as_ref()
            .and_then(|r| r["record_id"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| grant_id.clone());
        let created_at = current_row
            .as_ref()
            .and_then(|r| r["created_at"].as_str())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| stamp.clone());
        let payload = json!({
            "grant_id": grant_id,
            "connector_id": connector_id,
            "project_id": project_id,
            "allowed_purposes": allowed_purposes,
            "budget_limit": budget_limit,
            "used_requests": prev_used,
            "notes": notes,
            "granted_by": actor_id,
            "granted_at": stamp,
            "revoked_at": null,
            "revoked_by": null
        });
        let output = json!({
            "record_id": record_id,
            "kind": GRANT_KIND,
            "task_id": owner_task_id,
            "revision": prev_rev + 1,
            "parent_id": connector_id,
            "natural_key": connector_id,
            "generation": 0,
            "created_at": created_at,
            "updated_at": stamp,
            "payload": payload
        });
        let doc = json!({"version": prev_rev + 1, "values": output});
        let marker = json!({"command_id": command_id, "output": output});
        if s.store
            .put_many_if(
                &[(RECORDS, &key, current_doc.as_ref()), (GRANT_APPLIED, command_id, None)],
                &[(RECORDS, &key, &doc), (GRANT_APPLIED, command_id, &marker)],
            )
            .map_err(db)?
        {
            return Ok(output);
        }
    }
}

fn grant_revoke(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    if let Some(marker) = s.store.get(GRANT_APPLIED, command_id).map_err(db)? {
        return Ok(marker["output"].clone());
    }
    let connector_id = require(body, "connector_id")?;
    let base_revision = body.get("base_revision").and_then(Value::as_u64);
    loop {
        if let Some(marker) = s.store.get(GRANT_APPLIED, command_id).map_err(db)? {
            return Ok(marker["output"].clone());
        }
        let Some((key, row)) = grant_for_connector(s, task_id, connector_id)? else {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "src.record_not_found",
                "connector grant not found",
            ));
        };
        let current_doc = s.store.get(RECORDS, &key).map_err(db)?;
        let rev = current_doc
            .as_ref()
            .and_then(|d| d["version"].as_u64())
            .unwrap_or(0);
        if let Some(expected) = base_revision
            && rev != expected
        {
            return Err(Error::conflict("connector grant revision is stale"));
        }
        let stamp = now();
        let mut payload = row["payload"].clone();
        payload["revoked_at"] = json!(stamp);
        payload["revoked_by"] = json!(actor_id);
        let mut output = row.clone();
        output["revision"] = json!(rev + 1);
        output["updated_at"] = json!(stamp);
        output["payload"] = payload;
        let doc = json!({"version": rev + 1, "values": output});
        let marker = json!({"command_id": command_id, "output": output});
        if s.store
            .put_many_if(
                &[(RECORDS, &key, current_doc.as_ref()), (GRANT_APPLIED, command_id, None)],
                &[(RECORDS, &key, &doc), (GRANT_APPLIED, command_id, &marker)],
            )
            .map_err(db)?
        {
            return Ok(output);
        }
    }
}

fn save_status_snapshot(
    s: &AppState,
    task_id: &str,
    connector_id: &str,
    snapshot: Value,
) -> Api<Value> {
    let key = rec_key(task_id, STATUS_KIND, connector_id);
    loop {
        let current = s.store.get(RECORDS, &key).map_err(db)?;
        let rev = current
            .as_ref()
            .and_then(|d| d["version"].as_u64())
            .unwrap_or(0);
        let saved = save_rec(
            s,
            task_id,
            STATUS_KIND,
            connector_id,
            snapshot.clone(),
            Some(rev),
        );
        match saved {
            Ok(row) => return Ok(row["payload"].clone()),
            Err(e) if e.code == "version_conflict" => continue,
            Err(e) => return Err(e),
        }
    }
}

pub(super) fn connector_base_url(manifest_doc: &Value, settings: Option<&Value>) -> String {
    let connector = manifest_doc["connector_id"].as_str().unwrap_or("");
    if let Some(base) = settings
        .and_then(|v| v["base_url"].as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return base.trim_end_matches('/').to_owned();
    }
    if connector == "aws_ec2"
        && let Some(ep) = settings
            .and_then(|v| v["endpoint_url"].as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    {
        return ep.trim_end_matches('/').to_owned();
    }
    if let Some(base) = manifest_doc["runtime"]["base_url"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return base.trim_end_matches('/').to_owned();
    }
    match connector {
        "github" => "https://api.github.com".to_owned(),
        "gitlab" => "https://gitlab.com".to_owned(),
        "fofa" => "https://fofa.info".to_owned(),
        "quake" => "https://quake.360.net".to_owned(),
        "shodan" => "https://api.shodan.io".to_owned(),
        "zoomeye" => "https://api.zoomeye.org".to_owned(),
        _ => String::new(),
    }
}

fn header_quota(exchange: &Value) -> Option<i64> {
    exchange["response_headers"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|pair| {
            let name = pair[0].as_str()?.to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "x-ratelimit-remaining" | "ratelimit-remaining" | "x-rate-limit-remaining"
            ) {
                pair[1].as_str()?.trim().parse::<i64>().ok()
            } else {
                None
            }
        })
}

fn parse_quota_from_response(connector_id: &str, doc: &Value, exchange: &Value) -> Option<i64> {
    match connector_id {
        "fofa" => doc["remain_api_query"]
            .as_i64()
            .or_else(|| doc["fcoin"].as_i64())
            .or_else(|| doc["fpoint"].as_i64()),
        "quake" => doc["data"]["credit"]
            .as_i64()
            .or_else(|| doc["data"]["month_remaining_credit"].as_i64())
            .or_else(|| doc["credit"].as_i64()),
        "shodan" => doc["query_credits"]
            .as_i64()
            .or_else(|| doc["scan_credits"].as_i64()),
        "zoomeye" => {
            let free = doc["quota_info"]["remain_free_quota"].as_i64();
            let pay = doc["quota_info"]["remain_pay_quota"].as_i64();
            match (free, pay) {
                (Some(f), Some(p)) => Some(f + p),
                (Some(f), None) => Some(f),
                (None, Some(p)) => Some(p),
                (None, None) => doc["plan"]["quota"].as_i64().or_else(|| doc["quota"].as_i64()),
            }
        }
        "github" => doc["resources"]["core"]["remaining"]
            .as_i64()
            .or_else(|| doc["rate"]["remaining"].as_i64())
            .or_else(|| header_quota(exchange)),
        "gitlab" | "scopesentry" => doc["quota_remaining"]
            .as_i64()
            .or_else(|| doc["remaining"].as_i64())
            .or_else(|| header_quota(exchange)),
        _ => None,
    }
}

pub(super) async fn verify(s: Arc<AppState>, task_id: &str, body: &Value) -> Api<Value> {
    let connector_id = require(body, "connector_id")?;
    let manifest_doc = manifest(connector_id)?;
    let cfg = latest_config_optional(&s, task_id, connector_id)?;
    let stamped = now();

    if manifest_doc["runtime"]["kind"] == "file" && connector_id != "scopesentry" {
        let snapshot = json!({
            "connector_id": connector_id,
            "verification_state": "file_import_only",
            "readiness": "file_import_only",
            "quota_remaining": null,
            "verified_at": stamped,
            "last_error": null,
            "missing_fields": [],
            "config_version": cfg.as_ref().and_then(|c| c["version"].as_u64()).unwrap_or(0)
        });
        save_status_snapshot(&s, task_id, connector_id, snapshot.clone())?;
        return Ok(snapshot);
    }

    let Some(cfg_doc) = cfg else {
        let missing = missing_fields_for_config(&s, task_id, &manifest_doc, None)?;
        let snapshot = json!({
            "connector_id": connector_id,
            "verification_state": "unconnected",
            "readiness": "unconnected",
            "quota_remaining": null,
            "verified_at": stamped,
            "last_error": "连接器尚未配置",
            "missing_fields": missing,
            "config_version": 0
        });
        save_status_snapshot(&s, task_id, connector_id, snapshot.clone())?;
        return Ok(snapshot);
    };

    let config_version = cfg_doc["version"].as_u64().unwrap_or(0);
    let settings = &cfg_doc["config"];
    if connector_id == "scopesentry" && !nonempty(&settings["query_enabled"]) {
        let snapshot = json!({
            "connector_id": connector_id,
            "verification_state": "file_import_only",
            "readiness": "file_import_only",
            "quota_remaining": null,
            "verified_at": stamped,
            "last_error": null,
            "missing_fields": [],
            "config_version": config_version
        });
        save_status_snapshot(&s, task_id, connector_id, snapshot.clone())?;
        return Ok(snapshot);
    }

    let missing = missing_fields_for_config(&s, task_id, &manifest_doc, Some(settings))?;
    if !missing.is_empty() {
        let snapshot = json!({
            "connector_id": connector_id,
            "verification_state": "configured_unverified",
            "readiness": "configured_unverified",
            "quota_remaining": null,
            "verified_at": stamped,
            "last_error": format!("连接器缺少必要配置或未上传凭据: {}", missing.join("、")),
            "missing_fields": missing,
            "config_version": config_version
        });
        save_status_snapshot(&s, task_id, connector_id, snapshot.clone())?;
        return Ok(snapshot);
    }

    if connector_id == "aws_ec2" {
        let base = connector_base_url(&manifest_doc, Some(settings));
        if base.is_empty() || settings["verify_http"] != true {
            enforce_grant_for_purpose(&s, task_id, connector_id, "verify", true)?;
            if let Some(ref_val) = settings["credential_ref"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty() && v.starts_with("secret://"))
            {
                let secret = credential_store::resolve(&s, task_id, ref_val, None)?;
                let parsed: Result<Value, _> = serde_json::from_str(&secret);
                if parsed.is_err() {
                    let snapshot = json!({
                        "connector_id": connector_id,
                        "verification_state": "configured_unverified",
                        "readiness": "configured_unverified",
                        "quota_remaining": null,
                        "verified_at": stamped,
                        "last_error": "AWS 凭据内容必须为 JSON 环境对象",
                        "missing_fields": [],
                        "config_version": config_version
                    });
                    save_status_snapshot(&s, task_id, connector_id, snapshot.clone())?;
                    return Ok(snapshot);
                }
            }
            let snapshot = json!({
                "connector_id": connector_id,
                "verification_state": "quota_unknown",
                "readiness": "quota_unknown",
                "quota_remaining": null,
                "verified_at": stamped,
                "last_error": null,
                "missing_fields": [],
                "config_version": config_version
            });
            save_status_snapshot(&s, task_id, connector_id, snapshot.clone())?;
            return Ok(snapshot);
        }
    }

    let base = connector_base_url(&manifest_doc, Some(settings));
    if base.is_empty() {
        let snapshot = json!({
            "connector_id": connector_id,
            "verification_state": "configured_unverified",
            "readiness": "configured_unverified",
            "quota_remaining": null,
            "verified_at": stamped,
            "last_error": "连接器缺少可用的 base_url",
            "missing_fields": ["base_url"],
            "config_version": config_version
        });
        save_status_snapshot(&s, task_id, connector_id, snapshot.clone())?;
        return Ok(snapshot);
    }

    let mut bindings = Vec::new();
    let mut headers = serde_json::Map::new();
    headers.insert("Accept".into(), json!("application/json"));
    headers.insert("User-Agent".into(), json!("Muteki-SRC/0.4.8"));
    let probe_url = match connector_id {
        "fofa" => {
            if let Some(email) = settings["email_ref"].as_str().filter(|v| !v.is_empty()) {
                bindings.push(json!({"location":"query","name":"email","reference":email,"format":"raw"}));
            }
            if let Some(key) = settings["key_ref"].as_str().filter(|v| !v.is_empty()) {
                bindings.push(json!({"location":"query","name":"key","reference":key,"format":"raw"}));
            }
            format!("{base}/api/v1/info/my")
        }
        "quake" => {
            if let Some(tok) = settings["token_ref"].as_str().filter(|v| !v.is_empty()) {
                bindings.push(json!({"location":"header","name":"X-QuakeToken","reference":tok,"format":"raw"}));
            }
            format!("{base}/api/v3/user/info")
        }
        "shodan" => {
            if let Some(key) = settings["key_ref"].as_str().filter(|v| !v.is_empty()) {
                bindings.push(json!({"location":"query","name":"key","reference":key,"format":"raw"}));
            }
            format!("{base}/api-info")
        }
        "zoomeye" => {
            if let Some(key) = settings["key_ref"].as_str().filter(|v| !v.is_empty()) {
                bindings.push(json!({"location":"header","name":"API-KEY","reference":key,"format":"raw"}));
            }
            format!("{base}/resources-info")
        }
        "github" => {
            headers.insert("Accept".into(), json!("application/vnd.github+json"));
            headers.insert("X-GitHub-Api-Version".into(), json!("2022-11-28"));
            if let Some(tok) = settings["token_ref"]
                .as_str()
                .or_else(|| settings["credential_ref"].as_str())
                .filter(|v| !v.is_empty())
            {
                bindings.push(json!({"location":"header","name":"Authorization","reference":tok,"format":"bearer"}));
            }
            format!("{base}/rate_limit")
        }
        "gitlab" => {
            let has_tok = if let Some(tok) = settings["token_ref"]
                .as_str()
                .or_else(|| settings["credential_ref"].as_str())
                .filter(|v| !v.is_empty())
            {
                bindings.push(json!({"location":"header","name":"Authorization","reference":tok,"format":"bearer"}));
                true
            } else {
                false
            };
            if has_tok {
                format!("{base}/api/v4/user")
            } else {
                format!("{base}/api/v4/version")
            }
        }
        "scopesentry" => {
            if let Some(tok) = settings["token_ref"].as_str().filter(|v| !v.is_empty()) {
                bindings.push(json!({"location":"header","name":"Authorization","reference":tok,"format":"bearer"}));
            }
            let first_path = settings["endpoints"]
                .as_object()
                .and_then(|m| m.values().find_map(Value::as_str))
                .unwrap_or("/api/health");
            if first_path.starts_with('/') {
                format!("{base}{first_path}")
            } else {
                format!("{base}/{first_path}")
            }
        }
        "aws_ec2" => format!("{base}/"),
        _ => base.clone(),
    };

    let exec_body = json!({
        "url": probe_url,
        "method": "GET",
        "headers": headers,
        "credential_bindings": bindings,
        "budget": { "max_requests": 1, "timeout_seconds": 15 },
        "idempotency_key": body["idempotency_key"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(|k| format!("verify:{k}"))
            .unwrap_or_else(|| format!("verify:{}", Uuid::new_v4().simple()))
    });

    let exec_result = super::egress::execute_connector_draft(
        s.clone(),
        task_id,
        &exec_body,
        connector_id,
        &base,
        "verify",
    )
    .await;

    let mut verification_state = "configured_unverified";
    let mut quota_remaining: Option<i64> = None;
    let mut last_error: Option<String> = None;
    let mut status_code: Option<u64> = None;
    let mut exchange_id: Option<String> = None;

    match exec_result {
        Err(e)
            if e.status == StatusCode::FORBIDDEN
                || e.status == StatusCode::TOO_MANY_REQUESTS =>
        {
            return Err(e);
        }
        Err(e) => {
            verification_state = "configured_unverified";
            last_error = Some(e.message);
        }
        Ok(res) => {
            let output = &res["output"];
            let exchange = &output["exchange"];
            exchange_id = output["exchange_id"].as_str().map(str::to_owned);
            status_code = exchange["status_code"].as_u64();
            if output["execution_state"] != "completed" {
                last_error = Some(format!(
                    "连接器探测未完成: {}",
                    output["execution_state"].as_str().unwrap_or("unknown")
                ));
            } else if matches!(status_code, Some(401 | 403)) {
                verification_state = "permission_denied";
                last_error = Some(format!("连接器鉴权失败 (HTTP {})", status_code.unwrap_or(401)));
            } else if status_code.is_some_and(|c| c >= 400) {
                last_error = Some(format!("连接器返回错误状态码 HTTP {}", status_code.unwrap()));
            } else {
                let parsed_doc = super::connectors_online::body_bytes(&s, task_id, output)
                    .ok()
                    .map(|(doc, _)| doc)
                    .unwrap_or(Value::Null);
                let upstream_auth_err = (connector_id == "fofa" && parsed_doc["error"] == true)
                    || (connector_id == "quake"
                        && parsed_doc["code"]
                            .as_i64()
                            .is_some_and(|c| c != 0 && c != 200))
                    || (parsed_doc["error"] == true);
                if upstream_auth_err {
                    let msg = parsed_doc["errmsg"]
                        .as_str()
                        .or_else(|| parsed_doc["error_message"].as_str())
                        .or_else(|| parsed_doc["message"].as_str())
                        .unwrap_or("upstream returned error")
                        .to_owned();
                    let lower = msg.to_ascii_lowercase();
                    if lower.contains("401")
                        || lower.contains("403")
                        || lower.contains("auth")
                        || lower.contains("key")
                        || lower.contains("token")
                        || lower.contains("denied")
                        || lower.contains("permission")
                        || parsed_doc["code"].as_i64().is_some_and(|c| c == 401 || c == 403 || c == 4010)
                    {
                        verification_state = "permission_denied";
                    } else {
                        verification_state = "configured_unverified";
                    }
                    last_error = Some(msg);
                } else {
                    quota_remaining =
                        parse_quota_from_response(connector_id, &parsed_doc, exchange);
                    verification_state = if quota_remaining.is_some() {
                        "verified"
                    } else {
                        "quota_unknown"
                    };
                }
            }
        }
    }

    let snapshot = json!({
        "connector_id": connector_id,
        "verification_state": verification_state,
        "readiness": verification_state,
        "quota_remaining": quota_remaining,
        "verified_at": stamped,
        "last_error": last_error,
        "missing_fields": [],
        "config_version": config_version,
        "status_code": status_code,
        "exchange_id": exchange_id
    });
    save_status_snapshot(&s, task_id, connector_id, snapshot.clone())?;
    Ok(snapshot)
}

pub(super) fn status(s: &AppState, task_id: &str) -> Api<Value> {
    let project_id = credential_store::task_project_id(s, task_id)?;
    let list = manifests();
    let mut enriched = Vec::new();
    for manifest_doc in list.as_array().into_iter().flatten() {
        let connector_id = manifest_doc["connector_id"].as_str().unwrap_or("");
        let (owner_task_id, candidate_rows) = effective_rows(s, task_id, connector_id)?;
        let cfg = candidate_rows
            .into_iter()
            .max_by_key(|row| row["payload"]["version"].as_u64().unwrap_or(0))
            .map(|row| row["payload"].clone())
            .filter(|payload| !payload["revoked_at"].is_string());
        let grant = grant_for_connector(s, task_id, connector_id)?.map(|(_, row)| row["payload"].clone());
        let cfg_version = cfg
            .as_ref()
            .and_then(|c| c["version"].as_u64())
            .unwrap_or(0);
        let local_status = rec(s, task_id, STATUS_KIND, connector_id)
            .ok()
            .map(|r| r["payload"].clone());
        let saved_status = if owner_task_id != task_id
            && !local_status
                .as_ref()
                .is_some_and(|st| st["config_version"].as_u64() == Some(cfg_version))
        {
            rec(s, &owner_task_id, STATUS_KIND, connector_id)
                .ok()
                .map(|r| r["payload"].clone())
                .or(local_status)
        } else {
            local_status
        };

        let is_file_only = (manifest_doc["runtime"]["kind"] == "file"
            && connector_id != "scopesentry")
            || (connector_id == "scopesentry"
                && cfg
                    .as_ref()
                    .is_none_or(|c| !nonempty(&c["config"]["query_enabled"])));
        let missing = missing_fields_for_config(
            s,
            task_id,
            manifest_doc,
            cfg.as_ref().map(|c| &c["config"]),
        )?;
        let readiness = if is_file_only {
            "file_import_only"
        } else if cfg.is_none() {
            "unconnected"
        } else if !missing.is_empty() {
            "configured_unverified"
        } else if let Some(snap) = saved_status
            .as_ref()
            .filter(|st| st["config_version"].as_u64() == Some(cfg_version))
        {
            match snap["verification_state"].as_str() {
                Some("verified") => "verified",
                Some("permission_denied") => "permission_denied",
                Some("quota_unknown") => "quota_unknown",
                _ => "configured_unverified",
            }
        } else {
            "configured_unverified"
        };

        enriched.push(json!({
            "connector_id": connector_id,
            "manifest": manifest_doc,
            "config": cfg,
            "inherited_from_task_id": if cfg.is_some() && owner_task_id != task_id { json!(owner_task_id) } else { Value::Null },
            "grant": grant,
            "readiness": readiness,
            "verification_state": saved_status.as_ref().and_then(|st| st["verification_state"].as_str()).unwrap_or(readiness),
            "missing_fields": missing,
            "quota_remaining": saved_status.as_ref().and_then(|st| st.get("quota_remaining")).cloned().unwrap_or(Value::Null),
            "verified_at": saved_status.as_ref().and_then(|st| st.get("verified_at")).cloned().unwrap_or(Value::Null),
            "last_error": saved_status.as_ref().and_then(|st| st.get("last_error")).cloned().unwrap_or(Value::Null)
        }));
    }
    Ok(json!({
        "task_id": task_id,
        "project_id": project_id,
        "connectors": enriched
    }))
}

pub(super) fn summarize_batch(row: &Value) -> Value {
    let payload = &row["payload"];
    let batch_id = payload["batch_id"]
        .as_str()
        .or_else(|| row["natural_key"].as_str())
        .unwrap_or("");
    let connector_id = payload["connector_id"].as_str().unwrap_or("");
    let truncated = payload["acquisition"]["truncated"] == true;
    let ext = payload["external_state"].as_str().unwrap_or("completed");
    let status = payload["status"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if ext == "completed" && truncated {
                "partial".to_owned()
            } else {
                ext.to_owned()
            }
        });
    let outcome = payload["outcome"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if status == "failed" {
                "failed".to_owned()
            } else if status == "partial" {
                "partial".to_owned()
            } else if payload["items"].as_array().is_some_and(|v| v.is_empty()) {
                "empty".to_owned()
            } else {
                "completed".to_owned()
            }
        });
    let reconciliation_state = payload["reconciliation_state"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if payload["reconciled"].is_object() || payload["observations_ingested"] == true {
                "reconciled".to_owned()
            } else {
                "unreconciled".to_owned()
            }
        });
    let error = if !payload["error"].is_null() {
        payload["error"].clone()
    } else {
        payload["errors"]
            .as_array()
            .and_then(|arr| arr.first())
            .cloned()
            .unwrap_or(Value::Null)
    };
    let created_at = payload["created_at"]
        .as_str()
        .or_else(|| row["created_at"].as_str())
        .unwrap_or("");
    json!({
        "batch_id": batch_id,
        "connector_id": connector_id,
        "config_version": payload["config_version"],
        "status": status,
        "outcome": outcome,
        "reconciliation_state": reconciliation_state,
        "counts": payload["counts"],
        "error": error,
        "created_at": created_at,
        "external_state": ext,
        "acquisition": payload["acquisition"],
        "source_refs": payload["source_refs"]
    })
}

pub(super) fn batch_list(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let filter_connector = params["connector_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let mut rows = records_with_rowid(s, task_id, "connector_batch")?;
    rows.sort_by(|(ra, va), (rb, vb)| {
        let ca = va["payload"]["created_at"]
            .as_str()
            .or_else(|| va["created_at"].as_str())
            .unwrap_or("");
        let cb = vb["payload"]["created_at"]
            .as_str()
            .or_else(|| vb["created_at"].as_str())
            .unwrap_or("");
        cb.cmp(ca).then(rb.cmp(ra))
    });
    let batches: Vec<Value> = rows
        .into_iter()
        .filter(|(_, r)| {
            filter_connector.is_none_or(|cid| r["payload"]["connector_id"] == cid)
        })
        .map(|(_, r)| summarize_batch(&r))
        .collect();
    Ok(json!({
        "task_id": task_id,
        "total": batches.len(),
        "batches": batches
    }))
}

pub(super) fn batch_get(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let batch_id = require(params, "batch_id")?;
    let row = rec(s, task_id, "connector_batch", batch_id)?;
    let mut out = row["payload"].clone();
    let summary = summarize_batch(&row);
    for field in ["status", "outcome", "reconciliation_state", "error", "created_at"] {
        if out.get(field).is_none_or(Value::is_null) {
            out[field] = summary[field].clone();
        }
    }
    out["record_revision"] = row["revision"].clone();
    Ok(out)
}
