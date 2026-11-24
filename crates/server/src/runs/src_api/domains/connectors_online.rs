//! Public repository acquisition through the same scoped, approval-gated HTTP gateway.
use super::{
    Api, Error, connector_config, policy, rec, record_exchange, records, require,
    save_exchange_draft, save_rec, verify_ref,
};
use crate::AppState;
use axum::http::StatusCode;
use base64::Engine;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

pub(super) fn url(
    base: &str,
    provider: &str,
    repository: &str,
    branch: Option<&str>,
) -> Api<String> {
    let mut url =
        reqwest::Url::parse(base).map_err(|_| Error::invalid("repository base_url invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err(Error::invalid(
            "repository base_url must be a plain HTTP(S) origin or path",
        ));
    }
    let parts = repository.split('/').collect::<Vec<_>>();
    if parts.len() < 2
        || parts
            .iter()
            .any(|v| v.is_empty() || *v == "." || *v == ".." || v.contains(['?', '#', '\\']))
    {
        return Err(Error::invalid(
            "repository must be owner/name or group/project",
        ));
    }
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| Error::invalid("repository base_url has no path segments"))?;
        if provider == "github" {
            segments.push("repos");
            for part in &parts {
                segments.push(part);
            }
        } else {
            segments
                .push("api")
                .push("v4")
                .push("projects")
                .push(repository);
        }
        if let Some(branch) = branch {
            if provider == "github" {
                segments.push("commits").push(branch);
            } else {
                segments.push("repository").push("commits").push(branch);
            }
        }
    }
    Ok(url.to_string())
}
fn repository_request_headers(url: &reqwest::Url) -> &'static str {
    let github = url.host_str() == Some("api.github.com") || url.path().starts_with("/repos/");
    if github {
        "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28"
    } else {
        "Accept: application/json"
    }
}
fn repository_stable_id(meta: &Value, repository: &str) -> String {
    meta["id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| meta["id"].as_number().map(ToString::to_string))
        .unwrap_or_else(|| repository.to_owned())
}
fn request_bytes(url: &str) -> Api<Vec<u8>> {
    let parsed = reqwest::Url::parse(url).map_err(|_| Error::invalid("repository URL invalid"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::invalid("repository host missing"))?;
    let authority = if let Some(port) = parsed.port() {
        format!("{host}:{port}")
    } else {
        host.to_owned()
    };
    let path = if let Some(query) = parsed.query() {
        format!("{}?{query}", parsed.path())
    } else {
        parsed.path().to_owned()
    };
    let headers=repository_request_headers(&parsed);
    Ok(format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\n{headers}\r\nUser-Agent: Muteki-SRC/0.4.8\r\n\r\n").into_bytes())
}
fn draft(s: &AppState, id: &str, url: &str) -> Api<String> {
    let raw = request_bytes(url)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    let exchange = record_exchange(
        s,
        id,
        &json!({"request_raw_b64":encoded,"observed_url":url,
        "source_kind":"import","execution_state":"created"}),
    )?;
    let source = require(&exchange, "exchange_id")?;
    let saved = save_exchange_draft(
        s,
        id,
        &json!({"source_exchange_id":source,"base_revision":1,
        "raw_request_b64":encoded,"editor":"connector:repository"}),
    )?;
    Ok(require(&saved["draft"], "draft_id")?.to_owned())
}
pub(super) fn body_bytes(s: &AppState, id: &str, output: &Value) -> Api<(Value, Value)> {
    let receipt = &output["receipt"];
    let exchange_id = output["exchange_id"]
        .as_str()
        .or_else(|| receipt["external_receipt_ref"].as_str())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            Error::new(
                StatusCode::CONFLICT,
                "src.connector_response_missing",
                "repository HTTP receipt has no exchange",
            )
        })?;
    let exchange = rec(s, id, "http_exchange", exchange_id)?["payload"].clone();
    let status_code = exchange["status_code"].as_u64().unwrap_or(0);
    let reference = &exchange["response_raw_ref"];
    if !(200..300).contains(&status_code) {
        let mut msg = format!("connector HTTP {status_code}");
        if reference.is_object()
            && let Ok(artifact) = verify_ref(s, id, reference)
            && let Ok(raw) = base64::engine::general_purpose::STANDARD
                .decode(artifact["data"].as_str().unwrap_or(""))
            && let Ok(parsed) = super::exchange_queries::parsed_response(
                &raw,
                exchange["method"].as_str().unwrap_or("GET"),
            )
            && let Ok((body, _)) = super::exchange_queries::decoded_body(
                &parsed,
                exchange["method"].as_str().unwrap_or("GET"),
            )
            && let Ok(doc) = serde_json::from_slice::<Value>(&body)
            && let Some(err_detail) = doc["errmsg"]
                .as_str()
                .or_else(|| doc["error_message"].as_str())
                .or_else(|| doc["message"].as_str())
                .or_else(|| doc["error"].as_str())
                .filter(|s| !s.trim().is_empty())
        {
            msg = format!("{msg}: {err_detail}");
        }
        return Err(Error::new(
            StatusCode::BAD_GATEWAY,
            "src.connector_upstream_http",
            msg,
        ));
    }
    let artifact = verify_ref(s, id, reference)?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(artifact["data"].as_str().unwrap_or(""))
        .map_err(|_| Error::invalid("repository response capture invalid"))?;
    let parsed=super::exchange_queries::parsed_response(&raw,exchange["method"].as_str().unwrap_or("GET"))?;
    let (body,limitations)=super::exchange_queries::decoded_body(&parsed,exchange["method"].as_str().unwrap_or("GET"))?;
    if !limitations.is_empty(){return Err(Error::new(StatusCode::BAD_GATEWAY,"src.connector_upstream_invalid",format!("connector body decoding failed: {}",limitations.join(","))))}
    let document:Value=serde_json::from_slice(&body).map_err(|_|Error::new(StatusCode::BAD_GATEWAY,"src.connector_upstream_invalid","repository response is not JSON"))?;
    if !document.is_object() && !document.is_array() {
        return Err(Error::new(
            StatusCode::BAD_GATEWAY,
            "src.connector_upstream_invalid",
            "connector response must be a JSON object or array",
        ));
    }
    Ok((document, reference.clone()))
}
fn call_row(s: &AppState, id: &str, key: &str) -> Api<Option<Value>> {
    Ok(records(s, id, "connector_repository_call")?
        .into_iter()
        .find(|v| v["natural_key"] == key))
}
pub(super) async fn repository(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let provider = require(body, "provider").or_else(|_| require(body, "connector_id"))?;
    if !matches!(provider, "github" | "gitlab") {
        return Err(Error::invalid(
            "repository provider must be github or gitlab",
        ));
    }
    let key = require(body, "idempotency_key")?;
    let config = connector_config(
        &s,
        id,
        &json!({"connector_id":provider,"version":body["config_version"]}),
    )?;
    if config["version"].as_u64().unwrap_or(0) == 0 {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.connector_config_missing",
            "saved repository config required",
        ));
    }
    let settings = &config["config"];
    let resolved_repository = body["repository"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let o = body["owner"].as_str().map(str::trim).filter(|v| !v.is_empty())?;
            let r = body["repo"].as_str().map(str::trim).filter(|v| !v.is_empty())?;
            Some(format!("{o}/{r}"))
        })
        .or_else(|| {
            settings["repository"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            let o = settings["owner"].as_str().map(str::trim).filter(|v| !v.is_empty())?;
            let r = settings["repo"].as_str().map(str::trim).filter(|v| !v.is_empty())?;
            Some(format!("{o}/{r}"))
        })
        .ok_or_else(|| Error::invalid("repository (or owner and repo) is required"))?;
    let repository = resolved_repository.as_str();
    let token_ref = settings["token_ref"]
        .as_str()
        .or_else(|| settings["credential_ref"].as_str())
        .filter(|v| !v.is_empty());
    let credential_bindings = token_ref.map_or_else(
        || json!([]),
        |reference| {
            json!([{"location":"header",
            "name":if provider=="github"{"Authorization"}else{"PRIVATE-TOKEN"},
            "format":if provider=="github"{"bearer"}else{"raw"},
            "reference":reference}])
        },
    );
    let base = settings["base_url"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(if provider == "github" {
            "https://api.github.com"
        } else {
            "https://gitlab.com"
        });
    let metadata_url = url(base, provider, repository, None)?;
    let policy = policy(&s, id)?;
    let policy_version = policy["version"].as_u64().unwrap_or(0);
    let natural = format!("{provider}:{key}");
    let request_digest = crate::runs::src_api::digest(&json!([
        id,
        provider,
        repository,
        base,
        config["version"],
        body
    ]));
    let mut row = call_row(&s, id, &natural)?;
    if row
        .as_ref()
        .is_some_and(|v| v["payload"]["request_digest"] != request_digest)
    {
        return Err(Error::conflict(
            "repository idempotency key used for different input",
        ));
    }
    if row.is_none() {
        let draft_id = draft(&s, id, &metadata_url)?;
        let value = json!({"provider":provider,"repository":repository,"config_version":config["version"],
            "metadata_url":metadata_url,"metadata_draft_id":draft_id,"policy_version":policy_version,
            "request_digest":request_digest,"state":"prepared"});
        row = Some(save_rec(
            &s,
            id,
            "connector_repository_call",
            &natural,
            value,
            Some(0),
        )?);
    }
    let mut call = row.unwrap()["payload"].clone();
    let saved_policy = call["policy_version"].as_u64().unwrap_or(0);
    let metadata = super::egress::execute_connector_draft(
        s.clone(),
        id,
        &json!({
            "draft_id": call["metadata_draft_id"],
            "draft_revision": 1,
            "authorization_version": saved_policy,
            "idempotency_key": format!("repo:{key}:metadata"),
            "identity_ref": format!("connector:{provider}"),
            "context_id": format!("connector:{provider}"),
            "credential_bindings": credential_bindings,
            "budget": {"max_requests": 1}
        }),
        provider,
        base,
        "repository",
    )
    .await?["output"]
        .clone();
    if metadata["execution_state"] != "completed" {
        return Ok(
            json!({"external_state":metadata["execution_state"],"stage":"metadata",
            "execution_id":metadata["execution_id"],"approval":metadata["approval"],"receipt":metadata["receipt"]}),
        );
    }
    let (meta, meta_ref) = body_bytes(&s, id, &metadata)?;
    if !meta.is_object() {
        return Err(Error::new(
            StatusCode::BAD_GATEWAY,
            "src.connector_upstream_invalid",
            "repository response must be an object",
        ));
    }
    let branch = body["ref"]
        .as_str()
        .or_else(|| body["branch"].as_str())
        .or_else(|| settings["ref"].as_str())
        .or_else(|| meta["default_branch"].as_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or("HEAD");
    let commit_url = url(base, provider, repository, Some(branch))?;
    if call["commit_draft_id"].is_null() {
        let draft_id = draft(&s, id, &commit_url)?;
        call["commit_url"] = json!(commit_url);
        call["commit_draft_id"] = json!(draft_id);
        call["state"] = json!("metadata_completed");
        let saved = rec(&s, id, "connector_repository_call", &natural)?;
        save_rec(
            &s,
            id,
            "connector_repository_call",
            &natural,
            call.clone(),
            saved["revision"].as_u64(),
        )?;
    }
    let commit = super::egress::execute_connector_draft(
        s.clone(),
        id,
        &json!({
            "draft_id": call["commit_draft_id"],
            "draft_revision": 1,
            "authorization_version": saved_policy,
            "idempotency_key": format!("repo:{key}:commit"),
            "identity_ref": format!("connector:{provider}"),
            "context_id": format!("connector:{provider}"),
            "credential_bindings": credential_bindings,
            "budget": {"max_requests": 1}
        }),
        provider,
        base,
        "repository",
    )
    .await?["output"]
        .clone();
    if commit["execution_state"] != "completed" {
        return Ok(
            json!({"external_state":commit["execution_state"],"stage":"commit",
            "execution_id":commit["execution_id"],"approval":commit["approval"],"receipt":commit["receipt"]}),
        );
    }
    let (commit_doc, commit_ref) = body_bytes(&s, id, &commit)?;
    if !commit_doc.is_object() {
        return Err(Error::new(
            StatusCode::BAD_GATEWAY,
            "src.connector_upstream_invalid",
            "repository response must be an object",
        ));
    }
    let commit_sha = commit_doc["sha"]
        .as_str()
        .or_else(|| commit_doc["id"].as_str())
        .unwrap_or("");
    let mapped = json!({"type":"repository","provider":provider,
        "repository_id":repository_stable_id(&meta, repository),
        "full_name":meta["full_name"].as_str().or_else(||meta["path_with_namespace"].as_str()).unwrap_or(repository),
        "default_branch":branch,"commit_sha":commit_sha,
        "web_url":meta["html_url"].as_str().or_else(||meta["web_url"].as_str()),
        "visibility":meta["visibility"],"permissions":meta["permissions"],"active_operation_authorized":false});
    let batch_id = format!(
        "srepo_{}",
        &crate::runs::src_api::digest(&json!([id, provider, key]))[..24]
    );
    let mut batch = json!({"schema":"muteki.src.connector-batch.v1","batch_id":batch_id,"connector_id":provider,
        "config_version":config["version"],"task_id":id,"product_mode":"src","items":[{"input_key":format!("{provider}:{repository}@{commit_sha}"),"mapped":mapped,"raw":{"metadata":meta,"commit":commit_doc}}],
        "source_refs":[meta_ref,commit_ref],"errors":[],"acquisition":{"pages":2,"complete":true,"truncated":false},
        "cost":{"state":"unknown","actual":null},"external_state":"completed","status":"completed",
        "outcome":"completed","reconciliation_state":"unreconciled","created_at":super::now(),"repository":mapped});
    if let Ok(ingested) = super::ingestion::ingest_connector_batch_observations(&s, id, &batch, None) {
        batch["observation_counts"] = ingested["counts"].clone();
        batch["observations_ingested"] = json!(true);
    }
    if records(&s, id, "connector_batch")?
        .iter()
        .all(|v| v["natural_key"] != batch_id)
    {
        save_rec(&s, id, "connector_batch", &batch_id, batch.clone(), Some(0))?;
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_repository_ids_keep_numbers_or_text_and_github_headers_stay_local() {
        assert_eq!(repository_stable_id(&json!({"id": 9001001}), "acme/fallback"), "9001001");
        assert_eq!(repository_stable_id(&json!({"id": "stable-text-i65"}), "acme/fallback"), "stable-text-i65");
        assert_eq!(repository_stable_id(&json!({}), "acme/fallback"), "acme/fallback");
        let github = String::from_utf8(request_bytes("https://api.github.com/repos/acme/widget").unwrap()).unwrap();
        assert!(github.contains("Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28"), "{github}");
        assert!(github.starts_with("GET /repos/acme/widget HTTP/1.1\r\n"));
        let gitlab = String::from_utf8(request_bytes("https://gitlab.com/api/v4/projects/acme%2Fwidget").unwrap()).unwrap();
        assert!(gitlab.contains("Accept: application/json\r\n"));
        assert!(!gitlab.contains("X-GitHub-Api-Version"));
    }
}
