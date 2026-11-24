//! Worker settings and secure file imports reuse the settings/account services.
use super::*;
use std::io::Read;

pub(super) fn supports(kind: &str) -> bool {
    matches!(kind,"worker_settings.update" | "worker_identity.update" | "credential_account.import_file"
        | "credential_account.import_host")
}
pub(super) fn event(envelope: &Value, kind: &str, id: &str) -> Option<Value> {
    let command = envelope["command_type"].as_str()?;
    let event_type = match command {
        "worker_settings.update" => "core.worker.settings.updated",
        "worker_identity.update" => "core.worker.identity.updated",
        "credential_account.import_file" | "credential_account.import_host" => "core.credential.account.import_requested",
        _ => return None,
    };
    let payload = if command.starts_with("credential_account.") {
        json!({"account_id":id,"engine":envelope["payload"]["engine"],"source":if command.ends_with("import_file"){"file"}else{"host_login"}})
    } else {
        let mut fields: Vec<&str> = envelope["payload"].as_object().into_iter().flatten().map(|(key,_)|key.as_str()).collect();
        fields.sort_unstable();
        json!({"fields":fields})
    };
    Some(domain_events::event(envelope,kind,id,event_type,"platform.agent_runtime",payload))
}

fn source_file(raw: &str, limit: usize) -> Result<Vec<u8>,Failure> {
    if raw.trim().is_empty() { return Err(fail(StatusCode::BAD_REQUEST,"credential.path_required","A local file path is required")); }
    let expanded = if raw == "~" || raw.starts_with("~/") {
        std::env::var_os("HOME").map(PathBuf::from).ok_or_else(||fail(StatusCode::BAD_REQUEST,"credential.path_invalid","Home directory is unavailable"))?
            .join(raw.strip_prefix("~/").unwrap_or(""))
    } else { PathBuf::from(raw) };
    let invalid = ||fail(StatusCode::BAD_REQUEST,"credential.file_invalid",format!("Import requires a regular file containing 1–{limit} bytes"));
    if expanded.is_symlink() { return Err(invalid()); }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK); }
    let file = options.open(expanded).map_err(|_|invalid())?;
    let metadata = file.metadata().map_err(|_|invalid())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > limit as u64 { return Err(invalid()); }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).map_err(|_|invalid())?;
    if bytes.is_empty() || bytes.len() > limit { return Err(invalid()); }
    Ok(bytes)
}

pub(super) async fn command(state: Arc<AppState>, envelope: &Value) -> Result<Response,Failure> {
    let kind = envelope["command_type"].as_str().unwrap_or("");
    let payload = &envelope["payload"];
    if !payload.is_object() { return Err(fail(StatusCode::BAD_REQUEST,"platform.request.invalid","payload must be an object")); }
    let output = match kind {
        "worker_settings.update" | "worker_identity.update" => {
            let mut patch = payload.clone();
            if kind == "worker_identity.update" {
                patch.as_object_mut().unwrap().retain(|key,_|matches!(key.as_str(),"seats" | "credentials" | "worker_backend" | "worker_network" | "seat_alias" | "credential_alias"));
            }
            let Json(body) = crate::settings_metadata::put_workers(State(state.clone()),Json(patch)).await?;
            json!({"config":body["config"]})
        },
        "credential_account.import_file" => {
            let bytes = source_file(payload["path"].as_str().unwrap_or(""),2*1024*1024)?;
            let text = std::str::from_utf8(&bytes).map_err(|_|fail(StatusCode::BAD_REQUEST,"credential.file_encoding","Credential file must contain UTF-8 text"))?;
            let engine = payload["engine"].as_str().unwrap_or("").trim().to_lowercase();
            if !crate::agent_runtime::descriptors::engine_ids().iter().any(|known| *known == engine) {
                return Err(fail(StatusCode::BAD_REQUEST,"credential.engine_invalid","Unknown credential engine"));
            }
            let format = payload["format"].as_str().unwrap_or("api_key");
            if !matches!(format,"api_key" | "codex_auth_json") || format == "codex_auth_json" && engine != "codex" {
                return Err(fail(StatusCode::BAD_REQUEST,"credential.format_invalid","Use api_key, or codex_auth_json for Codex"));
            }
            let mut request = json!({"worker_engine":engine,"connection":payload["connection"].as_str().unwrap_or("official")});
            for field in ["create_only","expected_revision","base_url","provider","target_model","models","model_capabilities"] {
                if let Some(value) = payload.get(field) { request[field] = value.clone(); }
            }
            request[if format == "api_key" {"secret"}else{"codex_auth_json"}] = json!(if format == "api_key"{text.trim()}else{text});
            let id = envelope["aggregate_id"].as_str().unwrap_or("").to_owned();
            let Json(body) = crate::compat::put_credential_account(State(state.clone()),Path(id),Json(request)).await?;
            json!({"account":body["account"]})
        },
        "credential_account.import_host" => {
            let id = envelope["aggregate_id"].as_str().unwrap_or("");
            let engine = payload["engine"].as_str().unwrap_or("").trim().to_lowercase();
            let root = crate::host_import::host_root(&engine).map_err(crate::host_import::reply)?;
            let imported = crate::host_import::import(&state,&crate::native_chat::state_dir(&state),id,&engine,&root)
                .map_err(crate::host_import::reply)?;
            json!({"account":imported})
        },
        _ => return Err(fail(StatusCode::NOT_IMPLEMENTED,"capability.unavailable","Unknown settings operation")),
    };
    let id = envelope["aggregate_id"].as_str().unwrap_or("");
    let aggregate = envelope["aggregate_type"].as_str().unwrap_or("");
    let command_id = envelope["command_id"].as_str().unwrap_or("");
    let prior = state.store.get(COMMANDS,command_id).map_err(storage)?.unwrap_or(Value::Null);
    let cursor = domain_event_cursor(&state,aggregate,id,aggregate_version(&state,aggregate,id)?,envelope["_verified_scope"].as_str().unwrap_or("operator:local-user"))
        .map_err(storage)?;
    let receipt = json!({"schema_version":1,"command_id":command_id,"receipt_id":prior["response"]["receipt"]["receipt_id"],
        "state":"completed","aggregate":{"type":aggregate,"id":id},"run_id":null,"event_cursor":cursor,
        "submitted_at":prior["response"]["receipt"]["submitted_at"],"completed_at":now(),"deduplicated":false,"output":output,"effect_ids":[],"error":null});
    Ok(Json(json!({"receipt":receipt})).into_response())
}
